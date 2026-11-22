//! Finite real-Store seam cases, with every sampled model world judged against actual ACKs.
//! Canonical-projector agreement is the content oracle; Genesis is a preservation baseline.
//! Transport Budget/Cancel injections are not caller-allowance/control-policy evidence.
#[path = "support/p16_fixture.rs"]
mod fixture;
#[path = "support/p16_probe.rs"]
mod p16_probe;
#[path = "support/p16_trace.rs"]
mod p16_trace;

use fixture::*;
use p16_probe::Probe;
use sage_db_store::{
    fs::{CreateDir, Effect, FsError, FsErrorKind},
    handle::{HandleOutcome, Store},
    init::bootstrap,
    lease::AttemptLedger,
    open::{Opened, OutcomeLookup, Resolution, open_existing},
    view::ViewDurability,
};
use sage_db_store_tests::{
    faults::{FaultPoint, Injection, injection_applies},
    model::{
        RelPath,
        fs_model::{ModelCapabilities, ModelFileSystem},
    },
    oracle::attempts::{AttemptHistory, AttemptOutcome, Commit, LogicalRequest, World},
};
use sage_db_types::{framing::Limits, persist::StateEntry, query::RunToCompletion};
use std::{path::Path, rc::Rc};

fn ledger() -> AttemptLedger<'static> {
    static CONTROL: RunToCompletion = RunToCompletion;
    AttemptLedger::open(BYTES, ENTRIES, &CONTROL).unwrap()
}
fn logical(v: u64) -> LogicalRequest {
    LogicalRequest::new(PRINCIPAL, request(v), [v as u8; 32])
}
fn check_error_payload(error: &FsError, injection: Injection) {
    use sage_db_types::query::ControlStop;
    match injection {
        Injection::FailNoEffect | Injection::FailUnknown | Injection::FailUnknownAfterEffect => {
            assert_eq!(error.kind(), FsErrorKind::Io(0));
            assert_eq!(
                error.effect(),
                if injection == Injection::FailNoEffect {
                    Effect::NoEffect
                } else {
                    Effect::Unknown
                }
            );
        }
        Injection::BudgetBytes {
            requested,
            remaining,
        }
        | Injection::BudgetEntries {
            requested,
            remaining,
        } => {
            assert_eq!(error.kind(), FsErrorKind::BudgetExceeded);
            let budget = error.budget().expect("transport budget payload");
            assert_eq!(
                budget.unit(),
                if matches!(injection, Injection::BudgetBytes { .. }) {
                    sage_db_types::ledger::BudgetUnit::Bytes
                } else {
                    sage_db_types::ledger::BudgetUnit::Entries
                }
            );
            assert_eq!(budget.requested(), requested);
            assert_eq!(budget.remaining(), remaining);
            assert_eq!(error.effect(), Effect::NoEffect);
        }
        Injection::Cancel => {
            assert_eq!(error.kind(), FsErrorKind::Interrupted);
            assert_eq!(error.control(), Some(ControlStop::Cancelled));
            assert_eq!(error.effect(), Effect::NoEffect);
        }
        Injection::ShortWrite(_) => panic!("partial-success write is not an error"),
    }
}
fn outcome_class(result: &HandleOutcome) -> AttemptOutcome {
    match result {
        HandleOutcome::Committed { version, .. } => {
            assert_eq!(*version, 3);
            AttemptOutcome::Durable { version: *version }
        }
        HandleOutcome::Unknown => AttemptOutcome::Unknown,
        HandleOutcome::NotCommitted { .. } => AttemptOutcome::NotCommitted,
        other => panic!("unexpected advancing outcome {other:?}"),
    }
}
fn sequence_accepts(head: u64, candidate: AttemptOutcome) -> bool {
    let mut history = AttemptHistory::default();
    history.record(
        "base-ack",
        logical(2),
        AttemptOutcome::Durable { version: 2 },
    );
    history.record("candidate", logical(3), candidate);
    let commits = (2..=head)
        .map(|version| Commit {
            version,
            request: logical(version),
        })
        .collect();
    history
        .accepts(
            1,
            &World {
                base_version: 1,
                commits,
            },
        )
        .unwrap()
}
fn rows(store: &Store<Probe>) -> Vec<StateEntry> {
    store.retained_entries().cloned().collect()
}
fn check_current(store: &Store<Probe>, genesis: &[StateEntry]) -> u64 {
    let head = store.head().expect("materialized head");
    assert!(
        matches!(head, 2 | 3),
        "the durable base2 ACK cannot be lost: {head}"
    );
    let actual = rows(store);
    for (i, row) in actual.iter().enumerate() {
        assert!(
            !actual[i + 1..]
                .iter()
                .any(|r| r.kind == row.kind && r.key == row.key),
            "duplicate current identity"
        );
    }
    assert_eq!(
        actual,
        expected_rows(genesis, head),
        "raw retained vector, no actual-side normalization"
    );
    assert!(store.coverage_complete());
    let basis = store.view().unwrap();
    assert_eq!(basis.durability(), ViewDurability::Durable);
    for version in [2, 3] {
        match store.outcome(PRINCIPAL, request(version)) {
            OutcomeLookup::Found {
                outcome,
                original_version,
                ..
            } if version <= head => {
                assert_eq!(original_version, version);
                assert_eq!(outcome.event(), &expected_outcome(version));
            }
            OutcomeLookup::Absent { .. } if version > head => {}
            other => panic!("head{head}/request{version}: {other:?}"),
        }
    }
    head
}
fn setup(
    point: FaultPoint,
    nth: usize,
    injection: Injection,
) -> (Rc<ModelFileSystem>, Probe, Store<Probe>, Vec<StateEntry>) {
    let inner = Rc::new(ModelFileSystem::new(ModelCapabilities::full()));
    let probe = Probe::new(Rc::clone(&inner), point, nth, injection);
    let mut l = ledger();
    let mut store = bootstrap(
        probe.clone(),
        Path::new(ROOT),
        CreateDir::IfMissing,
        kb(),
        RULES,
        &Limits::V1,
        &mut l,
    )
    .unwrap();
    let genesis = rows(&store);
    assert_eq!(genesis.len(), 2, "explicit Genesis preservation baseline");
    assert!(matches!(
        store.commit(carrier(2), &Limits::V1, &mut l),
        HandleOutcome::Committed { version: 2, .. }
    ));
    check_current(&store, &genesis);
    (inner, probe, store, genesis)
}
fn is_mutating(method: &str) -> bool {
    matches!(
        method,
        "write"
            | "sync_file"
            | "sync_entry"
            | "sync_dir"
            | "sync_parent_of"
            | "create_new"
            | "publish_no_overwrite"
            | "replace_current"
            | "remove"
    )
}
fn judge_worlds(
    inner: &ModelFileSystem,
    genesis: &[StateEntry],
    candidate: AttemptOutcome,
    case: &str,
) {
    let worlds = inner.model().crash_states();
    assert_eq!(worlds.sampled, worlds.states.len());
    assert!(worlds.sampled > 0);
    assert!(
        worlds.sampled <= 4096,
        "explicit bound exceeded, never truncate"
    );
    let mut examined = 0;
    let mut heads = Vec::new();
    for (index, world) in worlds.states.into_iter().enumerate() {
        let fs = Rc::new(
            ModelFileSystem::from_durable_state(ModelCapabilities::full(), Path::new(ROOT), world)
                .unwrap_or_else(|error| panic!("{case} world{index} restore failed: {error:?}")),
        );
        let probe = Probe::new(
            Rc::clone(&fs),
            FaultPoint::Write,
            usize::MAX,
            Injection::FailNoEffect,
        );
        let mut l = ledger();
        let opened = open_existing(
            probe.clone(),
            Path::new(ROOT),
            Some(kb()),
            &Limits::V1,
            &mut l,
        )
        .unwrap_or_else(|error| panic!("{case} world{index} open failed: {error:?}"));
        assert!(
            matches!(&opened, Opened::Ready(_)),
            "valid model sample must sync with ample real ledger"
        );
        let mut store = opened.into_store();
        let head = check_current(&store, genesis);
        assert!(
            sequence_accepts(head, candidate),
            "{case} world{index}: actual ACK/original-version sequence rejected at head{head}"
        );
        let before = probe
            .facts
            .borrow()
            .iter()
            .filter(|op| is_mutating(&op.0))
            .count();
        let mut retry_ledger = ledger();
        match store.commit(carrier(2), &Limits::V1, &mut retry_ledger) {
            HandleOutcome::AlreadyCommitted {
                version: 2,
                recorded: sage_db_store::commit::RecordedOutcome::Outcome(shared),
            } => {
                assert_eq!(shared.event(), &expected_outcome(2));
            }
            other => panic!("recovered exact retry: {other:?}"),
        }
        assert_eq!(
            before,
            probe
                .facts
                .borrow()
                .iter()
                .filter(|op| is_mutating(&op.0))
                .count(),
            "retry performed a mutating/sync call"
        );
        let mut query = ledger();
        let id = sage_db_store::fold::RecordIdentity::Identified {
            key: sage_db_types::persist::RecordKey::term(1),
        };
        match store.history(
            &id,
            1,
            sage_db_store::history::HistoryBudget::new(100),
            &Limits::V1,
            &mut query,
        ) {
            sage_db_store::history::HistoryOutcome::Found(hit) => {
                assert_eq!(hit.source_version, 2);
                assert_eq!(hit.image.record(), &expected_history_a1());
                assert_eq!(hit.durability, ViewDurability::Durable);
                assert_eq!(hit.basis, store.view().unwrap().basis());
            }
            _ => panic!("verified actual disk history must preserve base Term A1"),
        }
        assert_eq!(
            before,
            probe
                .facts
                .borrow()
                .iter()
                .filter(|op| is_mutating(&op.0))
                .count(),
            "history performed a mutating/sync call"
        );
        heads.push(head);
        examined += 1;
    }
    assert_eq!(
        examined, worlds.sampled,
        "every sample counted, including failure paths"
    );
    println!(
        "P16_CASE={case} ACK={candidate:?} SAMPLED={} EXAMINED={examined} HEADS={heads:?}",
        worlds.sampled
    );
}
fn commit_case(point: FaultPoint, injection: Injection, root_boundary: bool) {
    commit_case_at(point, 1, injection, root_boundary);
}
fn commit_case_at(point: FaultPoint, nth: usize, injection: Injection, root_boundary: bool) {
    let (inner, probe, mut store, genesis) = setup(point, nth, injection);
    let old = store.published_pin().unwrap();
    let old_entries = old.entries().to_vec();
    probe.arm_from_here(if root_boundary { Some(3) } else { None });
    let mut candidate_carrier = carrier(3);
    if point == FaultPoint::Hash {
        candidate_carrier.blobs[0].source = sage_db_types::prepared::BlobSource::Existing;
    }
    let mut l = ledger();
    let result = store.commit(candidate_carrier, &Limits::V1, &mut l);
    let candidate = outcome_class(&result);
    match &result {
        HandleOutcome::NotCommitted {
            reason:
                sage_db_store::handle::HandleRefusal::Derivation(
                    sage_db_store::commit::CommitRefusal::Filesystem { error, .. },
                ),
        } => {
            check_error_payload(error, injection);
            assert_eq!(
                store.is_fenced(),
                error.effect() == Effect::Unknown,
                "orphan ambiguity fences writes while retaining the durable current basis"
            );
        }
        HandleOutcome::Unknown => {
            let installed = probe.installed_at.borrow();
            let actual = installed.as_ref().expect("fault's actual operation");
            assert!(
                point == FaultPoint::ReplaceCurrent
                    || point == FaultPoint::SyncDir && actual.1.is_empty() && actual.3 == Some(3)
            );
        }
        HandleOutcome::Committed { version: 3, .. } => {
            assert!(matches!(injection, Injection::ShortWrite(3)))
        }
        other => panic!("fault class was lost or unexpected success: {other:?}"),
    }
    assert_eq!(
        inner.faults_handle().borrow().fired(),
        &[(point, injection)]
    );
    let installed = probe
        .installed_at
        .borrow()
        .clone()
        .expect("injection joined to real callback");
    if root_boundary {
        assert_eq!(installed.0, "sync_dir");
        assert!(installed.1.is_empty());
        assert_eq!(installed.3, Some(3));
    }
    let case = format!("commit/{point:?}/nth={nth}/{injection:?}/root={root_boundary}");
    println!("P16_FIRED={case} STEP={installed:?} OUTCOME={result:?}");
    match candidate {
        AttemptOutcome::Durable { .. } => {
            assert_eq!(check_current(&store, &genesis), 3);
        }
        AttemptOutcome::Unknown => {
            assert!(store.is_fenced());
            assert!(store.view().is_none());
            assert!(rows(&store).is_empty());
            assert!(!store.coverage_complete());
            assert!(matches!(
                store.outcome(PRINCIPAL, request(3)),
                OutcomeLookup::Incomplete { .. }
            ));
        }
        AttemptOutcome::NotCommitted => {
            assert_eq!(check_current(&store, &genesis), 2);
        }
    }
    assert_eq!(old.entries(), old_entries.as_slice());
    judge_worlds(&inner, &genesis, candidate, &case);
    // Resolution must use the same live backend's CURRENT, not a guessed highest file.
    if store.is_fenced() {
        let mut l = ledger();
        assert!(matches!(
            store.resolve(&Limits::V1, &mut l),
            Resolution::Ready
        ));
        let resolved = check_current(&store, &genesis);
        assert!(sequence_accepts(resolved, candidate));
    }
}

