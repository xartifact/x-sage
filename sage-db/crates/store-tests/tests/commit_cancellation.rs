//! PROBE B (t67) — §4.1 begin_persist cancellation boundary.
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
use sage_db_store_tests::model::fs_model::{ModelCapabilities, ModelFileSystem};
use sage_db_types::framing::Limits;
use sage_db_types::identity::KbUuid;
use sage_db_types::prepared::{
    BlobDeclaration, BlobSource, OwnedEvent, PreparedTransaction, RulesBinding,
};
use sage_db_types::relation::RuleProfile;

const RULES: &[u8] = include_bytes!("../fixtures/rules.formal-v1.v1.bin");
const UUID: [u8; 16] = [0x11; 16];

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

// ---------------------------------------------------------------------------
// Probe B — §4.1: cancellation honoured BEFORE begin_persist, ignored AFTER it.
// ---------------------------------------------------------------------------

use sage_db_types::query::{ControlStop, QueryControl};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// A real `QueryControl` whose verdict can be flipped at runtime by the seam observer.
struct FlipControl {
    cancelled: AtomicBool,
    checks: AtomicU64,
}
impl FlipControl {
    fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            checks: AtomicU64::new(0),
        }
    }
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
    fn checks(&self) -> u64 {
        self.checks.load(Ordering::SeqCst)
    }
}
impl QueryControl for FlipControl {
    fn checkpoint(&self) -> Result<(), ControlStop> {
        self.checks.fetch_add(1, Ordering::SeqCst);
        if self.cancelled.load(Ordering::SeqCst) {
            Err(ControlStop::Cancelled)
        } else {
            Ok(())
        }
    }
}

/// A delegating FS that flips the SAME control after the first actual `write`.
struct FlipFs {
    inner: Rc<ModelFileSystem>,
    mutations: Rc<RefCell<Mutations>>,
    ctl: Rc<FlipControl>,
    armed: Rc<AtomicBool>,
    writes_after_arm: Rc<AtomicU64>,
}
impl FlipFs {
    fn new(ctl: Rc<FlipControl>) -> Self {
        Self {
            inner: Rc::new(ModelFileSystem::new(ModelCapabilities::full())),
            mutations: Rc::new(RefCell::new(Mutations::default())),
            ctl,
            armed: Rc::new(AtomicBool::new(false)),
            writes_after_arm: Rc::new(AtomicU64::new(0)),
        }
    }
    /// Shared arming flag, usable after `bootstrap` takes the wrapper by value.
    fn armed_handle(&self) -> Rc<AtomicBool> {
        Rc::clone(&self.armed)
    }
    fn writes_handle(&self) -> Rc<AtomicU64> {
        Rc::clone(&self.writes_after_arm)
    }
}
macro_rules! delegate {
    ($self:ident, $m:ident, $body:expr) => {{
        $self.mutations.borrow_mut().$m += 1;
        $body
    }};
}
impl FileSystem for FlipFs {
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
        delegate!(self, create_root, self.inner.create_root(abs))
    }
    fn sync_parent_of(&self, abs: &Path) -> Result<(), FsError> {
        delegate!(self, sync_parent_of, self.inner.sync_parent_of(abs))
    }
    fn open_family(
        &self,
        root: &Self::Dir,
        family: Family,
        create: CreateDir,
    ) -> Result<Self::Dir, FsError> {
        if create == CreateDir::IfMissing {
            self.mutations.borrow_mut().open_family_if_missing += 1;
        }
        self.inner.open_family(root, family, create)
    }
    fn sync_dir(&self, dir: &Self::Dir) -> Result<(), FsError> {
        delegate!(self, sync_dir, self.inner.sync_dir(dir))
    }
    fn device_id(&self, dir: &Self::Dir) -> Result<u64, FsError> {
        self.inner.device_id(dir)
    }
    fn try_lock_exclusive(&self, root: &Self::Dir) -> Result<Self::Lock, FsError> {
        self.inner.try_lock_exclusive(root)
    }
    fn create_new(&self, dir: &Self::Dir, name: EntryName) -> Result<Self::WriteFile, FsError> {
        delegate!(self, create_new, self.inner.create_new(dir, name))
    }
    fn write(&self, file: &mut Self::WriteFile, buf: &[u8]) -> Result<WriteOutcome, FsError> {
        let out = delegate!(self, write, self.inner.write(file, buf));
        // Flip the SAME caller control AFTER the first actual persisted write made post-bootstrap.
        if self.armed.load(Ordering::SeqCst) {
            let n = self.writes_after_arm.fetch_add(1, Ordering::SeqCst) + 1;
            if n == 1 {
                self.ctl.cancel();
                println!("B/observer: cancelled the real control AFTER actual write #{n}");
            }
        }
        out
    }
    fn sync_file(&self, file: &Self::WriteFile) -> Result<(), FsError> {
        delegate!(self, sync_file, self.inner.sync_file(file))
    }
    fn sync_entry(&self, dir: &Self::Dir, name: EntryName) -> Result<(), FsError> {
        delegate!(self, sync_entry, self.inner.sync_entry(dir, name))
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
        self.mutations.borrow_mut().publish_no_overwrite += 1;
        self.inner
            .publish_no_overwrite(staged_dir, staged, dst_dir, dst, expected, ledger)
    }
    fn replace_current(
        &self,
        staged_dir: &Self::Dir,
        staged: EntryName,
        root: &Self::Dir,
    ) -> Result<(), FsError> {
        delegate!(
            self,
            replace_current,
            self.inner.replace_current(staged_dir, staged, root)
        )
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
        delegate!(self, remove, self.inner.remove(dir, name))
    }
}

