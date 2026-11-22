//! Declared fault injection for the model backend.
//!
//! The model in [`crate::model`] is a *correct* backend: every operation succeeds and every sync
//! honours the accepted contract. Testing crash behaviour therefore needs a way to make it fail — but
//! a backend that silently changes behaviour under some flag is not evidence, because the test can no
//! longer say what it actually exercised.
//!
//! This module is the alternative: a fault schedule is **data**, injected at **named** points, and
//! the model records every injection it served so a test can assert which ones fired. A scenario
//! that expects a short write must name the short write; a scenario that never triggers its declared
//! fault will fail the "all declared faults fired" check rather than passing quietly.
//!
//! Everything here is pure bookkeeping. No I/O, no store, no commit API.

use std::collections::BTreeMap;

/// Where an injection may fire, named by the operation the seam performs.
///
/// The names are the semantics: `SyncFile` means "the next `sync_file` call", not "some flush".
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum FaultPoint {
    /// The next `create_new`.
    CreateNew,
    /// The next `write`.
    Write,
    /// The next `sync_file`.
    SyncFile,
    /// The next `sync_entry`.
    SyncEntry,
    /// The next `sync_dir`.
    SyncDir,
    /// The next `sync_parent_of`.
    SyncParentOf,
    /// The next `publish_no_overwrite`.
    Publish,
    /// The next `replace_current`.
    ReplaceCurrent,
    /// The next `hash_bounded`.
    Hash,
    /// The next `for_each_entry`.
    Enumerate,
    /// The next `remove`.
    Remove,
    /// The next `open_read`.
    OpenRead,
    /// The next `read_exact_into`.
    ReadExact,
}

/// What the injected failure does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Injection {
    /// Accept only `k` bytes of the write, then report success with a short count.
    ///
    /// This is a legal seam outcome: the accepted bytes are visible and nothing is durable.
    ShortWrite(usize),
    /// Report the operation as failed with no effect.
    FailNoEffect,
    /// Report the operation as failed with an **unknown** effect, **before** any state change.
    ///
    /// The seam cannot tell the caller whether the operation took effect, so the caller must
    /// resolve. This says nothing about whether the change actually happened.
    FailUnknown,
    /// Report the operation as failed with an **unknown** effect **after** the state change was
    /// applied to the model.
    ///
    /// This is the case that matters most and the one the earlier schedule never produced: the
    /// rename/flush *did* take effect and the caller still receives `Effect::Unknown`. A store that
    /// assumed "error means nothing happened" would be wrong here, and only this branch can show it.
    /// The error's effect is still `Unknown`, **not** `Entailed`: the caller has no durable
    /// acknowledgement, and turning the error into a success would be a different defect.
    FailUnknownAfterEffect,
    /// Report the operation as cancelled.
    Cancel,
    /// Report the operation as refused by the byte budget.
    BudgetBytes {
        /// Amount requested.
        requested: u64,
        /// Amount remaining.
        remaining: u64,
    },
    /// Report the operation as refused by the entry budget.
    BudgetEntries {
        /// Amount requested.
        requested: u64,
        /// Amount remaining.
        remaining: u64,
    },
}

/// A set of injections, keyed by the point they fire at, each with a remaining count.
///
/// A count of `n` makes the injection fire on the next `n` occurrences and then stop, which is how a
/// "fail twice then succeed" schedule is expressed without a stateful test body.
#[derive(Clone, Debug, Default)]
pub struct FaultSchedule {
    planned: BTreeMap<FaultPoint, Vec<Injection>>,
    fired: Vec<(FaultPoint, Injection)>,
}

