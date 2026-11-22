//! Schedule integrity tests.
//!
//! These check the scenarios themselves, not a store. The value is that a scenario which could
//! never demonstrate its property — a fault pointing past the end, a commit schedule with no root
//! sync, an "ambiguous" case with no ambiguity — fails here rather than passing silently later.

use super::{BudgetUnitName, Fault, FaultKind, Op, Schedule, catalogue};
use crate::model::RelPath;

fn root() -> RelPath {
    RelPath::root()
}

/// A fault must name an operation that exists, or it can never fire.
#[test]
#[should_panic(expected = "fault targets op 5")]
fn fault_past_the_end_is_refused() {
    let _ = Schedule::new("bad")
        .op(Op::SyncFile)
        .fault(Fault::CrashAfter { at: 5 });
}

/// The happy-path commit schedule must sync content, names and the root pointer in that order,
/// with the pointer replacement last. A schedule that publishes `CURRENT` before syncing the
/// staged bytes would "test" a commit that no store could legally perform.
#[test]
fn commit_schedule_syncs_before_it_replaces_the_root() {
    let staging = RelPath::join(&root(), "staging");
    let manifests = RelPath::join(&root(), "manifests");
    let blobs = RelPath::join(&root(), "blobs");
    let schedules = catalogue(&staging, &manifests, &blobs);
    let happy = schedules
        .iter()
        .find(|s| s.name == "commit-happy-path")
        .expect("catalogue must contain the happy path");

    let replace_at = happy
        .ops
        .iter()
        .position(|op| matches!(op, Op::ReplaceCurrent { .. }))
        .expect("the happy path must replace CURRENT");
    assert_eq!(
        replace_at,
        happy.ops.len() - 2,
        "only the final directory sync may follow the root replacement"
    );

    let syncs = happy.sync_ops();
    assert!(
        syncs.iter().any(|i| *i < replace_at),
        "content must be synced before the pointer is replaced"
    );
    assert!(
        syncs.iter().any(|i| *i > replace_at),
        "and the pointer's directory must be synced after it"
    );
}

/// Every scenario in the catalogue must be non-trivial and self-consistent.
#[test]
fn catalogue_scenarios_are_wellformed() {
    let staging = RelPath::join(&root(), "staging");
    let manifests = RelPath::join(&root(), "manifests");
    let blobs = RelPath::join(&root(), "blobs");

    for schedule in catalogue(&staging, &manifests, &blobs) {
        assert!(
            !schedule.ops.is_empty(),
            "{} has no operations",
            schedule.name
        );
        // Constructing the loop already proved every fault index is in range, because `fault`
        // panics otherwise; assert we actually declared something in the fault scenarios.
        if schedule.name.starts_with("crash") || schedule.name.starts_with("ambiguous") {
            assert!(
                !schedule.faults.is_empty(),
                "{} declares no fault, so it tests nothing",
                schedule.name
            );
        }
    }
}

/// A crash point is reported as an ordered set, so a scenario with two crashes enumerates both.
#[test]
fn crash_points_are_enumerated() {
    let schedule = Schedule::new("two-crashes")
        .op(Op::SyncFile)
        .op(Op::SyncFile)
        .op(Op::SyncFile)
        .fault(Fault::CrashAfter { at: 0 })
        .fault(Fault::CrashAfter { at: 2 });
    let points: Vec<usize> = schedule.crash_points().into_iter().collect();
    assert_eq!(points, vec![0, 2]);
}

/// Ambiguity is reported, because "may or may not have happened" is what forces resolution.
#[test]
fn ambiguity_is_reported() {
    let clear = Schedule::new("clear").op(Op::SyncFile).fault(Fault::Fail {
        at: 0,
        kind: FaultKind::NoSpace,
    });
    assert!(!clear.has_ambiguity());

    let murky = Schedule::new("murky")
        .op(Op::SyncFile)
        .fault(Fault::Ambiguous { at: 0 });
    assert!(murky.has_ambiguity());
}

/// A budget refusal carries its unit, so bytes and entries cannot be confused in a scenario.
#[test]
fn budget_faults_carry_their_unit() {
    let schedule = Schedule::new("entries-starved")
        .op(Op::ForEachEntry {
            dir: RelPath::join(&root(), "blobs"),
        })
        .fault(Fault::Budget {
            at: 0,
            unit: BudgetUnitName::Entries,
            requested: 1,
            remaining: 0,
        });
    match &schedule.faults[0] {
        Fault::Budget { unit, .. } => assert_eq!(*unit, BudgetUnitName::Entries),
        other => panic!("expected a budget fault, got {other:?}"),
    }
}

/// Sync operations are identified so a schedule can be checked for a missing sync.
#[test]
fn sync_ops_are_identified() {
    let schedule = Schedule::new("sync-shapes")
        .op(Op::SyncFile)
        .op(Op::SyncEntry {
            dir: root(),
            name: crate::schedule::tests::dummy_current(),
        })
        .op(Op::SyncDir { dir: root() })
        .op(Op::SyncParentOfRoot)
        .op(Op::ForEachEntry { dir: root() });
    assert_eq!(schedule.sync_ops(), vec![0, 1, 2, 3]);
}

/// A schedule with no sync at all is detectable, which is the point of `sync_ops`.
#[test]
fn a_schedule_without_syncs_is_visible() {
    let schedule = Schedule::new("no-sync").op(Op::CreateNew {
        dir: root(),
        name: crate::schedule::tests::dummy_current(),
    });
    assert!(schedule.sync_ops().is_empty());
}

/// Build a root-name entry for tests, without importing the name crate in three places.
pub(crate) fn dummy_current() -> sage_db_store::names::EntryName {
    sage_db_store::names::EntryName::Root(sage_db_store::names::RootName::Current)
}
