#![allow(dead_code, unused_imports)]
//! ROOT P1-5 history regressions on real persisted bytes and the same model backend.
//! Fixture/observer helpers derive from t63's sealed 298c3eef source. Expected source versions and event bounds
//! come from this test's own legal commit sequence. The lifetime check compares a previously returned value as
//! a preservation baseline, not an independent codec golden. Read-call counters cover the named seam methods;
//! reservation statistics are not allocator/RSS measurements.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use sage_db_format::current::decode_current;
use sage_db_format::digest::raw_sha256;
use sage_db_format::manifest::read_manifest;
use sage_db_format::projection::project_event;
use sage_db_store::fs::{
    Capabilities, CreateDir, Each, Family, FileSystem, FsError, HashReport, Published, WriteOutcome,
};
use sage_db_store::init::bootstrap;
use sage_db_store::lease::AttemptLedger;
use sage_db_store::names::EntryName;
use sage_db_store::open::{Opened, open_existing};
use sage_db_store::view::ViewDurability;
use sage_db_store_tests::model::fs_model::{ModelCapabilities, ModelFileSystem};
use sage_db_types::framing::{FormatOutcome, Limits};
use sage_db_types::identity::{KbUuid, KbVersion};
use sage_db_types::ledger::SharedLedger;
use sage_db_types::persist::StateEntry;
use sage_db_types::prepared::{OwnedEvent, PreparedTransaction, RulesBinding};
use sage_db_types::query::RunToCompletion;
use sage_db_types::relation::RuleProfile;

const RULES: &[u8] = include_bytes!("../fixtures/rules.formal-v1.v1.bin");
const UUID: [u8; 16] = [0x11; 16];

// ---------------------------------------------------------------------------
// Same-instance observer: one ModelFileSystem, shared, forwarding every seam call
// ---------------------------------------------------------------------------

#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
struct Mutations {
    file_reads: u64,
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

struct Observed {
    inner: Rc<ModelFileSystem>,
    mutations: Rc<RefCell<Mutations>>,
}

impl Clone for Observed {
    /// A second handle onto the SAME model and counters.
    ///
    /// `open_existing` takes the backend by value, so reopening on the same filesystem needs a second
    /// handle rather than a second filesystem: the `Rc` is what makes the reopen genuinely observe the
    /// bytes the first store wrote.
    fn clone(&self) -> Self {
        Self {
            inner: Rc::clone(&self.inner),
            mutations: Rc::clone(&self.mutations),
        }
    }
}

impl Observed {
    fn new() -> Self {
        Self {
            inner: Rc::new(ModelFileSystem::new(ModelCapabilities::full())),
            mutations: Rc::new(RefCell::new(Mutations::default())),
        }
    }

    fn model(&self) -> Rc<ModelFileSystem> {
        Rc::clone(&self.inner)
    }

    fn counters(&self) -> Rc<RefCell<Mutations>> {
        Rc::clone(&self.mutations)
    }