fn open_case(point: FaultPoint, injection: Injection) {
    let (inner, probe, store, genesis) = setup(point, 1, injection);
    drop(store);
    probe.arm_from_here(None);
    let mut l = ledger();
    let result = open_existing(probe, Path::new(ROOT), Some(kb()), &Limits::V1, &mut l);
    assert_eq!(
        inner.faults_handle().borrow().fired(),
        &[(point, injection)]
    );
    assert!(
        result.is_err(),
        "read-side transport fault must surface, not an empty listing"
    );
    let error = result.err().unwrap();
    match &error {
        sage_db_store::open::OpenRefusal::Filesystem { error, .. } => {
            check_error_payload(error, injection)
        }
        sage_db_store::open::OpenRefusal::Refused(sage_db_types::framing::Refusal::Budget(
            budget,
        )) => {
            let (unit, requested, remaining) = match injection {
                Injection::BudgetBytes {
                    requested,
                    remaining,
                } => (
                    sage_db_types::ledger::BudgetUnit::Bytes,
                    requested,
                    remaining,
                ),
                Injection::BudgetEntries {
                    requested,
                    remaining,
                } => (
                    sage_db_types::ledger::BudgetUnit::Entries,
                    requested,
                    remaining,
                ),
                _ => panic!("fabricated canonical budget refusal"),
            };
            assert_eq!(
                (budget.unit(), budget.requested(), budget.remaining()),
                (unit, requested, remaining)
            );
        }
        sage_db_store::open::OpenRefusal::Refused(sage_db_types::framing::Refusal::Stopped(
            stop,
        )) => {
            assert_eq!(injection, Injection::Cancel);
            assert_eq!(*stop, sage_db_types::query::ControlStop::Cancelled);
        }
        other => panic!("read-side transport class lost: {other:?}"),
    }
    println!("P16_OPEN_FAULT={point:?}/{injection:?} RESULT={error:?}");
    judge_worlds(
        &inner,
        &genesis,
        AttemptOutcome::NotCommitted,
        &format!("open/{point:?}/{injection:?}"),
    );
}

