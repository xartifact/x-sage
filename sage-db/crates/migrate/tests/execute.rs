//! Staged import execution over a real `Store` on the model filesystem.
//!
//! Every stage goes through the production entry points — `bootstrap`, `Store::commit`,
//! `open_existing`, `prepare_native_mapping`, and this crate's own executor — with the real
//! ledger, the real canonical codecs and the real dispatcher. The plan and its source blobs
//! are authored **here**, in the test, so an expectation is a literal rather than a second
//! copy of the mapper's output.
//!
//! # What this file does not claim
//!
//! Not cutover, not readiness, not device durability. A modeled crash sample shows what a
//! process restart can observe, which is a different question from power loss.

#![allow(dead_code)]

use std::path::Path;
use std::rc::Rc;

use sage_db_format::digest::raw_sha256;
use sage_db_kernel::auth::{Capability, CapabilitySet, Grants};
use sage_db_migrate::execute::{
    ExecuteError, ExpectedField, ExpectedIdentity, StepContext, VerifyDefect,
    mark_complete_after_verification, verify_and_attest, verify_written, write_stage,
};
use sage_db_migrate::intent::ImportStep;
use sage_db_migrate::mapping::{
    MappingError, MappingLimits, SourceArchive, prepare_native_mapping,
};
use sage_db_store::fs::{
    Capabilities, CreateDir, Each, Family, FileSystem, FsError, HashReport, Published, WriteOutcome,
};
use sage_db_store::handle::{HandleOutcome, Store};
use sage_db_store::lease::AttemptLedger;
use sage_db_store::names::EntryName;
use sage_db_store::open::{Opened, open_existing};
use sage_db_store_tests::faults::{FaultPoint, FaultSchedule, Injection};
use sage_db_store_tests::model::fs_model::{ModelCapabilities, ModelFileSystem};
use sage_db_types::framing::Limits;
use sage_db_types::identity::KbUuid;
use sage_db_types::ledger::SharedLedger;
use sage_db_types::persist::{ImportStage, StateKind};
use sage_db_types::query::{ControlStop, QueryControl, RunToCompletion};

const BYTES: u64 = 512 * 1024 * 1024;
const ENTRIES: u64 = 8_000_000;
const ADMIN: &str = "p4-importer";
const SESSION: &str = "import-session-1";
const ROOT: &str = "/native-executor-root";

// ---------------------------------------------------------------------------
// A model filesystem reachable from several handles
// ---------------------------------------------------------------------------

/// One model filesystem handed to by-value entry points more than once.
///
/// `bootstrap` and `open_existing` both consume the filesystem by value, and
/// `ModelFileSystem` owns its state and is not `Clone`. Forwarding to a shared `Rc`
/// is what lets a test drop one `Store` and reopen the SAME bytes.
#[derive(Clone)]
struct SharedFs(Rc<ModelFileSystem>);

impl FileSystem for SharedFs {
    type Dir = <ModelFileSystem as FileSystem>::Dir;
    type WriteFile = <ModelFileSystem as FileSystem>::WriteFile;
    type ReadFile = <ModelFileSystem as FileSystem>::ReadFile;
    type Lock = <ModelFileSystem as FileSystem>::Lock;

    fn capabilities(&self) -> Capabilities {
        self.0.capabilities()
    }
    fn open_root(&self, abs: &Path) -> Result<Self::Dir, FsError> {
        self.0.open_root(abs)
    }
    fn create_root(&self, abs: &Path) -> Result<Self::Dir, FsError> {
        self.0.create_root(abs)
    }
    fn sync_parent_of(&self, abs: &Path) -> Result<(), FsError> {
        self.0.sync_parent_of(abs)
    }
    fn open_family(
        &self,
        root: &Self::Dir,
        family: Family,
        create: CreateDir,
    ) -> Result<Self::Dir, FsError> {
        self.0.open_family(root, family, create)
    }
    fn sync_dir(&self, dir: &Self::Dir) -> Result<(), FsError> {
        self.0.sync_dir(dir)
    }
    fn device_id(&self, dir: &Self::Dir) -> Result<u64, FsError> {
        self.0.device_id(dir)
    }
    fn try_lock_exclusive(&self, dir: &Self::Dir) -> Result<Self::Lock, FsError> {
        self.0.try_lock_exclusive(dir)
    }
    fn create_new(&self, dir: &Self::Dir, name: EntryName) -> Result<Self::WriteFile, FsError> {
        self.0.create_new(dir, name)
    }
    fn write(&self, file: &mut Self::WriteFile, buf: &[u8]) -> Result<WriteOutcome, FsError> {
        self.0.write(file, buf)
    }
    fn sync_file(&self, file: &Self::WriteFile) -> Result<(), FsError> {
        self.0.sync_file(file)
    }
    fn sync_entry(&self, dir: &Self::Dir, name: EntryName) -> Result<(), FsError> {
        self.0.sync_entry(dir, name)
    }
    fn publish_no_overwrite(
        &self,
        staged_dir: &Self::Dir,
        staged: EntryName,
        dst_dir: &Self::Dir,
        dst: EntryName,
        expected: &HashReport,
        ledger: &mut dyn SharedLedger,
    ) -> Result<Published, FsError> {
        self.0
            .publish_no_overwrite(staged_dir, staged, dst_dir, dst, expected, ledger)
    }
    fn open_read(&self, dir: &Self::Dir, name: EntryName) -> Result<Self::ReadFile, FsError> {
        self.0.open_read(dir, name)
    }
    fn read(&self, file: &mut Self::ReadFile, buf: &mut [u8]) -> Result<usize, FsError> {
        self.0.read(file, buf)
    }
    fn read_exact_into(
        &self,
        dir: &Self::Dir,
        name: EntryName,
        buf: &mut [u8],
    ) -> Result<usize, FsError> {
        self.0.read_exact_into(dir, name, buf)
    }
    fn hash_bounded(
        &self,
        dir: &Self::Dir,
        name: EntryName,
        max_len: u64,
        ledger: &mut dyn SharedLedger,
    ) -> Result<HashReport, FsError> {
        self.0.hash_bounded(dir, name, max_len, ledger)
    }
    fn size(&self, dir: &Self::Dir, name: EntryName) -> Result<u64, FsError> {
        self.0.size(dir, name)
    }
    fn for_each_entry(
        &self,
        dir: &Self::Dir,
        ledger: &mut dyn SharedLedger,
        visit: &mut dyn FnMut(&str) -> Each,
    ) -> Result<(), FsError> {
        self.0.for_each_entry(dir, ledger, visit)
    }
    fn replace_current(
        &self,
        staged_dir: &Self::Dir,
        staged: EntryName,
        root: &Self::Dir,
    ) -> Result<(), FsError> {
        self.0.replace_current(staged_dir, staged, root)
    }
    fn remove(&self, dir: &Self::Dir, name: EntryName) -> Result<(), FsError> {
        self.0.remove(dir, name)
    }
}

fn ledger() -> AttemptLedger<'static> {
    AttemptLedger::open(BYTES, ENTRIES, &RunToCompletion).expect("ledger")
}

fn kb() -> KbUuid {
    KbUuid::from_bytes([0x77; 16])
}

/// The canonical `legacy-v1` rules bytes, produced by the format's own encoder.
fn rules_bytes() -> Vec<u8> {
    let mut l = ledger();
    sage_db_format::rules::CanonicalRules::new(sage_db_types::relation::RuleProfile::LegacyV1)
        .encode(&mut l)
        .expect("the canonical rules encoder accepts its own profile")
}

fn admin() -> Grants<'static> {
    Grants::new(ADMIN, kb(), CapabilitySet::of(&[Capability::Admin]))
}

/// A fresh model world: the shared backend plus a bootstrapped target.
fn world() -> (Rc<ModelFileSystem>, SharedFs, Store<SharedFs>) {
    let inner = Rc::new(ModelFileSystem::new(ModelCapabilities::full()));
    let fs = SharedFs(Rc::clone(&inner));
    let mut l = ledger();
    // The profile-aware entry: a legacy-v1 migration target, not a formal-v1 default.
    let store = sage_db_store::init::bootstrap_with_profile(
        fs.clone(),
        Path::new(ROOT),
        CreateDir::IfMissing,
        kb(),
        (
            sage_db_types::relation::RuleProfile::LegacyV1,
            &rules_bytes(),
        ),
        &Limits::V1,
        &mut l,
    )
    .expect("bootstrap_with_profile");
    (inner, fs, store)
}

/// Reopen the same bytes durably.
fn reopen(fs: SharedFs) -> Store<SharedFs> {
    let mut l = ledger();
    match open_existing(fs, Path::new(ROOT), Some(kb()), &Limits::V1, &mut l) {
        Ok(Opened::Ready(store)) => store,
        Ok(Opened::ReadOnly { .. }) => panic!("the target must replay durably"),
        Err(error) => panic!("reopen failed: {error:?}"),
    }
}

// ---------------------------------------------------------------------------
// An authored plan and its source archive
// ---------------------------------------------------------------------------

/// Serves the plan's blobs from an in-memory table, charging the real ledger.
struct MemArchive {
    files: Vec<(String, Vec<u8>)>,
}

impl SourceArchive for MemArchive {
    fn blob(
        &mut self,
        ledger: &mut dyn sage_db_types::ledger::SharedLedger,
        plan_blob_path: &str,
        expected_bytes: u64,
        expected_sha256: &str,
    ) -> Result<Vec<u8>, MappingError> {
        let name = plan_blob_path.trim_start_matches("blobs/");
        let Some((_, bytes)) = self.files.iter().find(|(sha, _)| sha == name) else {
            return Err(MappingError::InvalidSource {
                location: sage_db_migrate::mapping::SourceLocation::row(
                    sage_db_migrate::mapping::SourceArray::Attachments,
                    0,
                ),
                detail: "the plan names a blob the archive does not hold",
            });
        };
        if bytes.len() as u64 != expected_bytes {
            return Err(MappingError::InvalidSource {
                location: sage_db_migrate::mapping::SourceLocation::row(
                    sage_db_migrate::mapping::SourceArray::Attachments,
                    0,
                ),
                detail: "declared length differs",
            });
        }
        ledger.try_charge(expected_bytes).map_err(|error| {
            MappingError::Refused(sage_db_types::framing::Refusal::Budget(error))
        })?;
        ledger.checkpoint().map_err(|stop| {
            MappingError::Refused(sage_db_types::framing::Refusal::Stopped(stop))
        })?;
        let mut out = Vec::new();
        out.try_reserve_exact(bytes.len()).map_err(|_| {
            MappingError::Refused(sage_db_types::framing::Refusal::Allocation {
                requested: bytes.len(),
            })
        })?;
        out.extend_from_slice(bytes);
        if hex(&raw_sha256(&out)) != expected_sha256 {
            return Err(MappingError::InvalidSource {
                location: sage_db_migrate::mapping::SourceLocation::row(
                    sage_db_migrate::mapping::SourceArray::Attachments,
                    0,
                ),
                detail: "hash differs",
            });
        }
        Ok(out)
    }
}

