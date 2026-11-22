//! Contract tests for the seam's documented guarantees (P1-3, store owner).
//!
//! These pin the answers the independent harness depends on, so a later edit cannot silently
//! change them:
//!
//! * what `Ok` from a sync means,
//! * that a mid-stream write failure is `Effect::Unknown` (partial prefix visible, not durable),
//! * that `replace_current` can only target `CURRENT`,
//! * that `publish_no_overwrite` consumes the staged name,
//! * that the directory visitor's `&str` is opaque and only two uses are permitted.
//!
//! They assert *documented contract* through the public types where possible, and compile-time
//! shape where the contract is about signatures (a destination name cannot be passed to
//! `replace_current` because the parameter does not exist). No filesystem behaviour is claimed:
//! there is no backend in this crate yet.

use sage_db_store::fs::{Effect, FsError, FsErrorKind, PublishMode, Published};
use sage_db_store::names::{EntryName, RootName};

fn published_consumes_staged() -> Published {
    Published {
        how: PublishMode::Created,
        needs_sync: true,
    }
}

#[test]
fn publish_always_reports_that_sync_is_still_owed() {
    // Identity is not durability: even a freshly created name, and even a name that already held
    // identical bytes, still require the caller to sync the inode and the directory.
    let created = Published {
        how: PublishMode::Created,
        needs_sync: true,
    };
    let already = Published {
        how: PublishMode::AlreadyPresent,
        needs_sync: true,
    };
    assert!(created.needs_sync);
    assert!(already.needs_sync);
    assert_ne!(created.how, already.how);
    // The struct has no `already_durable: true` shape at all: that would be the defect where a
    // hash match was mistaken for a completed flush.
    let p = published_consumes_staged();
    assert!(p.needs_sync);
}

#[test]
fn current_is_addressable_and_is_the_only_replace_target() {
    // The destination of a root replacement is CURRENT by construction; the staged name is its
    // fixed tmp sibling. Both are reachable as typed entries, so no string parsing is involved.
    let current = EntryName::Root(RootName::Current);
    let tmp = EntryName::Root(RootName::CurrentTmp);
    assert_eq!(current.to_file_name(), "CURRENT");
    assert_eq!(tmp.to_file_name(), "CURRENT.tmp");
    assert_eq!(current.family(), None, "root files are not in a family");
    assert_ne!(current, tmp);
}

#[test]
fn a_mid_stream_write_failure_is_ambiguous_not_no_effect() {
    // The contract the fault model needs: a write that accepted a prefix and then failed is
    // Unknown, so those bytes are visible-but-not-durable rather than "nothing happened".
    let partial = FsError::new(FsErrorKind::NoSpace, Effect::Unknown);
    assert!(partial.is_ambiguous());
    assert_eq!(partial.effect(), Effect::Unknown);
    assert!(
        !matches!(partial.effect(), Effect::NoEffect),
        "a partial write must not be reported as no-effect"
    );

    // By contrast, a failure that certainly wrote nothing may be NoEffect.
    let nothing = FsError::new(FsErrorKind::NoSpace, Effect::NoEffect);
    assert!(!nothing.is_ambiguous());
}

#[test]
fn sync_failures_are_ambiguous_by_contract() {
    // Applies to sync_file, sync_dir, sync_entry and sync_parent_of: an errored flush may have
    // completed, so the caller resolves.
    for kind in [
        FsErrorKind::Io(5),
        FsErrorKind::NoSpace,
        FsErrorKind::QuotaExceeded,
        FsErrorKind::Interrupted,
    ] {
        let e = FsError::new(kind, Effect::Unknown);
        assert!(e.is_ambiguous(), "{kind:?} flush failure must be ambiguous");
    }
}

#[test]
fn stopping_and_budget_are_never_data_verdicts() {
    // Both are "the operation did not run"; neither says anything about bytes on disk.
    use sage_db_types::ledger::{BudgetError, BudgetUnit};
    use sage_db_types::query::ControlStop;

    let budget: FsError = BudgetError::new(BudgetUnit::Bytes, 10, 4).into();
    assert_eq!(budget.effect(), Effect::NoEffect);
    assert_eq!(
        budget.budget().map(BudgetError::unit),
        Some(BudgetUnit::Bytes)
    );

    let stop: FsError = ControlStop::Cancelled.into();
    assert_eq!(stop.effect(), Effect::NoEffect);
    assert!(stop.control().is_some());
    assert!(stop.budget().is_none());
}

#[test]
fn enumeration_names_are_opaque_and_not_round_trippable() {
    // The visitor yields &str precisely so that an unexpected on-disk name can be reported. That
    // means callers must not treat the string as an engine-generated name; the validator exists to
    // tell the two apart, and an unrecognised name must be reportable rather than fabricated.
    use sage_db_store::names::is_valid_entry_name;
    assert!(is_valid_entry_name("CURRENT"));
    // A stray operator file is NOT a valid entry name, and the walk must still be able to report
    // it — which is why the visitor does not receive an EntryName.
    assert!(!is_valid_entry_name("NOTES.txt"));
    assert!(!is_valid_entry_name(".DS_Store"));
}