#[test]
fn finite_legal_point_class_cases_on_real_public_paths() {
    use FaultPoint::*;
    let commit_points = [
        CreateNew,
        Write,
        SyncFile,
        SyncEntry,
        SyncDir,
        Publish,
        ReplaceCurrent,
        Hash,
        Remove,
        OpenRead,
    ];
    let mut tested = Vec::new();
    for point in commit_points {
        for injection in [Injection::FailNoEffect, Injection::FailUnknown] {
            assert!(injection_applies(point, injection));
            commit_case(point, injection, false);
            tested.push((point, injection));
        }
    }
    for point in [Enumerate, ReadExact] {
        for injection in [Injection::FailNoEffect, Injection::FailUnknown] {
            assert!(injection_applies(point, injection));
            open_case(point, injection);
            tested.push((point, injection));
        }
    }
    for point in [SyncFile, SyncDir, ReplaceCurrent] {
        let injection = Injection::FailUnknownAfterEffect;
        commit_case(point, injection, point == SyncDir);
        tested.push((point, injection));
    }
    commit_case(Write, Injection::ShortWrite(3), false);
    tested.push((Write, Injection::ShortWrite(3)));
    let bytes = Injection::BudgetBytes {
        requested: 17,
        remaining: 0,
    };
    commit_case(Hash, bytes, false);
    tested.push((Hash, bytes));
    open_case(ReadExact, bytes);
    tested.push((ReadExact, bytes));
    let entries = Injection::BudgetEntries {
        requested: 1,
        remaining: 0,
    };
    open_case(Enumerate, entries);
    tested.push((Enumerate, entries));
    commit_case(Hash, Injection::Cancel, false);
    tested.push((Hash, Injection::Cancel));
    open_case(Enumerate, Injection::Cancel);
    tested.push((Enumerate, Injection::Cancel));
    // These are explicit executed cases, not all ordinals, amounts, shapes, or platform failures.
    assert_eq!(tested.len(), 33);
    println!(
        "P16_FINITE_CASES={} PLUS_BOOTSTRAP_PARENT_CASES=2",
        tested.len()
    );
}

