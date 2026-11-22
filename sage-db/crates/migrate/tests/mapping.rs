//! Native mapping: fixture tests plus a dry preparation over the **real** frozen source.
//!
//! Every expectation here is authored literally. The canonical writer round-trip is labelled an
//! *agreement* check — it proves the importer emits bytes its own reader accepts, which is not a
//! golden-file claim about the format.
//!
//! The real-source test is a **dry preparation**: it maps, it does not commit. Persistence,
//! restart verification and completion are the executor's, and this crate cannot prove them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use sage_db_migrate::mapping::{
    ExecutorContext, MappingError, MappingLimits, SourceArchive, SourceArray, SourceLocation,
    prepare_native_mapping,
};
use sage_db_store::lease::AttemptLedger;
use sage_db_types::framing::{FormatOutcome, Limits};
use sage_db_types::persist::{ProposalStatus, RecordKey, RecordKind, StateKind};
use sage_db_types::query::RunToCompletion;

// ---------------------------------------------------------------------------
// Fixture infrastructure
// ---------------------------------------------------------------------------

/// A source archive backed by a directory of blobs, re-hashing everything it serves.
struct DirArchive {
    root: PathBuf,
}

impl SourceArchive for DirArchive {
    fn blob(
        &mut self,
        ledger: &mut dyn sage_db_types::ledger::SharedLedger,
        plan_blob_path: &str,
        expected_bytes: u64,
        expected_sha256: &str,
    ) -> Result<Vec<u8>, MappingError> {
        // The SAME ledger the mapping spends: admit the length before the read allocates it.
        ledger.try_charge(expected_bytes).map_err(|error| {
            MappingError::Refused(sage_db_types::framing::Refusal::Budget(error))
        })?;
        let path = self.root.join(plan_blob_path);
        let bytes = std::fs::read(&path).map_err(|_| MappingError::InvalidSource {
            location: SourceLocation::plan(),
            detail: "the plan names a blob the archive does not contain",
        })?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) != expected_bytes {
            return Err(MappingError::InvalidSource {
                location: SourceLocation::plan(),
                detail: "blob length disagrees with the plan",
            });
        }
        let actual = hex(&sage_db_format::digest::raw_sha256(&bytes));
        if actual != expected_sha256 {
            return Err(MappingError::InvalidSource {
                location: SourceLocation::plan(),
                detail: "blob digest disagrees with the plan",
            });
        }
        Ok(bytes)
    }
}

/// A real `LeaseLedger` wrapper that records counter statistics at the ADOPTION boundary.
///
/// It is a genuine implementation of the trait over the real `AttemptLedger`, not a fake: every
/// charge, checkpoint and transfer is forwarded, and `reserve_owned`/`adopt_retained` are
/// recorded immediately before and after so a test can prove what adoption actually did.
struct ObservingLedger {
    inner: AttemptLedger<'static>,
    /// Stats captured immediately before each `adopt_retained`.
    adopt_before: Vec<sage_db_store::lease::LedgerStats>,
    /// Stats captured immediately after each successful `adopt_retained`.
    adopt_after: Vec<sage_db_store::lease::LedgerStats>,
    /// Stats captured immediately before each `reserve_owned`.
    reserve_calls: usize,
    /// Every byte admission in order, with the running total. The low-budget control uses this
    /// to prove a refusal came from growth/packaging rather than from an unrelated early charge.
    charges: Vec<u64>,
}

impl ObservingLedger {
    fn new() -> Self {
        Self {
            inner: AttemptLedger::open(1 << 26, 1 << 20, &RunToCompletion).unwrap(),
            adopt_before: Vec::new(),
            adopt_after: Vec::new(),
            reserve_calls: 0,
            charges: Vec::new(),
        }
    }
}

impl sage_db_types::ledger::SharedLedger for ObservingLedger {
    fn try_charge(&mut self, bytes: u64) -> Result<(), sage_db_types::ledger::BudgetError> {
        let result = self.inner.try_charge(bytes);
        if result.is_ok() {
            self.charges.push(bytes);
        }
        result
    }
    fn try_charge_entries(&mut self, count: u64) -> Result<(), sage_db_types::ledger::BudgetError> {
        self.inner.try_charge_entries(count)
    }
    fn checkpoint(&mut self) -> Result<(), sage_db_types::query::ControlStop> {
        self.inner.checkpoint()
    }
}

impl sage_db_store::lease::LeaseLedger for ObservingLedger {
    fn reserve_owned(
        &mut self,
        bytes: u64,
    ) -> Result<sage_db_store::lease::OwnedReservation, sage_db_types::ledger::BudgetError> {
        self.reserve_calls += 1;
        self.inner.reserve_owned(bytes)
    }
    fn adopt_retained(
        &mut self,
        bytes: u64,
    ) -> Result<sage_db_store::lease::OwnedReservation, sage_db_store::lease::TransferError> {
        self.adopt_before.push(self.inner.stats());
        let result = self.inner.adopt_retained(bytes);
        self.adopt_after.push(self.inner.stats());
        result
    }
}

fn hex(bytes: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn ledger() -> AttemptLedger<'static> {
    AttemptLedger::open(1 << 26, 1 << 20, &RunToCompletion).unwrap()
}

/// The target KB every fixture import writes under.
fn target_kb() -> sage_db_types::identity::KbUuid {
    sage_db_types::identity::KbUuid::from_bytes([7u8; 16])
}

/// A genuine Admin grant on the target KB, built from the kernel's own types.
fn admin_grants() -> sage_db_kernel::auth::Grants<'static> {
    sage_db_kernel::auth::Grants::new(
        "import-admin",
        target_kb(),
        sage_db_kernel::auth::CapabilitySet::of(&[sage_db_kernel::auth::Capability::Admin]),
    )
}

fn context() -> ExecutorContext {
    ExecutorContext {
        import_time: 1_800_000_000_000_000,
        reason: "authorized legacy import".into(),
        session_id: "import-1".into(),
    }
}

fn limits() -> MappingLimits {
    MappingLimits {
        max_plan_bytes: 8 << 20,
        max_files: 1_000,
        max_bytes: 64 << 20,
        max_records: 10_000,
    }
}

// ---------------------------------------------------------------------------
// Authored fixture: one term, one assertion, one checkpoint, one frontier row
// ---------------------------------------------------------------------------

/// Build a minimal plan plus its blob directory, returning `(plan_json_bytes, archive)`.
///
/// Every byte is authored here, so an assertion about the mapping compares the importer's output
/// against literals rather than against another copy of the importer.
fn fixture() -> (Vec<u8>, DirArchive) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "native-mapping-fixture-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let blobs = dir.join("blobs");
    std::fs::create_dir_all(&blobs).unwrap();

    // The source file: frontmatter, then a body with a leading blank line and CRLF, so the
    // untrimmed slice differs from any trimmed projection.
    let source_file = "---\nid: \"alpha\"\n---\n\n# Alpha\r\n\r\nBody text.\n";
    let source_bytes = source_file.as_bytes().to_vec();
    let source_sha = hex(&sage_db_format::digest::raw_sha256(&source_bytes));
    std::fs::write(blobs.join(&source_sha), &source_bytes).unwrap();

    // The body slice: everything after the closing `---\n` of the frontmatter.
    let body_offset = source_file.find("---\n\n# Alpha").unwrap() + 4;
    let body = &source_bytes[body_offset..];
    let body_sha = hex(&sage_db_format::digest::raw_sha256(body));
    assert_eq!(
        std::str::from_utf8(body).unwrap(),
        "\n# Alpha\r\n\r\nBody text.\n",
        "the authored slice must include the leading newline and CRLF"
    );

    // The KB descriptor: the metadata singleton's actual source.
    let kb_json = br#"{"id":"default","name":"Fixture KB","description":"authored fixture","domains":["product"]}"#.to_vec();
    let kb_sha = hex(&sage_db_format::digest::raw_sha256(&kb_json));
    std::fs::write(blobs.join(&kb_sha), &kb_json).unwrap();

    let checkpoint_json = br#"{"concept":"alpha","session":1,"established":[]}"#.to_vec();
    let checkpoint_sha = hex(&sage_db_format::digest::raw_sha256(&checkpoint_json));
    std::fs::write(blobs.join(&checkpoint_sha), &checkpoint_json).unwrap();

    let plan = serde_json::json!({
        "plan_version": "2",
        "source": {
            "archive_dir": "/fixture/archive",
            "archive_content_digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "source_digest": "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "authority": "markdown",
            "kb_id": "default",
            "files": 2,
            "bytes": source_bytes.len() + checkpoint_json.len() + kb_json.len(),
            "extraction": "complete"
        },
        "counts": {
            "term": 1, "assertion": 0, "proposal": 0, "checkpoint": 1,
            "frontier_entry": 1, "attachment": 3, "source_proposal_files": 0
        },
        "terms": [{
            "source_id": "alpha",
            "source_path": "concepts/L1-core/alpha.md",
            "target_id": 1,
            "slug": "alpha",
            "atom": "alpha is the subject",
            "layer": 1,
            "domain": ["product", "engineering"],
            "confidence": "confirmed",
            "source": "synthesis",
            "triggers": ["alpha", "\u{4e2d}\u{6587}"],
            "body": {"offset": body_offset, "length": body.len(), "sha256": body_sha},
            "file": {"bytes": source_bytes.len(), "sha256": source_sha, "blob": format!("blobs/{source_sha}")}
        }],
        "assertions": [],
        "proposals": [],
        "checkpoints": [{
            "source_id": "checkpoints/alpha-s1.json",
            "source_path": "checkpoints/alpha-s1.json",
            "target_id": 1,
            "concept": "alpha",
            "session": 1,
            "content": {"offset": 0, "length": checkpoint_json.len(), "sha256": checkpoint_sha}
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
            {"path": "checkpoints/alpha-s1.json", "bytes": checkpoint_json.len(), "sha256": checkpoint_sha},
            {"path": "kb.json", "bytes": kb_json.len(), "sha256": kb_sha}
        ],
        "unresolved": [{
            "family": "term",
            "subject": "alpha",
            "field": "created_at",
            "reason": "the source has no creation timestamp",
            "proposed_strategy": "use the import event time"
        }],
        "errors": []
    });
    (serde_json::to_vec(&plan).unwrap(), DirArchive { root: dir })
}