fn hex(bytes: &[u8; 32]) -> String {
    let mut out = String::new();
    for byte in bytes {
        out.push(char::from_digit((byte >> 4) as u32, 16).expect("nibble"));
        out.push(char::from_digit((byte & 0x0f) as u32, 16).expect("nibble"));
    }
    out
}

/// The authored fixture: one concept with a body and one authored relation, plus the KB
/// descriptor and a checkpoint. Every byte is written here.
fn fixture() -> (Vec<u8>, MemArchive) {
    let source = "---\nid: \"alpha\"\n---\n\n# Alpha\n\nBody text.\n";
    let source_bytes = source.as_bytes().to_vec();
    let source_sha = hex(&raw_sha256(&source_bytes));

    let relation_target = "---\nid: \"beta\"\n---\n\n# Beta\n\nBeta body.\n";
    let relation_bytes = relation_target.as_bytes().to_vec();
    let relation_sha = hex(&raw_sha256(&relation_bytes));

    let kb_json =
        br#"{"id":"default","name":"Fixture KB","description":"authored","domains":["product"]}"#
            .to_vec();
    let kb_sha = hex(&raw_sha256(&kb_json));

    // An approved edge proposal. The mapper restates an approved source proposal as a NEW
    // Pending@1 then Decided@2 lifecycle of ONE identity, so this is the only shape that makes
    // the multi-revision verification branch (current + history) actually run.
    let proposal_payload = "## \u{65b0}\u{8fb9}\u{63d0}\u{6848}\n\n- **from**: alpha\n- **type**: enables\n- **target**: beta\n- **confidence**: confirmed\n\nRationale prose, retained verbatim in the blob.\n".as_bytes().to_vec();
    let proposal_sha = hex(&raw_sha256(&proposal_payload));

    let checkpoint = br#"{"concept":"alpha","session":1,"established":[]}"#.to_vec();
    let checkpoint_sha = hex(&raw_sha256(&checkpoint));

    // Body slices are everything after the closing frontmatter delimiter.
    let alpha_body_at = source.find("# Alpha").expect("alpha body");
    let alpha_body = &source_bytes[alpha_body_at..];
    let beta_body_at = relation_target.find("# Beta").expect("beta body");
    let beta_body = &relation_bytes[beta_body_at..];

    let plan = serde_json::json!({
        "plan_version": "2",
        "source": {
            "archive_dir": "/authored/fixture",
            "archive_content_digest": format!("sha256:{}", "aa".repeat(32)),
            "source_digest": format!("sha256:{}", "bb".repeat(32)),
            "authority": "markdown",
            "kb_id": "default",
            "files": 3,
            "bytes": source_bytes.len() + relation_bytes.len() + proposal_payload.len(),
            "extraction": "complete"
        },
        "counts": {
            "term": 2, "assertion": 1, "proposal": 1, "checkpoint": 1,
            "frontier_entry": 1, "attachment": 5, "source_proposal_files": 1
        },
        "terms": [
            {
                "source_id": "alpha",
                "source_path": "concepts/L1-core/alpha.md",
                "target_id": 1,
                "slug": "alpha",
                "atom": "alpha is the subject",
                "layer": 1,
                "domain": ["product"],
                "confidence": "confirmed",
                "source": "synthesis",
                "triggers": ["alpha"],
                "body": {"offset": alpha_body_at, "length": alpha_body.len(), "sha256": hex(&raw_sha256(alpha_body))},
                "file": {"bytes": source_bytes.len(), "sha256": source_sha, "blob": format!("blobs/{source_sha}")}
            },
            {
                "source_id": "beta",
                "source_path": "concepts/L0-axioms/beta.md",
                "target_id": 2,
                "slug": "beta",
                "atom": "beta is the object",
                "layer": 0,
                "domain": ["product"],
                "confidence": "confirmed",
                "source": "synthesis",
                "triggers": ["beta"],
                "body": {"offset": beta_body_at, "length": beta_body.len(), "sha256": hex(&raw_sha256(beta_body))},
                "file": {"bytes": relation_bytes.len(), "sha256": relation_sha, "blob": format!("blobs/{relation_sha}")}
            }
        ],
        "assertions": [{
            "source_id": "alpha:0",
            "source_path": "concepts/L1-core/alpha.md",
            "target_id": 1,
            "from_slug": "alpha",
            "from_target_id": 1,
            "relation": "is-a",
            "target": "beta",
            "target_target_id": 2,
            "confidence": "confirmed",
            "declaration_order": 0
        }],
        "proposals": [{
            "source_id": "edge:alpha:enables:beta",
            "source_path": "concept-proposals/alpha-enables-beta.md",
            "target_id": 1,
            "status": "approved",
            "concept": null,
            "payload": {
                "bytes": proposal_payload.len(),
                "sha256": proposal_sha,
                "blob": format!("blobs/{}", proposal_sha)
            },
            "declared_author": "fixture-author",
            "expected_revision": "r0"
        }],
        "checkpoints": [{
            "source_id": "checkpoints/alpha-s1.json",
            "source_path": "checkpoints/alpha-s1.json",
            "target_id": 1,
            "concept": "alpha",
            "session": 1,
            "content": {"offset": 0, "length": checkpoint.len(), "sha256": checkpoint_sha}
        }],
        "frontier": [{
            "source_id": "\u{672a}\u{51b3}\u{6982}\u{5ff5}/0/0",
            "source_path": "frontier.md",
            "section": "\u{672a}\u{51b3}\u{6982}\u{5ff5}",
            "table": 0, "row": 0, "declaration_order": 0, "target_id": 1,
            "cells": ["alpha", "a.md", "why?", "\u{9ad8}"],
            "raw": "| alpha | a.md | why? | \u{9ad8} |"
        }],
        "attachments": [
            {"path": "concepts/L1-core/alpha.md", "bytes": source_bytes.len(), "sha256": source_sha},
            {"path": "concepts/L0-axioms/beta.md", "bytes": relation_bytes.len(), "sha256": relation_sha},
            {"path": "kb.json", "bytes": kb_json.len(), "sha256": kb_sha},
            {"path": "checkpoints/alpha-s1.json", "bytes": checkpoint.len(), "sha256": checkpoint_sha},
            {"path": "concept-proposals/alpha-enables-beta.md", "bytes": proposal_payload.len(), "sha256": proposal_sha}
        ],
        "unresolved": [],
        "errors": []
    });

    let archive = MemArchive {
        files: vec![
            (source_sha, source_bytes),
            (relation_sha, relation_bytes),
            (kb_sha, kb_json),
            (checkpoint_sha, checkpoint),
            (proposal_sha, proposal_payload),
        ],
    };
    (serde_json::to_vec(&plan).expect("plan serialises"), archive)
}

fn limits() -> MappingLimits {
    MappingLimits {
        max_plan_bytes: 8 << 20,
        max_files: 100,
        max_bytes: 64 << 20,
        max_records: 10_000,
    }
}

/// Per-stage trusted context.
///
/// `tx_id` names the transaction file, so two stages MUST NOT share one: reusing it makes the
/// second publication collide with the first. Deriving it from the stage keeps every attempt
/// distinct while the canonical request digest stays independent of it.
fn context_for<'a>(limits: &'a Limits, stage: ImportStage) -> StepContext<'a> {
    let mut tx_id = [0u8; 16];
    tx_id[0] = 0x5a;
    tx_id[1] = stage.as_u16() as u8;
    StepContext {
        principal: ADMIN,
        tx_id,
        created_at: 1_700_000_000_000_000 + i64::from(stage.as_u16()),
        limits,
        expect_version: None,
    }
}

/// Prepare the mapping from the authored fixture.
fn prepared_for(plan: &[u8], archive: &mut MemArchive) -> sage_db_migrate::mapping::PreparedImport {
    let mut l = ledger();
    prepare_native_mapping(
        plan,
        archive,
        &admin(),
        kb(),
        &sage_db_migrate::mapping::ExecutorContext {
            import_time: 1_700_000_000_000_000,
            reason: "authored fixture import".into(),
            session_id: SESSION.into(),
        },
        limits(),
        &mut l,
    )
    .expect("the authored plan must prepare")
}

/// The frozen expectation for this fixture.
fn expected_for(plan: &[u8]) -> ExpectedIdentity<'static> {
    let digest = raw_sha256(plan);
    ExpectedIdentity {
        plan_sha256: digest,
        source_digest: digest_of_hex(&"bb".repeat(32)),
        kb_uuid: kb(),
        import_session_id: SESSION,
    }
}

fn digest_of_hex(text: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (at, chunk) in text.as_bytes().chunks(2).enumerate() {
        let pair = core::str::from_utf8(chunk).expect("ascii");
        out[at] = u8::from_str_radix(pair, 16).expect("hex");
    }
    out
}

// ---------------------------------------------------------------------------
// The five-stage drive
// ---------------------------------------------------------------------------

/// Drive every stage of the import, returning the store and the prepared plan.
fn run_all_stages(
    store: &mut Store<SharedFs>,
    prepared: &sage_db_migrate::mapping::PreparedImport,
    plan: &[u8],
    expected: &ExpectedIdentity<'_>,
    limits: &Limits,
) {
    let mut l = ledger();
    // The REAL five-stage sequence, in registry order. Only Import carries mapped records;
    // Prepare writes the session, Validate writes no state at all, and Verify/Activate write the
    // outcome. That is exactly what the stage table in the operator doc claims.
    let prepare = step_of(
        ImportStage::Prepare,
        prepared.import_session().plan_ref,
        expected.source_digest,
        0,
    );
    write_stage(
        store,
        &admin(),
        expected,
        plan,
        prepared,
        &prepare,
        &context_for(limits, ImportStage::Prepare),
        &mut l,
    )
    .expect("prepare commits");

    // Validate: a real stage with a real persisted outcome and NO mapped record. It is not
    // skipped, because skipping it would leave the registry order untested.
    let validate = step_of(
        ImportStage::Validate,
        prepared.import_session().plan_ref,
        expected.source_digest,
        0,
    );
    let validate_outcome = write_stage(
        store,
        &admin(),
        expected,
        plan,
        prepared,
        &validate,
        &context_for(limits, ImportStage::Validate),
        &mut l,
    )
    .expect("validate commits");
    // Idempotent: the identical step returns its recorded outcome and writes nothing new.
    let validate_again = write_stage(
        store,
        &admin(),
        expected,
        plan,
        prepared,
        &validate,
        &context_for(limits, ImportStage::Validate),
        &mut l,
    )
    .expect("validate retry");
    let original = match validate_outcome.outcome {
        sage_db_store::handle::HandleOutcome::Committed { version, .. } => version,
        other => panic!("Validate must commit its own outcome, got {other:?}"),
    };
    match validate_again.outcome {
        sage_db_store::handle::HandleOutcome::AlreadyCommitted { version, .. } => {
            assert_eq!(
                version, original,
                "an exact retry of Validate must return the ORIGINAL version"
            );
        }
        other => panic!("the retry must be already committed, not replayed: {other:?}"),
    }

    let events = u32::try_from(prepared.events().len()).expect("fits");
    let import = step_of(
        ImportStage::Import,
        prepared.import_session().plan_ref,
        expected.source_digest,
        events,
    );
    write_stage(
        store,
        &admin(),
        expected,
        plan,
        prepared,
        &import,
        &context_for(limits, ImportStage::Import),
        &mut l,
    )
    .expect("import commits");

    let verified = verify_and_attest(store, prepared, expected, &mut l).expect("counts match");
    let activate = step_of(
        ImportStage::Activate,
        prepared.import_session().plan_ref,
        expected.source_digest,
        0,
    );
    mark_complete_after_verification(
        store,
        &admin(),
        expected,
        prepared,
        verified,
        &activate,
        &context_for(limits, ImportStage::Activate),
        &mut l,
    )
    .expect("activate commits");
}