#[test]
fn bootstrap_parent_sync_faults_use_real_callback_and_partial_namespace() {
    for injection in [Injection::FailNoEffect, Injection::FailUnknown] {
        let inner = Rc::new(ModelFileSystem::new(ModelCapabilities::full()));
        let probe = Probe::new(Rc::clone(&inner), FaultPoint::SyncParentOf, 1, injection);
        probe.arm_from_here(None);
        let mut l = ledger();
        let result = bootstrap(
            probe.clone(),
            Path::new(ROOT),
            CreateDir::IfMissing,
            kb(),
            RULES,
            &Limits::V1,
            &mut l,
        );
        assert!(result.is_err());
        assert_eq!(
            inner.faults_handle().borrow().fired(),
            &[(FaultPoint::SyncParentOf, injection)]
        );
        let at = probe.installed_at.borrow().clone().unwrap();
        assert_eq!(at.0, "sync_parent_of");
        assert_eq!(at.2, ROOT);
        let worlds = inner.model().crash_states();
        assert_eq!(worlds.states.len(), worlds.sampled);
        assert!(worlds.sampled > 0);
        for world in worlds.states {
            let fs = ModelFileSystem::from_durable_state(
                ModelCapabilities::full(),
                Path::new(ROOT),
                world,
            )
            .unwrap();
            let mut l = ledger();
            assert!(matches!(
                open_existing(fs, Path::new(ROOT), Some(kb()), &Limits::V1, &mut l),
                Err(sage_db_store::open::OpenRefusal::NotInitialized)
            ));
        }
        println!(
            "P16_BOOTSTRAP_PARENT={injection:?} SAMPLED={} EXAMINED={}",
            worlds.sampled, worlds.sampled
        );
    }
}

