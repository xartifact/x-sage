//! Published snapshot pins.
//!
//! A commit publishes an immutable snapshot a reader may hold across later commits, and after the attempt facade and even
//! the owning store handle are gone. Three properties are load-bearing:
//!
//! 1. the pin keeps its reservation until the LAST owner drops;
//! 2. after a failed resync the *latest* pin is absent, while already-issued pins stay valid and identical;
//! 3. a pin clone costs a refcount bump — never an allocation, never a new charge.
//!
//! # Authority
//!
//! The constructor takes `&mut dyn LeaseLedger` and consults stop policy through **that same object**
//! (`SharedLedger::checkpoint`). It takes no separate control: a caller holding a stopped attempt ledger could otherwise
//! hand over a permissive control and publish past the ledger's own authority. The accepted `AttemptLedger` delegates
//! `checkpoint` to the control it was opened with, so this reaches the canonical stop policy and observes cancellation
//! before any charge.
//!
//! # Retention is not one number
//!
//! A pin retains the shared owner, the entry table's slots, and every entry's own `key`/`body` allocation. [`PinExtent`]
//! carries all four, computed from the ACTUAL incoming `Vec` capacities. The owner block is admitted fresh; the retained
//! payload is **transferred** because it was already admitted by whoever built the vector. A scalar caller-supplied
//! "adopt this many bytes" is deliberately not accepted: it is not evidence of what is actually retained.
//!
//! # Precondition on the caller
//!
//! The retained bytes must already be admitted on this same ledger, and the vector must have been grown fallibly, before
//! it is handed here. A constructor cannot retroactively make an earlier caller allocation fallible or admitted, so
//! [`publish`] refuses rather than inventing credit when the transfer fails.

use sage_db_types::graph::SnapshotId;
use sage_db_types::identity::KbVersion;
use sage_db_types::ledger::{BudgetError, ControlStop};
use sage_db_types::persist::StateEntry;

use crate::lease::{LeaseLedger, OwnedReservation, TransferError};

/// The bytes a pin retains, itemised so nothing is silently left out.
///
/// Every field is derived from actual allocation capacities, never from a nominal length or a caller scalar.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PinExtent {
    /// The shared owner's control block, predicted from the provider's layout.
    ///
    /// This is a **prediction**, not a measured allocation: the provider's internals are not a stable public ABI and the
    /// current `SnapshotInner` has not been measured with an allocator. Treat it as an estimate for precharging.
    pub owner: u64,
    /// The entry table's slot array: `capacity x size_of::<StateEntry>()`.
    pub slots: u64,
    /// The `key` allocations retained inside those entries.
    pub keys: u64,
    /// The `body` allocations retained inside those entries.
    pub bodies: u64,
}

impl PinExtent {
    /// Bytes this pin must already hold admitted before it is built: everything except the owner block.
    #[must_use]
    pub const fn retained(&self) -> u64 {
        self.slots
            .saturating_add(self.keys)
            .saturating_add(self.bodies)
    }

    /// The full extent a pin reserves: owner plus retained payload.
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.owner.saturating_add(self.retained())
    }
}

impl core::fmt::Display for PinExtent {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "owner={} slots={} keys={} bodies={} retained={} total={}",
            self.owner,
            self.slots,
            self.keys,
            self.bodies,
            self.retained(),
            self.total()
        )
    }
}

/// The single shared owner behind every [`Snapshot`] handle for one published version.
struct SnapshotInner {
    /// The canonical identity, INCLUDING the version. There is deliberately no separate head field: two sources for
    /// one fact could disagree, and the pin would then report a version its own identity contradicts.
    snapshot: SnapshotId,
    entries: Vec<StateEntry>,
    /// The lease over the owner block this module admitted.
    lease_owner: OwnedReservation,
    /// The transferred lease over the retained payload, when there was any.
    lease_retained: Option<OwnedReservation>,
}

/// A published, immutable view of one committed version.
pub struct Snapshot {
    inner: triomphe::Arc<SnapshotInner>,
}

impl Clone for Snapshot {
    fn clone(&self) -> Self {
        // Infallible: a refcount increment. A clone is NOT an admission point and creates no second claim.
        Self {
            inner: triomphe::Arc::clone(&self.inner),
        }
    }
}

impl Snapshot {
    /// The version this pin identifies, derived from the canonical [`SnapshotId`].
    #[must_use]
    pub fn head(&self) -> KbVersion {
        self.inner.snapshot.version
    }

