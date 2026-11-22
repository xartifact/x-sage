//! T83 independent functional controls, second packet: seam observer readback.
//!
//! # Why a wrapper rather than a public Store accessor
//!
//! `Store` exposes only `head`, `published_pin`, `is_fenced`, `coverage_complete`, `kb_uuid`,
//! `retained_entries`, `commit` — no filesystem, root or family handle. That is a deliberate product
//! surface, and the right fix is NOT a new accessor: the backend is *mine*, the one I pass to
//! `bootstrap`. So this file wraps the SAME `ModelFileSystem` in a delegating newtype that shares it
//! through an `Rc`, forwards every seam method unchanged, and exposes the model to the test afterwards.
//!
//! That is not a second filesystem, not a fake Store, and not a production change: it is the same
//! backend, observed from outside after the `Store` has taken ownership of it.
//!
//! # What becomes observable, and what each observation is worth
//!
//! | observation | source | independence |
//! |---|---|---|
//! | CURRENT / manifest / transaction bytes | model, via the shared `Rc` | actual durable bytes |
//! | persisted `state_digest` | manifest header, decoded with the real reader | compared to this test's own projection |
//! | retry side effects | mutating-call counter + durable name set | actual seam effects |
//! | `AlreadyCommitted.recorded` | public API field | compared to the outcome event this test supplied |
//!
//! The Genesis half of the *expected* entry set still comes from the published pin (bootstrap authors its
//! own metadata defaults), and is therefore baseline preservation, not independent digest evidence.
//! The A/A2/B half comes from this test's bytes.
#![cfg(test)]

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use sage_db_format::current::decode_current;
use sage_db_format::digest::raw_sha256;
use sage_db_format::manifest::read_manifest;
use sage_db_format::projection::project_event;
use sage_db_store::commit::{CommitRefusal, RecordedOutcome};
use sage_db_store::fold::FoldDefect;
use sage_db_store::fs::{
    Capabilities, CreateDir, Each, Family, FileSystem, FsError, HashReport, Published, WriteOutcome,
};
use sage_db_store::handle::{HandleOutcome, HandleRefusal, Store};
use sage_db_store::init::bootstrap;
use sage_db_store::lease::AttemptLedger;
use sage_db_store::names::EntryName;
use sage_db_store_tests::model::fs_model::{ModelCapabilities, ModelFileSystem};
use sage_db_types::framing::{FormatOutcome, Limits};
use sage_db_types::identity::{KbUuid, KbVersion};
use sage_db_types::persist::{RecordSet, StateEntry};
use sage_db_types::prepared::{OwnedEvent, PreparedTransaction, RulesBinding};
use sage_db_types::query::RunToCompletion;
use sage_db_types::relation::RuleProfile;

const RULES: &[u8] = include_bytes!("../fixtures/rules.formal-v1.v1.bin");

// ---------------------------------------------------------------------------
// The delegating seam observer
// ---------------------------------------------------------------------------

/// Actual mutating seam calls, counted at the seam rather than inferred.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
struct Mutations {
    create_root: u64,
    sync_parent_of: u64,
    open_family_if_missing: u64,
    create_new: u64,
    write: u64,
    sync_file: u64,
    sync_entry: u64,
    sync_dir: u64,
    publish_no_overwrite: u64,
    replace_current: u64,
    remove: u64,
}

/// A `FileSystem` that forwards everything to a shared [`ModelFileSystem`].
///
/// The `Rc` is why this works at all: `bootstrap` takes the backend by value, so a plain reference would
/// end the observation at the call. Sharing keeps one model alive and lets the test read its durable
/// state afterwards.
struct ObservedFs {
    inner: Rc<ModelFileSystem>,
    mutations: Rc<RefCell<Mutations>>,
}

impl ObservedFs {
    fn new() -> Self {
        Self {
            inner: Rc::new(ModelFileSystem::new(ModelCapabilities::full())),
            mutations: Rc::new(RefCell::new(Mutations::default())),
        }
    }

    fn model_handle(&self) -> Rc<ModelFileSystem> {
        Rc::clone(&self.inner)
    }

    fn count(&self, f: impl FnOnce(&mut Mutations)) {
        f(&mut self.mutations.borrow_mut());
    }
}

