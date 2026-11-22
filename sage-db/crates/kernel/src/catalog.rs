//! Ordinary, Read-authorized current catalog and exact slug resolution.
//!
//! # What this is
//!
//! A client-facing **current-state read**: it lists the identified record families a
//! document adapter needs, and it resolves one exact Term slug. Both operations are ordinary
//! `Read` reads, not administrative exports.
//!
//! # What it is not
//!
//! Not a serializer, renderer, HTTP surface or search. Not a `SnapshotView` (that contract is
//! a closed four-family projection and is deliberately untouched). Not the physical
//! administrative scan (`Store::scan_current_records`), which stays ADMIN-only and must not be
//! repurposed for ordinary clients. Nothing here mints authority: the only permit is the
//! caller's own [`Grants`] value, checked against the actual KB.
//!
//! # The four guarantees this module adds on top of the Store seam
//!
//! 1. **Authorization first.** `Grants::authorize(actual_kb_uuid, Read)` runs before any
//!    record is visited, so a caller without `Read` learns nothing about the KB's contents —
//!    not even whether it holds records.
//! 2. **Import readiness before record visitation.** Persisted import bookkeeping is checked
//!    before the scan begins; an unfinished import refuses rather than returning a partial or
//!    empty-looking catalog.
//! 3. **One ledger.** The same caller `SharedLedger` pays authorization, readiness, traversal
//!    and visitor work. Nothing silently substitutes a fresh budget.
//! 4. **One durable basis.** Every result carries the [`ViewBasis`] it was read from, and both
//!    operations return exactly one basis — never a mix of publications.
//!
//! # Ordering, stated honestly
//!
//! `Store::scan_identified_records` iterates the fold's current index, which is populated in
//! commit order. **No stable-id or lexical ordering is promised**, so this module does not
//! advertise one: a visitor must not infer order from ids, and a consumer that needs an order
//! must impose it itself. Saying otherwise would be a promise the seam does not make.

use crate::{
    auth::{Capability, Grants},
    claims::{CommandError, ensure_import_ready},
};
use sage_db_store::{
    fold::FoldedRecord,
    fs::FileSystem,
    handle::Store,
    lease::LeaseLedger,
    view::{RecordScanError, ViewBasis},
};
use sage_db_types::framing::Refusal;
use sage_db_types::ledger::SharedLedger;
use sage_db_types::persist::StateKind;

/// Which identified families an ordinary catalog read may visit.
///
/// These are the six families a document adapter can act on. The remaining identified kinds
/// are not listed here because no ordinary client surface needs them yet; a caller that
/// genuinely needs another family should say so rather than have this module quietly widen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CatalogFamily {
    /// Concept terms.
    Term,
    /// Authored relations between terms.
    Assertion,
    /// Claims about terms.
    Claim,
    /// Reviewable proposals.
    Proposal,
    /// Activation-boundary entries.
    FrontierEntry,
    /// Session checkpoints.
    SessionCheckpoint,
}

impl CatalogFamily {
    /// Every family this module serves, in registry order.
    pub const ALL: [Self; 6] = [
        Self::Term,
        Self::Assertion,
        Self::Claim,
        Self::Proposal,
        Self::FrontierEntry,
        Self::SessionCheckpoint,
    ];

    /// The state kind this family folds into.
    #[must_use]
    pub const fn state_kind(self) -> StateKind {
        match self {
            Self::Term => StateKind::Term,
            Self::Assertion => StateKind::Assertion,
            Self::Claim => StateKind::Claim,
            Self::Proposal => StateKind::Proposal,
            Self::FrontierEntry => StateKind::FrontierEntry,
            Self::SessionCheckpoint => StateKind::SessionCheckpoint,
        }
    }