#[test]
fn sequence_oracle_rejects_lost_base_ack_and_lost_candidate_ack() {
    for candidate in [
        AttemptOutcome::Unknown,
        AttemptOutcome::NotCommitted,
        AttemptOutcome::Durable { version: 3 },
    ] {
        assert!(!sequence_accepts(1, candidate));
    }
    assert!(!sequence_accepts(2, AttemptOutcome::Durable { version: 3 }));
    assert!(!sequence_accepts(3, AttemptOutcome::NotCommitted));
    assert!(sequence_accepts(2, AttemptOutcome::Unknown));
    assert!(sequence_accepts(3, AttemptOutcome::Unknown));
}

type Fact = (String, String, String);
fn protocol_obligations(ops: &[Fact]) -> Result<(), String> {
    let locate = |method: &str, dir: &str, name: Option<&str>, start: usize| {
        ops.iter()
            .enumerate()
            .skip(start)
            .find(|(_, op)| op.0 == method && op.1 == dir && name.is_none_or(|n| op.2 == n))
            .map(|(i, _)| i)
            .ok_or_else(|| format!("missing {method}/{dir}/{name:?} after {start}"))
    };
    let mut last_blob = 0;
    for (index, op) in ops
        .iter()
        .enumerate()
        .filter(|(_, o)| o.0 == "publish_no_overwrite" && o.1 == "blobs")
    {
        last_blob = locate("sync_dir", "blobs", None, index + 1)?;
        let _ = op;
    }
    if last_blob == 0 {
        return Err("blob publication missing".into());
    }
    let tx = locate("publish_no_overwrite", "transactions", None, last_blob + 1)?;
    let tx_dir = locate("sync_dir", "transactions", None, tx + 1)?;
    let manifest = locate("publish_no_overwrite", "manifests", None, tx_dir + 1)?;
    let manifest_dir = locate("sync_dir", "manifests", None, manifest + 1)?;
    let current_sync = locate("sync_file", "", Some("CURRENT.tmp"), manifest_dir + 1)?;
    let replace = locate("replace_current", "", Some("CURRENT.tmp"), current_sync + 1)?;
    locate("sync_dir", "", None, replace + 1)?;
    // Every staged publication must have a completed file-sync call in its own write iteration.
    for (index, op) in ops
        .iter()
        .enumerate()
        .filter(|(_, o)| o.0 == "publish_no_overwrite")
    {
        let prefix = if op.1 == "blobs" {
            "blob-"
        } else if op.1 == "transactions" {
            "tx-"
        } else {
            "manifest-"
        };
        let prior = &ops[..index];
        let create = prior
            .iter()
            .rposition(|p| p.0 == "create_new" && p.1 == "staging" && p.2.starts_with(prefix))
            .ok_or_else(|| format!("missing stage create for {}", op.1))?;
        let name = &prior[create].2;
        let write = locate("write", "staging", Some(name), create + 1)?;
        let sync = locate("sync_file", "staging", Some(name), write + 1)?;
        if sync >= index {
            return Err(format!("{} published before staged sync", op.1));
        }
    }
    Ok(())
}
#[test]
fn real_reachability_and_fixed_protocol_obligations_with_mutant_negatives() {
    let (inner, probe, mut store, genesis) =
        setup(FaultPoint::Write, usize::MAX, Injection::FailNoEffect);
    let bootstrap_facts = probe.facts.borrow().clone();
    assert!(bootstrap_facts.iter().any(|o| o.0 == "sync_parent_of"));
    probe.arm_from_here(None);
    let mut l = ledger();
    assert!(matches!(
        store.commit(carrier(3), &Limits::V1, &mut l),
        HandleOutcome::Committed { version: 3, .. }
    ));
    let commit = probe.facts.borrow().clone();
    protocol_obligations(&commit).unwrap();
    let replace = commit
        .iter()
        .position(|o| o.0 == "replace_current")
        .unwrap();
    let mut missing = commit.clone();
    missing.retain(|o| !(o.0 == "sync_dir" && o.1.is_empty()));
    assert!(protocol_obligations(&missing).is_err());
    let mut inverted = commit.clone();
    let root_sync = commit
        .iter()
        .enumerate()
        .skip(replace + 1)
        .find(|(_, o)| o.0 == "sync_dir" && o.1.is_empty())
        .unwrap()
        .0;
    inverted.swap(replace, root_sync);
    assert!(protocol_obligations(&inverted).is_err());
    drop(store);
    let mut l = ledger();
    let reopened = open_existing(
        probe.clone(),
        Path::new(ROOT),
        Some(kb()),
        &Limits::V1,
        &mut l,
    )
    .unwrap();
    assert_eq!(check_current(reopened.store(), &genesis), 3);
    let all = probe.facts.borrow();
    for method in [
        "create_new",
        "write",
        "sync_file",
        "sync_entry",
        "sync_dir",
        "publish_no_overwrite",
        "replace_current",
        "hash_bounded",
        "for_each_entry",
        "remove",
        "open_read",
        "read_exact_into",
    ] {
        let count = all.iter().filter(|o| o.0 == method).count();
        assert!(
            count > 0,
            "missing required instrumentation/reachability: {method}"
        );
        println!("P16_REACH={method} COUNT={count}");
    }
    let root = RelPath::root();
    assert!(inner.model().inode_of(&root, "CURRENT").is_some());
}

