//! The first actual public reopen path: real bootstrap → real commit → real `open_existing`.
//!
//! This is the milestone the release asks for: genuine reopen after public bootstrap and an ordinary commit, through the
//! public API only, on a real finite ledger and the real model filesystem seam.

#[path = "support/observing_fs.rs"]
mod observing_fs;
#[path = "support/shared_fs.rs"]
mod shared_fs;

use sage_db_store::init::bootstrap;
use sage_db_store::lease::AttemptLedger;
use sage_db_store::open::{Opened, open_existing};
use sage_db_store::view::ViewDurability;
use sage_db_store_tests::model::fs_model::{ModelCapabilities, ModelFileSystem};
use shared_fs::SharedFs;

use sage_db_types::framing::Limits;
use sage_db_types::identity::KbUuid;
use sage_db_types::prepared::{
    BlobDeclaration, BlobSource, OwnedEvent, PreparedTransaction, RulesBinding,
};
use sage_db_types::query::RunToCompletion;
use sage_db_types::relation::RuleProfile;

const RULES: &[u8] = include_bytes!("../fixtures/rules.formal-v1.v1.bin");
const UUID: [u8; 16] = [0x5A; 16];

fn str_value(text: &str) -> Vec<u8> {
    let mut value = (text.len() as u32).to_le_bytes().to_vec();
    value.extend_from_slice(text.as_bytes());
    value
}

/// The payload behind an outcome's `result_ref`. Arbitrary bytes by design; what matters is that the ref names these
/// exact bytes and the declaration carries them with a real length.
fn result_payload(version: u64, affected: u32) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"opaque-result/");
    out.extend_from_slice(&version.to_le_bytes());
    out.extend_from_slice(&affected.to_le_bytes());
    out
}

/// The ordinary transaction's `RecordedOutcome` event (tag 192).
///
/// Tags 176/177 must match the header's `principal_id`/`request_id`; the composition layer cross-checks them, and 181
/// must be the header's non-zero version.
fn outcome_event(
    version: u64,
    principal: &str,
    request_id: &str,
    payload_digest: [u8; 32],
) -> OwnedEvent {
    let mut fields: Vec<(u16, Vec<u8>)> = vec![
        (5, str_value(principal)),
        (6, 0u16.to_le_bytes().to_vec()),
        (
            8,
            (1_700_000_000_000_000i64 + version as i64)
                .to_le_bytes()
                .to_vec(),
        ),
        (176, str_value(principal)),
        (177, str_value(request_id)),
        (178, payload_digest.to_vec()),
        (179, 0u16.to_le_bytes().to_vec()),
        (
            180,
            sage_db_format::digest::raw_sha256(&result_payload(version, 2)).to_vec(),
        ),
        (181, version.to_le_bytes().to_vec()),
    ];
    fields.sort_by_key(|(tag, _)| *tag);
    OwnedEvent { tag: 192, fields }
}

fn carrier(
    version: u64,
    prev: Option<u64>,
    request_id: &str,
    events: Vec<OwnedEvent>,
) -> PreparedTransaction {
    use sage_db_format::digest::{Domain, digest, raw_sha256};
    let rules_ref = raw_sha256(RULES);
    let rules_digest = digest(Domain::Rules, RULES);
    // Declarations are computed while `events` is still owned; every content-addressed ref the events name gets bytes.
    let declared_blobs = {
        let mut declared = vec![BlobDeclaration {
            sha256: rules_ref,
            declared_len: RULES.len() as u64,
            source: BlobSource::Provide(RULES.to_vec()),
        }];
        if events.iter().any(|e| e.tag == 192) {
            let payload = result_payload(version, 2);
            declared.push(BlobDeclaration {
                sha256: raw_sha256(&payload),
                declared_len: payload.len() as u64,
                source: BlobSource::Provide(payload),
            });
        }
        declared
    };
    PreparedTransaction {
        version,
        prev_version: prev,
        tx_id: [0xA0 + version as u8; 16],
        kb_uuid: KbUuid::from_bytes(UUID),
        created_at: 1_700_000_000_000_000 + version as i64,
        principal_id: "system".to_owned(),
        request_id: request_id.to_owned(),
        // §4.2: version 1 is `NonRequest` and requires the ALL-ZERO sentinel; later versions require a NON-zero digest.
        // A non-zero sentinel on version 1 is Corrupt(RequestCanonMismatch).
        payload_digest: if version == 1 {
            [0u8; 32]
        } else {
            [version as u8; 32]
        },
        expect_version: prev,
        extensions: Default::default(),
        events,
        blobs: declared_blobs,
        rules: RulesBinding {
            profile: RuleProfile::FormalV1,
            rules_digest,
            rules_ref,
        },
    }
}