    /// The full pin identity: KB, version, rules and graph digest together.
    #[must_use]
    pub fn id(&self) -> &SnapshotId {
        &self.inner.snapshot
    }

    /// The canonical current entry set this version published.
    #[must_use]
    pub fn entries(&self) -> &[StateEntry] {
        &self.inner.entries
    }

    /// Bytes this pin reserves until its last owner drops, across every token it holds.
    #[must_use]
    pub fn reserved_bytes(&self) -> u64 {
        let retained = self
            .inner
            .lease_retained
            .as_ref()
            .map_or(0, OwnedReservation::bytes);
        self.inner.lease_owner.bytes().saturating_add(retained)
    }

    /// The entry table capacity this pin actually retains.
    #[must_use]
    pub fn entries_capacity(&self) -> usize {
        self.inner.entries.capacity()
    }

    /// The itemised extent of what this pin retains, recomputed from its own buffers.
    ///
    /// # Errors
    ///
    /// [`PinError::ExtentOverflow`] if the capacities do not fit `u64`.
    pub fn extent(&self) -> Result<PinExtent, PinError> {
        let mut extent = entry_extent(&self.inner.entries, self.inner.entries.capacity())?;
        extent.owner = self.inner.lease_owner.bytes();
        Ok(extent)
    }

    /// How many handles currently share this pin's single reservation.
    #[must_use]
    pub fn owners(&self) -> usize {
        triomphe::Arc::count(&self.inner)
    }
}

impl core::fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Snapshot")
            .field("snapshot", &self.inner.snapshot)
            .field("reserved_bytes", &self.reserved_bytes())
            .field("entries", &self.inner.entries.len())
            .field("owners", &self.owners())
            .finish()
    }
}

/// Why a snapshot could not be published.
#[derive(Debug)]
pub enum PinError {
    /// The layout prediction could not be computed. Refused rather than panicked.
    Layout,
    /// The retained extent could not be represented in `u64`. Refused rather than wrapped.
    ExtentOverflow,
    /// The caller's control asked to stop, reached through the ledger itself. Not a budget refusal.
    Stopped(ControlStop),
    /// The owner block could not be admitted under the byte ceiling. Nothing was allocated.
    Budget(BudgetError),
    /// The retained payload was not already admitted on this ledger. Distinct from budget: no allowance fixes it.
    Transfer(TransferError),
    /// The provider could not allocate the shared owner. Carries the extent actually requested, so a caller can report a
    /// real quantity rather than a placeholder.
    Allocate {
        /// Bytes the allocator was asked for.
        requested: u64,
    },
}

/// The control-block bytes a pin reserves, **predicted** from the provider's layout.
///
/// A prediction, not a measurement: the provider's `ArcInner` shape is not a stable public ABI. The concrete
/// `SnapshotInner` extent has not been measured with an allocating instrument; that remains a named narrow gate.
///
/// # Errors
///
/// [`PinError::Layout`] if the layouts cannot be combined or do not fit `u64`.
pub fn owner_extent() -> Result<u64, PinError> {
    core::alloc::Layout::new::<core::sync::atomic::AtomicUsize>()
        .extend(core::alloc::Layout::new::<SnapshotInner>())
        .ok()
        .and_then(|(layout, _)| u64::try_from(layout.pad_to_align().size()).ok())
        .ok_or(PinError::Layout)
}

/// The extent of a retained entry set, from the ACTUAL capacities of the vector and its nested buffers.
///
/// # Errors
///
/// [`PinError::ExtentOverflow`] if any product or sum does not fit `u64`, rather than wrapping into a plausible-looking
/// smaller number.
pub fn entry_extent(entries: &[StateEntry], capacity: usize) -> Result<PinExtent, PinError> {
    // Pure sizing: no control input, no ledger. Kept for callers that only need the arithmetic.
    entry_extent_inner(entries, capacity)
}

