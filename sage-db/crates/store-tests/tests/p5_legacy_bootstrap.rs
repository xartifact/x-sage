//! Explicit import profile uses the same Genesis/commit/reopen path as the formal default.
#[path = "support/shared_fs.rs"]
mod shared_fs;
use sage_db_store::{
    commit::CommitRefusal,
    fs::CreateDir,
    handle::{HandleOutcome, HandleRefusal},
    init::{BootstrapRefusal, bootstrap, bootstrap_with_profile},
    lease::AttemptLedger,
    open::{Opened, open_existing},
    typed,
};
use sage_db_store_tests::model::fs_model::{ModelCapabilities, ModelFileSystem};
use sage_db_types::{
    framing::{CorruptReason, FormatError, FormatOutcome, Limits},
    identity::KbUuid,
    persist::IdSpace,
    query::RunToCompletion,
    relation::RuleProfile,
};
use shared_fs::SharedFs;
use std::{path::Path, rc::Rc};
const FORMAL: &[u8] = include_bytes!("../fixtures/rules.formal-v1.v1.bin");
const LEGACY: &[u8] = include_bytes!("../fixtures/rules.legacy-v1.v1.bin");
const BYTES: u64 = 128 * 1024 * 1024;
const ENTRIES: u64 = 1_000_000;
fn ledger() -> AttemptLedger<'static> {
    AttemptLedger::open(BYTES, ENTRIES, &RunToCompletion).expect("finite ledger")
}
fn kb() -> KbUuid {
    KbUuid::from_bytes([0x35; 16])
}
fn roundtrip(profile: RuleProfile, rules: &[u8]) {
    let fs = SharedFs::from_rc(Rc::new(ModelFileSystem::new(ModelCapabilities::full())));
    let mut l = ledger();
    let store = bootstrap_with_profile(
        fs.clone(),
        Path::new("/profiled-genesis"),
        CreateDir::IfMissing,
        kb(),
        (profile, rules),
        &Limits::V1,
        &mut l,
    )
    .expect("explicit profile commits");
    assert_eq!(store.head(), Some(1));
    let basis = store.commit_basis(IdSpace::TermId, &mut l).expect("basis");
    assert_eq!(basis.rules.profile, profile);
    let pin = store.view().expect("durable publication").basis();
    drop(store);
    drop(l);
    let mut reopened_ledger = ledger();
    let reopened = open_existing(
        fs,
        Path::new("/profiled-genesis"),
        Some(kb()),
        &Limits::V1,
        &mut reopened_ledger,
    )
    .expect("actual bytes reopen");
    let mut store = match reopened {
        Opened::Ready(store) => store,
        Opened::ReadOnly { .. } => panic!("must be durable"),
    };
    assert_eq!(store.view().expect("publication").basis(), pin);
    let binding = store
        .commit_basis(IdSpace::TermId, &mut reopened_ledger)
        .expect("reopened binding");
    assert_eq!(binding.rules.profile, profile);
    assert_eq!(binding.rules.rules_ref, basis.rules.rules_ref);
    let blob = store
        .read_committed_blob(
            binding.rules.rules_ref,
            rules.len() as u64,
            &mut reopened_ledger,
        )
        .expect("same committed rule bytes");
    assert_eq!(blob.as_bytes(), rules);
    let declared = typed::DeclaredRules {
        profile,
        rules_ref: binding.rules.rules_ref,
        rules_digest: binding.rules.rules_digest,
    };
    match typed::verify_rules(
        blob.as_bytes(),
        &declared,
        &Limits::V1,
        &mut reopened_ledger,
    ) {
        FormatOutcome::Ok(verified) => assert_eq!(verified.profile, profile),
        other => panic!("real rule verification: {other:?}"),
    }
}
#[test]
fn legacy_profile_and_actual_rule_blob_survive_genesis_and_restart() {
    roundtrip(RuleProfile::LegacyV1, LEGACY);
}
#[test]
fn explicit_formal_profile_uses_the_same_verified_path() {
    roundtrip(RuleProfile::FormalV1, FORMAL);
}
#[test]
fn ordinary_bootstrap_keeps_the_formal_default() {
    let fs = SharedFs::new(ModelFileSystem::new(ModelCapabilities::full()));
    let mut l = ledger();
    let store = bootstrap(
        fs,
        Path::new("/default-formal"),
        CreateDir::IfMissing,
        kb(),
        FORMAL,
        &Limits::V1,
        &mut l,
    )
    .expect("default formal");
    assert_eq!(
        store
            .commit_basis(IdSpace::TermId, &mut l)
            .expect("basis")
            .rules
            .profile,
        RuleProfile::FormalV1
    );
}
#[test]
fn an_explicit_profile_never_relabels_a_contradictory_blob() {
    let fs = SharedFs::new(ModelFileSystem::new(ModelCapabilities::full()));
    let mut l = ledger();
    let refusal = match bootstrap_with_profile(
        fs,
        Path::new("/wrong-profile"),
        CreateDir::IfMissing,
        kb(),
        (RuleProfile::FormalV1, LEGACY),
        &Limits::V1,
        &mut l,
    ) {
        Ok(_) => panic!("a contradictory declaration cannot initialize"),
        Err(refusal) => refusal,
    };
    match refusal {
        BootstrapRefusal::GenesisNotCommitted(outcome) => assert!(matches!(
            *outcome,
            HandleOutcome::NotCommitted {
                reason: HandleRefusal::Derivation(CommitRefusal::Format(FormatError::Corrupt(
                    CorruptReason::RulesProfileMismatch
                )))
            }
        )),
        other => panic!("the canonical mismatch class must survive: {other:?}"),
    }
}