impl FileSystem for ObservedFs {
    type Dir = <ModelFileSystem as FileSystem>::Dir;
    type WriteFile = <ModelFileSystem as FileSystem>::WriteFile;
    type ReadFile = <ModelFileSystem as FileSystem>::ReadFile;
    type Lock = <ModelFileSystem as FileSystem>::Lock;

    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
    fn open_root(&self, abs: &Path) -> Result<Self::Dir, FsError> {
        self.inner.open_root(abs)
    }
    fn create_root(&self, abs: &Path) -> Result<Self::Dir, FsError> {
        self.count(|m| m.create_root += 1);
        self.inner.create_root(abs)
    }
    fn sync_parent_of(&self, abs: &Path) -> Result<(), FsError> {
        self.count(|m| m.sync_parent_of += 1);
        self.inner.sync_parent_of(abs)
    }
    fn open_family(
        &self,
        root: &Self::Dir,
        family: Family,
        create: CreateDir,
    ) -> Result<Self::Dir, FsError> {
        // Invocation count, not an effect claim: a `IfMissing` open is a mutating attempt whether or not
        // the directory had to be created. `create_dir` had no honest observation point here (the seam
        // exposes no "did it exist" query), so the fiction is removed and the attempt is counted instead.
        if create == CreateDir::IfMissing {
            self.count(|m| m.open_family_if_missing += 1);
        }
        self.inner.open_family(root, family, create)
    }
    fn sync_dir(&self, dir: &Self::Dir) -> Result<(), FsError> {
        self.count(|m| m.sync_dir += 1);
        self.inner.sync_dir(dir)
    }
    fn device_id(&self, dir: &Self::Dir) -> Result<u64, FsError> {
        self.inner.device_id(dir)
    }
    fn try_lock_exclusive(&self, root: &Self::Dir) -> Result<Self::Lock, FsError> {
        self.inner.try_lock_exclusive(root)
    }
    fn create_new(&self, dir: &Self::Dir, name: EntryName) -> Result<Self::WriteFile, FsError> {
        self.count(|m| m.create_new += 1);
        self.inner.create_new(dir, name)
    }
    fn write(&self, file: &mut Self::WriteFile, buf: &[u8]) -> Result<WriteOutcome, FsError> {
        self.count(|m| m.write += 1);
        self.inner.write(file, buf)
    }
    fn sync_file(&self, file: &Self::WriteFile) -> Result<(), FsError> {
        self.count(|m| m.sync_file += 1);
        self.inner.sync_file(file)
    }
    fn sync_entry(&self, dir: &Self::Dir, name: EntryName) -> Result<(), FsError> {
        self.count(|m| m.sync_entry += 1);
        self.inner.sync_entry(dir, name)
    }
    fn publish_no_overwrite(
        &self,
        staged_dir: &Self::Dir,
        staged: EntryName,
        dst_dir: &Self::Dir,
        dst: EntryName,
        expected: &HashReport,
        ledger: &mut dyn sage_db_types::ledger::SharedLedger,
    ) -> Result<Published, FsError> {
        self.count(|m| m.publish_no_overwrite += 1);
        self.inner
            .publish_no_overwrite(staged_dir, staged, dst_dir, dst, expected, ledger)
    }
    fn replace_current(
        &self,
        staged_dir: &Self::Dir,
        staged: EntryName,
        root: &Self::Dir,
    ) -> Result<(), FsError> {
        self.count(|m| m.replace_current += 1);
        self.inner.replace_current(staged_dir, staged, root)
    }
    fn size(&self, dir: &Self::Dir, name: EntryName) -> Result<u64, FsError> {
        self.inner.size(dir, name)
    }
    fn read_exact_into(
        &self,
        dir: &Self::Dir,
        name: EntryName,
        buf: &mut [u8],
    ) -> Result<usize, FsError> {
        self.inner.read_exact_into(dir, name, buf)
    }
    fn open_read(&self, dir: &Self::Dir, name: EntryName) -> Result<Self::ReadFile, FsError> {
        self.inner.open_read(dir, name)
    }
    fn read(&self, file: &mut Self::ReadFile, buf: &mut [u8]) -> Result<usize, FsError> {
        self.inner.read(file, buf)
    }
    fn hash_bounded(
        &self,
        dir: &Self::Dir,
        name: EntryName,
        max_len: u64,
        ledger: &mut dyn sage_db_types::ledger::SharedLedger,
    ) -> Result<HashReport, FsError> {
        self.inner.hash_bounded(dir, name, max_len, ledger)
    }
    fn for_each_entry(
        &self,
        dir: &Self::Dir,
        ledger: &mut dyn sage_db_types::ledger::SharedLedger,
        visit: &mut dyn FnMut(&str) -> Each,
    ) -> Result<(), FsError> {
        self.inner.for_each_entry(dir, ledger, visit)
    }
    fn remove(&self, dir: &Self::Dir, name: EntryName) -> Result<(), FsError> {
        self.count(|m| m.remove += 1);
        self.inner.remove(dir, name)
    }
}

// ---------------------------------------------------------------------------
// Fixtures (client-supplied bytes; no Store behaviour assumed)
// ---------------------------------------------------------------------------

fn str_value(s: &str) -> Vec<u8> {
    let mut out = (s.len() as u32).to_le_bytes().to_vec();
    out.extend_from_slice(s.as_bytes());
    out
}

fn term_body(id: u128) -> Vec<u8> {
    let mut out = b"opaque-term-body/".to_vec();
    out.extend_from_slice(&id.to_le_bytes());
    out
}

fn term_body_of(digest: &[u8; 32]) -> Vec<u8> {
    for id in 0u128..64 {
        let candidate = term_body(id);
        if raw_sha256(&candidate) == *digest {
            return candidate;
        }
    }
    panic!("a body_ref was emitted that this test cannot supply bytes for");
}

fn term_revision(
    id: u128,
    revision: u64,
    slug: &str,
    atom: &str,
    created_at: i64,
    updated_at: i64,
    body: &[u8],
) -> OwnedEvent {
    let mut key = 1u16.to_le_bytes().to_vec();
    key.extend_from_slice(&id.to_le_bytes());
    let mut fields: Vec<(u16, Vec<u8>)> = vec![
        (1, key),
        (2, revision.to_le_bytes().to_vec()),
        (4, 0u16.to_le_bytes().to_vec()),
        (6, 2u16.to_le_bytes().to_vec()),
        (8, created_at.to_le_bytes().to_vec()),
        (16, str_value(slug)),
        (17, str_value(atom)),
        (18, vec![1u8]),
        (19, {
            let mut v = 1u32.to_le_bytes().to_vec();
            v.extend_from_slice(&str_value("d"));
            v
        }),
        (20, 0u16.to_le_bytes().to_vec()),
        (21, 2u16.to_le_bytes().to_vec()),
        (22, {
            let mut v = 1u32.to_le_bytes().to_vec();
            v.extend_from_slice(&str_value("t"));
            v
        }),
        (23, raw_sha256(body).to_vec()),
    ];
    if revision > 1 {
        fields.push((9, updated_at.to_le_bytes().to_vec()));
    }
    fields.sort_by_key(|(tag, _)| *tag);
    let tag = if revision == 1 { 1 } else { 2 };
    OwnedEvent { tag, fields }
}

fn result_payload(version: u64, affected: u32) -> Vec<u8> {
    let mut out = b"opaque-result/".to_vec();
    out.extend_from_slice(&version.to_le_bytes());
    out.extend_from_slice(&affected.to_le_bytes());
    out
}