#[test]
fn every_critical_operation_occurrence_in_this_fixture_is_faulted() {
    let (_inner, probe, mut store, genesis) =
        setup(FaultPoint::Write, usize::MAX, Injection::FailNoEffect);
    probe.arm_from_here(None);
    let mut l = ledger();
    assert!(matches!(
        store.commit(carrier(3), &Limits::V1, &mut l),
        HandleOutcome::Committed { version: 3, .. }
    ));
    assert_eq!(check_current(&store, &genesis), 3);
    let baseline = probe.facts.borrow().clone();
    // Reachability chooses ordinal targets; the FIXED normative checker independently requires
    // the protocol steps, so omission is a failure, never an empty passing matrix dimension.
    protocol_obligations(&baseline).unwrap();
    let mut instances = 0;
    for (point, method) in [
        (FaultPoint::Write, "write"),
        (FaultPoint::SyncFile, "sync_file"),
        (FaultPoint::SyncEntry, "sync_entry"),
        (FaultPoint::SyncDir, "sync_dir"),
        (FaultPoint::Publish, "publish_no_overwrite"),
        (FaultPoint::ReplaceCurrent, "replace_current"),
    ] {
        let count = baseline.iter().filter(|op| op.0 == method).count();
        assert!(count > 0);
        for nth in 1..=count {
            // The first occurrence's two pre-effect cases are already in the 35-instance test.
            if nth > 1 {
                for injection in [Injection::FailNoEffect, Injection::FailUnknown] {
                    commit_case_at(point, nth, injection, false);
                    instances += 1;
                }
            }
            if injection_applies(point, Injection::FailUnknownAfterEffect)
                && (nth > 1 || point == FaultPoint::SyncDir)
            {
                commit_case_at(point, nth, Injection::FailUnknownAfterEffect, false);
                instances += 1;
            }
            if point == FaultPoint::Write && nth > 1 {
                commit_case_at(point, nth, Injection::ShortWrite(3), false);
                instances += 1;
            }
        }
        println!(
            "P16_CRITICAL_POINT={point:?} BASELINE_OCCURRENCES={count} EVERY_ORDINAL_EXERCISED=true"
        );
    }
    println!("P16_ADDITIONAL_CRITICAL_INSTANCES={instances}");
}

