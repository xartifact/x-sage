#![allow(dead_code, unused_imports, unused_variables, unused_mut)]
//! ROOT public-open corruption regressions. Helpers derive from t63 preparation source 1b36c64a / manifest13452875.
//! One false historical state claim has legal canonical framing and repaired real hash joins. The other fixture
//! removes a HISTORICAL COMMITTED reference reachable from CURRENT while the current body remains intact.
//! It is not an orphan. Both exercise the actual public reader; fixture construction alone is not reader evidence.

use sage_db_format::current::{CurrentToEncode, decode_current, encode_current};
use sage_db_format::digest::raw_sha256;
use sage_db_format::manifest::read_manifest;
use sage_db_store::fs::{CreateDir, FileSystem};
use sage_db_store::init::bootstrap;
use sage_db_store::lease::AttemptLedger;
use sage_db_store::names::EntryName;
use sage_db_store_tests::model::fs_model::{ModelCapabilities, ModelFileSystem};
use sage_db_types::framing::{FormatOutcome, Limits};
use sage_db_types::identity::KbUuid;
use sage_db_types::prepared::{OwnedEvent, PreparedTransaction, RulesBinding};
use sage_db_types::query::RunToCompletion;
use sage_db_types::relation::RuleProfile;

const RULES: &[u8] = include_bytes!("../fixtures/rules.formal-v1.v1.bin");
const UUID: [u8; 16] = [0x11; 16];

// ---------------------------------------------------------------------------
// Fixture helpers (test-supplied bytes)
// ---------------------------------------------------------------------------

fn str_value(text: &str) -> Vec<u8> {
    let mut v = (text.len() as u32).to_le_bytes().to_vec();
    v.extend_from_slice(text.as_bytes());
    v
}

/// A body payload that is DISTINCT per (id, revision), so an old revision's blob differs from the new one's.
fn term_body(id: u128, revision: u64) -> Vec<u8> {
    let mut out = b"p15-corr-body/id=".to_vec();
    out.extend_from_slice(&id.to_le_bytes());
    out.extend_from_slice(b"/rev=");
    out.extend_from_slice(&revision.to_le_bytes());
    out.extend_from_slice(b"/payload-distinct");
    out
}

fn term_body_of(digest: &[u8; 32]) -> Option<Vec<u8>> {
    for id in 0u128..8 {
        for revision in 1u64..4 {
            let candidate = term_body(id, revision);
            if raw_sha256(&candidate) == *digest {
                return Some(candidate);
            }
        }
    }
    None
}

fn term_event(id: u128, revision: u64, slug: &str, created_at: i64, updated_at: i64) -> OwnedEvent {
    let body = term_body(id, revision);
    let mut key = 1u16.to_le_bytes().to_vec();
    key.extend_from_slice(&id.to_le_bytes());
    let mut fields: Vec<(u16, Vec<u8>)> = vec![
        (1, key),
        (2, revision.to_le_bytes().to_vec()),
        (4, 0u16.to_le_bytes().to_vec()),
        (5, str_value("p15-corr-actor")),
        (6, 0u16.to_le_bytes().to_vec()),
        (8, created_at.to_le_bytes().to_vec()),
        (16, str_value(slug)),
        (17, str_value("corruption fixture atom")),
        (18, vec![1u8]),
        (19, {
            let mut v = 1u32.to_le_bytes().to_vec();
            v.extend_from_slice(&str_value("corruption"));
            v
        }),
        (20, 0u16.to_le_bytes().to_vec()),
        (21, 2u16.to_le_bytes().to_vec()),
        (22, {
            let mut v = 1u32.to_le_bytes().to_vec();
            v.extend_from_slice(&str_value("corr"));
            v
        }),
        (23, raw_sha256(&body).to_vec()),
    ];
    if revision > 1 {
        fields.push((9, updated_at.to_le_bytes().to_vec()));
    }
    fields.sort_by_key(|(tag, _)| *tag);
    let tag = if revision == 1 { 1 } else { 2 };
    OwnedEvent { tag, fields }
}

fn result_payload(version: u64) -> Vec<u8> {
    let mut out = b"p15-corr-result/".to_vec();
    out.extend_from_slice(&version.to_le_bytes());
    out
}