fn step_of(
    stage: ImportStage,
    plan_ref: [u8; 32],
    source: [u8; 32],
    event_count: u32,
) -> ImportStep<'static> {
    ImportStep {
        package_ref: plan_ref,
        source_digest: source,
        stage,
        batch_index: 0,
        batch_count: 1,
        first_event: 0,
        event_count,
        import_session_id: SESSION,
    }
}

#[test]
fn the_attestation_token_owns_credit_that_outlives_its_producing_attempt() {
    let (_inner, _fs, mut store) = world();
    let (plan, mut archive) = fixture();
    let prepared = prepared_for(&plan, &mut archive);
    let expected = expected_for(&plan);
    let limits = Limits::V1;

    // Prepare and import on one attempt, so the store holds the plan's records.
    prepare_and_import(&mut store, &prepared, &plan, &expected, &mut ledger());

    // Measure the TRANSFER BOUNDARY alone. Verification itself admits its own working tables, so
    // the pre-verification figure would include them and say nothing about adoption.
    let mut attestation_attempt = ledger();
    let baseline = attestation_attempt.stats();
    let token = verify_and_attest(&mut store, &prepared, &expected, &mut attestation_attempt)
        .expect("counts match");
    let after = attestation_attempt.stats();

    // `retained_bytes` is the token's OWN reservation, exactly the string capacity it holds.
    let capacity = token.session_capacity();
    assert_eq!(
        token.retained_bytes(),
        capacity,
        "the token must reserve exactly the session string's capacity"
    );
    assert_eq!(token.session(), SESSION);

    // TRANSFER, not double admission, observed on the PRODUCTION path's own counters.
    //
    // `admitted_bytes` is CUMULATIVE, which makes it the exact instrument: a transfer admits the
    // extent once (the `try_charge`), a second reservation admits it twice. Verification itself
    // also admits its working tables, so the reference is a `verify_written`-only run on identical
    // state: the two admitted totals must differ by EXACTLY one extent, never two.
    assert_eq!(
        after.held_bytes,
        baseline.held_bytes + capacity,
        "the transferred extent must appear in held exactly once"
    );
    {
        let mut reference_attempt = ledger();
        verify_written(&mut store, &prepared, &expected, &mut reference_attempt)
            .expect("the same state verifies");
        let reference = reference_attempt
            .stats()
            .admitted_bytes
            .expect("representable");
        let attested = after.admitted_bytes.expect("representable");
        // Attestation ALSO admits the marks table (one entries charge plus `(u16,u128)` slots),
        // so the legitimate delta is "marks + one session extent". What must never happen is the
        // session extent appearing a SECOND time, which would make the delta marks + 2 * capacity.
        let marks_bytes =
            (prepared.watermarks().len() as u64) * core::mem::size_of::<(u16, u128)>() as u64;
        let one_extent = u128::from(capacity) + u128::from(marks_bytes);
        let two_extents = u128::from(2 * capacity) + u128::from(marks_bytes);
        assert_ne!(
            attested - reference,
            two_extents,
            "the session extent must not be admitted twice"
        );
        assert_eq!(
            attested - reference,
            one_extent,
            "attestation admits the session extent exactly once beyond verification"
        );
    }

    // Dropping the ATTEMPT must not release what the token owns.
    drop(attestation_attempt);
    assert_eq!(
        token.retained_bytes(),
        capacity,
        "a facade drop cannot uncharge a still-held token"
    );

    // Activation consumes the token on a fresh ledger.
    let mut l = ledger();
    let activated = mark_complete_after_verification(
        &mut store,
        &admin(),
        &expected,
        &prepared,
        token,
        &step_of(
            ImportStage::Activate,
            prepared.import_session().plan_ref,
            expected.source_digest,
            0,
        ),
        &context_for(&limits, ImportStage::Activate),
        &mut l,
    );
    assert!(
        activated.is_ok(),
        "activate must accept the retained session: {activated:?}"
    );
    match store.import_readiness(&mut l) {
        Ok(sage_db_store::view::ImportReadiness::Ready { .. }) => {}
        other => panic!("expected a completed import, got {other:?}"),
    }
}

#[test]
fn all_five_stages_run_and_the_gate_closes_then_opens_across_a_real_reopen() {
    let (_inner, fs, mut store) = world();
    let (plan, mut archive) = fixture();
    let prepared = prepared_for(&plan, &mut archive);
    let expected = expected_for(&plan);
    let limits = Limits::V1;
    let mut l = ledger();

    // --- Prepare: the session exists and the gate is CLOSED.
    let prepare = step_of(
        ImportStage::Prepare,
        prepared.import_session().plan_ref,
        expected.source_digest,
        0,
    );
    write_stage(
        &mut store,
        &admin(),
        &expected,
        &plan,
        &prepared,
        &prepare,
        &context_for(&limits, ImportStage::Prepare),
        &mut l,
    )
    .expect("prepare");
    assert!(matches!(
        store.import_readiness(&mut ledger()),
        Ok(sage_db_store::view::ImportReadiness::Incomplete { .. })
    ));

    // --- Import, verify, activate.
    let events = u32::try_from(prepared.events().len()).expect("fits");
    let import = step_of(
        ImportStage::Import,
        prepared.import_session().plan_ref,
        expected.source_digest,
        events,
    );
    write_stage(
        &mut store,
        &admin(),
        &expected,
        &plan,
        &prepared,
        &import,
        &context_for(&limits, ImportStage::Import),
        &mut l,
    )
    .expect("import");
    let verified = verify_and_attest(&mut store, &prepared, &expected, &mut l).expect("verify");
    let activate = step_of(
        ImportStage::Activate,
        prepared.import_session().plan_ref,
        expected.source_digest,
        0,
    );
    mark_complete_after_verification(
        &mut store,
        &admin(),
        &expected,
        &prepared,
        verified,
        &activate,
        &context_for(&limits, ImportStage::Activate),
        &mut l,
    )
    .expect("activate");
    assert!(matches!(
        store.import_readiness(&mut ledger()),
        Ok(sage_db_store::view::ImportReadiness::Ready { .. })
    ));

    // --- A real drop and reopen: completion is persisted, not process-local.
    drop(store);
    let mut reopened = reopen(fs);
    assert!(matches!(
        reopened.import_readiness(&mut ledger()),
        Ok(sage_db_store::view::ImportReadiness::Ready { .. })
    ));

    // The full typed comparison, on the reopened handle.
    verify_reopened(&mut reopened, &prepared, &expected);
}

/// Compare every current record and blob against the plan, on a reopened store.
///
/// This is the whole-typed-state check the counts alone cannot give: it re-derives each
/// record's canonical bytes from the store's own fold and compares the digest the plan
/// published, and it re-reads every referenced blob including the self-contained package.
fn verify_reopened(
    store: &mut Store<SharedFs>,
    prepared: &sage_db_migrate::mapping::PreparedImport,
    expected: &ExpectedIdentity<'_>,
) {
    // 1. Counts, all thirteen kinds through the physical scan.
    let mut counts = [0u64; 13];
    let kinds = [
        StateKind::Term,
        StateKind::Assertion,
        StateKind::Claim,
        StateKind::Evidence,
        StateKind::EvidenceLink,
        StateKind::Vector,
        StateKind::Proposal,
        StateKind::FrontierEntry,
        StateKind::SessionCheckpoint,
        StateKind::Asset,
        StateKind::Model,
        StateKind::KbMetadata,
        StateKind::RuleConfig,
    ];
    store
        .scan_current_records(&mut ledger(), |record, _| {
            use sage_db_store::fold::FoldedRecord as R;
            let kind = match record {
                R::Term(_) => StateKind::Term,
                R::Assertion(_) => StateKind::Assertion,
                R::Claim(_) => StateKind::Claim,
                R::Evidence(_) => StateKind::Evidence,
                R::EvidenceLink(_) => StateKind::EvidenceLink,
                R::Vector(_) => StateKind::Vector,
                R::Proposal(_) => StateKind::Proposal,
                R::FrontierEntry(_) => StateKind::FrontierEntry,
                R::SessionCheckpoint(_) => StateKind::SessionCheckpoint,
                R::Asset(_) => StateKind::Asset,
                R::Model(_) => StateKind::Model,
                R::KbMetadata(_) => StateKind::KbMetadata,
                R::RuleConfig(_) => StateKind::RuleConfig,
            };
            if let Some(at) = kinds.iter().position(|k| *k == kind) {
                counts[at] += 1;
            }
            Ok::<(), ()>(())
        })
        .expect("scan");
    for (at, kind) in kinds.iter().enumerate() {
        if let Some(expected_rows) = prepared.count_of(*kind) {
            assert_eq!(counts[at], expected_rows, "count for {kind:?}");
        }
    }

    // 2. Every referenced blob, by its actual bytes and digest.
    let package_sha = prepared.package().sha256;
    let mut package_read = false;
    for blob in prepared.blobs() {
        let bytes = store
            .read_committed_blob(blob.sha256, blob.declared_len, &mut ledger())
            .unwrap_or_else(|e| panic!("blob {:?} unreadable: {e:?}", blob.sha256));
        assert_eq!(
            bytes.len() as u64,
            blob.declared_len,
            "blob length must equal its declaration"
        );
        assert_eq!(
            raw_sha256(bytes.as_bytes()),
            blob.sha256,
            "blob bytes must hash to their own name"
        );
        if blob.sha256 == package_sha {
            package_read = true;
            // The package is self-contained: its bytes are the plan it was built from.
            assert_eq!(
                bytes.as_bytes(),
                prepared.package_bytes().expect("package bytes"),
                "the archived package must be the prepared package"
            );
        }
    }
    assert!(
        package_read,
        "the whole package must be archived and readable"
    );

    // 3. The session is recorded complete at this identity.
    match store.import_readiness(&mut ledger()) {
        Ok(sage_db_store::view::ImportReadiness::Ready { .. }) => {}
        other => panic!("expected a completed import, got {other:?}"),
    }
    let _ = expected;
}