const BYTES: u64 = 64 * 1024 * 1024;
const ENTRIES: u64 = 1_000_000;

fn uuid() -> KbUuid {
    KbUuid::from_bytes([0x5A; 16])
}

fn control() -> &'static RunToCompletion {
    Box::leak(Box::new(RunToCompletion))
}

#[test]
fn an_ordinary_commit_reopens_through_the_public_api_with_full_values() {
    let fs = SharedFs::new(ModelFileSystem::new(ModelCapabilities::full()));
    let path = std::path::Path::new("/kb");
    let ctl = control();
    let mut ledger = AttemptLedger::open(BYTES, ENTRIES, ctl).expect("a finite ledger opens");

    // --- create + commit an ORDINARY transaction through the public API ---
    let mut store = match bootstrap(
        fs.clone(),
        path,
        sage_db_store::fs::CreateDir::IfMissing,
        uuid(),
        RULES,
        &Limits::V1,
        &mut ledger,
    ) {
        Ok(store) => store,
        Err(refusal) => panic!("bootstrap must succeed: {refusal:?}"),
    };
    assert_eq!(store.head(), Some(1), "Genesis committed at version 1");

    let digest = [0x22; 32];
    let events = vec![outcome_event(2, "system", "p15", digest)];
    let mut tx = carrier(2, Some(1), "p15", events);
    tx.payload_digest = digest;
    match store.commit(tx, &Limits::V1, &mut ledger) {
        sage_db_store::handle::HandleOutcome::Committed { version, .. } => {
            assert_eq!(version, 2, "the ordinary transaction commits at version 2");
        }
        other => panic!("the ordinary transaction must commit, got {other:?}"),
    }
    assert_eq!(store.head(), Some(2));

    // The exact values the commit published, for byte/value comparison after reopen.
    let committed: Vec<_> = store.retained_entries().cloned().collect();
    assert!(
        committed.len() >= 2,
        "Genesis plus an ordinary 192 leaves several canonical rows, got {}",
        committed.len()
    );
    drop(store);

    // --- reopen the SAME bytes through the public P1-5 entry ---
    let opened = match open_existing(fs, path, Some(uuid()), &Limits::V1, &mut ledger) {
        Ok(opened) => opened,
        Err(refusal) => panic!("the committed root must reopen: {refusal:?}"),
    };
    assert_eq!(opened.durability(), ViewDurability::Durable);

    let store = opened.store();
    assert_eq!(
        store.head(),
        Some(2),
        "reopen recovers version 2, not just Genesis"
    );

    let view = opened.view().expect("a durable reopen publishes a view");
    assert_eq!(view.durability(), ViewDurability::Durable);
    assert_eq!(view.basis().version.get(), 2, "the basis names version 2");

    // FULL VALUE comparison, not just length: kind, key, revision and every body byte.
    assert_eq!(
        view.entries().len(),
        committed.len(),
        "reopen reproduces the committed current set size"
    );
    for (reopened, original) in view.entries().iter().zip(committed.iter()) {
        assert_eq!(reopened.kind, original.kind, "kind preserved");
        assert_eq!(reopened.key, original.key, "key bytes preserved");
        assert_eq!(reopened.revision, original.revision, "revision preserved");
        assert_eq!(
            reopened.body, original.body,
            "canonical body bytes preserved, including optional-field presence"
        );
    }

    let legacy: Vec<_> = store.retained_entries().cloned().collect();
    assert_eq!(legacy.len(), committed.len());
    assert!(
        store.published_pin().is_some(),
        "durable reopen publishes the pin"
    );

    match opened {
        Opened::Ready(_) => {}
        Opened::ReadOnly { .. } => panic!("a synced root must open Ready"),
    }
}