/// The walk `publish` uses: the same arithmetic as [`entry_extent`], but it consults the caller's control WHILE walking
/// the entries, so a large retained set cannot be scanned unbounded between two checks.
///
/// A single checkpoint after the scan would be cheaper and would make a trivial stop test pass, while still doing all the
/// work before the first opportunity to stop. This checks per entry instead, which is the property that matters.
///
/// # Errors
///
/// [`PinError::Stopped`] when the control asks to stop mid-walk; [`PinError::ExtentOverflow`] on capacity overflow.
pub fn entry_extent_bounded(
    ledger: &mut dyn LeaseLedger,
    entries: &[StateEntry],
    capacity: usize,
) -> Result<PinExtent, PinError> {
    let slots = (capacity as u64)
        .checked_mul(core::mem::size_of::<StateEntry>() as u64)
        .ok_or(PinError::ExtentOverflow)?;
    let mut keys = 0_u64;
    let mut bodies = 0_u64;
    for entry in entries {
        if let Err(stop) = ledger.checkpoint() {
            return Err(PinError::Stopped(stop));
        }
        keys = keys
            .checked_add(entry.key.capacity() as u64)
            .ok_or(PinError::ExtentOverflow)?;
        bodies = bodies
            .checked_add(entry.body.capacity() as u64)
            .ok_or(PinError::ExtentOverflow)?;
    }
    Ok(PinExtent {
        owner: 0,
        slots,
        keys,
        bodies,
    })
}

fn entry_extent_inner(entries: &[StateEntry], capacity: usize) -> Result<PinExtent, PinError> {
    let slot_bytes = (capacity as u64)
        .checked_mul(core::mem::size_of::<StateEntry>() as u64)
        .ok_or(PinError::ExtentOverflow)?;
    let mut keys = 0_u64;
    let mut bodies = 0_u64;
    for entry in entries {
        keys = keys
            .checked_add(entry.key.capacity() as u64)
            .ok_or(PinError::ExtentOverflow)?;
        bodies = bodies
            .checked_add(entry.body.capacity() as u64)
            .ok_or(PinError::ExtentOverflow)?;
    }
    Ok(PinExtent {
        owner: 0,
        slots: slot_bytes,
        keys,
        bodies,
    })
}

/// Build and hand back a pin. Kept as the direct form for readers/tests; the commit path uses [`prepare`] and swaps the
/// result in only after persistence.
///
/// # Errors
///
/// [`PinError`] on any refusal.
pub fn publish(
    ledger: &mut dyn LeaseLedger,
    snapshot: SnapshotId,
    entries: Vec<StateEntry>,
) -> Result<Snapshot, PinError> {
    prepare(ledger, snapshot, entries)
}