    /// The family a folded record belongs to, when this catalog serves it.
    ///
    /// Derived from the record's own variant rather than an identity copy, so no key is built
    /// and no variable-length natural key is cloned just to classify a row.
    #[must_use]
    pub fn of(record: &FoldedRecord) -> Option<Self> {
        match record {
            FoldedRecord::Term(_) => Some(Self::Term),
            FoldedRecord::Assertion(_) => Some(Self::Assertion),
            FoldedRecord::Claim(_) => Some(Self::Claim),
            FoldedRecord::Proposal(_) => Some(Self::Proposal),
            FoldedRecord::FrontierEntry(_) => Some(Self::FrontierEntry),
            FoldedRecord::SessionCheckpoint(_) => Some(Self::SessionCheckpoint),
            _ => None,
        }
    }

    /// Whether this catalog serves the record's family.
    #[must_use]
    pub fn includes(record: &FoldedRecord) -> bool {
        Self::of(record).is_some()
    }
}

/// An explicit activity filter.
///
/// Retired terms, retracted assertions and non-approved proposals are **source rows that
/// exist**. They are filtered only when the caller asks, never dropped silently: `All` shows
/// them, and a `Live` result still counts what it excluded in [`CatalogStats`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Activity {
    /// Only rows still in force.
    Live,
    /// Every row, including retired, retracted and rejected after-images.
    All,
}

impl Activity {
    /// Whether a folded record is in force under this filter.
    ///
    /// A proposal is "in force" when it is approved; pending/rejected/conflicted proposals are
    /// retained states rather than live knowledge. A term is live until retired; assertions and
    /// claims are live until retracted; frontier entries and checkpoints carry no retirement
    /// flag and are always live.
    #[must_use]
    pub fn keeps(self, record: &FoldedRecord) -> bool {
        if self == Self::All {
            return true;
        }
        match record {
            FoldedRecord::Term(r) => !r.retired,
            FoldedRecord::Assertion(r) => !r.retracted,
            FoldedRecord::Claim(r) => r.status == sage_db_types::persist::ClaimStatus::Accepted,
            FoldedRecord::Proposal(r) => r.status == PROPOSAL_STATUS_APPROVED,
            FoldedRecord::FrontierEntry(_) | FoldedRecord::SessionCheckpoint(_) => true,
            // Not a family this catalog serves; the family filter rejects it first.
            _ => true,
        }
    }
}

/// `ProposalStatus::Approved`, named as a constant so the comparison is type-checked rather
/// than a numeric literal.
const PROPOSAL_STATUS_APPROVED: sage_db_types::persist::ProposalStatus =
    sage_db_types::persist::ProposalStatus::Approved;

/// What one catalog visitation actually did.
///
/// Reported so a caller can tell "the catalog is empty" from "the filter excluded everything"
/// without guessing.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct CatalogStats {
    /// Records the fold offered.
    pub visited: u64,
    /// Records that passed both the family and activity filters.
    pub matched: u64,
    /// Records excluded because their family is not served by this read.
    pub excluded_family: u64,
    /// Records excluded by the activity filter.
    pub excluded_inactive: u64,
}

/// A catalog visitation outcome: the basis it was read from and what it did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CatalogRead {
    /// The single durable publication this read covered.
    pub basis: ViewBasis,
    /// Counts from the traversal.
    pub stats: CatalogStats,
}

/// Why a catalog read or slug lookup did not complete.
#[derive(Debug)]
pub enum CatalogError<E> {
    /// Authorization, import readiness, or a Store read refused the work.
    Command(CommandError),
    /// The caller's own visitor refused; its error is preserved verbatim and no partial result
    /// is reported.
    Visitor(E),
    /// A statistics counter would overflow.
    ///
    /// Unreachable with real data (it needs more than `u64::MAX` rows), but reported as a typed
    /// outcome instead of a saturated total that would silently break the invariant.
    Overflow {
        /// Which counter.
        counter: &'static str,
    },
    /// This module's own accounting invariant failed.
    Internal {
        /// Which invariant.
        invariant: &'static str,
    },
    /// A Term slug is claimed by more than one current Term.
    ///
    /// Slugs are the stable external identity and `data-model` §2 forbids reusing one, so two
    /// holders is a real contradiction. It is reported, never resolved first-wins.
    AmbiguousSlug {
        /// How many current Terms claim it.
        claimants: u64,
        /// The publication where the contradiction was observed.
        basis: ViewBasis,
    },
}