/// Whether `injection` can be meaningfully applied at `point`.
///
/// A schedule that pairs an injection with a point that cannot produce it is a **test** bug, not a
/// backend behaviour: `ShortWrite` at a sync would be "fired" while the sync still succeeded, which
/// is the worst kind of coverage — a green scenario that never applied the fault. Combinations are
/// therefore rejected when the schedule is built.
///
/// Rules, each for a concrete reason:
///
/// * `ShortWrite` belongs only to `Write`. No other operation accepts a byte prefix.
/// * `BudgetBytes` / `BudgetEntries` belong to the operations that charge that unit: byte units to
///   `Hash` and `ReadExact`, entry units to `Enumerate`. Charging bytes at an enumeration would be a
///   fabricated unit.
/// * `Cancel` is accepted only where the seam has a control point (`Hash`, `Enumerate`).
/// * `FailNoEffect` / `FailUnknown` are accepted anywhere, because any operation can fail.
#[must_use]
pub const fn injection_applies(point: FaultPoint, injection: Injection) -> bool {
    match injection {
        Injection::ShortWrite(_) => matches!(point, FaultPoint::Write),
        Injection::BudgetBytes { .. } => {
            matches!(point, FaultPoint::Hash | FaultPoint::ReadExact)
        }
        Injection::BudgetEntries { .. } => matches!(point, FaultPoint::Enumerate),
        Injection::Cancel => matches!(point, FaultPoint::Hash | FaultPoint::Enumerate),
        // A plain failure can happen anywhere: any operation can fail.
        Injection::FailNoEffect | Injection::FailUnknown => true,
        // An after-effect failure is only meaningful where there IS a model state change to apply
        // first. Accepting it anywhere was a real defect: `sync_file` advertised acceptance, then
        // returned `Unknown` without making anything durable, so the "applied" case was never
        // exercised and the schedule silently lied about what it tested.
        // An after-effect failure is only meaningful where the model has a state change to apply
        // before raising it, so this list is exactly the points whose implementation performs the
        // mutation and then returns `Effect::Unknown`. Advertising more than is implemented was the
        // original defect: `sync_file` accepted the pairing and then returned before mutating, so
        // the "applied" case was never exercised.
        //
        // Every entry here must have a test that observes the mutation before the error. Points not
        // listed (Write, SyncEntry, SyncParentOf, Publish, Enumerate, Hash, ReadExact, Remove,
        // CreateNew, OpenRead) are rejected at construction rather than silently accepted.
        Injection::FailUnknownAfterEffect => matches!(
            point,
            FaultPoint::SyncFile | FaultPoint::SyncDir | FaultPoint::ReplaceCurrent
        ),
    }
}

impl FaultSchedule {
    /// An empty schedule: the backend behaves correctly.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// Arrange for `injection` to fire the next `times` times `point` is reached.
    ///
    /// # Panics
    ///
    /// Panics when `times` is zero, or when `injection` cannot be produced by `point`. Both are
    /// scenario bugs that would otherwise show up as a fault "firing" without changing behaviour.
    #[must_use]
    pub fn at(mut self, point: FaultPoint, injection: Injection, times: usize) -> Self {
        assert!(times > 0, "a fault scheduled zero times can never fire");
        assert!(
            injection_applies(point, injection),
            "{injection:?} cannot occur at {point:?}: it would be consumed without changing behaviour"
        );
        let slot = self.planned.entry(point).or_default();
        for _ in 0..times {
            slot.push(injection);
        }
        self
    }

    /// Ask whether `point` should be injected now, consuming one occurrence if so.
    ///
    /// Returns the injection to apply, or `None` when the backend should behave correctly.
    pub fn take(&mut self, point: FaultPoint) -> Option<Injection> {
        let slot = self.planned.get_mut(&point)?;
        if slot.is_empty() {
            return None;
        }
        let injection = slot.remove(0);
        self.fired.push((point, injection));
        Some(injection)
    }

    /// Look at what would fire at `point` without consuming it.
    ///
    /// Needed by the apply-then-error branch: the model must know, *before* mutating, whether the
    /// pending injection expects the effect to have happened, and it must not consume the injection
    /// until the operation completes.
    #[must_use]
    pub fn peek(&self, point: FaultPoint) -> Option<Injection> {
        self.planned.get(&point).and_then(|v| v.first().copied())
    }

    /// Every injection that actually fired, in order.
    #[must_use]
    pub fn fired(&self) -> &[(FaultPoint, Injection)] {
        &self.fired
    }

    /// Injections still waiting to fire.
    #[must_use]
    pub fn pending(&self, point: FaultPoint) -> usize {
        self.planned.get(&point).map_or(0, Vec::len)
    }

    /// Injections that were declared but never fired.
    ///
    /// A scenario asserting this is empty proves every fault it declared was actually exercised.
    /// Without such an assertion, a schedule whose operations stop early silently tests less than it
    /// appears to.
    #[must_use]
    pub fn unfired(&self) -> Vec<FaultPoint> {
        self.planned
            .iter()
            .filter(|(_, v)| !v.is_empty())
            .map(|(k, _)| *k)
            .collect()
    }