#[test]
fn public_verify_checks_integrity_without_changing_anything() {
    let fs = SharedFs::new(ModelFileSystem::new(ModelCapabilities::full()));
    let path = std::path::Path::new("/kbv");
    let ctl = control();
    let mut ledger = AttemptLedger::open(BYTES, ENTRIES, ctl).expect("a finite ledger opens");

    let mut store = match bootstrap(
        fs.clone(),
        path,
        sage_db_store::fs::CreateDir::IfMissing,
        uuid(),
        RULES,
        &Limits::V1,
        &mut ledger,
    ) {
        Ok(store) => store,
        Err(refusal) => panic!("bootstrap must succeed: {refusal:?}"),
    };
    let digest = [0x22; 32];
    let events = vec![outcome_event(2, "system", "p15v", digest)];
    let mut tx = carrier(2, Some(1), "p15v", events);
    tx.payload_digest = digest;
    match store.commit(tx, &Limits::V1, &mut ledger) {
        sage_db_store::handle::HandleOutcome::Committed { version, .. } => assert_eq!(version, 2),
        other => panic!("must commit: {other:?}"),
    }
    drop(store);

    let opened = match open_existing(fs, path, Some(uuid()), &Limits::V1, &mut ledger) {
        Ok(opened) => opened,
        Err(refusal) => panic!("must reopen: {refusal:?}"),
    };
    let mut store = opened.into_store();

    let head_before = store.head();
    let entries_before: Vec<_> = store.retained_entries().cloned().collect();
    let durability_before = store.view().map(|v| v.durability());

    // Integrity verify: no write, no promotion, no root substitution.
    match store.verify(&Limits::V1, &mut ledger) {
        Ok(()) => {}
        Err(refusal) => panic!("a committed root must verify: {refusal:?}"),
    }

    // Nothing changed: same head, same entries byte-for-byte, same durability tag.
    assert_eq!(store.head(), head_before, "verify must not move the head");
    let entries_after: Vec<_> = store.retained_entries().cloned().collect();
    assert_eq!(entries_after.len(), entries_before.len());
    for (after, before) in entries_after.iter().zip(entries_before.iter()) {
        assert_eq!(after.kind, before.kind);
        assert_eq!(after.key, before.key);
        assert_eq!(after.revision, before.revision);
        assert_eq!(after.body, before.body);
    }
    assert_eq!(
        store.view().map(|v| v.durability()),
        durability_before,
        "verify must not promote or demote durability"
    );
}

/// The two defects the milestone review found, as discriminators.
#[test]
fn a_fresh_commit_publishes_a_view_and_reopen_can_advance() {
    let fs = SharedFs::new(ModelFileSystem::new(ModelCapabilities::full()));
    let path = std::path::Path::new("/kbm");
    let ctl = control();
    let mut ledger = AttemptLedger::open(BYTES, ENTRIES, ctl).expect("a finite ledger opens");

    let mut store = match bootstrap(
        fs.clone(),
        path,
        sage_db_store::fs::CreateDir::IfMissing,
        uuid(),
        RULES,
        &Limits::V1,
        &mut ledger,
    ) {
        Ok(store) => store,
        Err(refusal) => panic!("bootstrap must succeed: {refusal:?}"),
    };

    // DEFECT 1: a fresh bootstrap must publish a current view, not just a pin.
    assert!(
        store.view().is_some(),
        "fresh bootstrap must publish a current view"
    );

    let digest = [0x22; 32];
    let events = vec![outcome_event(2, "system", "m1", digest)];
    let mut tx = carrier(2, Some(1), "m1", events);
    tx.payload_digest = digest;
    match store.commit(tx, &Limits::V1, &mut ledger) {
        sage_db_store::handle::HandleOutcome::Committed { version, .. } => assert_eq!(version, 2),
        other => panic!("ordinary commit must succeed: {other:?}"),
    }
    assert!(
        store.view().is_some(),
        "a durable commit must publish a current view"
    );
    drop(store);

    // Reopen, then make a NEW legal ADVANCING commit — the milestone's second defect.
    let opened = match open_existing(fs, path, Some(uuid()), &Limits::V1, &mut ledger) {
        Ok(opened) => opened,
        Err(refusal) => panic!("must reopen: {refusal:?}"),
    };
    let mut store = opened.into_store();
    assert_eq!(store.head(), Some(2));
    assert!(store.view().is_some(), "reopen must publish a view");

    let digest3 = [0x33; 32];
    let events3 = vec![outcome_event(3, "system", "m2", digest3)];
    let mut tx3 = carrier(3, Some(2), "m2", events3);
    tx3.payload_digest = digest3;
    match store.commit(tx3, &Limits::V1, &mut ledger) {
        sage_db_store::handle::HandleOutcome::Committed { version, .. } => {
            assert_eq!(
                version, 3,
                "an advancing commit after reopen must commit at v3"
            );
        }
        other => panic!("advancing commit after reopen must succeed, got {other:?}"),
    }
    assert_eq!(store.head(), Some(3));
    assert!(
        store.view().is_some_and(|v| v.basis().version.get() == 3),
        "the published view must follow the advancing commit"
    );
}