#[test]
fn root_t87_after_write_cancel_does_not_abort_persisting() {
    // (1) POSITIVE UNHALTED control: same shape, control never flips.
    {
        let ctl = Rc::new(FlipControl::new());
        let fs = FlipFs::new(Rc::clone(&ctl));
        let mut ledger =
            AttemptLedger::open(64 * 1024 * 1024, 1_000_000, ctl.as_ref()).expect("ledger");
        let mut store: Store<FlipFs> = match bootstrap(
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
        let d2 = [0x22; 32];
        let mut tx2 = carrier(
            2,
            Some(1),
            "second",
            vec![outcome_event(2, "system", "second", d2)],
        );
        tx2.payload_digest = d2;
        let r = store.commit(tx2, &Limits::V1, &mut ledger);
        println!(
            "B/positive unhalted: {} checks={} -> {r:?}",
            matches!(r, HandleOutcome::Committed { .. }),
            ctl.checks()
        );
        assert!(
            matches!(r, HandleOutcome::Committed { version: 2, .. }),
            "unhalted positive must really commit"
        );
        // An unhalted phase records no deferred stop.
        assert_eq!(
            store.last_persist_stop(),
            None,
            "an unhalted commit must record no deferred stop"
        );
    }

    // (2) INITIALLY STOPPED before any new-request write: control cancelled from the start.
    {
        let ctl = Rc::new(FlipControl::new());
        let fs = FlipFs::new(Rc::clone(&ctl));
        let mut ledger =
            AttemptLedger::open(64 * 1024 * 1024, 1_000_000, ctl.as_ref()).expect("ledger");
        // Bootstrap needs a permissive control; flip only after it.
        let mut store: Store<FlipFs> = match bootstrap(
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
        let writes_before = store.retained_entries().count();
        ctl.cancel();
        let d2 = [0x22; 32];
        let mut tx2 = carrier(
            2,
            Some(1),
            "second",
            vec![outcome_event(2, "system", "second", d2)],
        );
        tx2.payload_digest = d2;
        let r = store.commit(tx2, &Limits::V1, &mut ledger);
        println!(
            "B/initially stopped: {r:?} head={:?} retained_before={writes_before} retained_after={}",
            store.head(),
            store.retained_entries().count()
        );
    }

    // (3) REAL flip by the delegating FS observer AFTER the first actual persistence write.
    {
        let ctl = Rc::new(FlipControl::new());
        let fs = FlipFs::new(Rc::clone(&ctl));
        let armed = fs.armed_handle();
        let writes = fs.writes_handle();
        let mut ledger =
            AttemptLedger::open(64 * 1024 * 1024, 1_000_000, ctl.as_ref()).expect("ledger");
        let mut store: Store<FlipFs> = match bootstrap(
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
        // Arm AFTER bootstrap, so the flip lands inside the NEW request's persistence.
        armed.store(true, Ordering::SeqCst);
        println!(
            "B/flip armed after bootstrap; checks so far={}",
            ctl.checks()
        );
        let d2 = [0x22; 32];
        let mut tx2 = carrier(
            2,
            Some(1),
            "second",
            vec![outcome_event(2, "system", "second", d2)],
        );
        tx2.payload_digest = d2;
        let r = store.commit(tx2, &Limits::V1, &mut ledger);
        println!("B/after-write flip: {r:?}");
        println!(
            "B/writes after arm       = {}",
            writes.load(Ordering::SeqCst)
        );
        println!("B/checks seen by control = {}", ctl.checks());
        println!("B/head after flip        = {:?}", store.head());
        println!(
            "B/control cancelled?     = {}",
            ctl.cancelled.load(Ordering::SeqCst)
        );
        assert!(ctl.cancelled.load(Ordering::SeqCst));
        assert!(
            writes.load(Ordering::SeqCst) >= 1,
            "flip must follow actual persistence write"
        );
        assert!(
            matches!(r, HandleOutcome::Committed { version: 2, .. }),
            "post-begin-persist caller stop must not abort: {r:?}"
        );
        // S2 metric clause: the stop was CONSULTED and RECORDED, but never aborted the phase. The
        // diagnostic surface is the only place that fact is visible.
        assert_eq!(
            store.last_persist_stop(),
            Some(sage_db_types::query::ControlStop::Cancelled),
            "the consulted stop must be recorded on the store"
        );
    }
}