impl<E> From<CommandError> for CatalogError<E> {
    fn from(error: CommandError) -> Self {
        Self::Command(error)
    }
}

enum CatalogWalkError<E> {
    Visitor(E),
    Overflow(&'static str),
}

fn count_visit<E>(counter: &mut u64, name: &'static str) -> Result<(), CatalogWalkError<E>> {
    *counter = counter
        .checked_add(1)
        .ok_or(CatalogWalkError::Overflow(name))?;
    Ok(())
}

/// Map a Store scan failure onto this module's error, preserving every distinguished class.
fn scan_error<E>(error: RecordScanError<E>) -> CatalogError<E> {
    match error {
        RecordScanError::Incomplete(reason) => {
            CatalogError::Command(CommandError::Incomplete(reason))
        }
        RecordScanError::Read(read) => CatalogError::Command(CommandError::Record(read)),
        RecordScanError::Visitor(inner) => CatalogError::Visitor(inner),
    }
}

/// Visit the ordinary current catalog, family by family, on one durable basis.
///
/// The visitor receives the **existing** borrowed [`FoldedRecord`] and the **same**
/// `&mut dyn SharedLedger` the traversal is spending, so a caller that needs to charge its own
/// comparison or copy work does so against the real budget. Nothing is copied out of the seam
/// and no owned DTO is introduced, so no buffer can outlive the credit that paid for it.
///
/// # What the caller must do
///
/// If the visitor returns an error, **discard any partial output it built**: this function
/// preserves the visitor's exact error and deliberately returns no partial catalog.
///
/// # Errors
///
/// [`CatalogError::Command`] when authorization, import readiness or the Store read refuses
/// (including a genuine byte/entry/stop refusal from the caller's ledger), or
/// [`CatalogError::Visitor`] carrying the caller's own error unchanged.
pub fn visit_catalog<F, E>(
    store: &Store<F>,
    grants: &Grants<'_>,
    activity: Activity,
    ledger: &mut dyn LeaseLedger,
    mut visitor: impl FnMut(&FoldedRecord, &mut dyn SharedLedger) -> Result<(), E>,
) -> Result<CatalogRead, CatalogError<E>>
where
    F: FileSystem,
{
    // 1. Authorization FIRST, against the KB this Store actually is. A caller holding `Read`
    //    on a different KB is refused here and learns nothing about this one.
    grants
        .authorize(store.kb_uuid(), Capability::Read, ledger)
        .map_err(CommandError::from)?;
    // 2. Persisted import readiness BEFORE any record is visited: an unfinished import must
    //    refuse, never present as a small or empty catalog.
    ensure_import_ready(store, ledger)?;

    let mut stats = CatalogStats::default();

    // 3. One traversal, one ledger, one basis. `scan_identified_records` handles the fold walk,
    //    per-row control, retained-owner lookup and the missing-owner verdicts.
    // Counts are accumulated in raw `u64`s inside the walk and folded into the reported
    // statistics afterwards with checked carries, so overflow is a typed outer outcome rather
    // than something the visitor error type would have to represent.
    let mut visited: u64 = 0;
    let mut matched: u64 = 0;
    let mut excluded_family: u64 = 0;
    let mut excluded_inactive: u64 = 0;

    let basis = store
        .scan_identified_records(ledger, |record, scan_ledger| {
            count_visit(&mut visited, "visited")?;
            if !CatalogFamily::includes(record) {
                count_visit(&mut excluded_family, "excluded_family")?;
                return Ok(());
            }
            if !activity.keeps(record) {
                count_visit(&mut excluded_inactive, "excluded_inactive")?;
                return Ok(());
            }
            count_visit(&mut matched, "matched")?;
            // The visitor gets the SAME ledger the scan is using.
            visitor(record, scan_ledger).map_err(CatalogWalkError::Visitor)
        })
        .map_err(|error| match error {
            RecordScanError::Visitor(CatalogWalkError::Visitor(error)) => {
                CatalogError::Visitor(error)
            }
            RecordScanError::Visitor(CatalogWalkError::Overflow(counter)) => {
                CatalogError::Overflow { counter }
            }
            RecordScanError::Incomplete(reason) => scan_error(RecordScanError::Incomplete(reason)),
            RecordScanError::Read(error) => scan_error(RecordScanError::Read(error)),
        })?;

    stats.visited = visited;
    stats.matched = matched;
    stats.excluded_family = excluded_family;
    stats.excluded_inactive = excluded_inactive;
    // The invariant is only meaningful if the sum cannot wrap.
    let accounted = matched
        .checked_add(excluded_family)
        .and_then(|total| total.checked_add(excluded_inactive))
        .ok_or(CatalogError::Overflow {
            counter: "catalog statistics",
        })?;
    if accounted != visited {
        return Err(CatalogError::Internal {
            invariant: "catalog statistics do not reconcile with the visited count",
        });
    }

    Ok(CatalogRead { basis, stats })
}

/// Compare slugs using the declared content-work convention.
/// Length mismatch costs zero; each compared stored/probe pair costs two bytes.
/// Admission precedes each comparison and the walk stops at its first mismatch.
/// Control is checked even when both strings are empty.
fn slugs_equal_charged(
    stored: &str,
    probe: &str,
    ledger: &mut dyn SharedLedger,
) -> Result<bool, Refusal> {
    // 4. Every path begins with a checkpoint, including the zero-byte paths below.
    ledger.checkpoint()?;

    // 1. Length first: no byte is read (or charged) when the lengths already differ.
    if stored.len() != probe.len() {
        return Ok(false);
    }

    let stored_bytes = stored.as_bytes();
    let probe_bytes = probe.as_bytes();
    for (stored_byte, probe_byte) in stored_bytes.iter().zip(probe_bytes.iter()) {
        // 2. Charge BOTH bytes of this pair before reading them.
        ledger.try_charge(2)?;
        if stored_byte != probe_byte {
            // 3. First mismatch ends the walk; the remaining pairs cost nothing.
            return Ok(false);
        }
    }
    // Equal length and every pair matched.
    Ok(true)
}

/// Resolve one Term by its **exact** slug on one durable basis.
///
/// # Exactness
///
/// The comparison is byte-exact: no case folding, no Unicode normalization, no trimming. Slug
/// identity is case- and encoding-sensitive by contract, so `"Risk"` and `"risk"` are different
/// slugs and a caller must supply the stored spelling.
///
/// # Retirement
///
/// A retired Term still **owns** its slug — `data-model` §2 says a slug is never reused — so
/// resolution deliberately searches every current Term, retired or not, and reports the
/// record's own `retired` flag to the caller. Filtering retired Terms out here would make a
/// permanently reserved name look free, which is exactly the collision the rule forbids.
///
/// # Cost
///
/// Every candidate comparison charges the caller's ledger for **exactly** the pairs it examined,
/// under the convention documented on `slugs_equal_charged`: a length difference costs zero
/// content bytes, and each equal-length pair costs two bytes (stored + probe) charged before it is
/// read, stopping at the first mismatch. A checkpoint runs on every path. The slug is **borrowed**,
/// never copied.
///
/// # Errors
///
/// [`CatalogError::AmbiguousSlug`] when more than one current Term claims the slug, and
/// [`CatalogError::Command`] when authorization, readiness or a read refuses. Absence is
/// reported only on a complete, authorized traversal — never because something refused.
pub fn resolve_term_slug<'a, F>(
    store: &'a Store<F>,
    grants: &Grants<'_>,
    slug: &str,
    ledger: &mut dyn LeaseLedger,
) -> Result<SlugResolution<'a>, CatalogError<SlugScanError>>
where
    F: FileSystem,
{
    grants
        .authorize(store.kb_uuid(), Capability::Read, ledger)
        .map_err(CommandError::from)?;
    ensure_import_ready(store, ledger)?;

    resolve_term_slug_in_store(store, slug, ledger)
}