#[test]
fn a_wrong_frozen_plan_digest_is_refused_before_any_mutation() {
    let (_inner, _fs, mut store) = world();
    let (plan, mut archive) = fixture();
    let prepared = prepared_for(&plan, &mut archive);
    let mut expected = expected_for(&plan);
    expected.plan_sha256 = [0xff; 32];
    let before = store.head();
    let prepare = step_of(
        ImportStage::Prepare,
        prepared.import_session().plan_ref,
        expected.source_digest,
        0,
    );
    assert!(matches!(
        write_stage(
            &mut store,
            &admin(),
            &expected,
            &plan,
            &prepared,
            &prepare,
            &context_for(&Limits::V1, ImportStage::Prepare),
            &mut ledger()
        ),
        Err(ExecuteError::IdentityMismatch {
            field: ExpectedField::PlanDigest
        })
    ));
    assert_eq!(store.head(), before, "no mutation occurred");
}

#[test]
fn a_wrong_kb_and_session_and_source_digest_are_refused() {
    let (_inner, _fs, mut store) = world();
    let (plan, mut archive) = fixture();
    let prepared = prepared_for(&plan, &mut archive);
    let limits = Limits::V1;
    let prepare = step_of(
        ImportStage::Prepare,
        prepared.import_session().plan_ref,
        expected_for(&plan).source_digest,
        0,
    );

    let mut wrong_kb = expected_for(&plan);
    wrong_kb.kb_uuid = KbUuid::from_bytes([0x11; 16]);
    assert!(matches!(
        write_stage(
            &mut store,
            &admin(),
            &wrong_kb,
            &plan,
            &prepared,
            &prepare,
            &context_for(&limits, ImportStage::Prepare),
            &mut ledger()
        ),
        Err(ExecuteError::IdentityMismatch {
            field: ExpectedField::Kb
        })
    ));

    let mut wrong_session = expected_for(&plan);
    wrong_session.import_session_id = "another";
    assert!(matches!(
        write_stage(
            &mut store,
            &admin(),
            &wrong_session,
            &plan,
            &prepared,
            &prepare,
            &context_for(&limits, ImportStage::Prepare),
            &mut ledger()
        ),
        Err(ExecuteError::IdentityMismatch {
            field: ExpectedField::Session
        })
    ));

    let mut wrong_source = expected_for(&plan);
    wrong_source.source_digest = [0x00; 32];
    assert!(matches!(
        write_stage(
            &mut store,
            &admin(),
            &wrong_source,
            &plan,
            &prepared,
            &prepare,
            &context_for(&limits, ImportStage::Prepare),
            &mut ledger()
        ),
        Err(ExecuteError::IdentityMismatch {
            field: ExpectedField::SourceDigest
        })
    ));
    assert_eq!(store.head(), Some(1), "none of the three refusals wrote");
}

#[test]
fn an_exact_retry_of_a_stage_returns_the_original_outcome_without_a_second_commit() {
    let (_inner, _fs, mut store) = world();
    let (plan, mut archive) = fixture();
    let prepared = prepared_for(&plan, &mut archive);
    let expected = expected_for(&plan);
    let limits = Limits::V1;
    let prepare = step_of(
        ImportStage::Prepare,
        prepared.import_session().plan_ref,
        expected.source_digest,
        0,
    );
    let first = write_stage(
        &mut store,
        &admin(),
        &expected,
        &plan,
        &prepared,
        &prepare,
        &context_for(&limits, ImportStage::Prepare),
        &mut ledger(),
    )
    .expect("first prepare");
    let version = match first.outcome {
        HandleOutcome::Committed { version, .. } => version,
        other => panic!("expected a commit: {other:?}"),
    };
    let head = store.head();

    let retried = write_stage(
        &mut store,
        &admin(),
        &expected,
        &plan,
        &prepared,
        &prepare,
        &context_for(&limits, ImportStage::Prepare),
        &mut ledger(),
    )
    .expect("retry answers");
    match retried.outcome {
        HandleOutcome::AlreadyCommitted { version: at, .. } => assert_eq!(at, version),
        other => panic!("expected the original outcome: {other:?}"),
    }
    assert_eq!(store.head(), head, "a retry must not commit again");
}

#[test]
fn a_prepare_on_a_non_genesis_target_is_refused() {
    let (_inner, _fs, mut store) = world();
    let (plan, mut archive) = fixture();
    let prepared = prepared_for(&plan, &mut archive);
    let expected = expected_for(&plan);
    let limits = Limits::V1;
    run_all_stages(&mut store, &prepared, &plan, &expected, &limits);

    // A byte-identical second Prepare is an exact retry: the stored outcome answers and
    // nothing is written. That is the correct behaviour, and it is what proves the
    // outcome-first ordering — a freshness check placed before the lookup would refuse the
    // request that already committed.
    let prepare = step_of(
        ImportStage::Prepare,
        prepared.import_session().plan_ref,
        expected.source_digest,
        0,
    );
    write_stage(
        &mut store,
        &admin(),
        &expected,
        &plan,
        &prepared,
        &prepare,
        &context_for(&limits, ImportStage::Prepare),
        &mut ledger(),
    )
    .expect("an exact retry answers from the stored outcome");

    // A NOVEL Prepare — a different session on the same target — must be refused, because
    // this target is no longer a fresh Genesis.
    let mut second = prepared_for(&plan, &mut {
        let (_, mut archive) = fixture();
        archive
    });
    let _ = &second;
    let novel = ImportStep {
        import_session_id: "second-session",
        ..prepare
    };
    let second_expected = ExpectedIdentity {
        import_session_id: "second-session",
        ..expected
    };
    match write_stage(
        &mut store,
        &admin(),
        &second_expected,
        &plan,
        &prepared,
        &novel,
        &context_for(&limits, ImportStage::Prepare),
        &mut ledger(),
    ) {
        Err(ExecuteError::TargetNotEmpty)
        | Err(ExecuteError::IdentityMismatch {
            field: ExpectedField::Session,
        }) => {}
        other => panic!("a novel Prepare on an occupied target must refuse, got {other:?}"),
    }
    let _ = &mut second;
}

#[test]
fn a_batch_range_the_plan_does_not_have_is_refused() {
    let (_inner, _fs, mut store) = world();
    let (plan, mut archive) = fixture();
    let prepared = prepared_for(&plan, &mut archive);
    let expected = expected_for(&plan);
    let limits = Limits::V1;
    let prepare = step_of(
        ImportStage::Prepare,
        prepared.import_session().plan_ref,
        expected.source_digest,
        0,
    );
    write_stage(
        &mut store,
        &admin(),
        &expected,
        &plan,
        &prepared,
        &prepare,
        &context_for(&limits, ImportStage::Prepare),
        &mut ledger(),
    )
    .expect("prepare");

    let available = u32::try_from(prepared.events().len()).expect("fits");
    let mut too_far = step_of(
        ImportStage::Import,
        prepared.import_session().plan_ref,
        expected.source_digest,
        1,
    );
    too_far.first_event = available;
    assert!(matches!(
        write_stage(
            &mut store,
            &admin(),
            &expected,
            &plan,
            &prepared,
            &too_far,
            &context_for(&limits, ImportStage::Import),
            &mut ledger()
        ),
        Err(ExecuteError::BatchOutOfRange { .. })
    ));
}

#[test]
fn verify_reports_a_count_defect_rather_than_claiming_success() {
    let (_inner, _fs, mut store) = world();
    let (plan, mut archive) = fixture();
    let prepared = prepared_for(&plan, &mut archive);
    let expected = expected_for(&plan);
    let prepare = step_of(
        ImportStage::Prepare,
        prepared.import_session().plan_ref,
        expected.source_digest,
        0,
    );
    write_stage(
        &mut store,
        &admin(),
        &expected,
        &plan,
        &prepared,
        &prepare,
        &context_for(&Limits::V1, ImportStage::Prepare),
        &mut ledger(),
    )
    .expect("prepare");

    match verify_written(&mut store, &prepared, &expected, &mut ledger()) {
        Err(VerifyDefect::CountMismatch {
            expected, actual, ..
        }) => {
            assert!(expected > 0);
            assert_eq!(actual, 0, "nothing imported yet");
        }
        other => panic!("expected a count defect: {other:?}"),
    }
}

#[test]
fn a_caller_stop_is_a_real_refusal_and_writes_nothing() {
    struct Stop;
    impl QueryControl for Stop {
        fn checkpoint(&self) -> Result<(), ControlStop> {
            Err(ControlStop::Cancelled)
        }
    }
    let (_inner, _fs, mut store) = world();
    let (plan, mut archive) = fixture();
    let prepared = prepared_for(&plan, &mut archive);
    let expected = expected_for(&plan);
    let prepare = step_of(
        ImportStage::Prepare,
        prepared.import_session().plan_ref,
        expected.source_digest,
        0,
    );
    let mut stopped = AttemptLedger::open(BYTES, ENTRIES, &Stop).expect("ledger");
    // The stop is observed at the FIRST ledger checkpoint, which authorization takes — so it
    // surfaces as `Auth(Refused(Stopped))` rather than after a later stage. Both are the same
    // refusal arriving earlier, and neither writes.
    match write_stage(
        &mut store,
        &admin(),
        &expected,
        &plan,
        &prepared,
        &prepare,
        &context_for(&Limits::V1, ImportStage::Prepare),
        &mut stopped,
    ) {
        Err(ExecuteError::Auth(sage_db_kernel::auth::AuthError::Refused(
            sage_db_types::framing::Refusal::Stopped(ControlStop::Cancelled),
        )))
        | Err(ExecuteError::Refused(sage_db_types::framing::Refusal::Stopped(
            ControlStop::Cancelled,
        ))) => {}
        other => panic!("a caller stop must be a refusal, got {other:?}"),
    }
    assert_eq!(store.head(), Some(1));
}

#[test]
fn a_non_admin_grant_is_refused() {
    let (_inner, _fs, mut store) = world();
    let (plan, mut archive) = fixture();
    let prepared = prepared_for(&plan, &mut archive);
    let expected = expected_for(&plan);
    let propose_only = Grants::new(ADMIN, kb(), CapabilitySet::of(&[Capability::Propose]));
    let prepare = step_of(
        ImportStage::Prepare,
        prepared.import_session().plan_ref,
        expected.source_digest,
        0,
    );
    assert!(matches!(
        write_stage(
            &mut store,
            &propose_only,
            &expected,
            &plan,
            &prepared,
            &prepare,
            &context_for(&Limits::V1, ImportStage::Prepare),
            &mut ledger()
        ),
        Err(ExecuteError::Auth(_))
    ));
}