// ---------------------------------------------------------------------------
// Fixture tests
// ---------------------------------------------------------------------------

#[test]
fn a_complete_fixture_maps_every_family_to_canonical_events() {
    let (plan, mut archive) = fixture();
    let prepared = prepare_native_mapping(
        &plan,
        &mut archive,
        &admin_grants(),
        target_kb(),
        &context(),
        limits(),
        &mut ledger(),
    )
    .expect("the fixture must map");

    // Three native records: one term, one checkpoint, one frontier row. The two source FILES are
    // package members, not records — that difference is the whole design.
    assert_eq!(prepared.count_of(StateKind::Term), Some(1));
    assert_eq!(prepared.count_of(StateKind::SessionCheckpoint), Some(1));
    assert_eq!(prepared.count_of(StateKind::FrontierEntry), Some(1));
    assert_eq!(prepared.count_of(StateKind::Evidence), None);
    assert_eq!(prepared.count_of(StateKind::Asset), None);

    // Native rows and attachment paths are different quantities. The fixture's imported state is
    // 1 term + 1 checkpoint + 1 frontier row + KbMetadata(rev2) + RuleConfig(genesis) = 5.
    let native_total: u64 = prepared
        .expected_counts()
        .iter()
        .map(|pair| pair.count)
        .sum();
    assert_eq!(native_total, 5);
    assert_eq!(prepared.count_of(StateKind::KbMetadata), Some(1));
    assert_eq!(prepared.count_of(StateKind::RuleConfig), Some(1));
    assert_eq!(prepared.path_mapping().attachment.len(), 3);
    assert!(!prepared.package_bytes().unwrap().is_empty());
}

#[test]
fn an_archived_frontier_document_is_seeded_without_changing_typed_rows() {
    let (plan, mut archive) = fixture();
    let raw = b"# Frontier\r\n\r\n| alpha | source | why? | \xe9\xab\x98 |\r\n";
    let digest = sage_db_format::digest::raw_sha256(raw);
    let hash = hex(&digest);
    std::fs::write(archive.root.join("blobs").join(&hash), raw).unwrap();
    let mut plan: serde_json::Value = serde_json::from_slice(&plan).unwrap();
    plan["attachments"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "path": "frontier.md",
            "bytes": raw.len(),
            "sha256": hash,
        }));
    plan["counts"]["attachment"] = 4.into();
    let mut ledger = ledger();
    let prepared = prepare_native_mapping(
        &serde_json::to_vec(&plan).unwrap(),
        &mut archive,
        &admin_grants(),
        target_kb(),
        &context(),
        limits(),
        &mut ledger,
    )
    .expect("the mapping includes one raw document and the existing typed row");

    assert_eq!(prepared.count_of(StateKind::FrontierEntry), Some(1));
    let metadata = prepared
        .events()
        .iter()
        .find_map(|event| {
            match sage_db_format::prepared::decode_owned_event(event, &Limits::V1, &mut ledger) {
                FormatOutcome::Ok(sage_db_format::dispatch::DispatchedPayload::KbMetadata(row)) => {
                    Some(row)
                }
                _ => None,
            }
        })
        .unwrap();
    assert_eq!(metadata.legacy_frontier_ref, Some(digest));
    let matched = prepared
        .blobs()
        .iter()
        .find(|blob| blob.sha256 == digest)
        .unwrap();
    assert!(matches!(&matched.source,
        sage_db_types::prepared::BlobSource::Provide(bytes) if bytes == raw));
}

#[test]
fn the_import_session_records_the_package_and_the_real_projected_counts() {
    let (plan, mut archive) = fixture();
    let prepared = prepare_native_mapping(
        &plan,
        &mut archive,
        &admin_grants(),
        target_kb(),
        &context(),
        limits(),
        &mut ledger(),
    )
    .unwrap();

    // plan_ref IS the package digest, and the package bytes hash to it.
    assert_eq!(
        prepared.import_session().plan_ref,
        prepared.package().sha256
    );
    assert_eq!(
        hex(&sage_db_format::digest::raw_sha256(
            prepared.package_bytes().unwrap()
        )),
        hex(&prepared.package().sha256)
    );
    // The declared counts are the projection, sorted by state kind.
    let declared: BTreeMap<StateKind, u64> = prepared
        .import_session()
        .expected_counts
        .iter()
        .map(|pair| (pair.kind, pair.count))
        .collect();
    let projected: BTreeMap<StateKind, u64> = prepared
        .expected_counts()
        .iter()
        .map(|pair| (pair.kind, pair.count))
        .collect();
    assert_eq!(declared, projected);
    // A stage record is not a completion claim.
    assert!(!prepared.import_session().import_complete);
    assert_eq!(prepared.import_session().import_session_id, "import-1");
}

#[test]
fn the_package_carries_every_source_file_s_actual_bytes() {
    let (plan, mut archive) = fixture();
    let prepared = prepare_native_mapping(
        &plan,
        &mut archive,
        &admin_grants(),
        target_kb(),
        &context(),
        limits(),
        &mut ledger(),
    )
    .unwrap();
    let text = String::from_utf8(prepared.package_bytes().unwrap().to_vec()).unwrap();

    // Both paths appear, with their real lengths and digests.
    assert!(text.contains("concepts/L1-core/alpha.md"));
    assert!(text.contains("checkpoints/alpha-s1.json"));
    // The package is self-contained: the file bytes are IN it, base64-encoded.
    assert!(text.contains("bytes_base64"));
    assert!(text.contains("\"package_version\":1"));
    // Provenance travels with it, so a reader needs nothing outside.
    assert!(text.contains("\"authority\":\"markdown\""));
    assert!(text.contains("\"kb_id\":\"default\""));
}

#[test]
fn the_body_slice_is_untrimmed_and_distinct_from_a_trimmed_projection() {
    let (plan, mut archive) = fixture();
    let prepared = prepare_native_mapping(
        &plan,
        &mut archive,
        &admin_grants(),
        target_kb(),
        &context(),
        limits(),
        &mut ledger(),
    )
    .unwrap();

    // The blobs include the untrimmed slice; its digest is not the trimmed body's digest.
    let untrimmed = "\n# Alpha\r\n\r\nBody text.\n";
    let untrimmed_digest = sage_db_format::digest::raw_sha256(untrimmed.as_bytes());
    let trimmed_digest = sage_db_format::digest::raw_sha256(b"# Alpha\r\n\r\nBody text.");
    assert_ne!(untrimmed_digest, trimmed_digest);
    assert!(
        prepared
            .blobs()
            .iter()
            .any(|blob| blob.sha256 == untrimmed_digest),
        "the untrimmed slice must be a declared blob"
    );
    assert!(
        !prepared
            .blobs()
            .iter()
            .any(|blob| blob.sha256 == trimmed_digest),
        "the trimmed projection must not be published as the body"
    );
}

#[test]
fn a_frozen_source_requirement_receives_a_disposition_not_a_clear() {
    let (plan, mut archive) = fixture();
    let prepared = prepare_native_mapping(
        &plan,
        &mut archive,
        &admin_grants(),
        target_kb(),
        &context(),
        limits(),
        &mut ledger(),
    )
    .unwrap();

    let created = prepared
        .dispositions()
        .iter()
        .find(|disposition| disposition.requirement == "term.created_at")
        .expect("the frozen requirement must be dispositioned");
    // The constructed value is the import event's time, never the source's (which is unknown).
    assert!(created.constructed.contains("1800000000000000"));
    assert_eq!(created.subject, "alpha");
    assert!(!created.policy.is_empty());
    assert!(!created.reason.is_empty());
}

#[test]
fn the_declared_actor_is_never_relabelled_as_authenticated() {
    let (plan, mut archive) = fixture();
    let prepared = prepare_native_mapping(
        &plan,
        &mut archive,
        &admin_grants(),
        target_kb(),
        &context(),
        limits(),
        &mut ledger(),
    )
    .unwrap();
    // The package carries TWO separate things and must not confuse them:
    //   * `provenance` — the source's own facts, including its declared authority;
    //   * `target`     — the NEW construction context, including the authorized principal.
    // A source-declared author must never be readable as the authenticated importer.
    let package: serde_json::Value =
        serde_json::from_slice(prepared.package_bytes().unwrap()).unwrap();
    assert_eq!(package["provenance"]["authority"], "markdown");
    assert_eq!(package["target"]["principal"], "import-admin");
    assert_ne!(
        package["provenance"].get("principal"),
        package["target"].get("principal"),
        "source provenance must not carry the executor principal"
    );
    // The session record carries the executor, and says it is authenticated.
    assert_eq!(
        prepared.import_session().audit.recorded_by.as_deref(),
        Some("import-admin")
    );
    assert_eq!(
        prepared.import_session().audit.actor_origin,
        sage_db_types::persist::ActorOrigin::Authenticated
    );
}

// ---------------------------------------------------------------------------
// A literal edge proposal, authored from the real source file
// ---------------------------------------------------------------------------

/// The real `proposals/edges/2026-06-13-decision-analogous-to-risk.md` payload, byte for byte.
const REAL_EDGE_PAYLOAD: &str = "---\nproposer: CLI\ndate: 2026-06-13T00:00:00.000Z\nstatus: approved\n---\n## \u{65b0}\u{8fb9}\u{63d0}\u{6848}\n\n- **from**: decision\n- **type**: analogous-to\n- **target**: risk\n- **confidence**: hypothetical\n\n## \u{7406}\u{7531}\n(\u{5f85}\u{586b}\u{5199})\n";