fn outcome_event(version: u64, principal: &str, request_id: &str) -> OwnedEvent {
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
        (178, [version as u8; 32].to_vec()),
        (179, 0u16.to_le_bytes().to_vec()),
        (180, raw_sha256(&result_payload(version, 2)).to_vec()),
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
            let body = term_body_of(&digest);
            declared.push(sage_db_types::prepared::BlobDeclaration {
                sha256: digest,
                declared_len: body.len() as u64,
                source: sage_db_types::prepared::BlobSource::Provide(body),
            });
        }
    }
    if carries_outcome {
        let payload = result_payload(version, 2);
        declared.push(sage_db_types::prepared::BlobDeclaration {
            sha256: raw_sha256(&payload),
            declared_len: payload.len() as u64,
            source: sage_db_types::prepared::BlobSource::Provide(payload),
        });
    }
    PreparedTransaction {
        version,
        prev_version: prev,
        tx_id: [0xA0 + version as u8; 16],
        kb_uuid: KbUuid::from_bytes([0x11; 16]),
        created_at: 1_700_000_000_000_000 + version as i64,
        principal_id: "system".to_owned(),
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

fn expected_entries(events: &[OwnedEvent]) -> Vec<StateEntry> {
    let mut entries = Vec::new();
    let control = RunToCompletion;
    let mut ledger = AttemptLedger::open(1 << 24, 1 << 20, &control).expect("ledger");
    for event in events {
        let fields: Vec<sage_db_format::event::Field<'_>> = event
            .fields
            .iter()
            .map(|(tag, value)| sage_db_format::event::Field::new(*tag, value.as_slice()))
            .collect();
        let outcome = project_event(event.tag, &fields, &mut entries, &mut ledger);
        assert!(
            matches!(outcome, FormatOutcome::Ok(_)),
            "projection: {outcome:?}"
        );
    }
    entries.sort_by(|a, b| (a.kind, &a.key).cmp(&(b.kind, &b.key)));
    entries
}

fn boot() -> (
    Store<ObservedFs>,
    Rc<ModelFileSystem>,
    Rc<RefCell<Mutations>>,
    PathBuf,
) {
    let observed = ObservedFs::new();
    let model = observed.model_handle();
    let mutations = Rc::clone(&observed.mutations);
    let dir = std::env::temp_dir().join(format!(
        "t83-observe-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let control = RunToCompletion;
    let mut ledger = AttemptLedger::open(1 << 24, 1 << 20, &control).expect("ledger");
    let store = bootstrap(
        observed,
        &dir,
        CreateDir::IfMissing,
        KbUuid::from_bytes([0x11; 16]),
        RULES,
        &Limits::V1,
        &mut ledger,
    )
    .unwrap_or_else(|e| panic!("bootstrap: {e:?}"));
    (store, model, mutations, dir)
}

/// Read a durable file's bytes out of the shared model, by relative path string.
///
/// `crash_states()` is the model's own path->bytes view. Every reachable state is searched and the file
/// must be present with the SAME bytes in all of them, otherwise `None` is returned rather than picking a
/// convenient one.
fn durable_bytes_of(model: &ModelFileSystem, rel: &str) -> Option<Vec<u8>> {
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
            _ => return None,
        }
    }
    found
}

/// Every durable file name, for a name-set comparison.
fn durable_names(model: &ModelFileSystem) -> Vec<String> {
    let view = model.model();
    let states = view.crash_states();
    let mut names: Vec<String> = states
        .states
        .iter()
        .flat_map(|s| s.files.keys().map(|p| p.as_str().to_owned()))
        .collect();
    names.sort();
    names.dedup();
    names
}

// ---------------------------------------------------------------------------
// Controls
// ---------------------------------------------------------------------------

/// CONTROL A: the pin taken AFTER tx2 (A1+B) keeps version 2 and its exact A1/B bytes across tx3(A2).
///
/// This is the strengthened baseline: the old pin holds an updated-soon record, so it exercises the case a
/// Genesis-only pin cannot — an entry whose successor is later committed must not be mutated retroactively.
#[test]
fn pin_after_tx2_is_immutable_across_a_later_update() {
    let (mut store, _model, _mutations, dir) = boot();
    let control = RunToCompletion;
    let mut ledger = AttemptLedger::open(1 << 24, 1 << 20, &control).expect("ledger");

    let a1 = term_revision(1, 1, "s1", "atom1", 1, 1, &term_body(1));
    let b = term_revision(2, 1, "s2", "atom2", 2, 2, &term_body(2));
    let a2 = term_revision(1, 2, "s1-updated", "atom1", 1, 9, &term_body(1));

    let mut tx2_events = vec![a1.clone(), b.clone()];
    tx2_events.push(outcome_event(2, "system", "req-2"));
    let tx2 = store.commit(
        carrier(2, Some(1), "req-2", tx2_events),
        &Limits::V1,
        &mut ledger,
    );
    assert!(
        matches!(tx2, HandleOutcome::Committed { version: 2, .. }),
        "tx2: {tx2:?}"
    );

    // The pin exists only after a commit; take it now, at version 2, holding A1 and B.
    let pin = store.published_pin().expect("a pin after tx2");
    assert_eq!(pin.head(), KbVersion::new(2), "pin identifies version 2");
    let pinned: Vec<(u16, Vec<u8>, u64, Vec<u8>)> = pin
        .entries()
        .iter()
        .map(|e| (e.kind, e.key.clone(), e.revision, e.body.clone()))
        .collect();
    assert!(
        pinned.iter().any(|(k, _, r, _)| *k == 1 && *r == 1),
        "the pin holds A at revision 1"
    );

    let mut tx3_events = vec![a2.clone()];
    tx3_events.push(outcome_event(3, "system", "req-3"));
    let tx3 = store.commit(
        carrier(3, Some(2), "req-3", tx3_events),
        &Limits::V1,
        &mut ledger,
    );
    assert!(
        matches!(tx3, HandleOutcome::Committed { version: 3, .. }),
        "tx3: {tx3:?}"
    );

    // The old pin is unchanged: same version, same entries, byte for byte.
    assert_eq!(pin.head(), KbVersion::new(2), "the old pin did not advance");
    let pinned_after: Vec<(u16, Vec<u8>, u64, Vec<u8>)> = pin
        .entries()
        .iter()
        .map(|e| (e.kind, e.key.clone(), e.revision, e.body.clone()))
        .collect();
    assert_eq!(pinned_after, pinned, "the old pin's entry set is immutable");

    // And the pin's A entry still carries the ORIGINAL revision-1 body, not tx3's revision-2 body.
    let a1_expected = expected_entries(std::slice::from_ref(&a1));
    let a1_body = a1_expected[0].body.clone();
    assert!(
        pinned
            .iter()
            .any(|(k, _, r, body)| *k == 1 && *r == 1 && *body == a1_body),
        "the pinned A entry must still be this test's A1 bytes"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Read one durable file, requiring it to be identical in every reachable crash state.
fn durable_file(model: &ModelFileSystem, rel: &str) -> Vec<u8> {
    durable_bytes_of(model, rel)
        .unwrap_or_else(|| panic!("durable file {rel} is absent or unstable"))
}

/// Walk a transaction file's events, returning each event's (tag, raw field bytes).
///
/// Uses the real `read_envelope` for framing and the real `read_event` / `read_header` for the field
/// stream: no hand-parsed header.
/// Read a transaction through the REAL reader path, returning its validated header and raw events.
///
/// Order, and each step is a real call rather than a re-parse:
///
/// 1. `read_transaction(bytes, .., None, &mut DiscardEvents, ..)` must return `Ok`; that validates the
///    framing, the header cross-checks and every event against the dispatch seam, and gives the
///    authoritative `event_count`;
/// 2. `read_envelope` supplies the body;
/// 3. `event::read_header` consumes the `tx_header` field table (a real call, not a hand loop);
/// 4. exactly `event_count` events are read with `event::read_event`, then `reader.remaining()` must be 0.
fn read_tx(
    bytes: &[u8],
    ledger: &mut dyn sage_db_types::ledger::SharedLedger,
) -> (
    sage_db_format::transaction::TransactionHeader,
    Vec<OwnedFieldEvent>,
) {
    use sage_db_format::event::{read_event, read_header};
    use sage_db_format::transaction::{DiscardEvents, read_transaction};
    use sage_db_format::wire::Reader;

    // 1. The real reader must accept the whole file.
    let summary = match read_transaction(bytes, &Limits::V1, None, &mut DiscardEvents, ledger) {
        FormatOutcome::Ok(s) => s,
        FormatOutcome::Format(e) => panic!("read_transaction format error: {e:?}"),
        FormatOutcome::Refused(r) => panic!("read_transaction refused: {r:?}"),
    };

    // 2. The body, then the header table via its own reader.
    let envelope = match sage_db_format::transaction::read_envelope(bytes, &Limits::V1, ledger) {
        FormatOutcome::Ok(e) => e,
        FormatOutcome::Format(e) => panic!("envelope format error: {e:?}"),
        FormatOutcome::Refused(r) => panic!("envelope refused: {r:?}"),
    };
    let mut reader = Reader::new(&envelope.body);
    let header_fields = read_header(&mut reader, ledger).expect("tx_header field table");
    assert!(!header_fields.is_empty(), "a transaction header has fields");

    // 3. Exactly the count the validated header states, leaving nothing behind.
    let mut out = Vec::new();
    for _ in 0..summary.header.event_count {
        let event = read_event(&mut reader, ledger).expect("event envelope");
        out.push((
            event.tag(),
            event
                .fields()
                .iter()
                .map(|f| (f.tag(), f.value().to_vec()))
                .collect(),
        ));
    }
    assert_eq!(
        reader.remaining(),
        0,
        "the event stream must consume the body exactly"
    );
    (summary.header, out)
}

/// One event as it appears in a decoded transaction: its tag and its raw field table.
///
/// Named so the reader/projection signatures stay legible; the shape itself is the wire form.
type OwnedFieldEvent = (u16, Vec<(u16, Vec<u8>)>);

/// Project persisted raw fields exactly as the store does, then digest them.
fn projected_entries(events: &[OwnedFieldEvent]) -> Vec<StateEntry> {
    let control = RunToCompletion;
    let mut ledger = AttemptLedger::open(1 << 24, 1 << 20, &control).expect("ledger");
    let mut entries = Vec::new();
    for (tag, raw) in events {
        let fields: Vec<sage_db_format::event::Field<'_>> = raw
            .iter()
            .map(|(t, v)| sage_db_format::event::Field::new(*t, v.as_slice()))
            .collect();
        let outcome = project_event(*tag, &fields, &mut entries, &mut ledger);
        assert!(
            matches!(outcome, FormatOutcome::Ok(_)),
            "projection: {outcome:?}"
        );
    }
    entries.sort_by(|a, b| (a.kind, &a.key).cmp(&(b.kind, &b.key)));
    entries
}

/// CONTROL B: the persisted `state_digest` equals the digest of the CURRENT projection, where the Genesis
/// contribution is READ BACK FROM DISK, not imported from the pin under test.
#[test]
fn persisted_state_digest_matches_the_current_projection() {
    let (mut store, model, _mutations, dir) = boot();
    let control = RunToCompletion;
    let mut ledger = AttemptLedger::open(1 << 24, 1 << 20, &control).expect("ledger");

    let a1 = term_revision(1, 1, "s1", "atom1", 1, 1, &term_body(1));
    let b = term_revision(2, 1, "s2", "atom2", 2, 2, &term_body(2));
    let a2 = term_revision(1, 2, "s1-updated", "atom1", 1, 9, &term_body(1));

    let mut e2 = vec![a1.clone(), b.clone()];
    e2.push(outcome_event(2, "system", "req-2"));
    assert!(matches!(
        store.commit(carrier(2, Some(1), "req-2", e2), &Limits::V1, &mut ledger),
        HandleOutcome::Committed { .. }
    ));
    let mut e3 = vec![a2.clone()];
    e3.push(outcome_event(3, "system", "req-3"));
    assert!(matches!(
        store.commit(carrier(3, Some(2), "req-3", e3), &Limits::V1, &mut ledger),
        HandleOutcome::Committed { .. }
    ));
    assert!(store.coverage_complete(), "coverage after ordinary commits");

    // CURRENT -> manifest -> transaction, all from durable bytes.
    let current_bytes = durable_file(&model, "CURRENT");
    let mut ledger = AttemptLedger::open(1 << 24, 1 << 20, &control).expect("ledger");
    let current = match decode_current(&current_bytes, &Limits::V1, &mut ledger) {
        FormatOutcome::Ok(c) => c,
        other => panic!("CURRENT: {other:?}"),
    };
    assert_eq!(current.version, 3, "CURRENT names the newest version");

    let manifest_rel = format!("manifests/{}", current.manifest_name());
    let manifest_bytes = durable_file(&model, &manifest_rel);
    assert_eq!(
        raw_sha256(&manifest_bytes),
        current.manifest_sha256,
        "CURRENT must name the manifest by its real file hash"
    );
    let mut ledger = AttemptLedger::open(1 << 24, 1 << 20, &control).expect("ledger");
    let manifest = match read_manifest(&manifest_bytes, &Limits::V1, &mut ledger) {
        FormatOutcome::Ok(m) => m,
        other => panic!("manifest: {other:?}"),
    };

    // CURRENT's version must be the version the manifest states.
    assert_eq!(
        current.version, manifest.header.version,
        "CURRENT version must equal the named manifest's version"
    );

    // The transaction this manifest names: verify the file hash join, then walk the chain back to Genesis.
    let tx_rel = format!("transactions/{}", manifest.header.tx_file_name);
    let tx_bytes = durable_file(&model, &tx_rel);
    assert_eq!(
        raw_sha256(&tx_bytes),
        manifest.header.tx_file_sha256,
        "the manifest must name the transaction by its real file hash"
    );

    // The LATEST transaction goes through the same real reader and joins as every ancestor: a raw hash
    // comparison alone would leave its decoded header unchecked.
    {
        let mut l = AttemptLedger::open(1 << 24, 1 << 20, &control).expect("ledger");
        let (latest, _events) = read_tx(&tx_bytes, &mut l);
        assert_eq!(
            latest.kb_uuid, manifest.header.kb_uuid,
            "the latest tx KB must match its manifest"
        );
        assert_eq!(
            latest.version, manifest.header.version,
            "the latest tx version must match its manifest"
        );
        assert_eq!(
            latest.rules_digest, manifest.header.rules_digest,
            "the latest tx rules digest must match its manifest"
        );
        assert_eq!(
            latest.rules_profile, manifest.header.rules_profile,
            "the latest tx rules profile must match its manifest"
        );
        let rendered = sage_db_store::names::EntryName::Transaction(
            sage_db_store::names::Id128::from_bytes(latest.tx_id),
        )
        .to_file_name();
        assert_eq!(
            rendered, manifest.header.tx_file_name,
            "the latest tx_id must render as the name the manifest used"
        );
    }

    // CURRENT and the manifest must agree about the KB as well as the version.
    assert_eq!(
        current.kb_uuid, manifest.header.kb_uuid,
        "CURRENT kb_uuid must equal the manifest's"
    );

    // Genesis transaction, found by following prev_manifest_name back to version 1.
    let mut genesis_tx: Option<Vec<u8>> = None;
    let mut cursor_name: Option<String> = manifest.header.prev_manifest_name.map(str::to_owned);
    // Seeded from the CURRENT manifest's own recorded predecessor hash, so the FIRST ancestor is
    // hash-joined too. Starting at `None` silently skipped that first comparison.
    let mut expected_prev_sha: Option<[u8; 32]> = manifest.header.prev_manifest_sha256;
    let mut guard = 0;
    while let Some(name) = cursor_name {
        guard += 1;
        assert!(guard < 16, "manifest chain is bounded");
        let rel = format!("manifests/{name}");
        let bytes = durable_file(&model, &rel);
        // Bind everything needed out of the borrow before it ends, including every field the joins
        // below compare against.
        let (
            tx_name,
            is_genesis,
            next_name,
            m_tx_sha,
            m_kb_uuid,
            m_version,
            m_rules_digest,
            m_rules_profile,
            m_sha,
        ) = {
            let mut ledger = AttemptLedger::open(1 << 24, 1 << 20, &control).expect("ledger");
            match read_manifest(&bytes, &Limits::V1, &mut ledger) {
                FormatOutcome::Ok(m) => (
                    m.header.tx_file_name.to_owned(),
                    m.header.version == 1,
                    m.header.prev_manifest_name.map(str::to_owned),
                    m.header.tx_file_sha256,
                    m.header.kb_uuid,
                    m.header.version,
                    m.header.rules_digest,
                    m.header.rules_profile,
                    // The hash THIS manifest claims for its own predecessor.
                    m.header.prev_manifest_sha256,
                ),
                FormatOutcome::Format(e) => panic!("ancestor manifest: {e:?}"),
                FormatOutcome::Refused(r) => panic!("ancestor manifest refused: {r:?}"),
            }
        };

        // The manifest we came from already named THIS file and recorded its hash. Comparing the two
        // closes the ancestry by hash instead of by name: `expected_prev_sha` was set from the child
        // manifest's `prev_manifest_sha256`, and `raw_sha256(&bytes)` is this ancestor's real file hash.
        if let Some(expected) = expected_prev_sha {
            assert_eq!(
                raw_sha256(&bytes),
                expected,
                "manifest ancestry must be joined by hash, not by name alone"
            );
        }

        let txb = durable_file(&model, &format!("transactions/{tx_name}"));

        // Ancestry joins, checked at every hop rather than trusting names:
        //  * the ancestor's transaction file hash must equal the manifest's recorded tx_file_sha256;
        //  * its decoded header must agree with the manifest about KB, version and rules;
        //  * its tx_id, rendered by the real EntryName renderer, must be the name the manifest used.
        assert_eq!(
            raw_sha256(&txb),
            m_tx_sha,
            "the ancestor transaction file must match its manifest's tx_file_sha256"
        );
        {
            let mut l = AttemptLedger::open(1 << 24, 1 << 20, &control).expect("ledger");
            let (h, _) = read_tx(&txb, &mut l);
            assert_eq!(
                h.kb_uuid, m_kb_uuid,
                "ancestor tx KB must match its manifest"
            );
            assert_eq!(
                h.version, m_version,
                "ancestor tx version must match its manifest"
            );
            assert_eq!(
                h.rules_digest, m_rules_digest,
                "ancestor tx rules digest must match its manifest"
            );
            assert_eq!(h.rules_profile, m_rules_profile, "rules profile must match");
            let rendered = sage_db_store::names::EntryName::Transaction(
                sage_db_store::names::Id128::from_bytes(h.tx_id),
            )
            .to_file_name();
            assert_eq!(
                rendered, tx_name,
                "the decoded tx_id must render as the name the manifest used"
            );
        }

        if is_genesis {
            genesis_tx = Some(txb);
            break;
        }
        cursor_name = next_name;
        // Carry down what THIS manifest claims for its own predecessor.
        expected_prev_sha = m_sha;
    }
    let genesis_tx = genesis_tx.expect("Genesis transaction must be reachable");

    // Expected CURRENT projection: REAL persisted Genesis fields + this test's A2/B bytes.
    let mut ledger = AttemptLedger::open(1 << 24, 1 << 20, &control).expect("ledger");
    let (genesis_header, genesis_events) = read_tx(&genesis_tx, &mut ledger);
    assert_eq!(genesis_header.version, 1, "the ancestor is Genesis");
    assert_eq!(genesis_header.prev_version, 0, "Genesis has no predecessor");
    let mut expected = projected_entries(&genesis_events);
    expected.extend(expected_entries(std::slice::from_ref(&a2)));
    expected.extend(expected_entries(std::slice::from_ref(&b)));
    expected.sort_by(|x, y| (x.kind, &x.key).cmp(&(y.kind, &y.key)));

    let mut ledger = AttemptLedger::open(1 << 24, 1 << 20, &control).expect("ledger");
    let set = RecordSet::from_sorted(expected.clone(), &mut ledger).expect("sorted set");
    let recomputed = sage_db_format::state::state_digest(&set, &mut ledger).expect("digest");

    assert_eq!(
        manifest.header.state_digest, recomputed,
        "the persisted state_digest must equal the digest of the projection rebuilt from disk"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// CONTROL C: an exact retry mutates nothing and returns the original recorded outcome.
#[test]
fn exact_retry_has_no_seam_side_effects_and_returns_the_original_outcome() {
    let (mut store, model, mutations, dir) = boot();
    let control = RunToCompletion;
    let mut ledger = AttemptLedger::open(1 << 24, 1 << 20, &control).expect("ledger");
    let a1 = term_revision(1, 1, "s1", "atom1", 1, 1, &term_body(1));
    let build = || {
        let mut events = vec![a1.clone()];
        events.push(outcome_event(2, "system", "req-2"));
        carrier(2, Some(1), "req-2", events)
    };

    let first = store.commit(build(), &Limits::V1, &mut ledger);
    let recorded_first = match first {
        HandleOutcome::Committed { recorded, .. } => recorded,
        other => panic!("first commit: {other:?}"),
    };

    let before_names = durable_names(&model);
    let before_current = durable_bytes_of(&model, "CURRENT").expect("CURRENT");
    // Invocation counts on the SAME observer, taken before and after the retry.
    let before_mutations = *mutations.borrow();

    let retry = store.commit(build(), &Limits::V1, &mut ledger);
    let recorded_retry = match retry {
        HandleOutcome::AlreadyCommitted { version, recorded } => {
            assert_eq!(version, 2, "the original version is reported");
            recorded
        }
        other => panic!("retry must be AlreadyCommitted: {other:?}"),
    };

    // 1. The returned outcome equals the originally committed one, field by field.
    match (&recorded_first, &recorded_retry) {
        (RecordedOutcome::Outcome(a), RecordedOutcome::Outcome(b)) => {
            assert_eq!(a, b, "the retry must return the ORIGINAL recorded outcome");
        }
        (RecordedOutcome::GenesisNone, RecordedOutcome::GenesisNone) => {
            panic!("this transaction carries a 192, so it cannot be GenesisNone")
        }
        other => panic!("recorded outcome kinds differ: {other:?}"),
    }

    // 2. The returned outcome equals the 192 DECODED FROM THE PERSISTED TRANSACTION, not merely the
    //    first call's in-memory value.
    let current_bytes = durable_bytes_of(&model, "CURRENT").expect("CURRENT");
    let mut ledger2 = AttemptLedger::open(1 << 24, 1 << 20, &control).expect("ledger");
    let cur = match decode_current(&current_bytes, &Limits::V1, &mut ledger2) {
        FormatOutcome::Ok(c) => c,
        other => panic!("CURRENT: {other:?}"),
    };
    let mbytes = {
        let rel = format!("manifests/{}", cur.manifest_name());
        durable_bytes_of(&model, &rel).expect("durable manifest")
    };
    let mut ledger2 = AttemptLedger::open(1 << 24, 1 << 20, &control).expect("ledger");
    let m = match read_manifest(&mbytes, &Limits::V1, &mut ledger2) {
        FormatOutcome::Ok(m) => m,
        other => panic!("manifest: {other:?}"),
    };
    assert_eq!(
        raw_sha256(&mbytes),
        cur.manifest_sha256,
        "CURRENT must name the manifest by its real file hash"
    );
    assert_eq!(
        cur.version, m.header.version,
        "CURRENT version must equal the manifest's"
    );
    let txb = {
        let rel = format!("transactions/{}", m.header.tx_file_name);
        durable_file(&model, &rel)
    };
    assert_eq!(
        raw_sha256(&txb),
        m.header.tx_file_sha256,
        "the manifest must name the transaction by its real file hash"
    );
    let mut ledger2 = AttemptLedger::open(1 << 24, 1 << 20, &control).expect("ledger");
    let (persisted_header, persisted) = read_tx(&txb, &mut ledger2);
    // The persisted transaction's header must agree with the manifest that named it.
    assert_eq!(
        persisted_header.kb_uuid, m.header.kb_uuid,
        "tx KB must match its manifest"
    );
    assert_eq!(
        persisted_header.version, m.header.version,
        "tx version must match"
    );
    assert_eq!(
        persisted_header.rules_digest, m.header.rules_digest,
        "tx rules digest must match"
    );
    assert_eq!(
        persisted_header.rules_profile, m.header.rules_profile,
        "tx rules profile must match"
    );
    let rendered_tx = sage_db_store::names::EntryName::Transaction(
        sage_db_store::names::Id128::from_bytes(persisted_header.tx_id),
    )
    .to_file_name();
    assert_eq!(
        rendered_tx, m.header.tx_file_name,
        "the decoded tx_id must render as the name the manifest used"
    );
    let persisted_192 = persisted
        .iter()
        .find(|(tag, _)| *tag == 192)
        .map(|(_, fields)| fields.clone())
        .expect("the persisted transaction carries a 192");
    // Decode the persisted 192 through the real reader.
    let mut ledger2 = AttemptLedger::open(1 << 24, 1 << 20, &control).expect("ledger");
    let decoded = {
        let owned = OwnedEvent {
            tag: 192,
            fields: persisted_192.clone(),
        };
        match sage_db_format::prepared::decode_owned_event(&owned, &Limits::V1, &mut ledger2) {
            FormatOutcome::Ok(sage_db_format::dispatch::DispatchedPayload::RequestOutcome(o)) => o,
            other => panic!("persisted 192 did not decode as an outcome: {other:?}"),
        }
    };
    match &recorded_retry {
        RecordedOutcome::Outcome(shared) => {
            // `RecordedOutcome::Outcome` now carries the public opaque `SharedOutcome`; `event()` is its
            // immutable accessor. The comparison itself is unchanged.
            assert_eq!(
                shared.event(),
                &decoded,
                "the retry's recorded outcome must equal the persisted 192"
            );
        }
        RecordedOutcome::GenesisNone => panic!("this transaction records an outcome"),
    }

    // 3. NO mutating seam call happened during the retry. This is an invocation count taken before
    //    delegation, so a call that failed or reported Unknown would also have been counted.
    let after_mutations = *mutations.borrow();
    // A vacuous equality would also pass if the counters never moved at all, so the first commit's
    // totals are asserted non-zero here: the observer really does see mutating calls.
    assert!(
        before_mutations.create_new > 0
            && before_mutations.write > 0
            && before_mutations.replace_current > 0
            && before_mutations.sync_dir > 0,
        "the observer must have counted real mutating calls: {before_mutations:?}"
    );
    assert_eq!(
        after_mutations, before_mutations,
        "an exact retry must issue no mutating seam call; before {before_mutations:?} after {after_mutations:?}"
    );

    // 4. Nothing durable changed: same file name set, same CURRENT bytes.
    assert_eq!(
        durable_names(&model),
        before_names,
        "a retry must not add or remove files"
    );
    assert_eq!(
        durable_bytes_of(&model, "CURRENT").expect("CURRENT"),
        before_current,
        "a retry must not rewrite CURRENT"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn root_t87_new_identity_must_start_at_revision_one() {
    let (mut store, _model, counts, _dir) = boot();
    let control = RunToCompletion;
    let mut ledger = AttemptLedger::open(1 << 24, 1 << 20, &control).unwrap();
    let first = term_revision(1, 1, "first", "first atom", 1, 1, &term_body(1));
    let positive = store.commit(
        carrier(
            2,
            Some(1),
            "positive",
            vec![first, outcome_event(2, "system", "positive")],
        ),
        &Limits::V1,
        &mut ledger,
    );
    println!("REVISION/positive={positive:?}");
    assert!(matches!(
        positive,
        HandleOutcome::Committed { version: 2, .. }
    ));
    let before = *counts.borrow();
    let skipped = term_revision(2, 2, "new-second", "second atom", 1, 2, &term_body(2));
    let result = store.commit(
        carrier(
            3,
            Some(2),
            "invalid-creation",
            vec![skipped, outcome_event(3, "system", "invalid-creation")],
        ),
        &Limits::V1,
        &mut ledger,
    );
    println!(
        "REVISION/new identity revision2 result={result:?}; head={:?}; mutation_calls_unchanged={}",
        store.head(),
        before == *counts.borrow()
    );
    // Exact typed refusal, not merely "some NotCommitted": a creation must be revision 1, and the fold
    // reports precisely which identity broke continuity and what it expected.
    match &result {
        HandleOutcome::NotCommitted {
            reason:
                HandleRefusal::Derivation(CommitRefusal::Fold(FoldDefect::RevisionDiscontinuity {
                    expected,
                    found,
                    ..
                })),
        } => {
            assert_eq!(*expected, Some(1), "a creation is expected at revision 1");
            assert_eq!(*found, 2, "the candidate presented revision 2");
        }
        other => panic!("expected a typed RevisionDiscontinuity refusal, got {other:?}"),
    }
    assert_eq!(
        store.head(),
        Some(2),
        "the refused candidate must not advance head"
    );
    assert_eq!(
        before,
        *counts.borrow(),
        "revision refusal must precede mutating seam calls"
    );
}

#[test]
fn root_guard_order_keeps_stored_retry_ahead_of_budget_and_invalid_candidate_checks() {
    use sage_db_store::handle::HandleRefusal;
    let (mut store, _model, counts, _dir) = boot();
    let control = RunToCompletion;
    let mut ledger = AttemptLedger::open(1 << 24, 1 << 20, &control).unwrap();
    let make = || {
        carrier(
            2,
            Some(1),
            "original",
            vec![outcome_event(2, "system", "original")],
        )
    };
    let original = match store.commit(make(), &Limits::V1, &mut ledger) {
        HandleOutcome::Committed {
            version: 2,
            recorded,
        } => recorded,
        other => panic!("original must really commit: {other:?}"),
    };
    let before = *counts.borrow();
    let mut zero = AttemptLedger::open(0, 0, &control).unwrap();
    let mut retry = make();
    retry.version = 99;
    retry.prev_version = Some(98);
    retry.expect_version = Some(98);
    retry.events.clear(); // Later validation is intentionally invalid; stored idempotency comes first.
    let reply = store.commit(retry, &Limits::V1, &mut zero);
    println!("GUARD/stale-invalid retry with zero allowance: {reply:?}");
    match reply {
        HandleOutcome::AlreadyCommitted {
            version: 2,
            recorded,
        } => assert_eq!(recorded, original),
        other => panic!("stored result must beat later guards and zero allowance: {other:?}"),
    }
    assert_eq!(zero.stats().admitted_bytes, Some(0));
    assert_eq!(zero.stats().admitted_entries, Some(0));
    assert_eq!(before, *counts.borrow());
    let mut reused = make();
    reused.payload_digest = [0x44; 32];
    reused.events.clear();
    assert!(matches!(
        store.commit(reused, &Limits::V1, &mut zero),
        HandleOutcome::NotCommitted {
            reason: HandleRefusal::IdempotencyMismatch
        }
    ));
    let mut stale = carrier(3, Some(2), "new-stale", Vec::new());
    stale.expect_version = Some(1);
    let result = store.commit(stale, &Limits::V1, &mut zero);
    println!("GUARD/expect before invalid format or budget: {result:?}");
    assert!(matches!(
        result,
        HandleOutcome::NotCommitted {
            reason: HandleRefusal::ExpectVersionMismatch { .. }
        }
    ));
    let mut wrong_version = carrier(9, Some(8), "new-version", Vec::new());
    wrong_version.expect_version = Some(2);
    let result = store.commit(wrong_version, &Limits::V1, &mut zero);
    println!("GUARD/version before invalid format or budget: {result:?}");
    assert!(matches!(
        result,
        HandleOutcome::NotCommitted {
            reason: HandleRefusal::VersionConflict { .. }
        }
    ));
    assert_eq!(before, *counts.borrow());
    assert_eq!(store.head(), Some(2));
    assert_eq!(zero.stats().admitted_bytes, Some(0));
    assert_eq!(zero.stats().admitted_entries, Some(0));
}

#[test]
fn committed_chain_uses_tx_domain_over_the_actual_body() {
    use sage_db_format::digest::{Domain, chain_digest, digest};
    let (mut store, model, _counts, _dir) = boot();
    let control = RunToCompletion;
    let mut ledger = AttemptLedger::open(1 << 26, 1 << 22, &control).unwrap();
    for version in 2..=3 {
        let request = if version == 2 {
            "chain-two"
        } else {
            "chain-three"
        };
        let tx = carrier(
            version,
            Some(version - 1),
            request,
            vec![outcome_event(version, "system", request)],
        );
        assert!(matches!(store.commit(tx, &Limits::V1, &mut ledger),
            HandleOutcome::Committed { version: found, .. } if found == version));
    }
    let current_bytes = durable_file(&model, "CURRENT");
    let current = match decode_current(&current_bytes, &Limits::V1, &mut ledger) {
        FormatOutcome::Ok(value) => value,
        other => panic!("actual CURRENT: {other:?}"),
    };
    let mut name = current.manifest_name().to_owned();
    let mut expected_file_hash = current.manifest_sha256;
    for version in (1..=3).rev() {
        let manifest_bytes = durable_file(&model, &format!("manifests/{name}"));
        assert_eq!(raw_sha256(&manifest_bytes), expected_file_hash);
        let manifest = match read_manifest(&manifest_bytes, &Limits::V1, &mut ledger) {
            FormatOutcome::Ok(value) => value,
            other => panic!("actual manifest: {other:?}"),
        };
        assert_eq!(manifest.header.version, version);
        let previous_chain = if let Some(previous_name) = manifest.header.prev_manifest_name {
            let previous_bytes = durable_file(&model, &format!("manifests/{previous_name}"));
            assert_eq!(
                Some(raw_sha256(&previous_bytes)),
                manifest.header.prev_manifest_sha256
            );
            match read_manifest(&previous_bytes, &Limits::V1, &mut ledger) {
                FormatOutcome::Ok(previous) => previous.header.chain_digest,
                other => panic!("actual predecessor: {other:?}"),
            }
        } else {
            assert_eq!(version, 1);
            [0; 32]
        };
        let tx_bytes = durable_file(
            &model,
            &format!("transactions/{}", manifest.header.tx_file_name),
        );
        let tx_file_sha = raw_sha256(&tx_bytes);
        assert_eq!(tx_file_sha, manifest.header.tx_file_sha256);
        let envelope =
            match sage_db_format::transaction::read_envelope(&tx_bytes, &Limits::V1, &mut ledger) {
                FormatOutcome::Ok(value) => value,
                FormatOutcome::Format(error) => panic!("actual tx envelope format: {error:?}"),
                FormatOutcome::Refused(refusal) => {
                    panic!("actual tx envelope refusal: {refusal:?}")
                }
            };
        let normative_tx_digest = digest(Domain::Tx, &envelope.body);
        assert_ne!(
            normative_tx_digest, tx_file_sha,
            "domain/body and raw/file must discriminate"
        );
        assert_ne!(
            normative_tx_digest,
            digest(Domain::Tx, &tx_bytes),
            "adding the domain alone cannot repair the wrong byte scope"
        );
        assert_eq!(
            manifest.header.chain_digest,
            chain_digest(previous_chain, normative_tx_digest)
        );
        assert_ne!(
            manifest.header.chain_digest,
            chain_digest(previous_chain, tx_file_sha)
        );
        if let Some(previous_name) = manifest.header.prev_manifest_name {
            name = previous_name.to_owned();
            expected_file_hash = manifest.header.prev_manifest_sha256.unwrap();
        }
    }
}