// Kernel commands can resolve their own endpoints after their operation-specific
// authorization and readiness gates, without requiring a separate Read grant.
pub(crate) fn resolve_term_slug_in_store<'a>(
    store: &'a Store<impl FileSystem>,
    slug: &str,
    ledger: &mut dyn SharedLedger,
) -> Result<SlugResolution<'a>, CatalogError<SlugScanError>> {
    let mut found: Option<&sage_db_types::persist::TermRecord> = None;
    let mut claimants: u64 = 0;

    // The visitor's own error type names exactly the two failures it can produce: the caller's
    // ledger refusing/stopping, or the claimant counter overflowing. Both stay distinguishable
    // from a missing record.
    let basis = store
        .scan_identified_records(ledger, |record, scan_ledger| -> Result<(), SlugScanError> {
            let FoldedRecord::Term(term) = record else {
                return Ok(());
            };
            // Compare explicitly so the ledger is charged for exactly the pairs examined.
            // See `slugs_equal_charged` for the whole convention.
            if !slugs_equal_charged(&term.slug, slug, scan_ledger)
                .map_err(SlugScanError::Refused)?
            {
                return Ok(());
            }
            // Checked, not saturating: a wrapped count would report the wrong number of
            // holders, so overflow is a typed outcome rather than a fabricated figure.
            claimants = claimants
                .checked_add(1)
                .ok_or(SlugScanError::ClaimantOverflow)?;
            if found.is_none() {
                // Borrow the retained owner; never copy the record.
                found = Some(term);
            }
            scan_ledger
                .checkpoint()
                .map_err(|stop| SlugScanError::Refused(Refusal::Stopped(stop)))?;
            Ok(())
        })
        .map_err(scan_error)?;

    match (claimants, found) {
        (0, _) => Ok(SlugResolution::Absent { basis }),
        (1, Some(term)) => Ok(SlugResolution::Found { term, basis }),
        // Two holders of a never-reusable name is a contradiction, not a tie to break.
        _ => Err(CatalogError::AmbiguousSlug { claimants, basis }),
    }
}