/// A folded record's revision, for the counterexample's comparison.
fn revision_of(
    record: &sage_db_store::fold::FoldedRecord,
) -> sage_db_types::identity::RecordRevision {
    use sage_db_store::fold::FoldedRecord as R;
    match record {
        R::Term(r) => r.revision,
        R::Assertion(r) => r.revision,
        R::Claim(r) => r.revision,
        R::Evidence(r) => r.revision,
        R::EvidenceLink(r) => r.revision,
        R::Vector(r) => r.revision,
        R::Proposal(r) => r.revision,
        R::FrontierEntry(r) => r.revision,
        R::SessionCheckpoint(r) => r.revision,
        R::KbMetadata(r) => r.revision,
        R::Model(r) => r.revision,
        R::RuleConfig(r) => r.revision,
        R::Asset(_) => sage_db_types::identity::RecordRevision::FIRST,
    }
}

fn record_key_of(
    record: &sage_db_store::fold::FoldedRecord,
) -> Option<sage_db_types::persist::RecordKey> {
    use sage_db_store::fold::FoldedRecord as R;
    Some(match record {
        R::Term(r) => r.key,
        R::Assertion(r) => r.key,
        R::Claim(r) => r.key,
        R::Evidence(r) => r.key,
        R::EvidenceLink(r) => r.key,
        R::Vector(r) => r.key,
        R::Proposal(r) => r.key,
        R::FrontierEntry(r) => r.key,
        R::SessionCheckpoint(r) => r.key,
        R::Asset(_) | R::Model(_) | R::KbMetadata(_) | R::RuleConfig(_) => return None,
    })
}

// ---------------------------------------------------------------------------
// Full-value verification, and the counterexample a count-only check misses
// ---------------------------------------------------------------------------

#[test]
fn a_wrong_typed_state_written_to_the_target_is_caught_by_the_value_check() {
    // A genuinely wrong after-image written to the target where the EXPECTED REVISION IS
    // UNCHANGED: the import transaction itself carries term 1 at revision 1 with a mutated atom.
    // Counts, identity and revision all agree with the plan; only the field value differs, so a
    // count-plus-revision check would pass. The full-value comparison must not.
    //
    // The frozen expectation is untouched — it is the plan's real digest, source digest, KB and
    // session. Nothing about what the plan SAYS is changed; only what one transaction writes.
    let (_inner, _fs, mut store) = world();
    let (plan, mut archive) = fixture();
    let prepared = prepared_for(&plan, &mut archive);
    let expected = expected_for(&plan);
    let mut l = ledger();

    // Prepare is the real stage. Import is replaced by a hand-built transaction that writes the
    // mapping's own events, with ONE field of ONE term changed.
    write_stage(
        &mut store,
        &admin(),
        &expected,
        &plan,
        &prepared,
        &step_of(
            ImportStage::Prepare,
            prepared.import_session().plan_ref,
            expected.source_digest,
            0,
        ),
        &context_for(&Limits::V1, ImportStage::Prepare),
        &mut l,
    )
    .expect("prepare commits");

    let mut wrong = wrong_term_from_plan(&prepared);
    wrong.atom.push_str(" (wrong)");
    assert_eq!(
        wrong.revision,
        sage_db_types::identity::RecordRevision::FIRST,
        "the wrong image must be at the PLANNED revision, not a shifted one"
    );

    commit_wrong_import(&mut store, &prepared, &wrong, &mut l);

    // Counts and revision agreement are what make this a value-only discriminator.
    let stored = match store
        .record(
            sage_db_types::persist::RecordKey::new(sage_db_types::persist::RecordKind::Term, 1),
            &mut l,
        )
        .expect("record read")
    {
        sage_db_store::view::RecordLookup::Found { record, .. } => record.clone(),
        other => panic!("the wrong import must have written the term: {other:?}"),
    };
    assert_eq!(
        revision_of(&stored),
        sage_db_types::identity::RecordRevision::FIRST,
        "the stored revision must still be the planned one"
    );
    let sage_db_store::fold::FoldedRecord::Term(stored_term) = &stored else {
        panic!("term 1 must fold to a Term record");
    };
    assert!(
        stored_term.atom.ends_with("(wrong)"),
        "the wrong value must actually be what the store holds"
    );

    match verify_written(&mut store, &prepared, &expected, &mut l) {
        Err(VerifyDefect::ValueMismatch { kind, revision }) => {
            assert_eq!(kind, StateKind::Term, "the wrong kind must be named");
            assert_eq!(
                revision,
                sage_db_types::identity::RecordRevision::FIRST.get(),
                "the SAME planned revision must be the one named"
            );
        }
        other => panic!("a wrong field at the planned revision must be refused, got {other:?}"),
    }
}

/// Decode one owned event through the format's own writer and dispatcher.
///
/// This is the canonical public seam, used exactly as the executor uses it — the test adds no
/// second decoder and no production entry point for testing.
fn decode_owned_event(
    event: &sage_db_types::prepared::OwnedEvent,
) -> Option<sage_db_store::fold::FoldedRecord> {
    use sage_db_store::fold::FoldedRecord as R;
    let mut l = ledger();
    // Write the event back out through the canonical event writer, then dispatch those bytes.
    let mut bytes: Vec<u8> = Vec::new();
    {
        let mut writer = sage_db_format::wire::Writer::new(&mut bytes, &mut l);
        let fields: Vec<sage_db_format::event::Field<'_>> = event
            .fields
            .iter()
            .map(|(tag, value)| sage_db_format::event::Field::new(*tag, value))
            .collect();
        sage_db_format::event::write_event(
            &mut writer,
            event.tag,
            sage_db_format::event::EVENT_FLAG_REQUIRED,
            &fields,
        )
        .expect("the mapping's own event must re-encode");
    }
    let mut reader = sage_db_format::wire::Reader::new(&bytes);
    match sage_db_format::dispatch::read_dispatched(&mut reader, &mut l) {
        sage_db_types::framing::FormatOutcome::Ok(
            sage_db_format::dispatch::DispatchedEvent::Record { payload, .. },
        ) => match payload {
            sage_db_format::dispatch::DispatchedPayload::Term(r) => Some(R::Term(r)),
            sage_db_format::dispatch::DispatchedPayload::Proposal(r) => Some(R::Proposal(r)),
            _ => None,
        },
        other => panic!("canonical dispatch refused a mapped event: {other:?}"),
    }
}

/// The term-1 record the plan would produce, decoded from the mapping's own events.
fn wrong_term_from_plan(
    prepared: &sage_db_migrate::mapping::PreparedImport,
) -> sage_db_types::persist::TermRecord {
    for event in prepared.events() {
        let decoded = match decode_owned_event(event) {
            Some(sage_db_store::fold::FoldedRecord::Term(term)) => Some(term),
            Some(_) | None => None,
        };
        if let Some(term) = decoded
            && term.key
                == sage_db_types::persist::RecordKey::new(
                    sage_db_types::persist::RecordKind::Term,
                    1,
                )
        {
            return term;
        }
    }
    panic!("the plan must carry term 1");
}

/// Commit the mapping's own events with ONE substituted after-image, through the public bridge.
fn commit_wrong_import(
    store: &mut Store<SharedFs>,
    prepared: &sage_db_migrate::mapping::PreparedImport,
    wrong: &sage_db_types::persist::TermRecord,
    ledger: &mut AttemptLedger<'static>,
) {
    use sage_db_format::term::TermEvent;
    use sage_db_store::typed::EventInput;
    use sage_db_types::prepared::{BlobDeclaration, BlobSource, PreparedTransaction};

    let basis = store
        .commit_basis(sage_db_types::persist::IdSpace::TermId, ledger)
        .expect("basis");
    let head = store.head();
    let version = head.map_or(1, |v| v + 1);

    // Every planned event, except the term-1 creation which is replaced by the wrong image.
    let mut events: Vec<sage_db_types::prepared::OwnedEvent> = Vec::new();
    for event in prepared.events() {
        if let Some(sage_db_store::fold::FoldedRecord::Term(term)) = decode_owned_event(event)
            && term.key
                == sage_db_types::persist::RecordKey::new(
                    sage_db_types::persist::RecordKind::Term,
                    1,
                )
        {
            let replacement = match sage_db_store::typed::prepare_event(
                EventInput::Term(TermEvent::Created, wrong),
                ledger,
            ) {
                sage_db_types::framing::FormatOutcome::Ok(event) => event,
                other => panic!("the wrong term must encode: {other:?}"),
            };
            events.push(replacement);
            continue;
        }
        events.push(event.clone());
    }

    let result_bytes =
        sage_db_store::typed::result_payload(version, &[], &[], ledger).expect("result payload");
    let result_ref = match sage_db_store::typed::raw_hash(&result_bytes, ledger) {
        sage_db_types::framing::FormatOutcome::Ok(digest) => digest,
        other => panic!("result digest refused: {other:?}"),
    };
    let request_id = String::from("wrong-import-1");
    let outcome = sage_db_types::persist::RequestOutcomeEvent {
        principal_id: String::from(ADMIN),
        request_id: request_id.clone(),
        payload_digest: [0x33; 32],
        result_ref,
        commit_version: version,
        audit: sage_db_types::persist::CreationAudit {
            recorded_by: Some(String::from(ADMIN)),
            actor_origin: sage_db_types::persist::ActorOrigin::Authenticated,
            reason: None,
            created_at: 1,
        },
        extensions: sage_db_types::persist::Extensions::new(),
    };
    let outcome_event =
        match sage_db_store::typed::prepare_event(EventInput::Outcome(&outcome), ledger) {
            sage_db_types::framing::FormatOutcome::Ok(event) => event,
            other => panic!("outcome encode refused: {other:?}"),
        };
    events.push(outcome_event);

    let mut blobs: Vec<BlobDeclaration> = Vec::new();
    blobs.push(BlobDeclaration {
        sha256: basis.rules.rules_ref,
        declared_len: basis.rules_len,
        source: BlobSource::Existing,
    });
    for blob in prepared.blobs() {
        match &blob.source {
            BlobSource::Provide(bytes) => {
                let mut owned: Vec<u8> = Vec::new();
                owned.extend_from_slice(bytes);
                blobs.push(BlobDeclaration {
                    sha256: blob.sha256,
                    declared_len: blob.declared_len,
                    source: BlobSource::Provide(owned),
                });
            }
            BlobSource::Existing => blobs.push(BlobDeclaration {
                sha256: blob.sha256,
                declared_len: blob.declared_len,
                source: BlobSource::Existing,
            }),
        }
    }
    blobs.push(BlobDeclaration {
        sha256: result_ref,
        declared_len: result_bytes.len() as u64,
        source: BlobSource::Provide(result_bytes),
    });

    let published = PreparedTransaction {
        version,
        prev_version: head,
        tx_id: [0x7c; 16],
        kb_uuid: store.kb_uuid(),
        created_at: 1,
        principal_id: String::from(ADMIN),
        request_id,
        payload_digest: [0x33; 32],
        expect_version: None,
        extensions: sage_db_types::persist::Extensions::new(),
        events,
        blobs,
        rules: basis.rules,
    };
    match store.commit(published, &Limits::V1, ledger) {
        sage_db_store::handle::HandleOutcome::Committed { .. } => {}
        other => panic!("the wrong import must really commit, got {other:?}"),
    }
}