fn outcome_event(version: u64, request_id: &str) -> OwnedEvent {
    let mut fields: Vec<(u16, Vec<u8>)> = vec![
        (5, str_value("p15-corr-actor")),
        (6, 0u16.to_le_bytes().to_vec()),
        (
            8,
            (1_700_000_000_000_000i64 + version as i64)
                .to_le_bytes()
                .to_vec(),
        ),
        (176, str_value("p15-corr-actor")),
        (177, str_value(request_id)),
        (178, [version as u8; 32].to_vec()),
        (179, 0u16.to_le_bytes().to_vec()),
        (180, raw_sha256(&result_payload(version)).to_vec()),
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
    let rules_ref = raw_sha256(RULES);
    let rules_digest = sage_db_format::digest::digest(sage_db_format::digest::Domain::Rules, RULES);
    let carries_outcome = events.iter().any(|e| e.tag == 192);
    let mut declared = vec![sage_db_types::prepared::BlobDeclaration {
        sha256: rules_ref,
        declared_len: RULES.len() as u64,
        source: sage_db_types::prepared::BlobSource::Provide(RULES.to_vec()),
    }];
    for (tag, value) in events.iter().flat_map(|e| e.fields.iter()) {
        if *tag == 23 && value.len() == 32 {
            let mut digest = [0u8; 32];
            digest.copy_from_slice(value);
            let body =
                term_body_of(&digest).expect("fixture body must be reproducible from its ref");
            declared.push(sage_db_types::prepared::BlobDeclaration {
                sha256: digest,
                declared_len: body.len() as u64,
                source: sage_db_types::prepared::BlobSource::Provide(body),
            });
        }
    }
    if carries_outcome {
        let payload = result_payload(version);
        declared.push(sage_db_types::prepared::BlobDeclaration {
            sha256: raw_sha256(&payload),
            declared_len: payload.len() as u64,
            source: sage_db_types::prepared::BlobSource::Provide(payload),
        });
    }
    PreparedTransaction {
        version,
        prev_version: prev,
        tx_id: [0xB0 + version as u8; 16],
        kb_uuid: KbUuid::from_bytes(UUID),
        created_at: 1_700_000_000_000_000 + version as i64,
        principal_id: "p15-corr-actor".to_owned(),
        request_id: request_id.to_owned(),
        payload_digest: if version == 1 {
            [0u8; 32]
        } else {
            [version as u8; 32]
        },
        expect_version: prev,
        extensions: Default::default(),
        events,
        blobs: declared,
        rules: RulesBinding {
            profile: RuleProfile::FormalV1,
            rules_digest,
            rules_ref,
        },
    }
}

// ---------------------------------------------------------------------------
// A healthy chain, and the handles a corruption step needs
// ---------------------------------------------------------------------------

/// Everything a corruption step needs from a healthy chain, all read from disk.
struct Healthy {
    /// The SAME model the store wrote through.
    ///
    /// `bootstrap` takes its backend by value and `Store` exposes no way to hand it back, so the test holds
    /// a shared `Rc` to it instead: the store receives a thin forwarding handle, and after the store is
    /// dropped this `Rc` is the unique owner again, which is what the model's arrange-only mutators need.
    model: std::rc::Rc<ModelFileSystem>,
    dir: std::path::PathBuf,
    /// Version and manifest file NAME at the old (v2) and new (v3) root.
    old_manifest_name: String,
    old_manifest_bytes: Vec<u8>,
    new_manifest_name: String,
    new_manifest_bytes: Vec<u8>,
    /// The blob referenced by the v2 row's `body_ref` — orphaned once v3 revises that record.
    old_body_ref: [u8; 32],
    /// The blob referenced by the v3 (current) row.
    new_body_ref: [u8; 32],
    /// Key bytes of the revised term, as it appears in the projection (NOT the wire RecordKey).
    revised_key: Vec<u8>,
}

fn durable_file(model: &ModelFileSystem, rel: &str) -> Vec<u8> {
    let view = model.model();
    let states = view.crash_states();
    let mut found: Option<Vec<u8>> = None;
    for state in &states.states {
        let bytes = state
            .files
            .iter()
            .find(|(path, _)| path.as_str() == rel)
            .map(|(_, b)| b.clone());
        match (&found, bytes) {
            (None, Some(b)) => found = Some(b),
            (Some(prev), Some(b)) if *prev == b => {}
            _ => panic!("durable file {rel} is absent or unstable across crash states"),
        }
    }
    found.unwrap_or_else(|| panic!("{rel} must be durable"))
}

/// A file-system handle that forwards every seam call to a shared [`ModelFileSystem`].
///
/// `bootstrap` takes its backend by value and `Store` never returns it, so a test that needs to corrupt the
/// model afterwards hands over this thin forwarding handle and keeps the `Rc`. Nothing about the seam's
/// behaviour changes: every method is the real one.
struct SharedFs {
    inner: std::rc::Rc<ModelFileSystem>,
}

impl sage_db_store::fs::FileSystem for SharedFs {
    type Dir = <ModelFileSystem as sage_db_store::fs::FileSystem>::Dir;
    type WriteFile = <ModelFileSystem as sage_db_store::fs::FileSystem>::WriteFile;
    type ReadFile = <ModelFileSystem as sage_db_store::fs::FileSystem>::ReadFile;
    type Lock = <ModelFileSystem as sage_db_store::fs::FileSystem>::Lock;

    fn capabilities(&self) -> sage_db_store::fs::Capabilities {
        self.inner.capabilities()
    }
    fn open_root(&self, abs: &std::path::Path) -> Result<Self::Dir, sage_db_store::fs::FsError> {
        self.inner.open_root(abs)
    }
    fn create_root(&self, abs: &std::path::Path) -> Result<Self::Dir, sage_db_store::fs::FsError> {
        self.inner.create_root(abs)
    }
    fn sync_parent_of(&self, abs: &std::path::Path) -> Result<(), sage_db_store::fs::FsError> {
        self.inner.sync_parent_of(abs)
    }
    fn open_family(
        &self,
        root: &Self::Dir,
        family: sage_db_store::fs::Family,
        create: CreateDir,
    ) -> Result<Self::Dir, sage_db_store::fs::FsError> {
        self.inner.open_family(root, family, create)
    }
    fn sync_dir(&self, dir: &Self::Dir) -> Result<(), sage_db_store::fs::FsError> {
        self.inner.sync_dir(dir)
    }
    fn device_id(&self, dir: &Self::Dir) -> Result<u64, sage_db_store::fs::FsError> {
        self.inner.device_id(dir)
    }
    fn try_lock_exclusive(
        &self,
        root: &Self::Dir,
    ) -> Result<Self::Lock, sage_db_store::fs::FsError> {
        self.inner.try_lock_exclusive(root)
    }
    fn create_new(
        &self,
        dir: &Self::Dir,
        name: EntryName,
    ) -> Result<Self::WriteFile, sage_db_store::fs::FsError> {
        self.inner.create_new(dir, name)
    }
    fn write(
        &self,
        file: &mut Self::WriteFile,
        buf: &[u8],
    ) -> Result<sage_db_store::fs::WriteOutcome, sage_db_store::fs::FsError> {
        self.inner.write(file, buf)
    }
    fn sync_file(&self, file: &Self::WriteFile) -> Result<(), sage_db_store::fs::FsError> {
        self.inner.sync_file(file)
    }
    fn sync_entry(
        &self,
        dir: &Self::Dir,
        name: EntryName,
    ) -> Result<(), sage_db_store::fs::FsError> {
        self.inner.sync_entry(dir, name)
    }
    fn publish_no_overwrite(
        &self,
        staged_dir: &Self::Dir,
        staged: EntryName,
        dst_dir: &Self::Dir,
        dst: EntryName,
        expected: &sage_db_store::fs::HashReport,
        ledger: &mut dyn sage_db_types::ledger::SharedLedger,
    ) -> Result<sage_db_store::fs::Published, sage_db_store::fs::FsError> {
        self.inner
            .publish_no_overwrite(staged_dir, staged, dst_dir, dst, expected, ledger)
    }
    fn replace_current(
        &self,
        staged_dir: &Self::Dir,
        staged: EntryName,
        root: &Self::Dir,
    ) -> Result<(), sage_db_store::fs::FsError> {
        self.inner.replace_current(staged_dir, staged, root)
    }
    fn size(&self, dir: &Self::Dir, name: EntryName) -> Result<u64, sage_db_store::fs::FsError> {
        self.inner.size(dir, name)
    }
    fn read_exact_into(
        &self,
        dir: &Self::Dir,
        name: EntryName,
        buf: &mut [u8],
    ) -> Result<usize, sage_db_store::fs::FsError> {
        self.inner.read_exact_into(dir, name, buf)
    }
    fn open_read(
        &self,
        dir: &Self::Dir,
        name: EntryName,
    ) -> Result<Self::ReadFile, sage_db_store::fs::FsError> {
        self.inner.open_read(dir, name)
    }
    fn read(
        &self,
        file: &mut Self::ReadFile,
        buf: &mut [u8],
    ) -> Result<usize, sage_db_store::fs::FsError> {
        self.inner.read(file, buf)
    }
    fn hash_bounded(
        &self,
        dir: &Self::Dir,
        name: EntryName,
        max_len: u64,
        ledger: &mut dyn sage_db_types::ledger::SharedLedger,
    ) -> Result<sage_db_store::fs::HashReport, sage_db_store::fs::FsError> {
        self.inner.hash_bounded(dir, name, max_len, ledger)
    }
    fn for_each_entry(
        &self,
        dir: &Self::Dir,
        ledger: &mut dyn sage_db_types::ledger::SharedLedger,
        visit: &mut dyn FnMut(&str) -> sage_db_store::fs::Each,
    ) -> Result<(), sage_db_store::fs::FsError> {
        self.inner.for_each_entry(dir, ledger, visit)
    }
    fn remove(&self, dir: &Self::Dir, name: EntryName) -> Result<(), sage_db_store::fs::FsError> {
        self.inner.remove(dir, name)
    }
}

fn build_healthy(observed: ModelFileSystem) -> Healthy {
    let model = std::rc::Rc::new(observed);
    let dir = std::env::temp_dir().join(format!("p15-corr-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let control = RunToCompletion;
    let mut ledger = AttemptLedger::open(1 << 24, 1 << 20, &control).expect("ledger");
    let mut store = match bootstrap(
        SharedFs {
            inner: std::rc::Rc::clone(&model),
        },
        &dir,
        CreateDir::IfMissing,
        KbUuid::from_bytes(UUID),
        RULES,
        &Limits::V1,
        &mut ledger,
    ) {
        Ok(s) => s,
        Err(e) => panic!("bootstrap: {e:?}"),
    };

    let a1 = term_event(1, 1, "s1", 1, 1);
    let b1 = term_event(2, 1, "s2", 2, 2);
    let mut e2 = vec![a1.clone(), b1.clone()];
    e2.push(outcome_event(2, "req-2"));
    assert!(matches!(
        store.commit(carrier(2, Some(1), "req-2", e2), &Limits::V1, &mut ledger),
        sage_db_store::handle::HandleOutcome::Committed { version: 2, .. }
    ));

    // v3 revises A. Its body payload differs, so A's revision-1 blob becomes orphaned by the CURRENT row.
    let a2 = term_event(1, 2, "s1-revised", 1, 9);
    let mut e3 = vec![a2.clone()];
    e3.push(outcome_event(3, "req-3"));
    assert!(matches!(
        store.commit(carrier(3, Some(2), "req-3", e3), &Limits::V1, &mut ledger),
        sage_db_store::handle::HandleOutcome::Committed { version: 3, .. }
    ));
    drop(store);

    // Read the chain back from disk through the canonical readers.
    let current_bytes = durable_file(&model, "CURRENT");
    let mut ledger2 = AttemptLedger::open(1 << 24, 1 << 20, &control).expect("ledger");
    let current = match decode_current(&current_bytes, &Limits::V1, &mut ledger2) {
        FormatOutcome::Ok(c) => c,
        other => panic!("CURRENT: {other:?}"),
    };
    assert_eq!(current.version, 3, "healthy chain ends at v3");
    let new_manifest_name = current.manifest_name().to_owned();
    let new_manifest_bytes = durable_file(&model, &format!("manifests/{new_manifest_name}"));
    assert_eq!(
        raw_sha256(&new_manifest_bytes),
        current.manifest_sha256,
        "CURRENT must name the v3 manifest by its real file hash"
    );
    let mut ledger3 = AttemptLedger::open(1 << 24, 1 << 20, &control).expect("ledger");
    let new_manifest = match read_manifest(&new_manifest_bytes, &Limits::V1, &mut ledger3) {
        FormatOutcome::Ok(m) => m,
        other => panic!("v3 manifest: {other:?}"),
    };
    let old_manifest_name = new_manifest
        .header
        .prev_manifest_name
        .expect("v3 names its predecessor")
        .to_owned();
    let old_manifest_bytes = durable_file(&model, &format!("manifests/{old_manifest_name}"));
    assert_eq!(
        raw_sha256(&old_manifest_bytes),
        new_manifest
            .header
            .prev_manifest_sha256
            .expect("v3 records its predecessor hash"),
        "the v3 predecessor hash must be the v2 manifest's real file hash"
    );

    // The two distinct body blobs, and the revised record's projected key.
    let old_body = term_body(1, 1);
    let new_body = term_body(1, 2);
    assert_ne!(
        raw_sha256(&old_body),
        raw_sha256(&new_body),
        "the two revisions must have DISTINCT body payloads"
    );
    Healthy {
        model,
        dir,
        old_manifest_name,
        old_manifest_bytes,
        new_manifest_name,
        new_manifest_bytes,
        old_body_ref: raw_sha256(&old_body),
        new_body_ref: raw_sha256(&new_body),
        revised_key: 1u32.to_le_bytes().to_vec(),
    }
}

// ---------------------------------------------------------------------------
// Fixture (A): old manifest STATE claim false, everything else valid
// ---------------------------------------------------------------------------

/// Replace a file's bytes.
///
/// `ModelFs::write` appends (`extend_from_slice`), so rewriting in place would leave the old bytes in front
/// and produce an over-length file — that is what made the first attempt fail with `Limit(CurrentFile)`.
/// Unlinking and re-creating gives a fresh inode holding exactly `bytes`.
fn replace_file(
    m: &mut sage_db_store_tests::model::ModelFs,
    dir: &sage_db_store_tests::model::RelPath,
    name: &str,
    bytes: &[u8],
) {
    m.remove(dir, name);
    let ino = m.create_new(dir, name);
    m.write(ino, bytes);
    m.sync_file(ino);
    m.sync_dir(dir);
}

/// Take the unique `&mut ModelFileSystem` needed by the model's arrange-only mutators.
///
/// Valid only after the store has been dropped: until then the store holds a second handle, so
/// `Rc::get_mut` returns `None` and this panics with a clear message instead of silently doing nothing.
fn model_mut(healthy: &mut Healthy) -> &mut sage_db_store_tests::model::ModelFs {
    let fs = std::rc::Rc::get_mut(&mut healthy.model)
        .unwrap_or_else(|| panic!("the store must be dropped before corrupting the model"));
    fs.model_mut()
}

/// Rebuild the v2 manifest with a deliberately WRONG `state_digest`, leaving every hash join valid.
///
/// Only the historical state claim changes. The manifest's own trailer/file hash is recomputed, the
/// successor's `prev_manifest_sha256` and `CURRENT` are updated to point at the new bytes, so a reader sees
/// a legal, self-consistent chain whose v2 state claim is simply false.
fn corrupt_old_state_claim(healthy: &mut Healthy) {
    use sage_db_format::encode::DecodedManifest;

    let control = RunToCompletion;
    let mut ledger = AttemptLedger::open(1 << 24, 1 << 20, &control).expect("ledger");
    let old = match read_manifest(&healthy.old_manifest_bytes, &Limits::V1, &mut ledger) {
        FormatOutcome::Ok(m) => m,
        other => panic!("v2 manifest: {other:?}"),
    };
    let mut forged_state = old.header.state_digest;
    forged_state[0] ^= 0xFF; // one flipped byte: a false claim, not malformed bytes

    let mut reencoded = DecodedManifest {
        kb_uuid: old.header.kb_uuid,
        version: old.header.version,
        created_at: old.header.created_at,
        rules_profile: old.header.rules_profile,
        rules_digest: old.header.rules_digest,
        state_digest: forged_state,
        graph_digest: old.header.graph_digest,
        chain_digest: old.header.chain_digest,
        bookkeeping_digest: old.header.bookkeeping_digest,
        prev_manifest_name: old.header.prev_manifest_name.map(str::to_owned),
        prev_manifest_sha256: old.header.prev_manifest_sha256,
        tx_file_name: old.header.tx_file_name.to_owned(),
        tx_file_sha256: old.header.tx_file_sha256,
    };
    let mut sink = sage_db_store::commit::Collect { bytes: Vec::new() };
    match sage_db_format::encode::encode_manifest(&reencoded, &mut sink, &Limits::V1, &mut ledger) {
        FormatOutcome::Ok(()) => {}
        other => panic!("re-encoded v2 manifest must be legal: {other:?}"),
    }
    let new_old_bytes = sink.bytes;
    assert_ne!(
        raw_sha256(&new_old_bytes),
        raw_sha256(&healthy.old_manifest_bytes),
        "the forged manifest must actually differ"
    );

    // Write the forged bytes over the old manifest name, through the model.
    let old_rel = format!("manifests/{}", healthy.old_manifest_name);
    let old_name = healthy.old_manifest_name.clone();
    let m = model_mut(healthy);
    assert!(
        m.inode_of(&relpath("manifests"), &old_name).is_some(),
        "the old manifest must exist before it is replaced"
    );
    replace_file(m, &relpath("manifests"), &old_name, &new_old_bytes);
    let _ = old_rel;

    // The successor and CURRENT must be re-derived from the FORGED bytes, so the new value has to be in
    // place first: passing the old bytes here was the bug that made the prev-manifest hash disagree.
    healthy.old_manifest_bytes = new_old_bytes;
    reencode_successor_and_current(healthy);
}

/// Re-encode the v3 manifest with an updated `prev_manifest_sha256`, then re-point `CURRENT`.
///
/// This is what keeps every join valid: the successor genuinely records the new predecessor hash, and
/// `CURRENT` genuinely names the new successor file hash.
fn reencode_successor_and_current(healthy: &mut Healthy) {
    use sage_db_format::encode::DecodedManifest;

    let control = RunToCompletion;
    let mut ledger = AttemptLedger::open(1 << 24, 1 << 20, &control).expect("ledger");
    let new = match read_manifest(&healthy.new_manifest_bytes, &Limits::V1, &mut ledger) {
        FormatOutcome::Ok(m) => m,
        other => panic!("v3 manifest: {other:?}"),
    };
    let mut reencoded = DecodedManifest {
        kb_uuid: new.header.kb_uuid,
        version: new.header.version,
        created_at: new.header.created_at,
        rules_profile: new.header.rules_profile,
        rules_digest: new.header.rules_digest,
        state_digest: new.header.state_digest,
        graph_digest: new.header.graph_digest,
        chain_digest: new.header.chain_digest,
        bookkeeping_digest: new.header.bookkeeping_digest,
        prev_manifest_name: new.header.prev_manifest_name.map(str::to_owned),
        prev_manifest_sha256: Some(raw_sha256(&healthy.old_manifest_bytes)),
        tx_file_name: new.header.tx_file_name.to_owned(),
        tx_file_sha256: new.header.tx_file_sha256,
    };
    let mut sink = sage_db_store::commit::Collect { bytes: Vec::new() };
    match sage_db_format::encode::encode_manifest(&reencoded, &mut sink, &Limits::V1, &mut ledger) {
        FormatOutcome::Ok(()) => {}
        other => panic!("re-encoded v3 manifest must be legal: {other:?}"),
    }
    let new_bytes = sink.bytes;

    let new_name = healthy.new_manifest_name.clone();
    let m = model_mut(healthy);
    replace_file(m, &relpath("manifests"), &new_name, &new_bytes);
    healthy.new_manifest_bytes = new_bytes;

    // CURRENT: same version and name, new manifest file hash.
    let current_name = healthy.new_manifest_name.clone();
    let current_hash = raw_sha256(&healthy.new_manifest_bytes);
    let current =
        match CurrentToEncode::new(KbUuid::from_bytes(UUID), 3, &current_name, current_hash) {
            FormatOutcome::Ok(c) => c,
            other => panic!("CurrentToEncode: {other:?}"),
        };
    let mut out = Vec::new();
    match encode_current(&current, &mut out, &Limits::V1, &mut ledger) {
        FormatOutcome::Ok(()) => {}
        other => panic!("encode_current: {other:?}"),
    }
    let m = model_mut(healthy);
    replace_file(m, &relpath(""), "CURRENT", &out);
}

/// A tiny helper so the model's `RelPath` stays a real value rather than a fabricated one.
fn relpath(dir: &str) -> sage_db_store_tests::model::RelPath {
    if dir.is_empty() {
        sage_db_store_tests::model::RelPath::root()
    } else {
        sage_db_store_tests::model::RelPath::join(&sage_db_store_tests::model::RelPath::root(), dir)
    }
}

// ---------------------------------------------------------------------------
// Fixture (B): old referenced blob absent, current references intact
// ---------------------------------------------------------------------------

fn remove_old_blob(healthy: &mut Healthy) {
    let name = {
        let mut hex = String::new();
        for byte in healthy.old_body_ref {
            hex.push_str(&format!("{byte:02x}"));
        }
        hex
    };
    let blobs = relpath("blobs");
    let m = model_mut(healthy);
    let _ = m
        .inode_of(&blobs, &name)
        .expect("the historical committed body blob must exist before removal");
    m.remove(&blobs, &name);
    m.sync_dir(&blobs);

    // The current blob must still be present: this is the discriminator between "old reference lost" and
    // "everything lost".
    let mut new_hex = String::new();
    for byte in healthy.new_body_ref {
        new_hex.push_str(&format!("{byte:02x}"));
    }
    assert!(
        healthy.model.model().has_entry(&blobs, &new_hex),
        "the CURRENT row's body blob must still exist"
    );
}

// ---------------------------------------------------------------------------
// Contract tests: the fixtures must be LEGAL bytes with ONE wrong claim
// ---------------------------------------------------------------------------

/// Fixture (A): after corrupting the v2 state claim, the chain is still codec-legal and every hash join
/// holds; only the v2 `state_digest` is false.
use sage_db_store::open::{OpenRefusal, Opened, open_existing};
use sage_db_types::framing::FormatError;

fn actual_open_class(healthy: &Healthy) -> &'static str {
    let control = RunToCompletion;
    let mut ledger =
        AttemptLedger::open(1 << 26, 1 << 20, &control).expect("fresh real open ledger");
    let fs = SharedFs {
        inner: std::rc::Rc::clone(&healthy.model),
    };
    match open_existing(
        fs,
        &healthy.dir,
        Some(KbUuid::from_bytes(UUID)),
        &Limits::V1,
        &mut ledger,
    ) {
        Err(OpenRefusal::Corrupt(_)) | Err(OpenRefusal::Format(FormatError::Corrupt(_))) => {
            "Corrupt"
        }
        Err(OpenRefusal::Filesystem { .. }) => "Filesystem",
        Err(_) => "OtherRefusal",
        Ok(Opened::Ready(_)) => "Ready",
        Ok(Opened::ReadOnly { .. }) => "ReadOnly",
    }
}

#[test]
fn legal_hash_joined_false_old_state_claim_is_rejected_by_public_open() {
    let mut healthy = build_healthy(ModelFileSystem::new(ModelCapabilities::full()));
    corrupt_old_state_claim(&mut healthy);
    let control = RunToCompletion;
    let mut ledger = AttemptLedger::open(1 << 26, 1 << 20, &control).expect("fixture check ledger");
    let bytes = durable_file(&healthy.model, "CURRENT");
    let current = match decode_current(&bytes, &Limits::V1, &mut ledger) {
        FormatOutcome::Ok(c) => c,
        _ => panic!("forged fixture CURRENT remains codec-legal"),
    };
    assert_eq!(
        current.manifest_sha256,
        raw_sha256(&healthy.new_manifest_bytes)
    );
    let latest = match read_manifest(&healthy.new_manifest_bytes, &Limits::V1, &mut ledger) {
        FormatOutcome::Ok(m) => m,
        _ => panic!("forged successor remains codec-legal"),
    };
    assert_eq!(
        latest.header.prev_manifest_sha256,
        Some(raw_sha256(&healthy.old_manifest_bytes))
    );
    assert_eq!(
        actual_open_class(&healthy),
        "Corrupt",
        "the actual reader must reject the false PRIOR semantic state claim even with correct file joins and latest tail"
    );
}

#[test]
fn missing_historical_committed_blob_is_corrupt_before_any_view() {
    let mut healthy = build_healthy(ModelFileSystem::new(ModelCapabilities::full()));
    let before_current = durable_file(&healthy.model, "CURRENT");
    let before_latest = healthy.new_manifest_bytes.clone();
    assert_ne!(healthy.old_body_ref, healthy.new_body_ref);
    remove_old_blob(&mut healthy);
    assert_eq!(durable_file(&healthy.model, "CURRENT"), before_current);
    assert_eq!(healthy.new_manifest_bytes, before_latest);
    assert_eq!(
        actual_open_class(&healthy),
        "Corrupt",
        "the v2 historical-only COMMITTED reference is not an orphan; no Ready/Unverified handle may be published when it is missing"
    );
}