/// G0: the public outcome query answers only on a durable basis, with the ORIGINAL version.
#[test]
fn a_durable_outcome_lookup_returns_the_original_version_and_absence_is_gated() {
    use sage_db_store::open::OutcomeLookup;

    let fs = SharedFs::new(ModelFileSystem::new(ModelCapabilities::full()));
    let path = std::path::Path::new("/kbo");
    let ctl = control();
    let mut ledger = AttemptLedger::open(BYTES, ENTRIES, ctl).expect("a finite ledger opens");

    let mut store = match bootstrap(
        fs.clone(),
        path,
        sage_db_store::fs::CreateDir::IfMissing,
        uuid(),
        RULES,
        &Limits::V1,
        &mut ledger,
    ) {
        Ok(store) => store,
        Err(refusal) => panic!("bootstrap must succeed: {refusal:?}"),
    };
    let digest = [0x22; 32];
    let events = vec![outcome_event(2, "system", "g0", digest)];
    let mut tx = carrier(2, Some(1), "g0", events);
    tx.payload_digest = digest;
    match store.commit(tx, &Limits::V1, &mut ledger) {
        sage_db_store::handle::HandleOutcome::Committed { version, .. } => assert_eq!(version, 2),
        other => panic!("must commit: {other:?}"),
    }

    // Found: the ORIGINAL version and payload, on a durable basis.
    match store.outcome("system", "g0") {
        OutcomeLookup::Found {
            outcome,
            original_version,
            basis,
        } => {
            assert_eq!(
                original_version, 2,
                "the ORIGINAL version, not the current head"
            );
            assert_eq!(outcome.event().request_id, "g0");
            assert_eq!(basis.version.get(), 2);
        }
        other => panic!("a durable basis must answer Found, got {other:?}"),
    }

    // Absent requires the SAME durable basis plus complete coverage.
    match store.outcome("system", "never-sent") {
        OutcomeLookup::Absent { basis } => assert_eq!(basis.version.get(), 2),
        other => panic!("complete coverage means Absent, got {other:?}"),
    }
}

/// Bounded history over the REAL committed chain: a hit reports its source VERSION and real scan cost, and a zero cap yields
/// Incomplete rather than Absent.
#[test]
fn bounded_history_searches_the_committed_chain() {
    use sage_db_store::history::{HistoryBudget, HistoryOutcome};

    let fs = SharedFs::new(ModelFileSystem::new(ModelCapabilities::full()));
    let path = std::path::Path::new("/kbh");
    let ctl = control();
    let mut ledger = AttemptLedger::open(BYTES, ENTRIES, ctl).expect("a finite ledger opens");

    let mut store = match bootstrap(
        fs.clone(),
        path,
        sage_db_store::fs::CreateDir::IfMissing,
        uuid(),
        RULES,
        &Limits::V1,
        &mut ledger,
    ) {
        Ok(store) => store,
        Err(refusal) => panic!("bootstrap must succeed: {refusal:?}"),
    };
    let d2 = [0x22; 32];
    let mut tx = carrier(2, Some(1), "h1", vec![outcome_event(2, "system", "h1", d2)]);
    tx.payload_digest = d2;
    match store.commit(tx, &Limits::V1, &mut ledger) {
        sage_db_store::handle::HandleOutcome::Committed { .. } => {}
        other => panic!("must commit: {other:?}"),
    }
    drop(store);

    let opened = match open_existing(fs.clone(), path, Some(uuid()), &Limits::V1, &mut ledger) {
        Ok(opened) => opened,
        Err(refusal) => panic!("must reopen: {refusal:?}"),
    };
    let mut store = opened.into_store();

    let identity = sage_db_store::fold::RecordIdentity::Natural {
        key: sage_db_store::fold::NaturalKey::KbMetadata,
    };

    // A generous budget: the search reads real transaction bytes and answers definitively.
    match store.history(
        &identity,
        1,
        HistoryBudget::new(10_000),
        &Limits::V1,
        &mut ledger,
    ) {
        HistoryOutcome::Found(hit) => {
            assert_eq!(
                hit.source_version, 1,
                "KB metadata was established by Genesis"
            );
            assert!(hit.scanned_events > 0);
            assert_eq!(hit.basis, store.view().expect("Ready view").basis());
        }
        _ => panic!("the known committed Genesis record must be Found"),
    }

    // Zero cap: Incomplete with a truthful refusal, never Absent and never a fake remaining.
    match store.history(
        &identity,
        1,
        HistoryBudget::new(0),
        &Limits::V1,
        &mut ledger,
    ) {
        HistoryOutcome::Incomplete {
            reason: sage_db_store::history::HistoryIncomplete::EventLimit { limit: 0 },
            scanned_events: 0,
        } => {}
        _ => panic!("a zero-event budget must be Incomplete, never Absent"),
    }
}