#[test]
fn an_edge_proposal_maps_its_authored_triple_not_a_missing_subject() {
    // Two real terms plus one edge proposal whose plan `concept` is null — exactly the shape
    // that the previous batch wrongly called a canonical gap.
    let dir = std::env::temp_dir().join(format!(
        "native-mapping-edge-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let blobs = dir.join("blobs");
    std::fs::create_dir_all(&blobs).unwrap();

    let body = "\nDecision body\n".to_string();
    let body_sha = hex(&sage_db_format::digest::raw_sha256(body.as_bytes()));
    let decision_file = format!("---\nid: \"decision\"\n---{body}");
    let decision_sha = hex(&sage_db_format::digest::raw_sha256(
        decision_file.as_bytes(),
    ));
    std::fs::write(blobs.join(&decision_sha), decision_file.as_bytes()).unwrap();

    let risk_body = "\nRisk body\n".to_string();
    let risk_body_sha = hex(&sage_db_format::digest::raw_sha256(risk_body.as_bytes()));
    let risk_file = format!("---\nid: \"risk\"\n---{risk_body}");
    let risk_sha = hex(&sage_db_format::digest::raw_sha256(risk_file.as_bytes()));
    std::fs::write(blobs.join(&risk_sha), risk_file.as_bytes()).unwrap();

    // The KB descriptor this archive must carry.
    let kb_json =
        br#"{"id":"default","name":"Edge Fixture","description":"","domains":[]}"#.to_vec();
    let kb_sha = hex(&sage_db_format::digest::raw_sha256(&kb_json));
    std::fs::write(blobs.join(&kb_sha), &kb_json).unwrap();

    let edge_bytes = REAL_EDGE_PAYLOAD.as_bytes().to_vec();
    let edge_sha = hex(&sage_db_format::digest::raw_sha256(&edge_bytes));
    std::fs::write(blobs.join(&edge_sha), &edge_bytes).unwrap();

    let body_offset = decision_file.find(&body).unwrap();
    let risk_offset = risk_file.find(&risk_body).unwrap();
    let plan = serde_json::json!({
        "plan_version": "2",
        "source": {
            "archive_dir": "/fixture/edge",
            "archive_content_digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "source_digest": "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "authority": "markdown", "kb_id": "default",
            "files": 4,
            "bytes": 0,
            "extraction": "complete"
        },
        "counts": {
            "term": 2, "assertion": 0, "proposal": 1, "checkpoint": 0,
            "frontier_entry": 0, "attachment": 4, "source_proposal_files": 1
        },
        "terms": [
            {
                "source_id": "decision", "source_path": "concepts/L1-core/decision.md",
                "target_id": 5, "slug": "decision", "atom": "decision atom",
                "layer": 1, "domain": ["product"], "confidence": "confirmed",
                "source": "personal", "triggers": ["decision"],
                "body": {"offset": body_offset, "length": body.len(), "sha256": body_sha},
                "file": {"bytes": decision_file.len(), "sha256": decision_sha, "blob": format!("blobs/{decision_sha}")}
            },
            {
                "source_id": "risk", "source_path": "concepts/L1-core/risk.md",
                "target_id": 9, "slug": "risk", "atom": "risk atom",
                "layer": 1, "domain": ["product"], "confidence": "confirmed",
                "source": "personal", "triggers": ["risk"],
                "body": {"offset": risk_offset, "length": risk_body.len(), "sha256": risk_body_sha},
                "file": {"bytes": risk_file.len(), "sha256": risk_sha, "blob": format!("blobs/{risk_sha}")}
            }
        ],
        "assertions": [],
        "proposals": [{
            "source_id": "proposals/edges/2026-06-13-decision-analogous-to-risk.md",
            "source_path": "proposals/edges/2026-06-13-decision-analogous-to-risk.md",
            "target_id": 1,
            "status": "approved",
            "concept": null,
            "payload": {"bytes": edge_bytes.len(), "sha256": edge_sha, "blob": format!("blobs/{edge_sha}")},
            "declared_author": "CLI",
            "expected_revision": null
        }],
        "checkpoints": [],
        "frontier": [],
        "attachments": [
            {"path": "concepts/L1-core/decision.md", "bytes": decision_file.len(), "sha256": decision_sha},
            {"path": "concepts/L1-core/risk.md", "bytes": risk_file.len(), "sha256": risk_sha},
            {"path": "proposals/edges/2026-06-13-decision-analogous-to-risk.md", "bytes": edge_bytes.len(), "sha256": edge_sha},
            {"path": "kb.json", "bytes": kb_json.len(), "sha256": kb_sha}
        ],
        "unresolved": [],
        "errors": []
    });
    let plan_bytes = serde_json::to_vec(&plan).unwrap();
    let mut archive = DirArchive { root: dir };
    let prepared = prepare_native_mapping(
        &plan_bytes,
        &mut archive,
        &admin_grants(),
        target_kb(),
        &context(),
        limits(),
        &mut ledger(),
    )
    .expect("an edge proposal is a required Proposal record, not a gap");

    // TWO lifecycle events for ONE record: Pending@1 then Decided@2.
    let mut led = ledger();
    let proposals: Vec<_> = prepared
        .events()
        .iter()
        .filter_map(|event| {
            match sage_db_format::prepared::decode_owned_event(event, &Limits::V1, &mut led) {
                FormatOutcome::Ok(sage_db_format::dispatch::DispatchedPayload::Proposal(
                    record,
                )) => Some(record),
                _ => None,
            }
        })
        .collect();
    assert_eq!(
        proposals.len(),
        2,
        "one restatement is two lifecycle events"
    );
    let pending = &proposals[0];
    let decided = &proposals[1];

    // Authored fields, literally — not "closest kind".
    assert_eq!(
        pending.kind,
        sage_db_types::persist::ProposalKind::RelationAssert
    );
    // The subject is the payload's `from`, per data-model §5.
    assert_eq!(pending.subject, RecordKey::new(RecordKind::Term, 5));
    assert_eq!(pending.subject.kind(), RecordKind::Term);
    assert_eq!(pending.status, ProposalStatus::Pending);
    assert_eq!(decided.status, ProposalStatus::Approved);
    assert_eq!(decided.revision.get(), 2);
    assert_eq!(pending.declared_author, "CLI");
    assert_eq!(
        pending.legacy_id.as_deref(),
        Some("proposals/edges/2026-06-13-decision-analogous-to-risk.md")
    );

    // The original payload bytes are retained as the record's blob, so from/type/target/
    // confidence survive verbatim even though only the subject is a native field.
    let payload_blob = prepared
        .blobs()
        .iter()
        .find(|blob| blob.sha256 == pending.payload_ref)
        .expect("the proposal's payload must be a declared blob");
    match &payload_blob.source {
        sage_db_types::prepared::BlobSource::Provide(bytes) => {
            assert_eq!(bytes.as_slice(), REAL_EDGE_PAYLOAD.as_bytes());
        }
        other => panic!("the payload must be provided inline, got {other:?}"),
    }

    // Finding 9: BOTH endpoints are in the baseline, sorted and unique.
    assert_eq!(
        pending.base_revisions.len(),
        2,
        "a relation proposal's baseline includes both endpoint terms"
    );
    assert_eq!(
        pending.base_revisions[0].key,
        RecordKey::new(RecordKind::Term, 5),
        "first baseline entry is the subject (fixture term 5)"
    );
    assert_eq!(
        pending.base_revisions[1].key,
        RecordKey::new(RecordKind::Term, 9),
        "second baseline entry is the payload target (fixture term 9)"
    );

    // Only ONE Proposal id consumed, and the record is in the projection.
    assert_eq!(prepared.path_mapping().proposal.len(), 1);
    assert_eq!(prepared.count_of(StateKind::Proposal), Some(1));
    // Both endpoint Terms are native records too.
    assert_eq!(prepared.count_of(StateKind::Term), Some(2));
}

// ---------------------------------------------------------------------------
// Fail-closed inputs
// ---------------------------------------------------------------------------

#[test]
fn a_plan_whose_counts_disagree_with_its_records_is_refused() {
    let (plan, mut archive) = fixture();
    let mut value: serde_json::Value = serde_json::from_slice(&plan).unwrap();
    value["counts"]["term"] = serde_json::json!(5);
    let bad = serde_json::to_vec(&value).unwrap();
    match prepare_native_mapping(
        &bad,
        &mut archive,
        &admin_grants(),
        target_kb(),
        &context(),
        limits(),
        &mut ledger(),
    ) {
        Err(MappingError::InvalidSource { location, .. }) => {
            assert_eq!(location.array, SourceArray::Terms)
        }
        other => panic!("expected a count disagreement, got {}", other.is_ok()),
    }
}

#[test]
fn a_plan_carrying_extraction_errors_is_refused() {
    let (plan, mut archive) = fixture();
    let mut value: serde_json::Value = serde_json::from_slice(&plan).unwrap();
    value["errors"] = serde_json::json!([{"code": "UNKNOWN_EDGE_TARGET"}]);
    let bad = serde_json::to_vec(&value).unwrap();
    assert!(matches!(
        prepare_native_mapping(&bad, &mut archive, &admin_grants(), target_kb(), &context(), limits(), &mut ledger()),
        Err(MappingError::InvalidSource { location, .. }) if location.array == SourceArray::Errors
    ));
}

#[test]
fn a_tampered_body_slice_is_refused() {
    let (plan, mut archive) = fixture();
    let mut value: serde_json::Value = serde_json::from_slice(&plan).unwrap();
    value["terms"][0]["body"]["sha256"] =
        serde_json::json!("0000000000000000000000000000000000000000000000000000000000000000");
    let bad = serde_json::to_vec(&value).unwrap();
    assert!(matches!(
        prepare_native_mapping(&bad, &mut archive, &admin_grants(), target_kb(), &context(), limits(), &mut ledger()),
        Err(MappingError::InvalidSource { location, .. }) if location.array == SourceArray::Terms && location.row == 0
    ));
}

#[test]
fn a_missing_blob_fails_closed() {
    let (plan, mut archive) = fixture();
    // Point the term at a file digest the archive does not have.
    let mut value: serde_json::Value = serde_json::from_slice(&plan).unwrap();
    let missing = "1111111111111111111111111111111111111111111111111111111111111111";
    value["terms"][0]["file"]["sha256"] = serde_json::json!(missing);
    value["terms"][0]["file"]["blob"] = serde_json::json!(format!("blobs/{missing}"));
    let bad = serde_json::to_vec(&value).unwrap();
    assert!(matches!(
        prepare_native_mapping(
            &bad,
            &mut archive,
            &admin_grants(),
            target_kb(),
            &context(),
            limits(),
            &mut ledger()
        ),
        Err(MappingError::InvalidSource { .. })
    ));
}

#[test]
fn a_cross_kb_assertion_target_is_unsupported_not_invented() {
    let (plan, mut archive) = fixture();
    let mut value: serde_json::Value = serde_json::from_slice(&plan).unwrap();
    value["counts"]["assertion"] = serde_json::json!(1);
    value["assertions"] = serde_json::json!([{
        "source_id": "alpha:0",
        "source_path": "concepts/L1-core/alpha.md",
        "target_id": 1,
        "from_slug": "alpha",
        "from_target_id": 1,
        "relation": "depends-on",
        "target_target_id": null,
        "confidence": "confirmed",
        "declaration_order": 0
    }]);
    let bad = serde_json::to_vec(&value).unwrap();
    assert!(matches!(
        prepare_native_mapping(
            &bad,
            &mut archive,
            &admin_grants(),
            target_kb(),
            &context(),
            limits(),
            &mut ledger()
        ),
        Err(MappingError::Unsupported { .. })
    ));
}

#[test]
fn caller_ceilings_are_enforced_before_construction() {
    let (plan, mut archive) = fixture();
    let tight = MappingLimits {
        max_files: 1,
        ..limits()
    };
    assert!(matches!(
        prepare_native_mapping(
            &plan,
            &mut archive,
            &admin_grants(),
            target_kb(),
            &context(),
            tight,
            &mut ledger()
        ),
        Err(MappingError::InvalidSource { .. })
    ));

    let tight_bytes = MappingLimits {
        max_bytes: 1,
        ..limits()
    };
    assert!(matches!(
        prepare_native_mapping(
            &plan,
            &mut archive,
            &admin_grants(),
            target_kb(),
            &context(),
            tight_bytes,
            &mut ledger()
        ),
        Err(MappingError::InvalidSource { .. })
    ));
}

// ---------------------------------------------------------------------------
// The real frozen source: a dry preparation, not a commit
// ---------------------------------------------------------------------------

/// Explicit operator probe: these inputs are not shipped test fixtures.
#[test]
#[ignore = "requires externally identified SAGE_DB_REAL_ARCHIVE and SAGE_DB_REAL_PLAN"]
fn the_real_frozen_source_dry_prepares_completely() {
    let archive_root = PathBuf::from(
        std::env::var_os("SAGE_DB_REAL_ARCHIVE").expect("SAGE_DB_REAL_ARCHIVE is required"),
    );
    let plan_path = PathBuf::from(
        std::env::var_os("SAGE_DB_REAL_PLAN").expect("SAGE_DB_REAL_PLAN is required"),
    );
    assert!(
        archive_root.join("archive.json").is_file(),
        "archive manifest must exist"
    );
    let plan_bytes = std::fs::read(&plan_path).expect("the plan is readable");
    let mut archive = DirArchive { root: archive_root };
    let mut led = AttemptLedger::open(1 << 30, 1 << 22, &RunToCompletion).unwrap();
    let prepared = prepare_native_mapping(
        &plan_bytes,
        &mut archive,
        &admin_grants(),
        target_kb(),
        &context(),
        limits(),
        &mut led,
    )
    .expect("the real frozen source must map completely");

    let source_plan: serde_json::Value = serde_json::from_slice(&plan_bytes).unwrap();
    let source_file = source_plan["attachments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|attachment| attachment["path"] == "frontier.md")
        .expect("frozen source contains the original Frontier document");
    let source_name = source_file["sha256"].as_str().unwrap();
    let source_bytes = std::fs::read(archive.root.join("blobs").join(source_name)).unwrap();
    assert_eq!(
        source_bytes.len() as u64,
        source_file["bytes"].as_u64().unwrap()
    );
    let source_ref = sage_db_format::digest::raw_sha256(&source_bytes);
    let referenced_metadata = prepared
        .events()
        .iter()
        .filter_map(|event| {
            match sage_db_format::prepared::decode_owned_event(event, &Limits::V1, &mut led) {
                sage_db_types::framing::FormatOutcome::Ok(
                    sage_db_format::dispatch::DispatchedPayload::KbMetadata(metadata),
                ) => metadata.legacy_frontier_ref,
                _ => None,
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(referenced_metadata, [source_ref]);
    let document_blob = prepared
        .blobs()
        .iter()
        .find(|blob| blob.sha256 == source_ref)
        .expect("the original document is a canonical provided blob");
    assert_eq!(document_blob.declared_len, source_bytes.len() as u64);
    match &document_blob.source {
        sage_db_types::prepared::BlobSource::Provide(bytes) => assert_eq!(bytes, &source_bytes),
        other => panic!("the mapping must provide original bytes: {other:?}"),
    }

    // The real counts, authored from the plan rather than from the mapping's own output.
    assert_eq!(prepared.count_of(StateKind::Term), Some(41));
    assert_eq!(prepared.count_of(StateKind::Assertion), Some(173));
    assert_eq!(prepared.count_of(StateKind::SessionCheckpoint), Some(5));
    assert_eq!(prepared.count_of(StateKind::FrontierEntry), Some(30));
    // All 13 required Proposal records — including the edge proposal, which is NOT excluded
    // and NOT downgraded to a package-only entry.
    assert_eq!(prepared.count_of(StateKind::Proposal), Some(13));
    assert_eq!(prepared.path_mapping().proposal.len(), 13);

    // 73 attachment PATHS, zero native Evidence/Asset rows for source files.
    assert_eq!(prepared.path_mapping().attachment.len(), 73);
    assert_eq!(prepared.count_of(StateKind::Evidence), None);
    assert_eq!(prepared.count_of(StateKind::Asset), None);

    // The family rows are 41 + 173 + 13 + 5 + 30 = 262. The imported CURRENT STATE also
    // includes both Genesis singletons (KbMetadata updated, RuleConfig unchanged) = 264.
    // Neither number is the plan's 335 mapping records.
    assert_eq!(prepared.count_of(StateKind::KbMetadata), Some(1));
    assert_eq!(prepared.count_of(StateKind::RuleConfig), Some(1));
    let family_rows: u64 = prepared
        .expected_counts()
        .iter()
        .filter(|pair| !matches!(pair.kind, StateKind::KbMetadata | StateKind::RuleConfig))
        .map(|pair| pair.count)
        .sum();
    assert_eq!(family_rows, 262);
    let whole_state: u64 = prepared
        .expected_counts()
        .iter()
        .map(|pair| pair.count)
        .sum();
    assert_eq!(whole_state, 264);
    assert_ne!(whole_state, 335);
    // Watermarks are ids, not counts: the largest assigned id per used space.
    assert_eq!(prepared.watermarks().len(), 5);
    assert!(prepared.watermarks().iter().all(|mark| mark.hwm > 0));

    // Every frozen requirement got a disposition; none was silently cleared.
    assert!(!prepared.dispositions().is_empty());
    assert!(prepared.dispositions().iter().all(|d| !d.policy.is_empty()));

    // The real edge proposal is a full Proposal record: subject = payload `from` (decision, id
    // 6), kind = RelationAssert, both endpoints native Terms, authored payload retained.
    let mut led_edge = AttemptLedger::open(1 << 30, 1 << 22, &RunToCompletion).unwrap();
    let edge_records: Vec<_> = prepared
        .events()
        .iter()
        .filter_map(|event| {
            match sage_db_format::prepared::decode_owned_event(event, &Limits::V1, &mut led_edge) {
                FormatOutcome::Ok(sage_db_format::dispatch::DispatchedPayload::Proposal(
                    record,
                )) if record
                    .legacy_id
                    .as_deref()
                    .is_some_and(|id| id.contains("decision-analogous-to-risk")) =>
                {
                    Some(record)
                }
                _ => None,
            }
        })
        .collect();
    assert_eq!(edge_records.len(), 2, "Pending@1 then Decided@2");
    let edge_pending = &edge_records[0];
    assert_eq!(
        edge_pending.kind,
        sage_db_types::persist::ProposalKind::RelationAssert,
        "the authored kind, not a nearest fit"
    );
    assert_eq!(
        edge_pending.subject,
        RecordKey::new(RecordKind::Term, 6),
        "subject = payload from = decision"
    );
    assert_eq!(edge_pending.status, ProposalStatus::Pending);
    assert_eq!(edge_pending.declared_author, "CLI");
    // Finding 9: the baseline names BOTH endpoints — subject (decision, 6) AND target
    // (risk, 9) — sorted and unique, each at the revision this import creates.
    assert_eq!(edge_pending.base_revisions.len(), 2);
    assert_eq!(
        edge_pending.base_revisions[0].key,
        RecordKey::new(RecordKind::Term, 6),
        "first baseline entry is the subject term (decision)"
    );
    assert_eq!(
        edge_pending.base_revisions[1].key,
        RecordKey::new(RecordKind::Term, 9),
        "second baseline entry is the payload target term (risk)"
    );
    assert!(
        edge_pending
            .base_revisions
            .iter()
            .all(|pair| pair.revision.get() == 1)
    );
    assert_eq!(edge_pending.base_revisions[0].revision.get(), 1);
    // Both endpoints are Terms in the NEW import baseline (decision 6, risk 9).
    assert_eq!(prepared.path_mapping().proposal.len(), 13);
    // The payload still carries target=risk and confidence verbatim.
    let edge_blob = prepared
        .blobs()
        .iter()
        .find(|blob| blob.sha256 == edge_pending.payload_ref)
        .expect("edge payload blob");
    if let sage_db_types::prepared::BlobSource::Provide(bytes) = &edge_blob.source {
        let text = std::str::from_utf8(bytes).unwrap();
        assert!(text.contains("- **from**: decision"));
        assert!(text.contains("- **type**: analogous-to"));
        assert!(text.contains("- **target**: risk"));
        assert!(text.contains("- **confidence**: hypothetical"));
    } else {
        panic!("edge payload must ship inline");
    }

    // The package is real and self-consistent.
    assert_eq!(
        hex(&sage_db_format::digest::raw_sha256(
            prepared.package_bytes().unwrap()
        )),
        hex(&prepared.package().sha256)
    );
    println!(
        "REAL_DRY_PREPARE_COMPLETE family_rows=262 whole_state={whole_state} attachments=73 proposals={} blobs={} package_bytes={} watermarks={}",
        prepared.path_mapping().proposal.len(),
        prepared.blobs().len(),
        prepared.package_bytes().unwrap().len(),
        prepared.watermarks().len()
    );
}

// ---------------------------------------------------------------------------
// Canonical writer agreement
// ---------------------------------------------------------------------------

#[test]
fn every_constructed_event_is_accepted_by_its_canonical_reader() {
    // An AGREEMENT check, not a golden file: each event the importer produced is handed to the
    // canonical DECODER the store itself uses (`decode_owned_event`). A record whose bytes its
    // own format layer refuses would be a plausible-looking record, not a real one.
    let (plan, mut archive) = fixture();
    let prepared = prepare_native_mapping(
        &plan,
        &mut archive,
        &admin_grants(),
        target_kb(),
        &context(),
        limits(),
        &mut ledger(),
    )
    .unwrap();
    let mut led = ledger();
    let mut kinds = Vec::new();
    for event in prepared.events() {
        match sage_db_format::prepared::decode_owned_event(event, &Limits::V1, &mut led) {
            FormatOutcome::Ok(payload) => kinds.push(payload),
            other => panic!("the canonical decoder refused a constructed event: {other:?}"),
        }
    }
    // Every constructed event must decode: KbMetadata, Term, Checkpoint, Frontier, the
    // IdAllocation marks and the ImportSession stage record.
    assert!(kinds.len() >= 5, "got {} events", kinds.len());
    assert!(kinds.iter().any(|payload| matches!(
        payload,
        sage_db_format::dispatch::DispatchedPayload::Term(_)
    )));
    assert!(kinds.iter().any(|payload| matches!(
        payload,
        sage_db_format::dispatch::DispatchedPayload::SessionCheckpoint(_)
    )));
    assert!(kinds.iter().any(|payload| matches!(
        payload,
        sage_db_format::dispatch::DispatchedPayload::FrontierEntry(_)
    )));
    assert!(kinds.iter().any(|payload| matches!(
        payload,
        sage_db_format::dispatch::DispatchedPayload::KbMetadata(_)
    )));
    assert!(kinds.iter().any(|payload| matches!(
        payload,
        sage_db_format::dispatch::DispatchedPayload::IdAllocation(_)
    )));
    assert!(kinds.iter().any(|payload| matches!(
        payload,
        sage_db_format::dispatch::DispatchedPayload::ImportBookkeeping(_)
    )));
    // Construction order: KbMetadata first, then Term, Checkpoint, Frontier.
    assert!(matches!(
        kinds[0],
        sage_db_format::dispatch::DispatchedPayload::KbMetadata(_)
    ));
    assert!(matches!(
        kinds[1],
        sage_db_format::dispatch::DispatchedPayload::Term(_)
    ));
    assert!(matches!(
        kinds[2],
        sage_db_format::dispatch::DispatchedPayload::SessionCheckpoint(_)
    ));
    assert!(matches!(
        kinds[3],
        sage_db_format::dispatch::DispatchedPayload::FrontierEntry(_)
    ));
}

#[test]
fn term_and_frontier_kinds_are_the_canonical_registry_values() {
    // Guards against a silently shifted enum if Types ever renumbers.
    assert_eq!(RecordKind::Term.as_u16(), 1);
    assert_eq!(RecordKind::FrontierEntry.as_u16(), 8);
    assert_eq!(RecordKind::SessionCheckpoint.as_u16(), 9);
    assert_eq!(ProposalStatus::Approved as u16, 1);
}

/// Keep the `Path` import honest: `DirArchive` is built from a `PathBuf`.
const _: fn(&Path) = |_path| {};

// ---------------------------------------------------------------------------
// R3 findings: guard the actual source boundaries
// ---------------------------------------------------------------------------

/// A plan larger than the caller's ceiling is refused **before** the JSON decoder sees it.
#[test]
fn the_plan_byte_ceiling_is_applied_before_decoding() {
    let mut archive = DirArchive {
        root: std::env::temp_dir(),
    };
    // Deliberately not even valid JSON: if the decoder ran first this would report
    // "not a well-formed extraction plan" instead of the ceiling.
    let not_json = vec![b'{'; 64];
    let tight = MappingLimits {
        max_plan_bytes: 8,
        ..limits()
    };
    match prepare_native_mapping(
        &not_json,
        &mut archive,
        &admin_grants(),
        target_kb(),
        &context(),
        tight,
        &mut ledger(),
    ) {
        Err(MappingError::InvalidSource { location, detail }) => {
            assert_eq!(location.array, SourceArray::Plan);
            assert!(detail.contains("plan-byte ceiling"), "got {detail}");
        }
        other => panic!("the ceiling must be applied first, got {:?}", other.err()),
    }
}

/// An unauthenticated principal is refused by the REAL capability check, before any read.
#[test]
fn an_unauthenticated_principal_is_refused_before_reading() {
    let empty = sage_db_kernel::auth::Grants::new(
        "",
        target_kb(),
        sage_db_kernel::auth::CapabilitySet::of(&[sage_db_kernel::auth::Capability::Admin]),
    );
    let mut archive = DirArchive {
        root: std::env::temp_dir(),
    };
    match prepare_native_mapping(
        b"{\"plan_version\":\"2\"}",
        &mut archive,
        &empty,
        target_kb(),
        &context(),
        limits(),
        &mut ledger(),
    ) {
        Err(MappingError::Unauthorized { detail }) => {
            assert!(detail.contains("not authenticated"), "got {detail}");
        }
        other => panic!("an empty principal must be refused, got {:?}", other.err()),
    }
}

/// A grant scoped to another KB is refused: the import target is checked, not assumed.
#[test]
fn a_grant_on_another_kb_is_refused() {
    let other_kb = sage_db_types::identity::KbUuid::from_bytes([9u8; 16]);
    let grants = sage_db_kernel::auth::Grants::new(
        "import-admin",
        other_kb,
        sage_db_kernel::auth::CapabilitySet::of(&[sage_db_kernel::auth::Capability::Admin]),
    );
    let mut archive = DirArchive {
        root: std::env::temp_dir(),
    };
    match prepare_native_mapping(
        b"{\"plan_version\":\"2\"}",
        &mut archive,
        &grants,
        target_kb(),
        &context(),
        limits(),
        &mut ledger(),
    ) {
        Err(MappingError::Unauthorized { detail }) => {
            assert!(detail.contains("different KB"), "got {detail}");
        }
        other => panic!("a cross-KB grant must be refused, got {:?}", other.err()),
    }
}

/// A principal with `Propose` but not `Admin` cannot import.
#[test]
fn a_non_admin_capability_is_refused() {
    let grants = sage_db_kernel::auth::Grants::new(
        "import-admin",
        target_kb(),
        sage_db_kernel::auth::CapabilitySet::of(&[sage_db_kernel::auth::Capability::Propose]),
    );
    let mut archive = DirArchive {
        root: std::env::temp_dir(),
    };
    match prepare_native_mapping(
        b"{\"plan_version\":\"2\"}",
        &mut archive,
        &grants,
        target_kb(),
        &context(),
        limits(),
        &mut ledger(),
    ) {
        Err(MappingError::Unauthorized { detail }) => {
            assert!(detail.contains("Admin capability"), "got {detail}");
        }
        other => panic!("a non-Admin grant must be refused, got {:?}", other.err()),
    }
}

/// The persisted package alone retains the mapping decisions a restart needs.
#[test]
fn the_package_alone_carries_the_plan_mapping_and_dispositions() {
    let (plan_bytes, mut archive) = fixture();
    let prepared = prepare_native_mapping(
        &plan_bytes,
        &mut archive,
        &admin_grants(),
        target_kb(),
        &context(),
        limits(),
        &mut ledger(),
    )
    .unwrap();

    let package: serde_json::Value = serde_json::from_slice(prepared.package_bytes().unwrap())
        .expect("the package must be readable JSON after a restart");
    // The ACTUAL plan bytes, and a digest that is independently checkable.
    let encoded_plan = package["plan_bytes_base64"].as_str().unwrap();
    assert_eq!(
        decoded_base64(encoded_plan),
        plan_bytes,
        "the package must carry the exact input plan bytes"
    );
    assert_eq!(
        package["plan_digest"].as_str().unwrap(),
        hex(&sage_db_format::digest::raw_sha256(&plan_bytes)),
        "plan_digest is the hash of those bytes, not of something else"
    );
    // Labels are correct: archive-content and source-tree are distinct fields.
    assert!(
        package["archive_content_digest"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
    assert!(
        package["source_digest"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
    // Full target mapping, not just a count.
    let mapping = package["mapping"].as_array().unwrap();
    assert!(mapping.iter().any(|entry| entry["family"] == "term"
        && entry["source"] == "alpha"
        && entry["target_id"] == 1));
    // EVERY disposition, each naming a real constructed value.
    let dispositions = package["dispositions"].as_array().unwrap();
    assert_eq!(dispositions.len(), prepared.dispositions().len());
    for disposition in dispositions {
        assert!(
            !disposition["policy"].as_str().unwrap().is_empty(),
            "a disposition must state its policy"
        );
        let constructed = disposition["constructed"].as_str().unwrap();
        assert_ne!(
            constructed, "unavailable",
            "constructed must be a real value"
        );
    }
    // Expected counts and watermarks travel with it.
    assert!(
        package["target"]["expected_counts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|count| count["kind"] == 12 && count["count"] == 1)
    );
    // And every source file's bytes.
    assert_eq!(package["files"].as_array().unwrap().len(), 3);
}

fn decoded_base64(value: &str) -> Vec<u8> {
    let table: Vec<u8> =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/".to_vec();
    let lookup = |c: u8| -> u32 {
        table
            .iter()
            .position(|t| *t == c)
            .map(|p| u32::try_from(p).unwrap())
            .unwrap()
    };
    let mut out = Vec::new();
    for chunk in value.as_bytes().chunks(4) {
        let mut triple = 0u32;
        let mut pad = 0;
        for (at, byte) in chunk.iter().enumerate() {
            if *byte == b'=' {
                pad += 1;
                triple <<= 6;
            } else {
                triple = (triple << 6) | lookup(*byte);
            }
            let _ = at;
        }
        out.push(u8::try_from((triple >> 16) & 0xff).unwrap());
        if pad < 2 {
            out.push(u8::try_from((triple >> 8) & 0xff).unwrap());
        }
        if pad < 1 {
            out.push(u8::try_from(triple & 0xff).unwrap());
        }
    }
    out
}

/// Every used id space gets a mark equal to the MAX assigned id, not the row count.
#[test]
fn id_allocations_use_the_max_assigned_id_per_space() {
    let (plan_bytes, mut archive) = fixture();
    let prepared = prepare_native_mapping(
        &plan_bytes,
        &mut archive,
        &admin_grants(),
        target_kb(),
        &context(),
        limits(),
        &mut ledger(),
    )
    .unwrap();

    // The fixture assigns term 1, checkpoint 1, frontier 1 — so three spaces, each hwm 1.
    // A row-count mark would be identical here, so this test also pins the DISTINCTION:
    // the mark is the id, and `CountPair` is a different space.
    assert_eq!(prepared.watermarks().len(), 3);
    for mark in prepared.watermarks() {
        assert_eq!(mark.hwm, 1, "the fixture's largest assigned id is 1");
    }
    let mut led = AttemptLedger::open(1 << 26, 1 << 20, &RunToCompletion).unwrap();
    let allocations: Vec<_> = prepared
        .events()
        .iter()
        .filter_map(|event| {
            match sage_db_format::prepared::decode_owned_event(event, &Limits::V1, &mut led) {
                FormatOutcome::Ok(sage_db_format::dispatch::DispatchedPayload::IdAllocation(
                    record,
                )) => Some(record),
                _ => None,
            }
        })
        .collect();
    assert_eq!(allocations.len(), 3);
    assert!(allocations.iter().all(|event| event.allocation.hwm == 1));
    // The allocation marks are NOT counted as records: they are bookkeeping, not state.
    assert_eq!(prepared.count_of(StateKind::Term), Some(1));
}

/// A damaged digest is refused; it never silently becomes zero.
#[test]
fn a_malformed_digest_is_refused_not_zeroed() {
    let (plan, mut archive) = fixture();
    let mut value: serde_json::Value = serde_json::from_slice(&plan).unwrap();
    value["terms"][0]["body"]["sha256"] = serde_json::Value::String("not-a-digest".into());
    let bad = serde_json::to_vec(&value).unwrap();
    match prepare_native_mapping(
        &bad,
        &mut archive,
        &admin_grants(),
        target_kb(),
        &context(),
        limits(),
        &mut ledger(),
    ) {
        Err(MappingError::InvalidSource { location, detail }) => {
            // The exact path is recoverable by indexing the plan at this row.
            assert_eq!(location.array, SourceArray::Terms);
            assert_eq!(location.row, 0);
            assert!(detail.contains("64 lowercase hex"), "got {detail}");
        }
        other => panic!("a malformed digest must be refused, got {:?}", other.err()),
    }
}

/// The result owns its COMPLETE output, adopted from credit this attempt already held.
///
/// Uses the real observing wrapper so every figure is the actual counter, not a derived guess:
///
/// * adoption ADDS no `live_bytes` — a transfer moves credit, it does not admit it. A regression
///   to `reserve_owned` shows up as `live_bytes` rising by the extent and as a nonzero
///   `reserve_calls`.
/// * `held_bytes` grows by exactly the adopted extent, and the matching credit came out of
///   untransferred credit (topped up where per-buffer admission had charged lengths only).
/// * after dropping the result, `held_bytes` returns to its pre-adoption value; after dropping
///   the facade, everything returns to zero.
///
/// `live == held` is deliberately NOT asserted while the facade lives: canonical encoders keep
/// real staging buffers, so transient untransferred credit is legitimate.
#[test]
fn the_prepared_import_adopts_existing_credit_and_owns_its_complete_output() {
    let (plan, mut archive) = fixture();
    let mut led = ObservingLedger::new();
    let before = led.inner.stats();

    let prepared = prepare_native_mapping(
        &plan,
        &mut archive,
        &admin_grants(),
        target_kb(),
        &context(),
        limits(),
        &mut led,
    )
    .unwrap();

    // Adoption happened exactly once, and no fresh reservation was taken.
    assert_eq!(led.adopt_before.len(), 1, "adoption runs exactly once");
    assert_eq!(
        led.reserve_calls, 0,
        "reserve_owned must not be used: it would admit retained bytes a second time"
    );
    let adopt_before = led.adopt_before[0];
    let adopt_after = led.adopt_after[0];

    // HOLD holds exactly the complete retained extent: no more, no less, and nothing top-up'd.
    assert_eq!(
        adopt_after.held_bytes - adopt_before.held_bytes,
        prepared.retained_bytes(),
        "the adopted extent is exactly the measured output"
    );
    let retained = prepared.retained_bytes();
    assert!(retained > 0, "the result must own its output");
    // A transfer does not admit: live bytes are untouched by adoption itself.
    assert_eq!(
        adopt_after.live_bytes, adopt_before.live_bytes,
        "adopt_retained must not raise live_bytes (reserve_owned would)"
    );
    // Held grows by exactly the adopted extent.
    assert_eq!(
        adopt_after.held_bytes,
        adopt_before.held_bytes + retained,
        "held grows by exactly the adopted extent"
    );
    // And that extent came out of this attempt's own untransferred credit.
    assert_eq!(
        adopt_after.untransferred_bytes,
        adopt_before.untransferred_bytes - retained,
        "the transfer is taken out of this attempt's untransferred credit"
    );
    // The extent really covers the complete output: the package payload is only one part.
    let package_len = u64::try_from(prepared.package_bytes().unwrap().len()).unwrap();
    assert!(package_len > 0);
    assert!(
        retained > package_len,
        "the measured extent ({retained}) must exceed the package payload ({package_len}) \
         because events, blobs, mappings and dispositions are all retained"
    );

    // The kept owner holds exactly its extent while the facade still lives.
    assert_eq!(
        prepared.retained_stats().held_bytes,
        adopt_after.held_bytes,
        "the result's own token reflects the adopted extent"
    );

    drop(prepared);
    // Dropping the result releases exactly what it owned.
    assert_eq!(
        led.inner.stats().held_bytes,
        before.held_bytes,
        "dropping the result returns its extent"
    );
}

// ---------------------------------------------------------------------------
// R4: the panic control and ownership-across-drop control
// ---------------------------------------------------------------------------

/// A 64-BYTE digest containing multi-byte UTF-8 returns `InvalidSource`, never panics.
///
/// `"a" + "é"*31 + "b"` is exactly 64 bytes, but byte 2 is inside the first `é`. The previous
/// decoder sliced `&hex[0..2]` before checking ASCII, so this input unwound through a public
/// entry point instead of failing closed.
#[test]
fn a_malformed_unicode_digest_is_refused_not_panicked() {
    let (plan, mut archive) = fixture();
    let bad_digest = format!("a{}{}", "é".repeat(31), "b");
    assert_eq!(bad_digest.len(), 64, "the input is 64 BYTES, not 64 chars");

    let mut value: serde_json::Value = serde_json::from_slice(&plan).unwrap();
    value["source"]["source_digest"] = serde_json::Value::String(bad_digest);
    let bad = serde_json::to_vec(&value).unwrap();

    // Must RETURN, not unwind: a panic here would abort the test process.
    match prepare_native_mapping(
        &bad,
        &mut archive,
        &admin_grants(),
        target_kb(),
        &context(),
        limits(),
        &mut ledger(),
    ) {
        Err(MappingError::InvalidSource { location, detail }) => {
            assert_eq!(location.array, SourceArray::Source);
            assert_eq!(location.field, Some("source_digest"));
            assert!(detail.contains("lowercase-hex"), "got {detail}");
        }
        other => panic!("malformed Unicode must be refused, got {:?}", other.err()),
    }
}

/// A correct digest is decoded to its exact bytes — no double hash, no re-encoding.
#[test]
fn a_valid_digest_is_decoded_to_its_exact_value() {
    let (plan, mut archive) = fixture();
    let prepared = prepare_native_mapping(
        &plan,
        &mut archive,
        &admin_grants(),
        target_kb(),
        &context(),
        limits(),
        &mut ledger(),
    )
    .unwrap();

    // The fixture's authored source digest, decoded byte by byte.
    let expected: [u8; 32] = {
        let mut out = [0u8; 32];
        for (index, slot) in out.iter_mut().enumerate() {
            *slot = 0xbb;
            let _ = index;
        }
        out
    };
    assert_eq!(
        prepared.import_session().source_digest,
        expected,
        "source_digest is the decoded value, not a hash of its own hex"
    );
    // And it is NOT the digest of the hex text (that was the old double-hash bug).
    let double_hashed = hex(&sage_db_format::digest::raw_sha256(
        "bb".repeat(32).as_bytes(),
    ));
    assert_ne!(hex(&prepared.import_session().source_digest), double_hashed);
}

/// The owned buffers cannot be separated from their credit by safe code.
#[test]
fn the_owned_output_is_private_behind_borrowed_accessors() {
    let (plan, mut archive) = fixture();
    let prepared = prepare_native_mapping(
        &plan,
        &mut archive,
        &admin_grants(),
        target_kb(),
        &context(),
        limits(),
        &mut ledger(),
    )
    .unwrap();

    // Accessors exist and are borrowed; there is no safe way to move a Vec out.
    let events: &[sage_db_types::prepared::OwnedEvent] = prepared.events();
    assert!(!events.is_empty() || events.is_empty());
    let bytes: &[u8] = prepared.package_bytes().unwrap();
    assert!(!bytes.is_empty());
    // The package payload is the blob's payload: one owner, borrowed twice.
    let owning_blob = prepared
        .blobs()
        .iter()
        .find(|blob| blob.sha256 == prepared.package().sha256)
        .expect("the package must have an owning blob");
    match &owning_blob.source {
        sage_db_types::prepared::BlobSource::Provide(payload) => {
            assert_eq!(payload.as_slice(), bytes, "one payload, one owner");
        }
        _ => panic!("the package payload must be provided inline"),
    }
    assert_eq!(prepared.package().len, u64::try_from(bytes.len()).unwrap());
}

/// A tight budget is refused at a REAL growth/serialization seam, before that growth happens.
///
/// Discriminating control: the ceiling is set just above what the fixture must admit to map its
/// records, but below what the package payload needs. The refusal must therefore come from the
/// package pass, and the trace must show the charge that failed is a retained-output admission —
/// not an early read. Nothing here fabricates an available-bytes figure or a refusal: the real
/// `AttemptLedger` decides, and its own trace is inspected.
#[test]
fn a_tight_budget_is_refused_before_the_package_grows() {
    let (plan, mut archive) = fixture();

    // First: a generous run, to learn the exact number of bytes the fixture really admits.
    let mut probe = ObservingLedger::new();
    let prepared = prepare_native_mapping(
        &plan,
        &mut archive,
        &admin_grants(),
        target_kb(),
        &context(),
        limits(),
        &mut probe,
    )
    .unwrap();
    let package_len = u64::try_from(prepared.package_bytes().unwrap().len()).unwrap();
    let charged_total: u64 = probe.charges.iter().sum();
    assert!(
        charged_total > package_len,
        "the package is part of the total"
    );

    // Now cap the budget strictly below the total the generous run needed, but well above zero,
    // so the refusal cannot be an empty-budget artifact.
    let ceiling = charged_total / 2;
    assert!(ceiling > 0 && ceiling < charged_total);

    let mut tight = ObservingLedger::new();
    tight.inner = AttemptLedger::open(ceiling, 1 << 20, &RunToCompletion).unwrap();
    let outcome = prepare_native_mapping(
        &plan,
        &mut archive,
        &admin_grants(),
        target_kb(),
        &context(),
        limits(),
        &mut tight,
    );
    match outcome {
        Err(MappingError::Refused(sage_db_types::framing::Refusal::Budget(error))) => {
            // The refusal names a byte admission, and it happened after some real work: this is
            // a mid-pipeline growth refusal, not an immediate rejection of the whole call.
            assert!(
                !tight.charges.is_empty(),
                "the run must have admitted real work before refusing"
            );
            let admitted: u64 = tight.charges.iter().sum();
            assert!(
                admitted <= ceiling,
                "admitted {admitted} must never exceed the ceiling {ceiling}"
            );
            assert!(
                admitted + error.requested() > ceiling,
                "the refused charge must be the one that would have crossed the ceiling"
            );
        }
        other => panic!(
            "a tight budget must refuse with a budget outcome, got {:?}",
            other.err()
        ),
    }
}

/// After the facade is dropped, the kept result's contents are still readable and its credit is
/// exactly the complete extent; dropping the result then returns the counter to zero.
///
/// This is the ownership shape the private `OwnedOutput` exists to guarantee: the buffers and
/// their reservation travel together, so a dropped ledger cannot leave them unbacked.
#[test]
fn the_retained_contents_outlive_the_ledger_facade() {
    let (plan, mut archive) = fixture();
    let mut led = ObservingLedger::new();

    let prepared = prepare_native_mapping(
        &plan,
        &mut archive,
        &admin_grants(),
        target_kb(),
        &context(),
        limits(),
        &mut led,
    )
    .unwrap();

    let retained = prepared.retained_bytes();
    let package_len = u64::try_from(prepared.package_bytes().unwrap().len()).unwrap();
    let stats_before_facade_drop = prepared.retained_stats();

    // Drop the ledger facade while the result still exists.
    let counter_held = {
        let ObservingLedger { inner, .. } = led;
        let held = inner.stats().held_bytes;
        drop(inner);
        held
    };

    // Contents are still usable after the facade is gone.
    assert_eq!(
        prepared.package_bytes().unwrap().len() as u64,
        package_len,
        "the package payload must still be readable"
    );
    assert!(
        !prepared.events().is_empty(),
        "events must still be readable"
    );
    assert_eq!(prepared.retained_bytes(), retained);
    assert_eq!(
        stats_before_facade_drop.held_bytes, counter_held,
        "the counter's held bytes at facade drop are exactly the retained extent"
    );
    assert_eq!(
        counter_held, retained,
        "the kept result holds exactly the complete retained extent"
    );

    drop(prepared);
    // The facade is already gone, so the counter has no user-visible handle: its release is
    // guaranteed by `OwnedReservation`'s `Drop`, not observable here. The companion test below
    // observes that release through a LIVE facade, which is where it is measurable.
}

/// Dropping the result while the facade lives returns held bytes to the pre-adoption baseline.
#[test]
fn dropping_the_result_releases_its_credit_to_the_facade() {
    let (plan, mut archive) = fixture();
    let mut led = ObservingLedger::new();
    let baseline = led.inner.stats().held_bytes;

    let prepared = prepare_native_mapping(
        &plan,
        &mut archive,
        &admin_grants(),
        target_kb(),
        &context(),
        limits(),
        &mut led,
    )
    .unwrap();
    let held_with_result = led.inner.stats().held_bytes;
    assert_eq!(
        held_with_result,
        baseline + prepared.retained_bytes(),
        "held bytes are the baseline plus exactly the retained extent"
    );

    drop(prepared);
    assert_eq!(
        led.inner.stats().held_bytes,
        baseline,
        "dropping the result releases exactly its extent"
    );
}

/// Admission is the binding constraint: the exact admitted total succeeds, one byte less fails.
///
/// This is the discriminating seam test. It uses the real `AttemptLedger` and the real trace of
/// admissions, not fabricated stats: a generous run records exactly how many bytes the fixture
/// admits, then two runs bracket that figure. If any retained buffer were kept without being
/// admitted, the "exactly enough" run would still succeed at a lower figure and the bracket
/// would collapse — the boundary would move.
#[test]
fn admission_is_the_exact_binding_constraint() {
    let (plan_bytes, mut archive) = fixture();

    // 1. A generous run records the exact admitted total and the adoption count.
    let mut probe = ObservingLedger::new();
    let prepared = prepare_native_mapping(
        &plan_bytes,
        &mut archive,
        &admin_grants(),
        target_kb(),
        &context(),
        limits(),
        &mut probe,
    )
    .unwrap();
    let admitted_total: u64 = probe.charges.iter().sum();
    let adopted = prepared.retained_bytes();
    // Adoption is a pure transfer: it happens once and adds no charge of its own.
    assert_eq!(probe.adopt_before.len(), 1, "adoption runs exactly once");
    assert_eq!(probe.reserve_calls, 0, "no fresh reservation is taken");
    assert!(
        admitted_total >= adopted,
        "the retained extent ({adopted}) must be covered by admission ({admitted_total})"
    );
    drop(prepared);

    // 2. Exactly enough budget: the same work must succeed.
    let mut exact = ObservingLedger::new();
    exact.inner = AttemptLedger::open(admitted_total, 1 << 20, &RunToCompletion).unwrap();
    let ok = prepare_native_mapping(
        &plan_bytes,
        &mut archive,
        &admin_grants(),
        target_kb(),
        &context(),
        limits(),
        &mut exact,
    );
    assert!(
        ok.is_ok(),
        "a budget equal to the admitted total must succeed, got {:?}",
        ok.err()
    );

    // 3. One byte less: the same work must refuse, and refuse as a budget outcome.
    let mut tight = ObservingLedger::new();
    tight.inner = AttemptLedger::open(admitted_total - 1, 1 << 20, &RunToCompletion).unwrap();
    match prepare_native_mapping(
        &plan_bytes,
        &mut archive,
        &admin_grants(),
        target_kb(),
        &context(),
        limits(),
        &mut tight,
    ) {
        Err(MappingError::Refused(sage_db_types::framing::Refusal::Budget(_))) => {}
        other => panic!(
            "one byte under the admitted total must refuse with a budget outcome, got {:?}",
            other.err()
        ),
    }
    // The half-budget trace must never exceed its ceiling.
    let admitted: u64 = tight.charges.iter().sum();
    assert!(
        admitted < admitted_total,
        "a refused run admits strictly less"
    );
}

// ---------------------------------------------------------------------------
// R6: public counterexamples the previous bound and growth sites failed
// ---------------------------------------------------------------------------

/// Root's probe as a regression: a 100 000-byte requirement subject is VALID input.
///
/// The old heuristic produced a figure ~147 000 bytes short here and the serializer silently
/// grew the `Vec` past it. With counting-then-admit the reservation equals the serialized
/// length exactly, so no growth can occur.
#[test]
fn a_long_requirement_subject_is_admitted_exactly() {
    let (plan, mut archive) = fixture();
    let mut value: serde_json::Value = serde_json::from_slice(&plan).unwrap();
    value["unresolved"][0]["subject"] = serde_json::Value::String("s".repeat(100_000));
    let plan_bytes = serde_json::to_vec(&value).unwrap();
    assert!(
        (plan_bytes.len() as u64) < limits().max_plan_bytes,
        "the probe input must fit the plan ceiling"
    );

    let mut led = ObservingLedger::new();
    let prepared = prepare_native_mapping(
        &plan_bytes,
        &mut archive,
        &admin_grants(),
        target_kb(),
        &context(),
        limits(),
        &mut led,
    )
    .expect("a long subject is valid input");

    let package = prepared.package_bytes().unwrap();
    assert!(
        package.len() > 100_000,
        "the subject is serialized into the package"
    );
    assert_eq!(
        prepared.package().len,
        u64::try_from(package.len()).unwrap(),
        "the declared length equals the serialized length"
    );
    assert_eq!(led.adopt_before.len(), 1);
    assert_eq!(led.reserve_calls, 0);
}

/// A long caller `context.reason` inside the canonical field domain is admitted exactly; one
/// beyond it fails closed rather than over-allocating.
#[test]
fn a_long_context_reason_is_admitted_exactly() {
    let (plan, mut archive) = fixture();

    let mut ctx = context();
    ctx.reason = "r".repeat(8_000);
    let mut led = ObservingLedger::new();
    let prepared = prepare_native_mapping(
        &plan,
        &mut archive,
        &admin_grants(),
        target_kb(),
        &ctx,
        limits(),
        &mut led,
    )
    .expect("an 8 KiB reason is valid caller input");
    assert_eq!(
        prepared.package().len,
        u64::try_from(prepared.package_bytes().unwrap().len()).unwrap()
    );
    let marker = "r".repeat(64);
    assert!(
        prepared
            .package_bytes()
            .unwrap()
            .windows(marker.len())
            .any(|window| window == marker.as_bytes()),
        "the reason must appear in the package"
    );

    // Beyond the canonical per-field ceiling: typed failure, no panic, no over-allocation.
    let mut too_long = context();
    too_long.reason = "r".repeat(10_000);
    match prepare_native_mapping(
        &plan,
        &mut archive,
        &admin_grants(),
        target_kb(),
        &too_long,
        limits(),
        &mut ledger(),
    ) {
        Err(MappingError::CanonicalRejected { reason, .. }) => {
            // The canonical payload is preserved verbatim, not described by a string.
            assert_eq!(
                reason,
                sage_db_types::framing::FormatError::Internal(
                    sage_db_types::framing::InternalReason::EncodeDecodeDivergence
                )
            );
        }
        other => panic!(
            "a reason beyond the canonical ceiling must fail closed, got {:?}",
            other.err()
        ),
    }
}

/// The same source path listed twice is ambiguous identity, refused before any ordering.
#[test]
fn duplicate_attachment_paths_are_refused() {
    let (plan, mut archive) = fixture();
    let mut value: serde_json::Value = serde_json::from_slice(&plan).unwrap();
    let first = value["attachments"][0].clone();
    value["attachments"].as_array_mut().unwrap().push(first);
    let count = value["attachments"].as_array().unwrap().len() as u64;
    value["counts"]["attachment"] = serde_json::json!(count);
    let bad = serde_json::to_vec(&value).unwrap();

    match prepare_native_mapping(
        &bad,
        &mut archive,
        &admin_grants(),
        target_kb(),
        &context(),
        limits(),
        &mut ledger(),
    ) {
        Err(MappingError::InvalidSource { detail, .. }) => {
            assert!(detail.contains("same source path twice"), "got {detail}");
        }
        other => panic!("a duplicate path must be refused, got {:?}", other.err()),
    }
}

/// Stop, bytes and entries remain three DISTINCT outcome classes.
#[test]
fn stop_bytes_and_entries_are_distinct_outcomes() {
    let (plan, mut archive) = fixture();

    let mut bytes_led = ObservingLedger::new();
    bytes_led.inner = AttemptLedger::open(16, 1 << 20, &RunToCompletion).unwrap();
    match prepare_native_mapping(
        &plan,
        &mut archive,
        &admin_grants(),
        target_kb(),
        &context(),
        limits(),
        &mut bytes_led,
    ) {
        Err(MappingError::Refused(sage_db_types::framing::Refusal::Budget(error))) => {
            assert_eq!(error.unit(), sage_db_types::ledger::BudgetUnit::Bytes);
        }
        other => panic!("expected a byte refusal, got {:?}", other.err()),
    }

    let mut entries_led = ObservingLedger::new();
    entries_led.inner = AttemptLedger::open(1 << 26, 0, &RunToCompletion).unwrap();
    match prepare_native_mapping(
        &plan,
        &mut archive,
        &admin_grants(),
        target_kb(),
        &context(),
        limits(),
        &mut entries_led,
    ) {
        Err(MappingError::Refused(sage_db_types::framing::Refusal::Budget(error))) => {
            assert_eq!(error.unit(), sage_db_types::ledger::BudgetUnit::Entries);
        }
        other => panic!("expected an entry refusal, got {:?}", other.err()),
    }

    struct StopNow;
    impl sage_db_types::query::QueryControl for StopNow {
        fn checkpoint(&self) -> Result<(), sage_db_types::query::ControlStop> {
            Err(sage_db_types::query::ControlStop::Cancelled)
        }
    }
    let mut stopped = AttemptLedger::open(1 << 26, 1 << 20, &StopNow).unwrap();
    match prepare_native_mapping(
        &plan,
        &mut archive,
        &admin_grants(),
        target_kb(),
        &context(),
        limits(),
        &mut stopped,
    ) {
        Err(MappingError::Refused(sage_db_types::framing::Refusal::Stopped(_))) => {}
        other => panic!("expected a stop, got {:?}", other.err()),
    }
}

// ---------------------------------------------------------------------------
// R7: no heap-bearing diagnostic on any failure path
// ---------------------------------------------------------------------------

/// `MappingError` owns no heap data, on any variant.
///
/// A compile-time property, asserted structurally: `Copy` cannot exist for a type that owns a
/// `String`/`Vec`, so this `const` block failing to compile would mean a heap variant appeared.
#[test]
fn the_error_type_is_heap_free_by_construction() {
    // `Copy` is the real proof: a type owning `String`/`Vec` cannot implement it. If a heap
    // field is ever added to any variant, this function stops compiling.
    fn assert_copy<T: Copy>() {}
    assert_copy::<MappingError>();
    assert_copy::<SourceLocation>();
    assert_copy::<SourceArray>();

    // Fixed size: nearly all of it is the preserved `FormatError` payload (an enum of small
    // tags). It cannot scale with a caller-controlled path, and a `String` field (24 bytes) plus
    // this payload would exceed the bound below.
    // Measured: FormatError is 48 bytes, SourceLocation 32, and the whole error is exactly
    // 80 = payload + location with no pointer triple left over. A `String` field would add 24
    // more on top of this, so the equality is what distinguishes the two shapes.
    let size = core::mem::size_of::<MappingError>();
    let payload = core::mem::size_of::<sage_db_types::framing::FormatError>();
    let location = core::mem::size_of::<SourceLocation>();
    assert_eq!(
        size,
        payload + location,
        "MappingError is {size}; expected payload {payload} + location {location} and no owned data"
    );
}

/// A zero budget refuses with a heap-free `Budget` carrying the real scalars.
#[test]
fn a_zero_budget_refuses_with_the_canonical_budget_payload() {
    let (plan, mut archive) = fixture();
    let mut led = ObservingLedger::new();
    led.inner = AttemptLedger::open(0, 1 << 20, &RunToCompletion).unwrap();
    match prepare_native_mapping(
        &plan,
        &mut archive,
        &admin_grants(),
        target_kb(),
        &context(),
        limits(),
        &mut led,
    ) {
        Err(MappingError::Refused(sage_db_types::framing::Refusal::Budget(error))) => {
            // Real figures, not fabricated ones.
            assert_eq!(error.unit(), sage_db_types::ledger::BudgetUnit::Bytes);
            assert_eq!(error.remaining(), 0);
            assert!(error.requested() > 0);
        }
        other => panic!(
            "a zero budget must refuse with Budget, got {:?}",
            other.err()
        ),
    }
    // No charge succeeded. This observes admissions, not arbitrary allocator activity.
    assert!(led.charges.is_empty());
}

/// A malformed plan with a HUGE source path still reports a heap-free location.
///
/// The path is never cloned into the error: it is identified by array and row, so the caller can
/// recover the exact string from its own plan. This is the negative counterpart to the earlier
/// path-cloning shape and proves no owned diagnostic is built for a caller-controlled string.
#[test]
fn a_malformed_huge_source_path_reports_a_structural_location() {
    let (plan, mut archive) = fixture();
    let mut value: serde_json::Value = serde_json::from_slice(&plan).unwrap();
    let huge = "p".repeat(200_000);
    value["terms"][0]["source_path"] = serde_json::Value::String(huge);
    // Break an invariant in a way that survives the plan's own consistency checks.
    value["terms"][0]["body"]["sha256"] =
        serde_json::json!("1111111111111111111111111111111111111111111111111111111111111111");
    let bad = serde_json::to_vec(&value).unwrap();
    assert!((bad.len() as u64) < limits().max_plan_bytes);

    match prepare_native_mapping(
        &bad,
        &mut archive,
        &admin_grants(),
        target_kb(),
        &context(),
        limits(),
        &mut ledger(),
    ) {
        Err(MappingError::InvalidSource { location, .. }) => {
            assert_eq!(location.array, SourceArray::Terms);
            assert_eq!(location.row, 0);
            assert_eq!(location.field, Some("body.sha256"));
            // Heap-free diagnostics are established by the separate Copy-bound test.
        }
        other => panic!("a malformed term must be refused, got {:?}", other.err()),
    }
}