#[test]
fn actual_partial_write_then_enospc_preserves_the_acked_prefix() {
    let (inner, probe, mut store, genesis) =
        setup(FaultPoint::Write, usize::MAX, Injection::FailNoEffect);
    let old = store.published_pin().unwrap();
    let old_rows = old.entries().to_vec();
    // This is a separately labelled filesystem transport quota, NOT a new FaultSchedule
    // class or caller budget. Exactly16 bytes reach the REAL inner writer; its next call
    // is refused with NoSpace and no effect for THAT call. The partial staged file remains.
    probe.arm_write_quota(16);
    let mut l = ledger();
    let result = store.commit(carrier(3), &Limits::V1, &mut l);
    match &result {
        HandleOutcome::NotCommitted {
            reason:
                sage_db_store::handle::HandleRefusal::Derivation(
                    sage_db_store::commit::CommitRefusal::Filesystem { error, .. },
                ),
        } => {
            assert_eq!(error.kind(), FsErrorKind::NoSpace);
            assert_eq!(error.effect(), Effect::NoEffect);
            assert!(error.budget().is_none());
            assert!(error.control().is_none());
        }
        other => panic!("real quota refusal lost its class: {other:?}"),
    }
    assert_eq!(probe.quota_written.get(), 16);
    assert_eq!(probe.quota_denials.get(), 1);
    {
        let stage = RelPath::join(&RelPath::root(), "staging");
        let name = sage_db_store::names::EntryName::Staging {
            kind: sage_db_store::names::StagingKind::Blob,
            id: sage_db_store::names::Id128::from_bytes(carrier(3).tx_id),
        }
        .to_file_name();
        let model = inner.model();
        let inode = model
            .inode_of(&stage, &name)
            .expect("the real partial staged file remains");
        assert_eq!(
            model.visible_bytes(inode).unwrap(),
            &RULES[..16],
            "quota accounting must correspond to actual written bytes"
        );
    }
    assert!(
        inner.faults_handle().borrow().fired().is_empty(),
        "own quota is not a declared FaultSchedule firing"
    );
    assert_eq!(check_current(&store, &genesis), 2);
    assert_eq!(old.entries(), old_rows.as_slice());
    judge_worlds(
        &inner,
        &genesis,
        AttemptOutcome::NotCommitted,
        "write-quota/16-real-bytes/NoSpace",
    );
    println!("P16_QUOTA_FORWARDED_BYTES=16 DENIALS=1 KIND=NoSpace CALL_EFFECT=NoEffect");
}