// ---------------------------------------------------------------------------
// State-path controls: both Unknown origins, and G0 refusal branches.
// ---------------------------------------------------------------------------

use sage_db_store_tests::faults::{FaultPoint, FaultSchedule, Injection};

/// Attempted-authority Unknown: writes fenced, BOTH fresh current views absent, explicit old pin unaffected.
#[test]
fn attempted_authority_unknown_hides_both_current_views() {
    use sage_db_store::open::{OutcomeIncomplete, OutcomeLookup};

    let model = std::rc::Rc::new(ModelFileSystem::new(ModelCapabilities::full()));
    let fs = SharedFs::from_rc(std::rc::Rc::clone(&model));
    let path = std::path::Path::new("/kbu");
    let ctl = control();
    let mut ledger = AttemptLedger::open(BYTES, ENTRIES, ctl).expect("a finite ledger opens");

    let mut store = match bootstrap(
        fs,
        path,
        sage_db_store::fs::CreateDir::IfMissing,
        uuid(),
        RULES,
        &Limits::V1,
        &mut ledger,
    ) {
        Ok(store) => store,
        Err(refusal) => panic!("bootstrap must succeed: {refusal:?}"),
    };
    let d2 = [0x22; 32];
    let mut tx = carrier(2, Some(1), "u1", vec![outcome_event(2, "system", "u1", d2)]);
    tx.payload_digest = d2;
    match store.commit(tx, &Limits::V1, &mut ledger) {
        sage_db_store::handle::HandleOutcome::Committed { .. } => {}
        other => panic!("must commit: {other:?}"),
    }

    let old_pin = store.published_pin().expect("a durable pin exists");
    let old_entries = old_pin.entries().to_vec();

    // Arm the root-replace ambiguity, then drive it.
    *model.faults_handle().borrow_mut() = FaultSchedule::none().at(
        FaultPoint::ReplaceCurrent,
        Injection::FailUnknownAfterEffect,
        1,
    );
    let d3 = [0x33; 32];
    let mut tx3 = carrier(3, Some(2), "u2", vec![outcome_event(3, "system", "u2", d3)]);
    tx3.payload_digest = d3;
    match store.commit(tx3, &Limits::V1, &mut ledger) {
        sage_db_store::handle::HandleOutcome::Unknown => {}
        other => panic!("the armed ambiguity must surface as Unknown, got {other:?}"),
    }

    // BOTH fresh current views are absent.
    assert!(
        store.view().is_none(),
        "attempted-authority Unknown must hide the tagged current view"
    );
    assert!(
        store.published_pin().is_none(),
        "attempted-authority Unknown must hide the legacy pin too"
    );
    assert_eq!(store.retained_entries().count(), 0, "and the entry view");
    assert!(store.is_fenced(), "writes are fenced");

    // G0 refusal branch: no current view means the outcome query cannot answer at all.
    match store.outcome("system", "u1") {
        OutcomeLookup::Incomplete(OutcomeIncomplete::NoCurrentView) => {}
        other => panic!("an undecided handle cannot answer, got {other:?}"),
    }

    // The explicit old pin the caller already held is untouched.
    assert_eq!(old_pin.entries(), old_entries.as_slice());
    assert_eq!(old_pin.head().get(), 2);
}

