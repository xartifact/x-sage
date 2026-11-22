//! PROBE A (t67) — S2 state machine K12/6.2: real Unknown, then a fresh request.
//! Wrapper below is t63's SAME-backend delegating observer (fa8da4a7) used purely as a helper,
//! so the model can be armed AFTER bootstrap. No raw-header parser is used.
#![cfg(test)]
#![allow(clippy::too_many_lines)]

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use sage_db_store::fs::{
    Capabilities, CreateDir, Each, Family, FileSystem, FsError, HashReport, Published, WriteOutcome,
};
use sage_db_store::handle::{HandleOutcome, Store};
use sage_db_store::init::bootstrap;
use sage_db_store::lease::AttemptLedger;
use sage_db_store::names::EntryName;
use sage_db_store_tests::faults::{FaultPoint, FaultSchedule, Injection};
use sage_db_store_tests::model::fs_model::{ModelCapabilities, ModelFileSystem};
use sage_db_types::framing::Limits;
use sage_db_types::identity::KbUuid;
use sage_db_types::prepared::{
    BlobDeclaration, BlobSource, OwnedEvent, PreparedTransaction, RulesBinding,
};
use sage_db_types::query::RunToCompletion;
use sage_db_types::relation::RuleProfile;

const RULES: &[u8] = include_bytes!("../fixtures/rules.formal-v1.v1.bin");
const UUID: [u8; 16] = [0x11; 16];

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

    /// The shared mutating counters, usable after the wrapper is moved into `bootstrap`.
    fn counters_handle(&self) -> Rc<RefCell<Mutations>> {
        Rc::clone(&self.mutations)
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
// Fixtures
// ---------------------------------------------------------------------------

fn str_value(text: &str) -> Vec<u8> {
    let mut v = (text.len() as u32).to_le_bytes().to_vec();
    v.extend_from_slice(text.as_bytes());
    v
}
fn result_payload(version: u64) -> Vec<u8> {
    let mut o = Vec::new();
    o.extend_from_slice(b"opaque-result/");
    o.extend_from_slice(&version.to_le_bytes());
    o.extend_from_slice(&2u32.to_le_bytes());
    o
}
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
            sage_db_format::digest::raw_sha256(&result_payload(version)).to_vec(),
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
    let mut declared = vec![BlobDeclaration {
        sha256: rules_ref,
        declared_len: RULES.len() as u64,
        source: BlobSource::Provide(RULES.to_vec()),
    }];
    if events.iter().any(|e| e.tag == 192) {
        let payload = result_payload(version);
        declared.push(BlobDeclaration {
            sha256: raw_sha256(&payload),
            declared_len: payload.len() as u64,
            source: BlobSource::Provide(payload),
        });
    }
    PreparedTransaction {
        version,
        prev_version: prev,
        tx_id: [0xB0 + version as u8; 16],
        kb_uuid: KbUuid::from_bytes(UUID),
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

#[test]
fn root_t87_unknown_hides_current_but_keeps_explicit_old_pin() {
    let fs = ObservedFs::new();
    let model = fs.model_handle();
    // Keep the shared counters alive after `bootstrap` takes the wrapper by value.
    let counters: Rc<RefCell<Mutations>> = fs.counters_handle();
    let ctl: &'static RunToCompletion = Box::leak(Box::new(RunToCompletion));
    let mut ledger = AttemptLedger::open(64 * 1024 * 1024, 1_000_000, ctl).expect("finite ledger");

    let mut store: Store<ObservedFs> = match bootstrap(
        fs,
        Path::new("/kb"),
        CreateDir::IfMissing,
        KbUuid::from_bytes(UUID),
        RULES,
        &Limits::V1,
        &mut ledger,
    ) {
        Ok(s) => s,
        Err(r) => panic!("bootstrap: {r:?}"),
    };
    println!("A/bootstrap: head={:?}", store.head());
    let genesis_pin = store.published_pin().expect("genesis pin");
    println!(
        "A/genesis pin: head={:?} owners={} reserved={}",
        genesis_pin.head(),
        genesis_pin.owners(),
        genesis_pin.reserved_bytes()
    );

    // Ordinary success #2.
    let d2 = [0x22; 32];
    let mut tx2 = carrier(
        2,
        Some(1),
        "second",
        vec![outcome_event(2, "system", "second", d2)],
    );
    tx2.payload_digest = d2;
    let r2 = store.commit(tx2, &Limits::V1, &mut ledger);
    println!("A/commit#2 = {r2:?}");

    let old_pin = store.published_pin().expect("pin after #2");
    let old_ver = old_pin.head();
    let old_len = old_pin.entries().len();
    let old_reserved = old_pin.reserved_bytes();
    let old_entries = old_pin.entries().to_vec();
    println!(
        "A/old pin retained: head={old_ver:?} entries={old_len} reserved={old_reserved} owners={}",
        old_pin.owners()
    );

    // ONE actual root-replace fault, armed AFTER bootstrap through the existing model schedule.
    *model.faults_handle().borrow_mut() = FaultSchedule::none().at(
        FaultPoint::ReplaceCurrent,
        Injection::FailUnknownAfterEffect,
        1,
    );
    println!(
        "A/armed: ReplaceCurrent FailUnknownAfterEffect x1 (pending={})",
        model
            .faults_handle()
            .borrow()
            .pending(FaultPoint::ReplaceCurrent)
    );

    // Fresh request #3 drives it.
    let d3 = [0x33; 32];
    let mut tx3 = carrier(
        3,
        Some(2),
        "third",
        vec![outcome_event(3, "system", "third", d3)],
    );
    tx3.payload_digest = d3;
    let r3 = store.commit(tx3, &Limits::V1, &mut ledger);
    println!("A/commit#3 = {r3:?}");
    println!(
        "A/is Unknown             : {}",
        matches!(r3, HandleOutcome::Unknown)
    );
    println!("A/writer fenced          : {}", store.is_fenced());
    println!("A/head after unknown     : {:?}", store.head());
    println!(
        "A/fault fired            : {:?}",
        model.faults_handle().borrow().fired()
    );
    println!(
        "A/old pin after fault    : head={:?} (was {old_ver:?}) entries={} (was {old_len}) reserved={} (was {old_reserved})",
        old_pin.head(),
        old_pin.entries().len(),
        old_pin.reserved_bytes()
    );

    // Mutating-call counts before the follow-up, to prove "no mutating calls".
    let before = *counters.borrow();
    println!("A/counters before followup: {before:?}");

    // NEW current/latest request while undecided.
    let d4 = [0x44; 32];
    let mut tx4 = carrier(
        4,
        Some(3),
        "fourth",
        vec![outcome_event(4, "system", "fourth", d4)],
    );
    tx4.payload_digest = d4;
    let r4 = store.commit(tx4, &Limits::V1, &mut ledger);
    println!("A/commit#4 while undecided = {r4:?}");
    let after = *counters.borrow();
    println!("A/counters after followup : {after:?}");
    println!("A/followup made NO mutating calls: {}", before == after);
    println!(
        "A/latest pin now          : {:?}",
        store.published_pin().map(|p| p.head())
    );
    println!(
        "A/retained entries now    : {}",
        store.retained_entries().count()
    );
    assert!(matches!(r2, HandleOutcome::Committed { version: 2, .. }));
    assert!(matches!(r3, HandleOutcome::Unknown));
    assert!(store.is_fenced());
    assert_eq!(
        before, after,
        "fenced follow-up must make no mutating calls"
    );
    assert_eq!(old_pin.head(), old_ver);
    assert_eq!(old_pin.entries(), old_entries.as_slice());
    assert_eq!(old_pin.reserved_bytes(), old_reserved);
    assert!(
        store.published_pin().is_none(),
        "current pin must be absent while authority is undecided"
    );
    assert_eq!(
        store.retained_entries().count(),
        0,
        "the OTHER current-view route must also be absent"
    );
    // Third current-view route: coverage. While CURRENT's meaning is undecided the handle cannot claim it
    // has indexed everything it committed, because the unknown root may include this very commit.
    assert!(
        !store.coverage_complete(),
        "coverage must NOT be claimed while authority is undecided"
    );
}

#[test]
fn root_t87_orphan_unknown_fences_writes_but_keeps_durable_current() {
    let fs = ObservedFs::new();
    let model = fs.model_handle();
    let counters = fs.counters_handle();
    let control = RunToCompletion;
    let mut ledger = AttemptLedger::open(64 * 1024 * 1024, 1_000_000, &control).unwrap();
    let mut store = bootstrap(
        fs,
        Path::new("/orphan"),
        CreateDir::IfMissing,
        KbUuid::from_bytes(UUID),
        RULES,
        &Limits::V1,
        &mut ledger,
    )
    .unwrap();
    let d2 = [0x22; 32];
    let mut tx2 = carrier(
        2,
        Some(1),
        "second",
        vec![outcome_event(2, "system", "second", d2)],
    );
    tx2.payload_digest = d2;
    assert!(matches!(
        store.commit(tx2, &Limits::V1, &mut ledger),
        HandleOutcome::Committed { version: 2, .. }
    ));
    let old = store.published_pin().unwrap();
    let old_entries = old.entries().to_vec();
    *model.faults_handle().borrow_mut() =
        FaultSchedule::none().at(FaultPoint::Publish, Injection::FailUnknown, 1);
    let d3 = [0x33; 32];
    let mut tx3 = carrier(
        3,
        Some(2),
        "third",
        vec![outcome_event(3, "system", "third", d3)],
    );
    tx3.payload_digest = d3;
    let result = store.commit(tx3, &Limits::V1, &mut ledger);
    println!(
        "ORPHAN/result={result:?} fenced={} current={:?}",
        store.is_fenced(),
        store.published_pin().map(|p| p.head())
    );
    assert!(
        matches!(result, HandleOutcome::NotCommitted { reason: sage_db_store::handle::HandleRefusal::Derivation(sage_db_store::commit::CommitRefusal::Filesystem { error, .. }) } if error.effect() == sage_db_store::fs::Effect::Unknown)
    );
    assert_eq!(
        model.faults_handle().borrow().fired(),
        &[(FaultPoint::Publish, Injection::FailUnknown)]
    );
    assert!(store.is_fenced());
    assert_eq!(store.published_pin().unwrap().head(), old.head());
    // An Unknown that left the durable CURRENT untouched is still a root this handle indexed: coverage
    // is NOT invalidated, and treating it as undecided would be the opposite error.
    assert!(
        store.coverage_complete(),
        "an orphan Unknown must not revoke an otherwise complete current view"
    );
    assert_eq!(
        store.retained_entries().collect::<Vec<_>>(),
        old_entries.iter().collect::<Vec<_>>()
    );
    let before = *counters.borrow();
    let d4 = [0x44; 32];
    let mut tx4 = carrier(
        4,
        Some(3),
        "fourth",
        vec![outcome_event(4, "system", "fourth", d4)],
    );
    tx4.payload_digest = d4;
    assert!(matches!(
        store.commit(tx4, &Limits::V1, &mut ledger),
        HandleOutcome::NotCommitted {
            reason: sage_db_store::handle::HandleRefusal::Fenced
        }
    ));
    assert_eq!(before, *counters.borrow());
}