    fn count(&self, f: impl FnOnce(&mut Mutations)) {
        f(&mut self.mutations.borrow_mut());
    }
}

impl FileSystem for Observed {
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
        ledger: &mut dyn SharedLedger,
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
        self.count(|m| m.file_reads += 1);
        self.inner.read_exact_into(dir, name, buf)
    }
    fn open_read(&self, dir: &Self::Dir, name: EntryName) -> Result<Self::ReadFile, FsError> {
        self.count(|m| m.file_reads += 1);
        self.inner.open_read(dir, name)
    }
    fn read(&self, file: &mut Self::ReadFile, buf: &mut [u8]) -> Result<usize, FsError> {
        self.count(|m| m.file_reads += 1);
        self.inner.read(file, buf)
    }
    fn hash_bounded(
        &self,
        dir: &Self::Dir,
        name: EntryName,
        max_len: u64,
        ledger: &mut dyn SharedLedger,
    ) -> Result<HashReport, FsError> {
        self.count(|m| m.file_reads += 1);
        self.inner.hash_bounded(dir, name, max_len, ledger)
    }
    fn for_each_entry(
        &self,
        dir: &Self::Dir,
        ledger: &mut dyn SharedLedger,
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
// Fixtures: test-supplied bytes only
// ---------------------------------------------------------------------------

fn str_value(text: &str) -> Vec<u8> {
    let mut v = (text.len() as u32).to_le_bytes().to_vec();
    v.extend_from_slice(text.as_bytes());
    v
}

fn term_body(id: u128, revision: u64) -> Vec<u8> {
    let mut out = b"p15-independent-body/".to_vec();
    out.extend_from_slice(&id.to_le_bytes());
    out.extend_from_slice(&revision.to_le_bytes());
    out
}

fn term_body_of(digest: &[u8; 32]) -> Vec<u8> {
    for id in 0u128..8 {
        for revision in 1u64..4 {
            let candidate = term_body(id, revision);
            if raw_sha256(&candidate) == *digest {
                return candidate;
            }
        }
    }
    panic!("a body_ref was emitted that this test cannot supply bytes for");
}

/// A term at an explicit revision, with every optional field the accepted codec allows.
fn term_event(
    id: u128,
    revision: u64,
    slug: &str,
    atom: &str,
    times: (i64, i64),
    confidence: u16,
    extensions: &[(&str, &[u8])],
) -> OwnedEvent {
    let (created_at, updated_at) = times;
    let mut key = 1u16.to_le_bytes().to_vec();
    key.extend_from_slice(&id.to_le_bytes());
    let body = term_body(id, revision);
    let mut fields: Vec<(u16, Vec<u8>)> = vec![
        (1, key),
        (2, revision.to_le_bytes().to_vec()),
        (4, 0u16.to_le_bytes().to_vec()),
        // audit: recorded_by present with actor_origin authenticated, plus a reason.
        (5, str_value("p15-actor")),
        (6, 0u16.to_le_bytes().to_vec()),
        (7, str_value("independent control")),
        (8, created_at.to_le_bytes().to_vec()),
        (16, str_value(slug)),
        (17, str_value(atom)),
        (18, vec![2u8]),
        (19, {
            let mut v = 1u32.to_le_bytes().to_vec();
            v.extend_from_slice(&str_value("independent"));
            v
        }),
        (20, 1u16.to_le_bytes().to_vec()),
        (21, confidence.to_le_bytes().to_vec()),
        (22, {
            let mut v = 1u32.to_le_bytes().to_vec();
            v.extend_from_slice(&str_value("p15"));
            v
        }),
        (23, raw_sha256(&body).to_vec()),
    ];
    if revision > 1 {
        fields.push((9, updated_at.to_le_bytes().to_vec()));
    }
    if !extensions.is_empty() {
        let mut v = (extensions.len() as u32).to_le_bytes().to_vec();
        for (k, value) in extensions {
            v.extend_from_slice(&str_value(k));
            v.extend_from_slice(&(value.len() as u32).to_le_bytes());
            v.extend_from_slice(value);
        }
        fields.push((14, v));
    }
    fields.sort_by_key(|(tag, _)| *tag);
    let tag = if revision == 1 { 1 } else { 2 };
    OwnedEvent { tag, fields }
}

fn result_payload(version: u64, affected: u32) -> Vec<u8> {
    let mut out = b"p15-opaque-result/".to_vec();
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
        kb_uuid: KbUuid::from_bytes(UUID),
        created_at: 1_700_000_000_000_000 + version as i64,
        principal_id: "p15-actor".to_owned(),
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

/// Expected state entries, derived by the REAL projection over the bytes this test supplies.
fn expected_entries(events: &[OwnedEvent]) -> Vec<StateEntry> {
    let control = RunToCompletion;
    let mut ledger = AttemptLedger::open(1 << 24, 1 << 20, &control).expect("ledger");
    let mut entries = Vec::new();
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

// ---------------------------------------------------------------------------
// Durable read-back through the shared model (no product accessor)
// ---------------------------------------------------------------------------

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
    found.unwrap_or_else(|| panic!("durable file {rel} not found"))
}

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

/// Resolve CURRENT -> named manifest, verifying both hash joins, and return (version, manifest bytes).
fn read_root(view: &sage_db_store::view::TaggedView, model: &ModelFileSystem) -> (u64, Vec<u8>) {
    let current_bytes = durable_file(model, "CURRENT");
    let control = RunToCompletion;
    let mut ledger = AttemptLedger::open(1 << 24, 1 << 20, &control).expect("ledger");
    let current = match decode_current(&current_bytes, &Limits::V1, &mut ledger) {
        FormatOutcome::Ok(c) => c,
        other => panic!("CURRENT: {other:?}"),
    };
    // The tagged basis must name the SAME manifest file CURRENT names.
    assert_eq!(
        view.basis().manifest_sha256,
        current.manifest_sha256,
        "the view's basis must be the manifest file CURRENT names"
    );
    assert_eq!(
        view.basis().version,
        KbVersion::new(current.version),
        "the view's basis version must be CURRENT's version"
    );
    let manifest_rel = format!("manifests/{}", current.manifest_name());
    let manifest_bytes = durable_file(model, &manifest_rel);
    assert_eq!(
        raw_sha256(&manifest_bytes),
        current.manifest_sha256,
        "CURRENT must name the manifest by its real file hash"
    );
    (current.version, manifest_bytes)
}

// ---------------------------------------------------------------------------

// Root-only independent history probes. Fixture/observer prefix comes from t63 298c3eef;
// only actual read-call counters were added. No product input source is edited.
use sage_db_store::fold::RecordIdentity;
use sage_db_store::history::{HistoryBudget, HistoryOutcome};
use sage_db_types::framing::Refusal;
use sage_db_types::ledger::BudgetUnit;
use sage_db_types::persist::RecordKey;

fn history_fixture<'a>(
    control: &'a RunToCompletion,
) -> (
    sage_db_store::handle::Store<Observed>,
    Rc<RefCell<Mutations>>,
    AttemptLedger<'a>,
    Observed,
) {
    let observed = Observed::new();
    let second = observed.clone();
    let counters = observed.counters();
    let path = std::path::Path::new("/root-history-r7");
    let mut write_ledger =
        AttemptLedger::open(1 << 26, 1 << 20, control).expect("finite write ledger");
    let mut store = match bootstrap(
        observed,
        path,
        CreateDir::IfMissing,
        KbUuid::from_bytes(UUID),
        RULES,
        &Limits::V1,
        &mut write_ledger,
    ) {
        Ok(store) => store,
        Err(_) => panic!("real bootstrap must succeed"),
    };
    let mut events = vec![term_event(
        1,
        1,
        "root-term",
        "root-atom",
        (1, 1),
        2,
        &[("p15.note", b"root")],
    )];
    events.push(outcome_event(2, "p15-actor", "root-history-2"));
    assert!(
        matches!(
            store.commit(
                carrier(2, Some(1), "root-history-2", events),
                &Limits::V1,
                &mut write_ledger
            ),
            sage_db_store::handle::HandleOutcome::Committed { version: 2, .. }
        ),
        "legal prefix must really commit v2"
    );
    drop(store);
    drop(write_ledger);
    let mut read_ledger =
        AttemptLedger::open(1 << 26, 1 << 20, control).expect("fresh finite replay ledger");
    let observer = second.clone();
    let opened = match open_existing(
        second,
        path,
        Some(KbUuid::from_bytes(UUID)),
        &Limits::V1,
        &mut read_ledger,
    ) {
        Ok(opened) => opened,
        Err(_) => panic!("real public reopen must succeed before the history call"),
    };
    assert!(
        matches!(opened, Opened::Ready(_)),
        "healthy fixture must reopen Ready"
    );
    let reopened = opened.into_store();
    assert_eq!(reopened.head(), Some(2));
    (reopened, counters, read_ledger, observer)
}

#[test]
fn history_source_version_is_the_committed_transaction_version() {
    let control = RunToCompletion;
    let (mut store, _, _replay_ledger, _) = history_fixture(&control);
    let mut query = AttemptLedger::open(1 << 26, 1 << 20, &control).expect("finite query ledger");
    let identity = RecordIdentity::Identified {
        key: RecordKey::term(1),
    };
    match store.history(
        &identity,
        1,
        HistoryBudget::new(100),
        &Limits::V1,
        &mut query,
    ) {
        HistoryOutcome::Found(hit) => assert_eq!(
            hit.source_version, 2,
            "revision1 was committed in KB version2; record revision is not source version"
        ),
        _ => panic!("the actual persisted revision must be found"),
    }
}

#[test]
fn history_hit_verifies_actual_persisted_bytes_through_the_same_backend() {
    let control = RunToCompletion;
    let (mut store, counters, _replay_ledger, _) = history_fixture(&control);
    let mut query = AttemptLedger::open(1 << 26, 1 << 20, &control).expect("finite query ledger");
    let before = counters.borrow().file_reads;
    let identity = RecordIdentity::Identified {
        key: RecordKey::term(1),
    };
    assert!(
        matches!(
            store.history(
                &identity,
                1,
                HistoryBudget::new(100),
                &Limits::V1,
                &mut query
            ),
            HistoryOutcome::Found(_)
        ),
        "the persisted target must be found"
    );
    let after = counters.borrow().file_reads;
    assert!(
        after > before,
        "a RAM fold hit is not actual tx/blob verification: file read/hash calls before={before} after={after}"
    );
}

#[test]
fn explicit_history_scan_cap_does_not_fabricate_canonical_ledger_remaining() {
    let control = RunToCompletion;
    let (mut store, _, _replay_ledger, _) = history_fixture(&control);
    let mut query = AttemptLedger::open(1 << 26, 1 << 20, &control).expect("finite query ledger");
    let identity = RecordIdentity::Identified {
        key: RecordKey::term(1),
    };
    match store.history(&identity, 1, HistoryBudget::new(0), &Limits::V1, &mut query) {
        HistoryOutcome::Incomplete {
            reason: sage_db_store::history::HistoryIncomplete::EventLimit { limit },
            scanned_events,
        } => {
            assert_eq!(limit, 0);
            assert_eq!(scanned_events, 0);
            let stats = query.stats();
            assert_eq!(
                stats.admitted_entries,
                Some(0),
                "an explicit zero cap must not manufacture or consume ledger work"
            );
        }
        _ => panic!("a zero scan cap cannot claim Found/Absent"),
    }
}

#[test]
fn newest_first_event_boundaries_and_the_uncached_scanner_are_real() {
    use sage_db_store::history::HistoryIncomplete;
    let control = RunToCompletion;
    let (mut store, _, mut replay, _) = history_fixture(&control);
    let events = vec![
        term_event(
            1,
            2,
            "revised-term",
            "revised-atom",
            (1, 3),
            2,
            &[("p15.note", b"second")],
        ),
        outcome_event(3, "p15-actor", "root-history-3"),
    ];
    let mut next = carrier(3, Some(2), "root-history-3", events);
    next.created_at = -7; // valid signed tag-8 bits must survive the header/manifest join
    assert!(matches!(
        store.commit(next, &Limits::V1, &mut replay),
        sage_db_store::handle::HandleOutcome::Committed { version: 3, .. }
    ));
    assert!(
        store.verify(&Limits::V1, &mut replay).is_ok(),
        "negative created_at must compare on the same 64 wire bits"
    );
    let id = RecordIdentity::Identified {
        key: RecordKey::term(1),
    };
    let mut query = AttemptLedger::open(1 << 26, 1 << 20, &control).unwrap();
    match store.history(&id, 2, HistoryBudget::new(2), &Limits::V1, &mut query) {
        HistoryOutcome::Found(hit) => {
            assert_eq!(hit.source_version, 3);
            assert_eq!(hit.scanned_events, 2);
            assert_eq!(hit.durability, ViewDurability::Durable);
            assert_eq!(hit.basis, store.view().unwrap().basis());
        }
        _ => panic!("the second event in newest-first order must be found at its exact boundary"),
    }
    match store.history(&id, 2, HistoryBudget::new(1), &Limits::V1, &mut query) {
        HistoryOutcome::Incomplete {
            reason: HistoryIncomplete::EventLimit { limit: 1 },
            scanned_events: 1,
        } => {}
        _ => {
            panic!("one event is insufficient; do not claim absence or canonical budget exhaustion")
        }
    }
    match store.history(&id, 1, HistoryBudget::new(4), &Limits::V1, &mut query) {
        HistoryOutcome::Found(hit) => {
            assert_eq!(hit.source_version, 2);
            assert_eq!(hit.scanned_events, 4);
        }
        _ => panic!(
            "the older after-image comes from v2, never from the first arbitrary projected event"
        ),
    }
    match store.history(
        &id,
        2,
        HistoryBudget::new(100).without_cached_boundaries(),
        &Limits::V1,
        &mut query,
    ) {
        HistoryOutcome::Found(hit) => {
            assert_eq!(hit.source_version, 3);
            assert_eq!(
                hit.scanned_events, 4,
                "two boundary-discovery visits plus two reverse visits are actual work"
            );
        }
        _ => panic!("the true bounded fallback must work without cached boundaries"),
    }
    let absent = RecordIdentity::Identified {
        key: RecordKey::term(99),
    };
    match store.history(&absent, 1, HistoryBudget::new(14), &Limits::V1, &mut query) {
        HistoryOutcome::Absent {
            scanned_events: 14,
            basis,
            durability: ViewDurability::Durable,
        } => assert_eq!(basis, store.view().unwrap().basis()),
        _ => panic!(
            "absence requires all 10 Genesis + 2 v2 + 2 v3 WIRE events, not physical action count"
        ),
    }
    match store.history(&absent, 1, HistoryBudget::new(13), &Limits::V1, &mut query) {
        HistoryOutcome::Incomplete {
            reason: HistoryIncomplete::EventLimit { limit: 13 },
            scanned_events: 13,
        } => {}
        _ => panic!("the first insufficient absence bound is Incomplete"),
    }
}

#[test]
fn a_history_image_keeps_the_real_reservation_after_store_and_facades_drop() {
    let control = RunToCompletion;
    let (mut store, _, mut replay, _) = history_fixture(&control);
    let observer = sage_db_store::lease::LeaseLedger::reserve_owned(&mut replay, 0).unwrap();
    let mut query = AttemptLedger::open(1 << 26, 1 << 20, &control).unwrap();
    let id = RecordIdentity::Identified {
        key: RecordKey::term(1),
    };
    let image = match store.history(&id, 1, HistoryBudget::new(100), &Limits::V1, &mut query) {
        HistoryOutcome::Found(hit) => hit.image,
        _ => panic!("persisted target"),
    };
    let expected = image.record().clone(); // test-only value baseline, outside the product/query operation
    drop(query);
    drop(store);
    drop(replay);
    assert!(
        observer.stats().live_bytes > 0,
        "the returned record still has a retained claim"
    );
    assert_eq!(image.record(), &expected);
    let clone = image.clone();
    let held = observer.stats().live_bytes;
    drop(image);
    assert_eq!(
        observer.stats().live_bytes,
        held,
        "a clone shares, rather than mints, its ownership"
    );
    drop(clone);
    assert_eq!(
        observer.stats().live_bytes,
        0,
        "the last retained record releases the remaining real claim"
    );
}

#[test]
fn history_detects_missing_hit_blob_and_terminally_withdraws_current() {
    let control = RunToCompletion;
    let (mut store, _, _replay, fs) = history_fixture(&control);
    let old_pin = store.published_pin().unwrap();
    let old_entries = old_pin.entries().to_vec();
    let id = RecordIdentity::Identified {
        key: RecordKey::term(1),
    };
    let mut query = AttemptLedger::open(1 << 26, 1 << 20, &control).unwrap();
    let image = match store.history(&id, 1, HistoryBudget::new(100), &Limits::V1, &mut query) {
        HistoryOutcome::Found(hit) => hit.image,
        _ => panic!("healthy hit"),
    };
    let digest = match image.record() {
        sage_db_store::fold::FoldedRecord::Term(term) => term.body_ref,
        _ => panic!("term"),
    };
    let root = fs.open_root(Path::new("/root-history-r7")).unwrap();
    let blobs = fs
        .open_family(&root, Family::Blobs, CreateDir::Never)
        .unwrap();
    fs.remove(&blobs, EntryName::Blob(digest)).unwrap(); // actual SAME virtual backend, outside the query
    match store.history(&id, 1, HistoryBudget::new(100), &Limits::V1, &mut query) {
        HistoryOutcome::Error(sage_db_store::open::OpenRefusal::Corrupt(
            sage_db_store::open::CorruptReason::MissingBlob,
        )) => {}
        _ => panic!("the cached memory owner is not proof that the referenced file still exists"),
    }
    assert!(store.is_terminal_corrupt());
    assert!(store.is_fenced());
    assert!(store.view().is_none());
    assert!(store.published_pin().is_none());
    assert!(!store.coverage_complete());
    assert_eq!(old_pin.entries(), old_entries.as_slice());
    assert_eq!(old_pin.head().get(), 2);
    let restored = term_body(1, 1);
    assert_eq!(raw_sha256(&restored), digest);
    let mut file = fs.create_new(&blobs, EntryName::Blob(digest)).unwrap();
    assert_eq!(
        fs.write(&mut file, &restored).unwrap().written,
        restored.len()
    );
    fs.sync_file(&file).unwrap();
    fs.sync_dir(&blobs).unwrap();
    assert!(
        store.verify(&Limits::V1, &mut query).is_ok(),
        "successful diagnostic verification after test restoration is allowed"
    );
    assert!(
        store.is_terminal_corrupt(),
        "diagnostic success is not repair authorization"
    );
    assert!(store.view().is_none());
    assert!(store.is_fenced());
    assert!(
        matches!(
            store.resolve(&Limits::V1, &mut query),
            sage_db_store::open::Resolution::Corrupt(
                sage_db_store::open::CorruptReason::TerminalCorrupt
            )
        ),
        "ordinary resolution must not silently repair a terminal handle"
    );
}

#[test]
fn real_query_ledger_exhaustion_is_not_the_explicit_scan_cap() {
    let control = RunToCompletion;
    let (mut store, counters, _replay, _) = history_fixture(&control);
    let id = RecordIdentity::Identified {
        key: RecordKey::term(1),
    };
    let before = counters.borrow().file_reads;
    let mut query = AttemptLedger::open(1 << 26, 0, &control).unwrap();
    match store.history(&id, 1, HistoryBudget::new(100), &Limits::V1, &mut query) {
        HistoryOutcome::Incomplete {
            reason: sage_db_store::history::HistoryIncomplete::Refused(Refusal::Budget(error)),
            scanned_events: 0,
        } => {
            assert_eq!(error.unit(), BudgetUnit::Entries);
            assert_eq!(error.requested(), 1);
            assert_eq!(error.remaining(), 0);
            assert_eq!(query.stats().limit_entries, 0);
            assert_eq!(query.stats().admitted_entries, Some(0));
        }
        _ => panic!("an actual zero-entry ledger refuses before transaction I/O"),
    }
    assert_eq!(counters.borrow().file_reads, before);
}