/// Orphan-phase Unknown: writes fenced, but the known durable current view REMAINS visible (K12b).
#[test]
fn orphan_unknown_fences_writes_but_keeps_the_durable_view() {
    let model = std::rc::Rc::new(ModelFileSystem::new(ModelCapabilities::full()));
    let fs = SharedFs::from_rc(std::rc::Rc::clone(&model));
    let path = std::path::Path::new("/kbo2");
    let ctl = control();
    let mut ledger = AttemptLedger::open(BYTES, ENTRIES, ctl).expect("a finite ledger opens");

    let mut store = match bootstrap(
        fs,
        path,
        sage_db_store::fs::CreateDir::IfMissing,
        uuid(),
        RULES,
        &Limits::V1,
        &mut ledger,
    ) {
        Ok(store) => store,
        Err(refusal) => panic!("bootstrap must succeed: {refusal:?}"),
    };
    let d2 = [0x22; 32];
    let mut tx = carrier(2, Some(1), "o1", vec![outcome_event(2, "system", "o1", d2)]);
    tx.payload_digest = d2;
    match store.commit(tx, &Limits::V1, &mut ledger) {
        sage_db_store::handle::HandleOutcome::Committed { .. } => {}
        other => panic!("must commit: {other:?}"),
    }
    let durable_entries = store.retained_entries().count();
    assert!(durable_entries > 0);

    // Orphan-phase ambiguity: the publish step, BEFORE authority moves.
    *model.faults_handle().borrow_mut() =
        FaultSchedule::none().at(FaultPoint::Publish, Injection::FailUnknown, 1);
    let d3 = [0x33; 32];
    let mut tx3 = carrier(3, Some(2), "o2", vec![outcome_event(3, "system", "o2", d3)]);
    tx3.payload_digest = d3;
    match store.commit(tx3, &Limits::V1, &mut ledger) {
        sage_db_store::handle::HandleOutcome::NotCommitted { .. } => {}
        other => panic!("orphan Unknown is NotCommitted, got {other:?}"),
    }

    assert!(store.is_fenced(), "writes are fenced");
    assert!(
        store.view().is_some(),
        "K12b: the known durable current view must remain visible"
    );
    assert_eq!(
        store.retained_entries().count(),
        durable_entries,
        "the durable current set is unchanged"
    );
}

/// MEASURED read-only verify: zero mutating or syncing seam calls during `verify`.
#[test]
fn verify_performs_no_writes_and_no_syncs() {
    use observing_fs::ObservingFs;

    let model = std::rc::Rc::new(ModelFileSystem::new(ModelCapabilities::full()));
    let observing = ObservingFs::new(std::rc::Rc::clone(&model));
    let counts = observing.counts();
    let path = std::path::Path::new("/kbw");
    let ctl = control();
    let mut ledger = AttemptLedger::open(BYTES, ENTRIES, ctl).expect("a finite ledger opens");

    let mut store = match bootstrap(
        observing.clone(),
        path,
        sage_db_store::fs::CreateDir::IfMissing,
        uuid(),
        RULES,
        &Limits::V1,
        &mut ledger,
    ) {
        Ok(store) => store,
        Err(refusal) => panic!("bootstrap must succeed: {refusal:?}"),
    };
    let d2 = [0x22; 32];
    let mut tx = carrier(2, Some(1), "w1", vec![outcome_event(2, "system", "w1", d2)]);
    tx.payload_digest = d2;
    match store.commit(tx, &Limits::V1, &mut ledger) {
        sage_db_store::handle::HandleOutcome::Committed { .. } => {}
        other => panic!("must commit: {other:?}"),
    }

    let before = counts.borrow().total();
    match store.verify(&Limits::V1, &mut ledger) {
        Ok(()) => {}
        Err(refusal) => panic!("a committed root must verify: {refusal:?}"),
    }
    let after = counts.borrow().total();

    assert_eq!(
        after, before,
        "verify must make NO mutating or syncing seam calls (before={before} after={after})"
    );
}