    /// Whether every declared injection fired.
    #[must_use]
    pub fn all_fired(&self) -> bool {
        self.unfired().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::{FaultPoint, FaultSchedule, Injection};

    #[test]
    fn injections_fire_in_declared_order_and_then_stop() {
        let mut s = FaultSchedule::none()
            .at(FaultPoint::Write, Injection::ShortWrite(2), 1)
            .at(FaultPoint::SyncDir, Injection::FailUnknown, 1);

        assert_eq!(s.take(FaultPoint::Write), Some(Injection::ShortWrite(2)));
        assert_eq!(
            s.take(FaultPoint::Write),
            None,
            "a one-shot injection must not fire twice"
        );
        assert_eq!(s.take(FaultPoint::SyncDir), Some(Injection::FailUnknown));
        assert!(s.all_fired());
        assert_eq!(s.fired().len(), 2);
    }

    #[test]
    fn unfired_reports_what_the_scenario_never_reached() {
        let s = FaultSchedule::none().at(FaultPoint::Publish, Injection::FailNoEffect, 1);
        assert_eq!(s.unfired(), vec![FaultPoint::Publish]);
        assert!(!s.all_fired());
    }

    #[test]
    #[should_panic(expected = "scheduled zero times")]
    fn zero_times_is_refused() {
        let _ = FaultSchedule::none().at(FaultPoint::Write, Injection::Cancel, 0);
    }

    /// The two budget refusals must stay distinguishable at the injection site.
    #[test]
    fn budget_injections_keep_their_unit() {
        let mut s = FaultSchedule::none()
            .at(
                FaultPoint::Hash,
                Injection::BudgetBytes {
                    requested: 10,
                    remaining: 0,
                },
                1,
            )
            .at(
                FaultPoint::Enumerate,
                Injection::BudgetEntries {
                    requested: 1,
                    remaining: 0,
                },
                1,
            );
        assert!(matches!(
            s.take(FaultPoint::Hash),
            Some(Injection::BudgetBytes { .. })
        ));
        assert!(matches!(
            s.take(FaultPoint::Enumerate),
            Some(Injection::BudgetEntries { .. })
        ));
    }

    /// `FailNoEffect` and `FailUnknown` are different claims about the world.
    #[test]
    fn no_effect_and_unknown_are_distinct_injections() {
        let mut s = FaultSchedule::none()
            .at(FaultPoint::Publish, Injection::FailNoEffect, 1)
            .at(FaultPoint::Publish, Injection::FailUnknown, 1);
        assert_eq!(s.take(FaultPoint::Publish), Some(Injection::FailNoEffect));
        assert_eq!(s.take(FaultPoint::Publish), Some(Injection::FailUnknown));
        assert_ne!(Injection::FailNoEffect, Injection::FailUnknown);
    }
}

#[cfg(test)]
mod validity_tests {
    use super::{FaultPoint, FaultSchedule, Injection, injection_applies};

    /// `ShortWrite` at a sync is the exact combination that silently did nothing.
    #[test]
    #[should_panic(expected = "cannot occur at")]
    fn short_write_at_a_sync_is_rejected() {
        let _ = FaultSchedule::none().at(FaultPoint::SyncFile, Injection::ShortWrite(1), 1);
    }

    /// A byte budget at an enumeration would charge the wrong unit.
    #[test]
    #[should_panic(expected = "cannot occur at")]
    fn byte_budget_at_enumeration_is_rejected() {
        let _ = FaultSchedule::none().at(
            FaultPoint::Enumerate,
            Injection::BudgetBytes {
                requested: 1,
                remaining: 0,
            },
            1,
        );
    }

    /// An entry budget at a hash would likewise be the wrong unit.
    #[test]
    #[should_panic(expected = "cannot occur at")]
    fn entry_budget_at_hash_is_rejected() {
        let _ = FaultSchedule::none().at(
            FaultPoint::Hash,
            Injection::BudgetEntries {
                requested: 1,
                remaining: 0,
            },
            1,
        );
    }

    /// Cancellation is only meaningful where the seam has a control point.
    #[test]
    #[should_panic(expected = "cannot occur at")]
    fn cancel_at_a_write_is_rejected() {
        let _ = FaultSchedule::none().at(FaultPoint::Write, Injection::Cancel, 1);
    }

    /// The valid pairings must all be accepted, or the guard would be useless.
    #[test]
    fn valid_pairings_are_accepted() {
        assert!(injection_applies(
            FaultPoint::Write,
            Injection::ShortWrite(1)
        ));
        assert!(injection_applies(
            FaultPoint::Hash,
            Injection::BudgetBytes {
                requested: 1,
                remaining: 0
            }
        ));
        assert!(injection_applies(
            FaultPoint::Enumerate,
            Injection::BudgetEntries {
                requested: 1,
                remaining: 0
            }
        ));
        assert!(injection_applies(FaultPoint::Enumerate, Injection::Cancel));
        assert!(injection_applies(
            FaultPoint::SyncFile,
            Injection::FailUnknown
        ));
        assert!(injection_applies(
            FaultPoint::ReplaceCurrent,
            Injection::FailNoEffect
        ));
    }

    /// No operation may consume a fault belonging to another point.
    ///
    /// The cross-point defect was a `Publish` lookup inside `replace_current`; this asserts the
    /// schedule bookkeeping stays point-local, which is the invariant that made it visible.
    #[test]
    fn a_point_only_consumes_its_own_faults() {
        let mut s = FaultSchedule::none().at(FaultPoint::Publish, Injection::FailUnknown, 1);
        assert_eq!(
            s.take(FaultPoint::ReplaceCurrent),
            None,
            "a publish fault must not be consumed by a root replace"
        );
        assert_eq!(s.pending(FaultPoint::Publish), 1);
        assert_eq!(s.take(FaultPoint::Publish), Some(Injection::FailUnknown));
    }
}