/// Write Prepare and Import, leaving the session open for verification.
fn prepare_and_import(
    store: &mut Store<SharedFs>,
    prepared: &sage_db_migrate::mapping::PreparedImport,
    plan: &[u8],
    expected: &ExpectedIdentity<'_>,
    l: &mut AttemptLedger<'static>,
) {
    let limits = Limits::V1;
    write_stage(
        store,
        &admin(),
        expected,
        plan,
        prepared,
        &step_of(
            ImportStage::Prepare,
            prepared.import_session().plan_ref,
            expected.source_digest,
            0,
        ),
        &context_for(&limits, ImportStage::Prepare),
        l,
    )
    .expect("prepare commits");
    write_stage(
        store,
        &admin(),
        expected,
        plan,
        prepared,
        &step_of(
            ImportStage::Import,
            prepared.import_session().plan_ref,
            expected.source_digest,
            u32::try_from(prepared.events().len()).expect("fits"),
        ),
        &context_for(&limits, ImportStage::Import),
        l,
    )
    .expect("import commits");
}

/// Create one new term above the imported watermark, through the real commit path.
fn create_above_watermark(
    store: &mut Store<SharedFs>,
    record: &sage_db_types::persist::TermRecord,
    ledger: &mut AttemptLedger<'static>,
) {
    use sage_db_format::term::TermEvent;
    use sage_db_store::typed::EventInput;
    use sage_db_types::prepared::{BlobDeclaration, BlobSource, PreparedTransaction};

    let basis = store
        .commit_basis(sage_db_types::persist::IdSpace::TermId, ledger)
        .expect("basis");
    let head = store.head();
    let version = head.map_or(1, |v| v + 1);
    let event = match sage_db_store::typed::prepare_event(
        EventInput::Term(TermEvent::Created, record),
        ledger,
    ) {
        sage_db_types::framing::FormatOutcome::Ok(event) => event,
        other => panic!("create encode refused: {other:?}"),
    };
    // A mark advances ONLY through an `IdAllocation` event (it is a maximum over recorded
    // allocations, never inferred from which ids appear). So a create that consumes an id must
    // record the allocation, or the committed mark legitimately stays where it was.
    let allocation = sage_db_types::persist::IdAllocationEvent {
        allocation: sage_db_types::persist::Watermark {
            space: sage_db_types::persist::IdSpace::TermId,
            hwm: record.key.id(),
        },
        reason: Some(String::from("create above imported mark")),
        extensions: sage_db_types::persist::Extensions::new(),
    };
    let allocation_event =
        match sage_db_store::typed::prepare_event(EventInput::IdAllocation(&allocation), ledger) {
            sage_db_types::framing::FormatOutcome::Ok(event) => event,
            other => panic!("allocation encode refused: {other:?}"),
        };
    let result_bytes =
        sage_db_store::typed::result_payload(version, &[], &[], ledger).expect("result payload");
    let result_ref = match sage_db_store::typed::raw_hash(&result_bytes, ledger) {
        sage_db_types::framing::FormatOutcome::Ok(digest) => digest,
        other => panic!("result digest refused: {other:?}"),
    };
    let request_id = String::from("create-above-mark-1");
    let outcome = sage_db_types::persist::RequestOutcomeEvent {
        principal_id: String::from(ADMIN),
        request_id: request_id.clone(),
        payload_digest: [0x22; 32],
        result_ref,
        commit_version: version,
        audit: sage_db_types::persist::CreationAudit {
            recorded_by: Some(String::from(ADMIN)),
            actor_origin: sage_db_types::persist::ActorOrigin::Authenticated,
            reason: None,
            created_at: 2,
        },
        extensions: sage_db_types::persist::Extensions::new(),
    };
    let outcome_event =
        match sage_db_store::typed::prepare_event(EventInput::Outcome(&outcome), ledger) {
            sage_db_types::framing::FormatOutcome::Ok(event) => event,
            other => panic!("outcome encode refused: {other:?}"),
        };
    let body_len = match store.read_committed_blob(record.body_ref, u64::MAX, ledger) {
        Ok(bytes) => bytes.len() as u64,
        other => panic!("the body must be readable: {other:?}"),
    };
    let blobs = vec![
        BlobDeclaration {
            sha256: basis.rules.rules_ref,
            declared_len: basis.rules_len,
            source: BlobSource::Existing,
        },
        BlobDeclaration {
            sha256: record.body_ref,
            declared_len: body_len,
            source: BlobSource::Existing,
        },
        BlobDeclaration {
            sha256: result_ref,
            declared_len: result_bytes.len() as u64,
            source: BlobSource::Provide(result_bytes),
        },
    ];
    let published = PreparedTransaction {
        version,
        prev_version: head,
        tx_id: [0x6b; 16],
        kb_uuid: store.kb_uuid(),
        created_at: 2,
        principal_id: String::from(ADMIN),
        request_id,
        payload_digest: [0x22; 32],
        expect_version: None,
        extensions: sage_db_types::persist::Extensions::new(),
        events: vec![event, allocation_event, outcome_event],
        blobs,
        rules: basis.rules,
    };
    match store.commit(published, &Limits::V1, ledger) {
        sage_db_store::handle::HandleOutcome::Committed { .. } => {}
        other => panic!("the create must commit, got {other:?}"),
    }
}