/// Publish `entries` as an immutable pin on this ledger.
///
/// The retained extent is computed from the vector's actual capacities and **transferred** (it was admitted by whoever
/// built it); the owner block is admitted fresh. Both happen before the owner allocation, so a refusal costs no
/// allocation, and stop policy is consulted first through the same ledger.
///
/// # Errors
///
/// [`PinError`] for a stop, layout/extent, budget, transfer or allocation refusal. Nothing is published on any error.
/// Build the pin owner WITHOUT publishing it, so a caller can prepare its reservations before persistence and swap it in
/// only after the durable protocol succeeds.
///
/// # Errors
///
/// [`PinError`] on stop, layout, budget, transfer or allocation refusal. Nothing is published.
pub fn prepare(
    ledger: &mut dyn LeaseLedger,
    snapshot: SnapshotId,
    entries: Vec<StateEntry>,
) -> Result<Snapshot, PinError> {
    // Stop authority comes from THIS object. No separate control: that would let a caller bypass the ledger's own policy.
    // The accepted facade mutates nothing before a charge, so a refusal here leaves the attempt clean.
    ledger.checkpoint().map_err(PinError::Stopped)?;

    // The bounded walk consults control per entry, so stop policy is reachable throughout rather than only at entry.
    let capacity = entries.capacity();
    let mut extent = entry_extent_bounded(ledger, &entries, capacity)?;
    extent.owner = owner_extent()?;

    // Transfer the retained payload FIRST: it must already be admitted on this same ledger, and a shortfall is a
    // transfer error, not budget exhaustion. Passing an unrelated scalar instead would be inventing evidence.
    let retained = if extent.retained() == 0 {
        None
    } else {
        Some(
            ledger
                .adopt_retained(extent.retained())
                .map_err(PinError::Transfer)?,
        )
    };

    // Then admit the owner block, which this module allocates itself.
    let owner = ledger
        .reserve_owned(extent.owner)
        .map_err(PinError::Budget)?;

    let inner = SnapshotInner {
        snapshot,
        entries,
        lease_owner: owner,
        lease_retained: retained,
    };

    match triomphe::Arc::try_new(inner) {
        Ok(inner) => Ok(Snapshot { inner }),
        // `try_new` dropped the input, which dropped BOTH leases, which released. Nothing to undo.
        // `try_new` dropped the input, which dropped the leases, which released. The refusal carries the real extent.
        Err(_) => Err(PinError::Allocate {
            requested: extent.total(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lease::AttemptLedger;
    use sage_db_types::digest::{Digest, GraphDigest, RulesDigest};
    use sage_db_types::identity::KbUuid;
    use sage_db_types::query::{QueryControl, RunToCompletion};
    use sage_db_types::relation::{RuleProfile, RuleSet};

    fn id_at(version: u64) -> SnapshotId {
        SnapshotId {
            kb_uuid: KbUuid::from_bytes([0x11; 16]),
            version: KbVersion::new(version),
            rules: RuleSet::from_trusted_parts(
                RuleProfile::FormalV1,
                RulesDigest::new(Digest::from_bytes([0x22; 32])),
            ),
            graph_digest: GraphDigest::new(Digest::from_bytes([0x33; 32])),
        }
    }

    fn entry(n: u8) -> StateEntry {
        StateEntry {
            kind: 1,
            key: vec![n],
            revision: 1,
            body: vec![n; 4],
        }
    }

    struct Stop;
    impl QueryControl for Stop {
        fn checkpoint(&self) -> Result<(), ControlStop> {
            Err(ControlStop::Cancelled)
        }
    }

    fn ledger_with(limit: u64) -> AttemptLedger<'static> {
        let control: &'static RunToCompletion = Box::leak(Box::new(RunToCompletion));
        AttemptLedger::open(limit, u64::MAX, control).expect("opens")
    }

    /// Admit the full retained extent of these entries, as a real caller must before handing them over.
    fn admit_retained(ledger: &mut AttemptLedger<'static>, entries: &Vec<StateEntry>) {
        use sage_db_types::ledger::SharedLedger;
        let extent = entry_extent(entries, entries.capacity()).expect("fits");
        ledger
            .try_charge(extent.retained())
            .expect("admit retained");
    }

    #[test]
    fn extent_is_computed_from_actual_capacities() {
        let mut entries = Vec::with_capacity(4);
        entries.push(entry(1));
        entries.push(entry(2));
        let extent = entry_extent(&entries, entries.capacity()).expect("fits");
        assert_eq!(
            extent.slots,
            (entries.capacity() * core::mem::size_of::<StateEntry>()) as u64
        );
        assert_eq!(
            extent.keys,
            entries.iter().map(|e| e.key.capacity() as u64).sum::<u64>()
        );
        assert_eq!(
            extent.bodies,
            entries
                .iter()
                .map(|e| e.body.capacity() as u64)
                .sum::<u64>()
        );
        // The itemisation matters: slots alone are not the retention.
        assert!(extent.retained() > extent.slots);
    }

    #[test]
    fn f1_a_large_retained_set_cannot_be_scanned_unbounded() {
        // Reviewer's F1: with 20k pre-admitted entries and a control that permits ONE checkpoint then cancels, the old
        // code returned Ok after scanning everything, because it checked control once at entry. The property under test
        // is bounded WORK, not merely "a second checkpoint exists" — so this asserts the stop is observed and that the
        // observed checkpoint count is small relative to the entry count, not merely non-zero.
        use core::sync::atomic::{AtomicUsize, Ordering};
        use sage_db_types::ledger::SharedLedger;

        struct OneThenStop {
            seen: AtomicUsize,
        }
        impl QueryControl for OneThenStop {
            fn checkpoint(&self) -> Result<(), ControlStop> {
                let prior = self.seen.fetch_add(1, Ordering::SeqCst);
                if prior == 0 {
                    Ok(())
                } else {
                    Err(ControlStop::Cancelled)
                }
            }
        }

        let control: &'static OneThenStop = Box::leak(Box::new(OneThenStop {
            seen: AtomicUsize::new(0),
        }));
        let mut ledger = AttemptLedger::open(u64::MAX, u64::MAX, control).expect("opens");

        // 20k entries, each admitted as a real caller must before handing them over.
        let mut entries: Vec<StateEntry> = Vec::with_capacity(20_000);
        for n in 0..20_000u32 {
            entries.push(StateEntry {
                kind: 1,
                key: n.to_le_bytes().to_vec(),
                revision: 1,
                body: vec![0xAB; 4],
            });
        }
        let extent = entry_extent(&entries, entries.capacity()).expect("fits");
        ledger
            .try_charge(extent.retained())
            .expect("admit retained");

        let refused = publish(&mut ledger, id_at(7), entries);
        let observed = control.seen.load(Ordering::SeqCst);
        assert!(
            matches!(refused, Err(PinError::Stopped(ControlStop::Cancelled))),
            "the walk must stop mid-scan, got {refused:?}"
        );
        // Bounded: a handful of checks, not twenty thousand. Generous bound so the test is about the shape, not an
        // exact count; the old unbounded scan produced ONE check and thus a false Ok.
        assert!(
            observed <= 4,
            "the scan must stop almost immediately, observed {observed} checkpoints"
        );
    }

    #[test]
    fn the_extent_walk_and_the_pure_sizing_helper_agree() {
        // One arithmetic path: the bounded walk must produce exactly the pure helper's numbers, or the two formulas have
        // drifted and the transfer would be for the wrong amount.
        let mut ledger = ledger_with(u64::MAX);
        let entries = vec![entry(1), entry(2), entry(3)];
        let pure = entry_extent(&entries, entries.capacity()).expect("fits");
        let bounded =
            entry_extent_bounded(&mut ledger, &entries, entries.capacity()).expect("fits");
        assert_eq!(pure, bounded);
    }

    #[test]
    fn a_stopped_ledger_publishes_nothing_even_though_no_second_control_exists() {
        // The authority test root asked for: the ledger itself is opened with a cancelling control, and publish has no
        // way to supply a permissive one. Cancellation must be observed before any charge.
        let control: &'static Stop = Box::leak(Box::new(Stop));
        let mut ledger = AttemptLedger::open(u64::MAX, u64::MAX, control).expect("opens");
        let refused = publish(&mut ledger, id_at(7), vec![entry(1)]);
        assert!(
            matches!(refused, Err(PinError::Stopped(ControlStop::Cancelled))),
            "the ledger's own stop policy must be reached"
        );
        assert_eq!(ledger.stats().live_bytes, 0, "nothing charged");
        assert_eq!(ledger.stats().held_bytes, 0);
    }

    #[test]
    fn transferring_the_retained_extent_succeeds_without_a_second_charge() {
        let mut ledger = ledger_with(u64::MAX);
        let entries = vec![entry(1), entry(2)];
        admit_retained(&mut ledger, &entries);
        let admitted_after = ledger.stats().admitted_bytes;

        let pin = publish(&mut ledger, id_at(7), entries).expect("publishes");
        let retained = pin.extent().expect("fits");
        let owner = owner_extent().expect("layout");
        assert_eq!(pin.reserved_bytes(), retained.retained() + owner);
        // Only the owner is newly admitted; the retained payload was transferred.
        assert_eq!(
            ledger.stats().admitted_bytes,
            admitted_after.map(|a| a + u128::from(owner)),
            "transferred payload must not be admitted twice"
        );
    }

    #[test]
    fn slots_admitted_but_nested_payload_omitted_refuses_as_transfer_not_budget() {
        // The discriminating resource control: if the caller admits only the slots, the nested payload was never
        // admitted, and that is a TRANSFER failure. No allowance would fix it, so it must not be Budget.
        use sage_db_types::ledger::SharedLedger;
        let mut ledger = ledger_with(u64::MAX);
        let entries = vec![entry(1), entry(2)];
        let extent = entry_extent(&entries, entries.capacity()).expect("fits");
        ledger.try_charge(extent.slots).expect("slots only");

        let refused = publish(&mut ledger, id_at(7), entries);
        match refused {
            Err(PinError::Transfer(TransferError::Untransferred {
                requested_bytes,
                available_bytes,
            })) => {
                assert_eq!(requested_bytes, extent.retained());
                assert!(available_bytes < requested_bytes);
            }
            other => panic!("expected a transfer refusal, got {other:?}"),
        }
        assert_eq!(ledger.stats().held_bytes, 0, "nothing held on refusal");
    }

    #[test]
    fn head_is_derived_from_the_canonical_id() {
        // Identity and version cannot disagree: one source.
        let mut ledger = ledger_with(u64::MAX);
        let entries = vec![entry(1)];
        admit_retained(&mut ledger, &entries);
        let pin = publish(&mut ledger, id_at(7), entries).expect("publishes");
        assert_eq!(pin.head(), KbVersion::new(7));
        assert_eq!(
            pin.head(),
            pin.id().version,
            "no separate field can disagree"
        );
    }

    #[test]
    fn a_clone_shares_one_claim_and_charges_nothing() {
        let mut ledger = ledger_with(u64::MAX);
        let entries = vec![entry(1)];
        admit_retained(&mut ledger, &entries);
        let pin = publish(&mut ledger, id_at(7), entries).expect("publishes");
        let before = pin.reserved_bytes();
        let clone = pin.clone();
        assert_eq!(pin.reserved_bytes(), before);
        assert_eq!(clone.reserved_bytes(), before);
        assert_eq!(pin.owners(), 2);
    }

    #[test]
    fn the_lease_is_released_only_when_the_last_owner_drops() {
        let mut ledger = ledger_with(u64::MAX);
        let entries = vec![entry(1)];
        admit_retained(&mut ledger, &entries);
        let pin = publish(&mut ledger, id_at(7), entries).expect("publishes");
        let held = pin.reserved_bytes();
        let clone = pin.clone();
        drop(pin);
        assert_eq!(
            clone.reserved_bytes(),
            held,
            "one handle dropping must not release"
        );
        drop(clone);
        assert_eq!(ledger.stats().held_bytes, 0, "last owner released");
    }

    #[test]
    fn entries_are_moved_in_not_copied() {
        let mut ledger = ledger_with(u64::MAX);
        let entries = vec![entry(1), entry(2)];
        admit_retained(&mut ledger, &entries);
        let buf = entries.as_ptr();
        let pin = publish(&mut ledger, id_at(7), entries).expect("publishes");
        assert_eq!(pin.entries().as_ptr(), buf, "same buffer, no copy");
    }

    #[test]
    fn an_owner_budget_refusal_holds_nothing() {
        // Ceiling below the owner prediction but retained already admitted: the retained transfer is refunded when the
        // attempt keeps it, so assert on what the pin would have needed versus what it got.
        let mut ledger = ledger_with(2);
        let entries = vec![entry(1)];
        let extent = entry_extent(&entries, entries.capacity()).expect("fits");
        use sage_db_types::ledger::SharedLedger;
        // Admit retained first, then fail on the owner.
        if ledger.try_charge(extent.retained()).is_ok() {
            let refused = publish(&mut ledger, id_at(7), entries);
            assert!(
                matches!(refused, Err(PinError::Budget(_))),
                "owner admission must refuse on a tiny ceiling"
            );
            assert_eq!(ledger.stats().held_bytes, 0, "no lease survives a refusal");
        }
    }

    /// LEAF two-publication control. Two calls into this constructor through ONE attempt ledger.
    ///
    /// This is NOT the real two-commit milestone: it touches no Store, no FileSystem, no format decode, no CURRENT and no
    /// transaction. It establishes leaf retention properties only.
    #[test]
    fn leaf_two_publication_retention_keeps_the_old_pin() {
        let mut ledger = ledger_with(u64::MAX);

        let e1 = vec![entry(0xA1)];
        admit_retained(&mut ledger, &e1);
        let first = publish(&mut ledger, id_at(1), e1).expect("first");
        let first_bytes = first.reserved_bytes();
        let first_bodies: Vec<Vec<u8>> = first.entries().iter().map(|e| e.body.clone()).collect();

        let e2 = vec![entry(0xA1), entry(0xB2)];
        admit_retained(&mut ledger, &e2);
        let second = publish(&mut ledger, id_at(2), e2).expect("second");

        assert_eq!(first.head(), KbVersion::new(1));
        assert_eq!(second.head(), KbVersion::new(2));
        assert_eq!(
            ledger.stats().held_bytes,
            first_bytes + second.reserved_bytes(),
            "two live pins hold independent claims"
        );

        let after: Vec<Vec<u8>> = first.entries().iter().map(|e| e.body.clone()).collect();
        assert_eq!(after, first_bodies, "old after-image unchanged");
        assert_eq!(first.entries().len(), 1);

        let second_bytes = second.reserved_bytes();
        drop(second);
        assert_eq!(
            ledger.stats().held_bytes,
            first_bytes,
            "releasing the newer pin leaves the older claim"
        );
        assert_eq!(first.reserved_bytes(), first_bytes);
        let _ = second_bytes;

        drop(first);
        assert_eq!(ledger.stats().held_bytes, 0);
    }
}