/// What the slug scan's visitor can fail with.
///
/// Two real outcomes, both distinct from "no such slug".
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SlugScanError {
    /// The caller's ledger refused the comparison work or asked to stop.
    Refused(Refusal),
    /// The claimant counter would overflow, so the number of holders cannot be reported.
    ClaimantOverflow,
}

/// The outcome of an exact slug lookup.
#[derive(Debug)]
pub enum SlugResolution<'a> {
    /// Exactly one current Term owns this slug.
    Found {
        /// The existing retained owner. No copy; `retired` is the caller's to inspect, because a
        /// retired Term still holds its slug permanently.
        term: &'a sage_db_types::persist::TermRecord,
        /// The publication that established it.
        basis: ViewBasis,
    },
    /// A complete authorized traversal found no current Term with this exact slug.
    Absent {
        /// The publication that established absence.
        basis: ViewBasis,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use sage_db_store::lease::AttemptLedger;
    use sage_db_types::query::RunToCompletion;

    /// A real attempt ledger, so every figure below is the actual counter's, not a mock.
    fn ledger() -> AttemptLedger<'static> {
        AttemptLedger::open(1 << 20, 1 << 20, &RunToCompletion).unwrap()
    }

    /// Bytes admitted while comparing, as the real counter reports them.
    fn charged(stored: &str, probe: &str) -> (bool, u64) {
        let mut l = ledger();
        let before = l.stats().admitted_bytes.unwrap_or(0);
        let equal = slugs_equal_charged(stored, probe, &mut l).unwrap();
        let after = l.stats().admitted_bytes.unwrap_or(0);
        (equal, after.saturating_sub(before) as u64)
    }

    // ---- C1/C2: the charge is exactly the pairs examined -------------------------------

    /// Equal slugs: every pair is read, so `2 * len` bytes are charged (stored + probe each).
    #[test]
    fn equal_slugs_charge_two_bytes_per_pair() {
        for slug in ["abc", "a", ""] {
            let (equal, bytes) = charged(slug, slug);
            assert!(equal, "{slug:?} must equal itself");
            assert_eq!(
                bytes,
                2 * slug.len() as u64,
                "{slug:?}: each examined pair costs two bytes"
            );
        }
    }

    /// A first-byte mismatch charges exactly one pair and stops.
    #[test]
    fn first_mismatch_charges_one_pair_and_stops() {
        let (equal, bytes) = charged("abc", "xbc");
        assert!(!equal);
        assert_eq!(bytes, 2, "only the first pair was read");
    }

    /// A length difference reads NO pair, so no content byte is charged.
    ///
    /// This is the exact defect the review named: the previous rule charged `min+1` here, a byte
    /// that was never examined.
    #[test]
    fn length_difference_charges_zero_content_bytes() {
        for (stored, probe) in [
            ("", "a"),
            ("a", ""),
            ("a", "abc"),
            ("abc", "a"),
            ("", "xyz"),
        ] {
            let (equal, bytes) = charged(stored, probe);
            assert!(!equal, "{stored:?} vs {probe:?} must differ");
            assert_eq!(
                bytes, 0,
                "{stored:?} vs {probe:?}: a length mismatch reads no byte"
            );
        }
    }

    /// Empty vs empty is equal and costs zero bytes — but still takes a checkpoint.
    #[test]
    fn empty_vs_empty_is_equal_and_free() {
        let (equal, bytes) = charged("", "");
        assert!(equal);
        assert_eq!(bytes, 0);
    }

    /// Every path takes a control checkpoint, including the zero-byte ones.
    ///
    /// Proven with a ledger whose control is already stopping: the comparison must report a
    /// stop rather than silently completing.
    #[test]
    fn every_path_checkpoints_including_empty() {
        struct StopNow;
        impl sage_db_types::query::QueryControl for StopNow {
            fn checkpoint(&self) -> Result<(), sage_db_types::query::ControlStop> {
                Err(sage_db_types::query::ControlStop::Cancelled)
            }
        }
        let mut l = AttemptLedger::open(1 << 20, 1 << 20, &StopNow).unwrap();
        // Length mismatch: checkpoint happens BEFORE the length test.
        assert!(matches!(
            slugs_equal_charged("a", "abc", &mut l),
            Err(Refusal::Stopped(_))
        ));
        // Equal length, empty: the loop body never runs, so the opening checkpoint is the only
        // thing that can report the stop.
        assert!(matches!(
            slugs_equal_charged("", "", &mut l),
            Err(Refusal::Stopped(_))
        ));
    }

    /// A ledger with no byte allowance refuses the comparison instead of comparing for free.
    #[test]
    fn an_exhausted_ledger_refuses_the_comparison() {
        let mut l = AttemptLedger::open(0, 1 << 20, &RunToCompletion).unwrap();
        match slugs_equal_charged("abc", "abc", &mut l) {
            Err(Refusal::Budget(error)) => {
                assert_eq!(error.unit(), sage_db_types::ledger::BudgetUnit::Bytes)
            }
            other => panic!("expected a byte refusal, got {other:?}"),
        }
    }
}