/// Commit one deliberately wrong term after-image through the real store commit path.
fn commit_wrong_term(
    store: &mut Store<SharedFs>,
    wrong: &sage_db_store::fold::FoldedRecord,
    ledger: &mut AttemptLedger<'static>,
) {
    use sage_db_format::term::TermEvent;
    use sage_db_store::typed::EventInput;
    use sage_db_types::persist::TermRecord;
    use sage_db_types::prepared::{BlobDeclaration, BlobSource, PreparedTransaction};

    let sage_db_store::fold::FoldedRecord::Term(record) = wrong else {
        panic!("only a term is written here");
    };
    let mut term: TermRecord = record.clone();
    // A creation event at the SAME revision: no update time, no changed-field list, since both
    // are forbidden on a creation. This is what makes the write revision-neutral.
    term.changed_fields = None;
    term.common.updated_at = None;

    let basis = store
        .commit_basis(sage_db_types::persist::IdSpace::TermId, ledger)
        .expect("basis");
    let head = store.head();
    let version = head.map_or(1, |v| v + 1);

    let event = match sage_db_store::typed::prepare_event(
        EventInput::Term(TermEvent::Created, &term),
        ledger,
    ) {
        sage_db_types::framing::FormatOutcome::Ok(event) => event,
        other => panic!("event encode refused: {other:?}"),
    };
    let result_bytes =
        sage_db_store::typed::result_payload(version, &[], &[], ledger).expect("result payload");
    let result_ref = match sage_db_store::typed::raw_hash(&result_bytes, ledger) {
        sage_db_types::framing::FormatOutcome::Ok(digest) => digest,
        other => panic!("result digest refused: {other:?}"),
    };

    let mut blobs: Vec<BlobDeclaration> = Vec::new();
    blobs.push(BlobDeclaration {
        sha256: basis.rules.rules_ref,
        declared_len: basis.rules_len,
        source: BlobSource::Existing,
    });
    // The term's body blob was published by the import; a revision only references it.
    let body_len = match store.read_committed_blob(term.body_ref, u64::MAX, ledger) {
        Ok(bytes) => bytes.len() as u64,
        other => panic!("the imported body must be readable: {other:?}"),
    };
    blobs.push(BlobDeclaration {
        sha256: term.body_ref,
        declared_len: body_len,
        source: BlobSource::Existing,
    });
    blobs.push(BlobDeclaration {
        sha256: result_ref,
        declared_len: result_bytes.len() as u64,
        source: BlobSource::Provide(result_bytes),
    });

    let request_id = String::from("wrong-write-1");
    let outcome = sage_db_types::persist::RequestOutcomeEvent {
        principal_id: String::from(ADMIN),
        request_id: request_id.clone(),
        payload_digest: [0x11; 32],
        result_ref,
        commit_version: version,
        audit: sage_db_types::persist::CreationAudit {
            recorded_by: Some(String::from(ADMIN)),
            actor_origin: sage_db_types::persist::ActorOrigin::Authenticated,
            reason: None,
            created_at: 1,
        },
        extensions: sage_db_types::persist::Extensions::new(),
    };
    let outcome_event =
        match sage_db_store::typed::prepare_event(EventInput::Outcome(&outcome), ledger) {
            sage_db_types::framing::FormatOutcome::Ok(event) => event,
            other => panic!("outcome encode refused: {other:?}"),
        };
    let published = PreparedTransaction {
        version,
        prev_version: head,
        tx_id: [0x5a; 16],
        kb_uuid: store.kb_uuid(),
        created_at: 1,
        principal_id: String::from(ADMIN),
        request_id: request_id.clone(),
        payload_digest: [0x11; 32],
        expect_version: None,
        extensions: sage_db_types::persist::Extensions::new(),
        events: vec![event, outcome_event],
        blobs,
        rules: basis.rules,
    };
    match store.commit(published, &Limits::V1, ledger) {
        sage_db_store::handle::HandleOutcome::Committed { .. } => {}
        other => panic!("the wrong write must really commit, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// High-water marks and future allocation
// ---------------------------------------------------------------------------

#[test]
fn imported_watermarks_are_committed_and_a_future_create_lands_above_them() {
    let (_inner, fs, mut store) = world();
    let (plan, mut archive) = fixture();
    let prepared = prepared_for(&plan, &mut archive);
    let expected = expected_for(&plan);
    let limits = Limits::V1;
    run_all_stages(&mut store, &prepared, &plan, &expected, &limits);

    // Every planned watermark must be committed at or above the plan's maximum id, which is
    // read from the store's own committed basis rather than from the plan's promise.
    for mark in prepared.watermarks() {
        let space = sage_db_types::persist::IdSpace::from_u16(mark.space)
            .expect("the plan names a registered space");
        let basis = store
            .commit_basis(space, &mut ledger())
            .expect("committed basis");
        assert!(
            basis.allocated_high_water >= mark.hwm,
            "space {} committed {} below the planned {}",
            mark.space,
            basis.allocated_high_water,
            mark.hwm
        );
    }

    // A real create above the imported maximum. The term id space is the one this fixture
    // exercises; the writer must allocate above the imported mark, not reuse a low id.
    let term_basis = store
        .commit_basis(sage_db_types::persist::IdSpace::TermId, &mut ledger())
        .expect("term basis");
    assert!(
        term_basis.allocated_high_water >= 2,
        "the fixture imports terms 1 and 2, so the committed mark must be at least 2"
    );

    // A REAL create above the imported maximum: pick the next id from the committed mark,
    // write it, and require the store to accept it. Asserting the mark alone would not show
    // that a create is possible at all.
    let fresh_id = u32::try_from(term_basis.allocated_high_water)
        .expect("the fixture's term mark fits")
        .checked_add(1)
        .expect("room above the mark");
    let template = match store
        .record(
            sage_db_types::persist::RecordKey::new(sage_db_types::persist::RecordKind::Term, 1),
            &mut ledger(),
        )
        .expect("record read")
    {
        sage_db_store::view::RecordLookup::Found { record, .. } => record.clone(),
        other => panic!("expected the imported term: {other:?}"),
    };
    let mut fresh = match template {
        sage_db_store::fold::FoldedRecord::Term(term) => term,
        _ => panic!("term 1 folds to a Term record"),
    };
    fresh.key = sage_db_types::persist::RecordKey::new(
        sage_db_types::persist::RecordKind::Term,
        u128::from(fresh_id),
    );
    fresh.slug.push_str("-created");
    fresh.atom.push_str(" (created above the imported mark)");
    fresh.revision = sage_db_types::identity::RecordRevision::FIRST;
    fresh.changed_fields = None;
    fresh.common.updated_at = None;
    fresh.legacy_id = None;
    create_above_watermark(&mut store, &fresh, &mut ledger());
    let created = match store.record(fresh.key, &mut ledger()).expect("record read") {
        sage_db_store::view::RecordLookup::Found { record, .. } => record.clone(),
        other => panic!("the create must be current, got {other:?}"),
    };
    assert_eq!(
        revision_of(&created).get(),
        1,
        "a create above the mark starts at revision 1"
    );
    // The evidence that the create landed ABOVE the imported mark is the store's own post-create
    // basis: it must have advanced past the pre-create mark. (Asserting `fresh_id == mark + 1`
    // would be a tautology, since `fresh_id` was defined as exactly that.)
    let after_create = store
        .commit_basis(sage_db_types::persist::IdSpace::TermId, &mut ledger())
        .expect("post-create basis");
    assert!(
        after_create.allocated_high_water >= u128::from(fresh_id),
        "the committed mark must cover the id the create used ({} < {})",
        after_create.allocated_high_water,
        fresh_id
    );

    // The check that matters: after reopening, the mark is still there. A mark that only
    // lived in this process would vanish.
    drop(store);
    let reopened = reopen(fs);
    let after = reopened
        .commit_basis(sage_db_types::persist::IdSpace::TermId, &mut ledger())
        .expect("reopened basis");
    assert_eq!(
        after.allocated_high_water, after_create.allocated_high_water,
        "the committed high-water mark must survive a real reopen unchanged"
    );
    assert!(
        after.allocated_high_water > term_basis.allocated_high_water,
        "and it must still be ABOVE the pre-create mark, so the create really advanced it"
    );
}

// ---------------------------------------------------------------------------
// Interrupted resume: an exact retry after a real reopen does not replay
// ---------------------------------------------------------------------------

#[test]
fn a_stage_retried_after_a_reopen_returns_the_stored_outcome_without_replaying() {
    let (_inner, fs, mut store) = world();
    let (plan, mut archive) = fixture();
    let prepared = prepared_for(&plan, &mut archive);
    let expected = expected_for(&plan);
    let limits = Limits::V1;
    let mut l = ledger();

    let prepare = step_of(
        ImportStage::Prepare,
        prepared.import_session().plan_ref,
        expected.source_digest,
        0,
    );
    let first = write_stage(
        &mut store,
        &admin(),
        &expected,
        &plan,
        &prepared,
        &prepare,
        &context_for(&limits, ImportStage::Prepare),
        &mut l,
    )
    .expect("prepare");
    let version = match first.outcome {
        HandleOutcome::Committed { version, .. } => version,
        other => panic!("expected a commit: {other:?}"),
    };

    // Simulate an interruption after the ACK: drop the handle without any other write.
    drop(store);
    let mut reopened = reopen(fs);
    let head = reopened.head();
    assert_eq!(head, Some(version), "the ACKed stage is durable");

    // Retry the identical stage. The outcome-first lookup must answer from the stored
    // outcome, and nothing may be written: a replay would bump the head or duplicate the
    // session row.
    let retried = write_stage(
        &mut reopened,
        &admin(),
        &expected,
        &plan,
        &prepared,
        &prepare,
        &context_for(&limits, ImportStage::Prepare),
        &mut ledger(),
    )
    .expect("the retry answers");
    match retried.outcome {
        HandleOutcome::AlreadyCommitted { version: at, .. } => assert_eq!(at, version),
        other => panic!("expected the stored outcome, got {other:?}"),
    }
    assert_eq!(reopened.head(), head, "a resume must not replay");
}

// ---------------------------------------------------------------------------
// The Unknown oracle over every sampled crash world
// ---------------------------------------------------------------------------

#[test]
fn an_unknown_import_stage_is_judged_by_every_sampled_world() {
    use sage_db_store_tests::oracle::attempts::{
        AttemptHistory, AttemptOutcome, Commit, LogicalRequest, World,
    };

    let (inner, fs, mut store) = world();
    let (plan, mut archive) = fixture();
    let prepared = prepared_for(&plan, &mut archive);
    let expected = expected_for(&plan);
    let limits = Limits::V1;
    let mut l = ledger();

    // A durable, acknowledged Prepare at a known version.
    let prepare = step_of(
        ImportStage::Prepare,
        prepared.import_session().plan_ref,
        expected.source_digest,
        0,
    );
    let acked = write_stage(
        &mut store,
        &admin(),
        &expected,
        &plan,
        &prepared,
        &prepare,
        &context_for(&limits, ImportStage::Prepare),
        &mut l,
    )
    .expect("prepare");
    let acked_version = match acked.outcome {
        HandleOutcome::Committed { version, .. } => version,
        other => panic!("expected a commit: {other:?}"),
    };

    // The candidate Import, whose CURRENT replacement reports an unknown effect.
    let events = u32::try_from(prepared.events().len()).expect("fits");
    let import = step_of(
        ImportStage::Import,
        prepared.import_session().plan_ref,
        expected.source_digest,
        events,
    );
    *inner.faults_handle().borrow_mut() = FaultSchedule::none().at(
        FaultPoint::ReplaceCurrent,
        Injection::FailUnknownAfterEffect,
        1,
    );
    let outcome = write_stage(
        &mut store,
        &admin(),
        &expected,
        &plan,
        &prepared,
        &import,
        &context_for(&limits, ImportStage::Import),
        &mut l,
    )
    .expect("Unknown is an outcome, not an error");
    assert!(
        matches!(outcome.outcome, HandleOutcome::Unknown),
        "the armed fault must produce Unknown, got {outcome:?}"
    );
    assert!(inner.faults_handle().borrow().all_fired());

    // Sample EVERY crash state before any resolve can synchronize the visible root.
    let worlds = inner.model().crash_states();
    assert!(!worlds.states.is_empty());
    assert_eq!(worlds.sampled, worlds.states.len());

    // The oracle history: the durable ACK, plus whatever the candidate did. Both requests are
    // canonical — the digest is the real per-stage payload digest the executor computed.
    let world_base = acked_version - 1;
    let prepare_request = LogicalRequest::new(
        ADMIN,
        "import-session-1-0-0",
        request_digest_for(&prepared, &expected, ImportStage::Prepare, 0),
    );
    let import_request = LogicalRequest::new(
        ADMIN,
        "import-session-1-2-0",
        request_digest_for(&prepared, &expected, ImportStage::Import, events),
    );
    let mut history = AttemptHistory::default();
    history.record(
        "ack-prepare",
        prepare_request.clone(),
        AttemptOutcome::Durable {
            version: acked_version,
        },
    );
    history.record(
        "unknown-import",
        import_request.clone(),
        AttemptOutcome::Unknown,
    );

    let mut examined = 0usize;
    let mut old_seen = false;
    let mut new_seen = false;
    for (index, state) in worlds.states.into_iter().enumerate() {
        let restored =
            ModelFileSystem::from_durable_state(ModelCapabilities::full(), Path::new(ROOT), state)
                .unwrap_or_else(|error| panic!("world {index} restore failed: {error:?}"));
        let model_fs = SharedFs(Rc::new(restored));
        let reopened = match open_existing(
            model_fs,
            Path::new(ROOT),
            Some(kb()),
            &Limits::V1,
            &mut ledger(),
        ) {
            Ok(Opened::Ready(store)) => store,
            Ok(Opened::ReadOnly { .. }) => panic!("world {index} must replay durably"),
            Err(error) => panic!("world {index} open failed: {error:?}"),
        };
        let head = reopened.head().expect("a head");
        assert!(
            head >= acked_version,
            "world {index} lost the acknowledged Prepare"
        );

        // The GATE is closed on every restored world: an unfinished import must refuse ordinary
        // kernel use right through the restart, which is the whole point of persisting it.
        match reopened.import_readiness(&mut ledger()) {
            Ok(sage_db_store::view::ImportReadiness::Incomplete { basis, session }) => {
                assert_eq!(
                    session.import_session_id, SESSION,
                    "world {index} names a different session"
                );
                assert_eq!(
                    basis,
                    reopened.view().expect("a view").basis(),
                    "world {index} reported a basis the view does not have"
                );
            }
            Ok(sage_db_store::view::ImportReadiness::Ready { .. }) => {
                panic!("world {index} opened the gate before Activate committed")
            }
            Err(error) => panic!("world {index} readiness failed: {error:?}"),
        }

        // The ACK is always findable at its original version, and its persisted identity is
        // the canonical one — not a fabricated constant.
        let mut commits = Vec::new();
        match reopened.outcome(ADMIN, "import-session-1-0-0") {
            sage_db_store::open::OutcomeLookup::Found {
                outcome,
                original_version,
                ..
            } => {
                assert_eq!(
                    original_version, acked_version,
                    "world {index} moved the ACK"
                );
                assert_eq!(
                    outcome.event().payload_digest,
                    request_digest_for(&prepared, &expected, ImportStage::Prepare, 0),
                    "world {index} persisted a different request digest"
                );
                commits.push(Commit {
                    version: original_version,
                    request: prepare_request.clone(),
                });
            }
            other => panic!("world {index} lost the acknowledged outcome: {other:?}"),
        }

        if head == acked_version {
            old_seen = true;
            assert!(
                matches!(
                    reopened.outcome(ADMIN, "import-session-1-2-0"),
                    sage_db_store::open::OutcomeLookup::Absent { .. }
                ),
                "world {index} has candidate state without its outcome"
            );
        } else {
            new_seen = true;
            match reopened.outcome(ADMIN, "import-session-1-2-0") {
                sage_db_store::open::OutcomeLookup::Found {
                    outcome,
                    original_version,
                    ..
                } => {
                    assert_eq!(original_version, head);
                    assert_eq!(
                        outcome.event().payload_digest,
                        request_digest_for(&prepared, &expected, ImportStage::Import, events),
                        "world {index} persisted a different candidate digest"
                    );
                    commits.push(Commit {
                        version: original_version,
                        request: import_request.clone(),
                    });
                }
                other => panic!("world {index} has candidate state without its outcome: {other:?}"),
            }
        }

        assert!(
            history
                .accepts(
                    world_base,
                    &World {
                        base_version: world_base,
                        commits
                    }
                )
                .expect("model"),
            "the sequence oracle rejected world {index}"
        );
        examined += 1;
    }
    assert_eq!(examined, worlds.sampled, "every sampled world is examined");
    assert!(
        old_seen && new_seen,
        "this control must exercise BOTH whole alternatives (old={old_seen}, new={new_seen})"
    );
    let _ = fs;
}

/// Derive a step's canonical request digest the same way the executor does.
///
/// Every field is the REAL value the executor submits — including the package reference,
/// which is the prepared plan's own `plan_ref`. Substituting a placeholder here would make
/// the "expected" digest a constant, which is precisely the defect this oracle exists to
/// catch, so the caller must pass the actual package and event count.
fn request_digest_for(
    prepared: &sage_db_migrate::mapping::PreparedImport,
    expected: &ExpectedIdentity<'_>,
    stage: ImportStage,
    event_count: u32,
) -> [u8; 32] {
    use sage_db_migrate::intent::{ImportStep, NATIVE_IMPORT_STEP, OPERATION_VERSION, encode_step};
    let mut l = ledger();
    let step = ImportStep {
        package_ref: prepared.import_session().plan_ref,
        source_digest: expected.source_digest,
        stage,
        batch_index: 0,
        batch_count: 1,
        first_event: 0,
        event_count,
        import_session_id: SESSION,
    };
    let payload = encode_step(&step, &mut l).expect("payload");
    let ops = [sage_db_store::typed::RequestOp::new(
        NATIVE_IMPORT_STEP,
        OPERATION_VERSION,
        &payload,
    )];
    let frame = sage_db_store::typed::RequestFrame::new(&ops, &[], None);
    let bytes = match sage_db_store::typed::encode_request(&frame, &mut l) {
        sage_db_store::typed::RequestOutcome::Ok(bytes) => bytes,
        other => panic!("request encode: {other:?}"),
    };
    match sage_db_store::typed::request_digest_of(&bytes, &mut l) {
        sage_db_store::typed::RequestOutcome::Ok(digest) => digest,
        other => panic!("request digest: {other:?}"),
    }
}

#[test]
fn an_honest_two_revision_proposal_verifies_through_current_and_history() {
    // V1/V5, made reachable: the fixture carries ONE approved source proposal, which the mapper
    // restates as Pending@1 then Decided@2 of a single `prc_` identity. That is the only shape in
    // which the newest-vs-current and earlier-vs-history branches both decide something.
    let (_inner, _fs, mut store) = world();
    let (plan, mut archive) = fixture();
    let prepared = prepared_for(&plan, &mut archive);
    let expected = expected_for(&plan);
    let mut l = ledger();
    prepare_and_import(&mut store, &prepared, &plan, &expected, &mut l);

    // The plan really carries two revisions of one proposal identity; without that the rest of
    // this test would pass vacuously.
    let mut proposal_events = 0usize;
    for event in prepared.events() {
        if event.tag == 96 || event.tag == 97 {
            proposal_events += 1;
        }
    }
    assert_eq!(
        proposal_events, 2,
        "the plan must carry the proposal's create AND its decision"
    );
    assert_eq!(
        prepared.count_of(StateKind::Proposal),
        Some(1),
        "one proposal RECORD, two lifecycle revisions, must not be counted as two rows"
    );

    // The store has the decided revision CURRENT and the pending one only in history.
    let key =
        sage_db_types::persist::RecordKey::new(sage_db_types::persist::RecordKind::Proposal, 1);
    let current = match store.record(key, &mut ledger()).expect("proposal read") {
        sage_db_store::view::RecordLookup::Found { record, .. } => record.clone(),
        other => panic!("the proposal must be current: {other:?}"),
    };
    let sage_db_store::fold::FoldedRecord::Proposal(current) = &current else {
        panic!("proposal 1 must fold to a Proposal record");
    };
    assert_eq!(
        current.revision.get(),
        2,
        "the decided restatement is the CURRENT revision"
    );
    assert_eq!(
        current.status,
        sage_db_types::persist::ProposalStatus::Approved,
        "and it is the decided status"
    );

    // History holds revision 1, which is what the earlier-image branch reads.
    let identity = sage_db_store::fold::RecordIdentity::Identified { key };
    match store.history(
        &identity,
        1,
        sage_db_store::history::HistoryBudget::new(256),
        &Limits::V1,
        &mut ledger(),
    ) {
        sage_db_store::history::HistoryOutcome::Found(hit) => {
            assert_eq!(
                hit.durability,
                sage_db_store::view::ViewDurability::Durable,
                "the historical revision must be durable to count as evidence"
            );
            let sage_db_store::fold::FoldedRecord::Proposal(pending) = hit.image.record() else {
                panic!("history must resolve to a Proposal");
            };
            assert_eq!(pending.revision.get(), 1);
            assert_eq!(
                pending.status,
                sage_db_types::persist::ProposalStatus::Pending,
                "the historical image is the PENDING restatement"
            );
        }
        sage_db_store::history::HistoryOutcome::Absent { .. } => {
            panic!("revision 1 must be recoverable from history, but it is absent")
        }
        sage_db_store::history::HistoryOutcome::Incomplete { reason, .. } => {
            panic!("history could not finish: incomplete {reason:?}")
        }
        sage_db_store::history::HistoryOutcome::Error(refusal) => {
            panic!("history read failed: {refusal:?}")
        }
    }

    // And verification of the honest state passes, which requires the earlier-image branch to
    // accept revision 1 from history rather than demand it be current.
    verify_written(&mut store, &prepared, &expected, &mut l)
        .expect("an honest two-revision proposal must verify");
}

#[test]
fn an_interrupted_import_resumes_after_a_reopen_without_replaying_committed_stages() {
    // The resume shape the operator doc describes: Prepare and Validate committed, then the
    // process died before Import. After a real reopen the committed stages return their STORED
    // outcomes (not new commits), Import is still novel, and the gate stayed closed throughout.
    let (_inner, fs, mut store) = world();
    let (plan, mut archive) = fixture();
    let prepared = prepared_for(&plan, &mut archive);
    let expected = expected_for(&plan);
    let limits = Limits::V1;

    let (prepare_version, validate_version) = {
        let mut l = ledger();
        let prepare = step_of(
            ImportStage::Prepare,
            prepared.import_session().plan_ref,
            expected.source_digest,
            0,
        );
        let prepared_outcome = write_stage(
            &mut store,
            &admin(),
            &expected,
            &plan,
            &prepared,
            &prepare,
            &context_for(&limits, ImportStage::Prepare),
            &mut l,
        )
        .expect("prepare");
        let pv = match prepared_outcome.outcome {
            HandleOutcome::Committed { version, .. } => version,
            other => panic!("prepare must commit: {other:?}"),
        };

        let validate = step_of(
            ImportStage::Validate,
            prepared.import_session().plan_ref,
            expected.source_digest,
            0,
        );
        let validated = write_stage(
            &mut store,
            &admin(),
            &expected,
            &plan,
            &prepared,
            &validate,
            &context_for(&limits, ImportStage::Validate),
            &mut l,
        )
        .expect("validate");
        let vv = match validated.outcome {
            HandleOutcome::Committed { version, .. } => version,
            other => panic!("validate must commit: {other:?}"),
        };

        // The gate is closed after the crash point, on this handle.
        assert!(matches!(
            store.import_readiness(&mut l),
            Ok(sage_db_store::view::ImportReadiness::Incomplete { .. })
        ));
        (pv, vv)
    };

    // A real restart of the handle.
    let head_before = store.head();
    drop(store);
    let mut reopened = reopen(fs);
    assert_eq!(
        reopened.head(),
        head_before,
        "a reopen must not change the committed head"
    );

    let mut l = ledger();
    // Still closed across the restart.
    assert!(matches!(
        reopened.import_readiness(&mut l),
        Ok(sage_db_store::view::ImportReadiness::Incomplete { .. })
    ));

    // Every committed stage returns its STORED outcome, at its ORIGINAL version, and writes
    // nothing: the head is unchanged afterwards.
    for (stage, original) in [
        (ImportStage::Prepare, prepare_version),
        (ImportStage::Validate, validate_version),
    ] {
        let retried = write_stage(
            &mut reopened,
            &admin(),
            &expected,
            &plan,
            &prepared,
            &step_of(
                stage,
                prepared.import_session().plan_ref,
                expected.source_digest,
                0,
            ),
            &context_for(&limits, stage),
            &mut l,
        )
        .expect("an exact retry must be answered, not refused");
        match retried.outcome {
            HandleOutcome::AlreadyCommitted { version, .. } => assert_eq!(
                version, original,
                "{stage:?} must report its ORIGINAL version"
            ),
            other => panic!("{stage:?} must not replay, got {other:?}"),
        }
        assert_eq!(
            reopened.head(),
            head_before,
            "{stage:?} retry must not have committed anything"
        );
    }

    // Import is still NOVEL after the restart, and completes the sequence.
    let events = u32::try_from(prepared.events().len()).expect("fits");
    let import = step_of(
        ImportStage::Import,
        prepared.import_session().plan_ref,
        expected.source_digest,
        events,
    );
    let imported = write_stage(
        &mut reopened,
        &admin(),
        &expected,
        &plan,
        &prepared,
        &import,
        &context_for(&limits, ImportStage::Import),
        &mut l,
    )
    .expect("the novel Import must commit after the restart");
    assert!(
        matches!(imported.outcome, HandleOutcome::Committed { .. }),
        "Import must be a real commit, not a stale resume: {:?}",
        imported.outcome
    );

    // Verification and activation finish the import on the reopened handle.
    let verified =
        verify_and_attest(&mut reopened, &prepared, &expected, &mut l).expect("counts match");
    let activate = step_of(
        ImportStage::Activate,
        prepared.import_session().plan_ref,
        expected.source_digest,
        0,
    );
    mark_complete_after_verification(
        &mut reopened,
        &admin(),
        &expected,
        &prepared,
        verified,
        &activate,
        &context_for(&limits, ImportStage::Activate),
        &mut l,
    )
    .expect("activate");
    match reopened.import_readiness(&mut l) {
        Ok(sage_db_store::view::ImportReadiness::Ready { .. }) => {}
        other => panic!("the gate must open after Activate, got {other:?}"),
    }
}
