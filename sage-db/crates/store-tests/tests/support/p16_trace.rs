//! Operation tracing and NORMATIVE step obligations for the P1-6 matrix.
//!
//! # Two different things, kept apart
//!
//! A **trace** is reachability DATA: which named seam operations actually occurred, in order, during one
//! real public-path scenario. It says what this build did; it never says what a build is *required* to do.
//!
//! An **obligation** is the normative requirement for that scenario: the mandatory step CLASSES and their
//! relative order (`storage.md` §3 commit sequence, §5.1 resolution set). A mutant that omits a mandatory
//! sync must therefore FAIL an obligation check. If obligations were derived from the trace, omitting a
//! sync would simply shrink the expectation and pass — the exact escape this module exists to prevent.
//!
//! Nothing here reads product state to decide what should have happened, and nothing normalizes an actual
//! outcome to match an expectation.

// Shared test support: each target compiles this module separately, so an item used by one target and
// not another is not dead code. Allowing it here keeps ONE surface instead of a per-target copy; it hides
// no defect, because every item is exercised by the target that needs it.
#![allow(dead_code)]
use std::cell::RefCell;
use std::rc::Rc;

/// One named seam operation, as actually invoked.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct TraceOp {
    /// Seam method name, exactly as the backend received it.
    pub method: &'static str,
    /// Entry name when the operation names one (`""` otherwise).
    pub name: String,
    /// Directory the operation applied to, as the model resolved it (`""` for the root).
    pub dir: String,
}

/// A recorded operation sequence.
#[derive(Clone, Default, Debug)]
pub(super) struct Trace {
    ops: Rc<RefCell<Vec<TraceOp>>>,
}

impl Trace {
    /// A fresh empty trace.
    #[must_use]
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Record one operation. Called by the delegating backend.
    pub(super) fn push(&self, method: &'static str, name: &str, dir: &str) {
        self.ops.borrow_mut().push(TraceOp {
            method,
            name: name.to_owned(),
            dir: dir.to_owned(),
        });
    }

    /// The operations recorded so far.
    #[must_use]
    pub(super) fn ops(&self) -> Vec<TraceOp> {
        self.ops.borrow().clone()
    }
}

/// A mandatory step class for one scenario.
///
/// `point` is the seam operation the step must use; `predicate` narrows it (e.g. "a publish into
/// `blobs`"). Classes are ordered: each must first-occur at or after the previous one.
#[derive(Clone, Copy, Debug)]
pub(super) struct RequiredStep {
    /// Human name for failure messages.
    pub label: &'static str,
    /// Which seam operation must occur.
    pub point: &'static str,
}

impl RequiredStep {
    /// A step required to use `point`.
    #[must_use]
    pub(super) const fn new(label: &'static str, point: &'static str) -> Self {
        Self { label, point }
    }
}

/// Why an obligation failed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) enum ObligationFailure {
    /// A required step class never occurred.
    Missing {
        /// The step's label.
        label: &'static str,
        /// The seam operation it needed.
        point: &'static str,
    },
    /// A required step first occurred before an earlier required step — the order is inverted.
    OutOfOrder {
        /// The step that came too early.
        label: &'static str,
        /// The step it should have followed.
        after: &'static str,
    },
}

/// Check that `steps` all occur in `trace`, in the declared order, each first occurring no earlier than
/// the previous one's first occurrence.
///
/// Absence is a FAILURE, never a licence to shrink the expectation. Returning `Ok` means every mandatory
/// class was observed in a legal order; it says nothing about extra operations, which the caller may
/// constrain separately.
///
/// # Errors
///
/// [`ObligationFailure::Missing`] when a class never occurs;
/// [`ObligationFailure::OutOfOrder`] when it occurs only before a prerequisite.
pub(super) fn check_obligations(
    trace: &[TraceOp],
    steps: &[RequiredStep],
) -> Result<(), ObligationFailure> {
    let mut cursor = 0usize;
    let mut previous: Option<&'static str> = None;
    for step in steps {
        let found = trace
            .iter()
            .enumerate()
            .skip(cursor)
            .find(|(_, op)| op.method == step.point)
            .map(|(index, _)| index);
        match found {
            Some(index) => {
                cursor = index;
                previous = Some(step.label);
            }
            None => {
                // Distinguish "never at all" from "only before its prerequisite": the second is an
                // ordering defect and deserves its own message.
                let exists_earlier = trace.iter().any(|op| op.method == step.point);
                if exists_earlier && let Some(after) = previous {
                    return Err(ObligationFailure::OutOfOrder {
                        label: step.label,
                        after,
                    });
                }
                return Err(ObligationFailure::Missing {
                    label: step.label,
                    point: step.point,
                });
            }
        }
    }
    Ok(())
}

/// The normative commit sequence (`storage.md` §3 steps 3–7), as mandatory classes.
///
/// A commit must: sync the new blob files and their directory, encode+sync the tx and its directory,
/// encode+sync+publish the manifest and its directory, then replace CURRENT and sync the root directory
/// that holds it. The concrete entry names differ per scenario; the classes do not.
pub(super) const COMMIT_STEPS: &[RequiredStep] = &[
    RequiredStep::new("blob directory synced", "sync_dir"),
    RequiredStep::new("transaction file synced", "sync_file"),
    RequiredStep::new("transaction published", "publish_no_overwrite"),
    RequiredStep::new("manifest file synced", "sync_file"),
    RequiredStep::new("CURRENT replaced", "replace_current"),
    RequiredStep::new("root directory synced", "sync_dir"),
];
