use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use sage_db_store::fs::{
    Capabilities, CreateDir, Each, Family, FileSystem, FsError, HashReport, Published, WriteOutcome,
};
use sage_db_store::handle::HandleOutcome;
use sage_db_store::init::bootstrap;
use sage_db_store::lease::AttemptLedger;
use sage_db_store::names::{EntryName, RootName};
use sage_db_store_tests::model::RelPath;
use sage_db_store_tests::model::fs_model::{ModelCapabilities, ModelFileSystem};
use sage_db_types::framing::Limits;
use sage_db_types::ledger::SharedLedger;
use sage_db_types::persist::StateEntry;
use sage_db_types::query::RunToCompletion;

use crate::{fixture, snapshot};

pub(crate) const MAX_EXPORTED_WORLDS: usize = 4096;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum Boundary {
    FirstCandidateWrite,
    FirstSyncFile,
    FirstBlobPublish,
    TransactionPublish,
    ManifestPublish,
    ReplaceCurrent,
    RootSyncDir,
    AfterAck,
}

impl Boundary {
    pub(crate) const ALL: [Self; 8] = [
        Self::FirstCandidateWrite,
        Self::FirstSyncFile,
        Self::FirstBlobPublish,
        Self::TransactionPublish,
        Self::ManifestPublish,
        Self::ReplaceCurrent,
        Self::RootSyncDir,
        Self::AfterAck,
    ];

    #[must_use]
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::FirstCandidateWrite => "first_write",
            Self::FirstSyncFile => "first_sync_file",
            Self::FirstBlobPublish => "first_blob_publish",
            Self::TransactionPublish => "tx_publish",
            Self::ManifestPublish => "manifest_publish",
            Self::ReplaceCurrent => "replace_current",
            Self::RootSyncDir => "root_sync_dir",
            Self::AfterAck => "after_ack",
        }
    }

    #[must_use]
    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s {
            "first_write" => Some(Self::FirstCandidateWrite),
            "first_sync_file" => Some(Self::FirstSyncFile),
            "first_blob_publish" => Some(Self::FirstBlobPublish),
            "tx_publish" => Some(Self::TransactionPublish),
            "manifest_publish" => Some(Self::ManifestPublish),
            "replace_current" => Some(Self::ReplaceCurrent),
            "root_sync_dir" => Some(Self::RootSyncDir),
            "after_ack" => Some(Self::AfterAck),
            _ => None,
        }
    }

    #[must_use]
    pub(crate) const fn is_after_ack(self) -> bool {
        matches!(self, Self::AfterAck)
    }

    #[must_use]
    pub(crate) const fn allowed_head(self, head: u64) -> bool {
        match self {
            Self::FirstCandidateWrite
            | Self::FirstSyncFile
            | Self::FirstBlobPublish
            | Self::TransactionPublish
            | Self::ManifestPublish => head == 2,
            Self::ReplaceCurrent => head == 2 || head == 3,
            Self::RootSyncDir | Self::AfterAck => head == 3,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct FiredOpRecord {
    pub(crate) method: &'static str,
    pub(crate) dir: RelPath,
    pub(crate) name: String,
    pub(crate) replace_current_completed: bool,
    pub(crate) root_sync_after_replace_completed: bool,
}

#[derive(Clone, Debug)]
enum BarrierMode {
    RecordOnly,
    ExportAndBlock {
        case_dir: PathBuf,
        genesis_rows: Vec<StateEntry>,
    },
}

#[derive(Debug)]
struct ObserverState {
    target: Boundary,
    armed: bool,
    triggered: bool,
    replace_current_completed: bool,
    root_sync_after_replace_completed: bool,
    write_handles: BTreeMap<u64, (RelPath, EntryName)>,
    fired: Option<FiredOpRecord>,
    barrier_mode: BarrierMode,
}

pub(crate) fn check_export_bounds(
    sampled: usize,
    len: usize,
    ceiling: usize,
) -> Result<(), String> {
    if sampled == 0 || len == 0 {
        return Err(format!(
            "crash_states export must be non-empty (sampled={sampled}, len={len})"
        ));
    }
    if sampled != len {
        return Err(format!(
            "crash_states sampled={sampled} disagrees with states.len()={len}"
        ));
    }
    if sampled > ceiling || len > ceiling {
        return Err(format!(
            "crash_states export exceeded explicit ceiling {ceiling}: sampled={sampled}, len={len}"
        ));
    }
    Ok(())
}

#[derive(Clone)]
pub(crate) struct BoundaryObserverFs {
    inner: Rc<ModelFileSystem>,
    state: Rc<RefCell<ObserverState>>,
}

impl BoundaryObserverFs {
    #[must_use]
    pub(crate) fn new_record_only(inner: Rc<ModelFileSystem>, target: Boundary) -> Self {
        Self {
            inner,
            state: Rc::new(RefCell::new(ObserverState {
                target,
                armed: false,
                triggered: false,
                replace_current_completed: false,
                root_sync_after_replace_completed: false,
                write_handles: BTreeMap::new(),
                fired: None,
                barrier_mode: BarrierMode::RecordOnly,
            })),
        }
    }

    #[must_use]
    pub(crate) fn is_armed(&self) -> bool {
        self.state.borrow().armed
    }

    #[must_use]
    pub(crate) fn fired_record(&self) -> Option<FiredOpRecord> {
        self.state.borrow().fired.clone()
    }

    #[must_use]
    pub(crate) fn replace_current_completed(&self) -> bool {
        self.state.borrow().replace_current_completed
    }

    #[must_use]
    pub(crate) fn root_sync_after_replace_completed(&self) -> bool {
        self.state.borrow().root_sync_after_replace_completed
    }

    #[cfg(test)]
    pub(crate) fn arm_record_only(&self) {
        self.state.borrow_mut().armed = true;
    }

    pub(crate) fn arm_for_export(&self, case_dir: PathBuf, genesis_rows: Vec<StateEntry>) {
        let mut st = self.state.borrow_mut();
        st.barrier_mode = BarrierMode::ExportAndBlock {
            case_dir,
            genesis_rows,
        };
        st.armed = true;
    }

    fn handle_boundary(&self, rec: &FiredOpRecord, mode: &BarrierMode) {
        match mode {
            BarrierMode::RecordOnly => {}
            BarrierMode::ExportAndBlock {
                case_dir,
                genesis_rows,
            } => {
                if let Err(err) = export_worlds_and_block(
                    &self.inner,
                    case_dir,
                    genesis_rows,
                    self.state.borrow().target,
                    false,
                    0,
                    rec,
                ) {
                    eprintln!("PRODUCER_CALLBACK_EXPORT_ERROR: {err}");
                    let _ = io::stderr().flush();
                    std::process::exit(97);
                }
            }
        }
    }

    pub(crate) fn export_post_ack_and_block(&self, rec: &FiredOpRecord) -> Result<(), String> {
        let mode = self.state.borrow().barrier_mode.clone();
        let target = self.state.borrow().target;
        match mode {
            BarrierMode::ExportAndBlock {
                case_dir,
                genesis_rows,
            } => {
                export_worlds_and_block(&self.inner, &case_dir, &genesis_rows, target, true, 3, rec)
            }
            BarrierMode::RecordOnly => {
                Err("export_post_ack_and_block called in RecordOnly mode".into())
            }
        }
    }
}

fn export_worlds_and_block(
    model_fs: &ModelFileSystem,
    case_dir: &Path,
    genesis_rows: &[StateEntry],
    boundary: Boundary,
    ack_emitted: bool,
    ack_version: u64,
    rec: &FiredOpRecord,
) -> Result<(), String> {
    std::fs::create_dir_all(case_dir)
        .map_err(|e| format!("create case dir {}: {e}", case_dir.display()))?;

    let crash = model_fs.model().crash_states();
    let sampled = crash.sampled;
    let worlds_len = crash.states.len();
    check_export_bounds(sampled, worlds_len, MAX_EXPORTED_WORLDS)?;

    let genesis_path = case_dir.join("genesis_preservation_reference.snap");
    snapshot::write_rows(&genesis_path, genesis_rows)
        .map_err(|e| format!("write genesis preservation reference: {e}"))?;

    for (idx, world_state) in crash.states.iter().enumerate() {
        let world_path = case_dir.join(format!("world-{idx:04}.snap"));
        snapshot::write(&world_path, world_state)
            .map_err(|e| format!("write world {idx} snapshot: {e}"))?;
    }

    let pid = std::process::id();
    let dir_hex = snapshot::hex(rec.dir.as_str().as_bytes());
    let meta_path = case_dir.join("meta.kv");
    let meta_text = format!(
        "boundary={}\n\
         producer_pid={}\n\
         armed_after_version=2\n\
         ack_emitted={}\n\
         ack_version={}\n\
         op_method={}\n\
         op_dir_hex={}\n\
         op_name={}\n\
         replace_current_completed={}\n\
         root_sync_after_replace_completed={}\n\
         sampled={}\n\
         worlds_len={}\n\
         genesis_rows={}\n",
        boundary.as_str(),
        pid,
        ack_emitted,
        ack_version,
        rec.method,
        dir_hex,
        rec.name,
        rec.replace_current_completed,
        rec.root_sync_after_replace_completed,
        sampled,
        worlds_len,
        genesis_rows.len(),
    );
    let mut meta_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&meta_path)
        .map_err(|e| format!("create meta.kv: {e}"))?;
    meta_file
        .write_all(meta_text.as_bytes())
        .map_err(|e| format!("write meta.kv: {e}"))?;
    meta_file
        .sync_all()
        .map_err(|e| format!("sync meta.kv: {e}"))?;

    let dir_handle = File::open(case_dir).map_err(|e| format!("open case_dir for sync: {e}"))?;
    dir_handle
        .sync_all()
        .map_err(|e| format!("sync case_dir: {e}"))?;

    {
        let mut out = io::stdout().lock();
        writeln!(
            out,
            "READY boundary={} pid={} sampled={} worlds={} ack={}",
            boundary.as_str(),
            pid,
            sampled,
            worlds_len,
            ack_version
        )
        .map_err(|e| format!("write READY: {e}"))?;
        out.flush().map_err(|e| format!("flush READY: {e}"))?;
    }

    block_forever_until_killed();
}

fn block_forever_until_killed() -> ! {
    let mut stdin = io::stdin().lock();
    let mut buf = [0u8; 1];
    loop {
        match stdin.read(&mut buf) {
            Ok(0) => {
                eprintln!("PRODUCER_STDIN_EOF_WITHOUT_SIGKILL");
                let _ = io::stderr().flush();
                std::process::exit(98);
            }
            Ok(_) => {
                eprintln!("PRODUCER_UNEXPECTED_STDIN_BYTES");
                let _ = io::stderr().flush();
                std::process::exit(99);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                eprintln!("PRODUCER_STDIN_READ_ERROR: {e}");
                let _ = io::stderr().flush();
                std::process::exit(98);
            }
        }
    }
}

impl FileSystem for BoundaryObserverFs {
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
        self.inner.create_root(abs)
    }

    fn sync_parent_of(&self, abs: &Path) -> Result<(), FsError> {
        self.inner.sync_parent_of(abs)
    }

    fn open_family(
        &self,
        root: &Self::Dir,
        family: Family,
        create: CreateDir,
    ) -> Result<Self::Dir, FsError> {
        self.inner.open_family(root, family, create)
    }

    fn sync_dir(&self, dir: &Self::Dir) -> Result<(), FsError> {
        let res = self.inner.sync_dir(dir);
        let trigger = if res.is_ok() {
            let mut st = self.state.borrow_mut();
            if st.armed {
                let dir_rel = self
                    .inner
                    .dir_path(*dir)
                    .expect("sync_dir succeeded on dir handle with unknown dir_path");
                if dir_rel == RelPath::root() && st.replace_current_completed {
                    st.root_sync_after_replace_completed = true;
                    if !st.triggered && st.target == Boundary::RootSyncDir {
                        let rec = FiredOpRecord {
                            method: "sync_dir",
                            dir: dir_rel,
                            name: "<root>".to_owned(),
                            replace_current_completed: st.replace_current_completed,
                            root_sync_after_replace_completed: st.root_sync_after_replace_completed,
                        };
                        st.triggered = true;
                        st.fired = Some(rec.clone());
                        Some((rec, st.barrier_mode.clone()))
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };
        if let Some((rec, mode)) = trigger {
            self.handle_boundary(&rec, &mode);
        }
        res
    }

    fn device_id(&self, dir: &Self::Dir) -> Result<u64, FsError> {
        self.inner.device_id(dir)
    }

    fn try_lock_exclusive(&self, root: &Self::Dir) -> Result<Self::Lock, FsError> {
        self.inner.try_lock_exclusive(root)
    }

    fn create_new(&self, dir: &Self::Dir, name: EntryName) -> Result<Self::WriteFile, FsError> {
        let res = self.inner.create_new(dir, name);
        if let Ok(ref handle) = res {
            let mut st = self.state.borrow_mut();
            if st.armed {
                let dir_rel = self
                    .inner
                    .dir_path(*dir)
                    .expect("create_new succeeded on dir handle with unknown dir_path");
                st.write_handles.insert(handle.0, (dir_rel, name));
            }
        }
        res
    }

    fn write(&self, file: &mut Self::WriteFile, buf: &[u8]) -> Result<WriteOutcome, FsError> {
        let res = self.inner.write(file, buf);
        let trigger = if res.is_ok() {
            let mut st = self.state.borrow_mut();
            if st.armed && !st.triggered && st.target == Boundary::FirstCandidateWrite {
                let (dir, name) = st
                    .write_handles
                    .get(&file.0)
                    .cloned()
                    .expect("write handle must be tracked for armed write");
                let rec = FiredOpRecord {
                    method: "write",
                    dir,
                    name: name.to_file_name(),
                    replace_current_completed: st.replace_current_completed,
                    root_sync_after_replace_completed: st.root_sync_after_replace_completed,
                };
                st.triggered = true;
                st.fired = Some(rec.clone());
                Some((rec, st.barrier_mode.clone()))
            } else {
                None
            }
        } else {
            None
        };
        if let Some((rec, mode)) = trigger {
            self.handle_boundary(&rec, &mode);
        }
        res
    }

    fn sync_file(&self, file: &Self::WriteFile) -> Result<(), FsError> {
        let res = self.inner.sync_file(file);
        let trigger = if res.is_ok() {
            let mut st = self.state.borrow_mut();
            if st.armed && !st.triggered && st.target == Boundary::FirstSyncFile {
                let (dir, name) = st
                    .write_handles
                    .get(&file.0)
                    .cloned()
                    .expect("write handle must be tracked for armed sync_file");
                let rec = FiredOpRecord {
                    method: "sync_file",
                    dir,
                    name: name.to_file_name(),
                    replace_current_completed: st.replace_current_completed,
                    root_sync_after_replace_completed: st.root_sync_after_replace_completed,
                };
                st.triggered = true;
                st.fired = Some(rec.clone());
                Some((rec, st.barrier_mode.clone()))
            } else {
                None
            }
        } else {
            None
        };
        if let Some((rec, mode)) = trigger {
            self.handle_boundary(&rec, &mode);
        }
        res
    }

    fn sync_entry(&self, dir: &Self::Dir, name: EntryName) -> Result<(), FsError> {
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
        let res = self
            .inner
            .publish_no_overwrite(staged_dir, staged, dst_dir, dst, expected, ledger);
        let trigger = if res.is_ok() {
            let mut st = self.state.borrow_mut();
            if st.armed && !st.triggered {
                let dir_rel = self
                    .inner
                    .dir_path(*dst_dir)
                    .expect("publish_no_overwrite succeeded on dst_dir with unknown dir_path");
                let expected_dir = match dst {
                    EntryName::Blob(_) => Some((
                        Boundary::FirstBlobPublish,
                        RelPath::join(&RelPath::root(), Family::Blobs.as_str()),
                    )),
                    EntryName::Transaction(_) => Some((
                        Boundary::TransactionPublish,
                        RelPath::join(&RelPath::root(), Family::Transactions.as_str()),
                    )),
                    EntryName::Manifest { .. } => Some((
                        Boundary::ManifestPublish,
                        RelPath::join(&RelPath::root(), Family::Manifests.as_str()),
                    )),
                    _ => None,
                };
                if let Some((matched_boundary, expected_rel)) = expected_dir
                    && dir_rel == expected_rel
                    && st.target == matched_boundary
                {
                    let rec = FiredOpRecord {
                        method: "publish_no_overwrite",
                        dir: dir_rel,
                        name: dst.to_file_name(),
                        replace_current_completed: st.replace_current_completed,
                        root_sync_after_replace_completed: st.root_sync_after_replace_completed,
                    };
                    st.triggered = true;
                    st.fired = Some(rec.clone());
                    Some((rec, st.barrier_mode.clone()))
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };
        if let Some((rec, mode)) = trigger {
            self.handle_boundary(&rec, &mode);
        }
        res
    }

    fn replace_current(
        &self,
        staged_dir: &Self::Dir,
        staged: EntryName,
        root: &Self::Dir,
    ) -> Result<(), FsError> {
        let res = self.inner.replace_current(staged_dir, staged, root);
        let trigger = if res.is_ok() {
            let mut st = self.state.borrow_mut();
            if st.armed {
                let dir_rel = self
                    .inner
                    .dir_path(*root)
                    .expect("replace_current succeeded on root with unknown dir_path");
                if dir_rel == RelPath::root() && staged == EntryName::Root(RootName::CurrentTmp) {
                    st.replace_current_completed = true;
                    if !st.triggered && st.target == Boundary::ReplaceCurrent {
                        let rec = FiredOpRecord {
                            method: "replace_current",
                            dir: dir_rel,
                            name: RootName::Current.as_str().to_owned(),
                            replace_current_completed: st.replace_current_completed,
                            root_sync_after_replace_completed: st.root_sync_after_replace_completed,
                        };
                        st.triggered = true;
                        st.fired = Some(rec.clone());
                        Some((rec, st.barrier_mode.clone()))
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };
        if let Some((rec, mode)) = trigger {
            self.handle_boundary(&rec, &mode);
        }
        res
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
        ledger: &mut dyn SharedLedger,
    ) -> Result<HashReport, FsError> {
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
        self.inner.remove(dir, name)
    }
}

pub(crate) fn run_producer(boundary: Boundary, case_dir: &Path) -> Result<(), String> {
    let model_fs = Rc::new(ModelFileSystem::new(ModelCapabilities::full()));
    let observer = BoundaryObserverFs::new_record_only(Rc::clone(&model_fs), boundary);
    let ctl = RunToCompletion;
    let mut ledger = AttemptLedger::open(fixture::BYTES, fixture::ENTRIES, &ctl)
        .map_err(|e| format!("open AttemptLedger: {e:?}"))?;

    let mut store = bootstrap(
        observer.clone(),
        Path::new(fixture::ROOT),
        CreateDir::IfMissing,
        fixture::kb(),
        fixture::RULES,
        &Limits::V1,
        &mut ledger,
    )
    .map_err(|e| format!("bootstrap failed: {e:?}"))?;

    if store.head() != Some(1) {
        return Err(format!(
            "bootstrap must commit version 1, got head={:?}",
            store.head()
        ));
    }

    let genesis_rows: Vec<StateEntry> = store.retained_entries().cloned().collect();
    if genesis_rows.is_empty() {
        return Err("bootstrap retained_entries must not be empty".into());
    }

    match store.commit(fixture::carrier(2), &Limits::V1, &mut ledger) {
        HandleOutcome::Committed { version: 2, .. } => {}
        other => {
            return Err(format!(
                "base-2 commit must return Committed(2), got {other:?}"
            ));
        }
    }
    if store.head() != Some(2) {
        return Err(format!(
            "after base-2 commit expected head=Some(2), got {:?}",
            store.head()
        ));
    }

    if observer.is_armed() {
        return Err("observer must remain unarmed through Genesis and base-2".into());
    }
    if observer.fired_record().is_some() {
        return Err("observer fired prematurely before arming".into());
    }

    observer.arm_for_export(case_dir.to_path_buf(), genesis_rows);

    let outcome3 = store.commit(fixture::carrier(3), &Limits::V1, &mut ledger);
    if !boundary.is_after_ack() {
        return Err(format!(
            "unacked boundary {} returned from Store::commit without blocking (outcome={outcome3:?})",
            boundary.as_str()
        ));
    }

    match outcome3 {
        HandleOutcome::Committed { version: 3, .. } => {}
        other => {
            return Err(format!(
                "after_ack candidate-3 must return Committed(3), got {other:?}"
            ));
        }
    }
    if store.head() != Some(3) {
        return Err(format!(
            "after_ack expected store.head() == Some(3), got {:?}",
            store.head()
        ));
    }
    if !observer.replace_current_completed() {
        return Err("after_ack committed without recorded replace_current completion".into());
    }
    if !observer.root_sync_after_replace_completed() {
        return Err(
            "after_ack committed without recorded root sync_dir after replace_current".into(),
        );
    }

    {
        let mut out = io::stdout().lock();
        writeln!(out, "ACK_TARGET=3").map_err(|e| format!("write ACK_TARGET=3: {e}"))?;
        out.flush()
            .map_err(|e| format!("flush ACK_TARGET=3: {e}"))?;
    }

    let rec = FiredOpRecord {
        method: "after_ack",
        dir: RelPath::root(),
        name: "candidate-3".to_owned(),
        replace_current_completed: true,
        root_sync_after_replace_completed: true,
    };
    observer.export_post_ack_and_block(&rec)?;
    Err("after_ack returned unexpectedly after block".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sage_db_store_tests::faults::{FaultPoint, FaultSchedule, Injection};

    #[test]
    fn unarmed_observer_ignores_bootstrap_and_base2_then_arms_cleanly() {
        let model_fs = Rc::new(ModelFileSystem::new(ModelCapabilities::full()));
        let observer =
            BoundaryObserverFs::new_record_only(Rc::clone(&model_fs), Boundary::RootSyncDir);
        let ctl = RunToCompletion;
        let mut ledger = AttemptLedger::open(fixture::BYTES, fixture::ENTRIES, &ctl).unwrap();

        let mut store = bootstrap(
            observer.clone(),
            Path::new(fixture::ROOT),
            CreateDir::IfMissing,
            fixture::kb(),
            fixture::RULES,
            &Limits::V1,
            &mut ledger,
        )
        .unwrap();
        assert!(!observer.is_armed());
        assert!(observer.fired_record().is_none());

        match store.commit(fixture::carrier(2), &Limits::V1, &mut ledger) {
            HandleOutcome::Committed { version: 2, .. } => {}
            other => panic!("base-2 failed: {other:?}"),
        }
        assert!(!observer.is_armed());
        assert!(observer.fired_record().is_none());
        assert!(!observer.replace_current_completed());
        assert!(!observer.root_sync_after_replace_completed());

        observer.arm_record_only();
        match store.commit(fixture::carrier(3), &Limits::V1, &mut ledger) {
            HandleOutcome::Committed { version: 3, .. } => {}
            other => panic!("candidate-3 failed: {other:?}"),
        }
        let rec = observer.fired_record().expect("RootSyncDir must fire");
        assert_eq!(rec.method, "sync_dir");
        assert_eq!(rec.dir, RelPath::root());
        assert!(rec.replace_current_completed);
        assert!(rec.root_sync_after_replace_completed);
    }

    #[test]
    fn failed_inner_operations_never_fire_completed_op_hooks() {
        for (boundary, point) in [
            (Boundary::FirstCandidateWrite, FaultPoint::Write),
            (Boundary::FirstSyncFile, FaultPoint::SyncFile),
            (Boundary::FirstBlobPublish, FaultPoint::Publish),
            (Boundary::ReplaceCurrent, FaultPoint::ReplaceCurrent),
            (Boundary::RootSyncDir, FaultPoint::SyncDir),
        ] {
            let model_fs = Rc::new(ModelFileSystem::new(ModelCapabilities::full()));
            let observer = BoundaryObserverFs::new_record_only(Rc::clone(&model_fs), boundary);
            let ctl = RunToCompletion;
            let mut ledger = AttemptLedger::open(fixture::BYTES, fixture::ENTRIES, &ctl).unwrap();

            let mut store = bootstrap(
                observer.clone(),
                Path::new(fixture::ROOT),
                CreateDir::IfMissing,
                fixture::kb(),
                fixture::RULES,
                &Limits::V1,
                &mut ledger,
            )
            .unwrap();
            match store.commit(fixture::carrier(2), &Limits::V1, &mut ledger) {
                HandleOutcome::Committed { version: 2, .. } => {}
                other => panic!("base-2 failed: {other:?}"),
            }

            if boundary == Boundary::RootSyncDir {
                let root = observer.open_root(Path::new(fixture::ROOT)).unwrap();
                let staged = observer
                    .create_new(&root, EntryName::Root(RootName::CurrentTmp))
                    .unwrap();
                let _ = staged;
                observer
                    .replace_current(&root, EntryName::Root(RootName::CurrentTmp), &root)
                    .unwrap();
            }

            observer.arm_record_only();
            *model_fs.faults_handle().borrow_mut() =
                FaultSchedule::none().at(point, Injection::FailNoEffect, 1);

            match boundary {
                Boundary::FirstCandidateWrite => {
                    let root = observer.open_root(Path::new(fixture::ROOT)).unwrap();
                    let staging = observer
                        .open_family(&root, Family::Staging, CreateDir::Never)
                        .unwrap();
                    let mut wf = observer
                        .create_new(
                            &staging,
                            EntryName::Staging {
                                kind: sage_db_store::names::StagingKind::Blob,
                                id: sage_db_store::names::Id128::from_bytes([0xee; 16]),
                            },
                        )
                        .unwrap();
                    assert!(observer.write(&mut wf, b"x").is_err());
                }
                Boundary::FirstSyncFile => {
                    let root = observer.open_root(Path::new(fixture::ROOT)).unwrap();
                    let staging = observer
                        .open_family(&root, Family::Staging, CreateDir::Never)
                        .unwrap();
                    let wf = observer
                        .create_new(
                            &staging,
                            EntryName::Staging {
                                kind: sage_db_store::names::StagingKind::Blob,
                                id: sage_db_store::names::Id128::from_bytes([0xef; 16]),
                            },
                        )
                        .unwrap();
                    assert!(observer.sync_file(&wf).is_err());
                }
                Boundary::FirstBlobPublish => {
                    let root = observer.open_root(Path::new(fixture::ROOT)).unwrap();
                    let staging = observer
                        .open_family(&root, Family::Staging, CreateDir::Never)
                        .unwrap();
                    let blobs = observer
                        .open_family(&root, Family::Blobs, CreateDir::Never)
                        .unwrap();
                    let mut wf = observer
                        .create_new(
                            &staging,
                            EntryName::Staging {
                                kind: sage_db_store::names::StagingKind::Blob,
                                id: sage_db_store::names::Id128::from_bytes([0xf0; 16]),
                            },
                        )
                        .unwrap();
                    observer.write(&mut wf, b"abc").unwrap();
                    let digest = sage_db_format::digest::raw_sha256(b"abc");
                    let expected = HashReport {
                        len: 3,
                        sha256: digest,
                    };
                    assert!(
                        observer
                            .publish_no_overwrite(
                                &staging,
                                EntryName::Staging {
                                    kind: sage_db_store::names::StagingKind::Blob,
                                    id: sage_db_store::names::Id128::from_bytes([0xf0; 16]),
                                },
                                &blobs,
                                EntryName::Blob(digest),
                                &expected,
                                &mut ledger,
                            )
                            .is_err()
                    );
                }
                Boundary::ReplaceCurrent => {
                    let root = observer.open_root(Path::new(fixture::ROOT)).unwrap();
                    let _ = observer
                        .create_new(&root, EntryName::Root(RootName::CurrentTmp))
                        .unwrap();
                    assert!(
                        observer
                            .replace_current(&root, EntryName::Root(RootName::CurrentTmp), &root)
                            .is_err()
                    );
                    assert!(!observer.replace_current_completed());
                }
                Boundary::RootSyncDir => {
                    let root = observer.open_root(Path::new(fixture::ROOT)).unwrap();
                    assert!(observer.sync_dir(&root).is_err());
                    assert!(!observer.root_sync_after_replace_completed());
                }
                _ => unreachable!(),
            }

            assert!(
                observer.fired_record().is_none(),
                "failed op for {boundary:?} must not trigger hook"
            );
            model_fs.assert_all_faults_fired();
        }
    }

    #[test]
    fn root_sync_dir_requires_relpath_root_and_prior_replace_current() {
        let model_fs = Rc::new(ModelFileSystem::new(ModelCapabilities::full()));
        let observer =
            BoundaryObserverFs::new_record_only(Rc::clone(&model_fs), Boundary::RootSyncDir);
        let ctl = RunToCompletion;
        let mut ledger = AttemptLedger::open(fixture::BYTES, fixture::ENTRIES, &ctl).unwrap();

        let _store = bootstrap(
            observer.clone(),
            Path::new(fixture::ROOT),
            CreateDir::IfMissing,
            fixture::kb(),
            fixture::RULES,
            &Limits::V1,
            &mut ledger,
        )
        .unwrap();

        observer.arm_record_only();
        let root = observer.open_root(Path::new(fixture::ROOT)).unwrap();
        let blobs = observer
            .open_family(&root, Family::Blobs, CreateDir::Never)
            .unwrap();

        observer.sync_dir(&blobs).unwrap();
        assert!(observer.fired_record().is_none());

        observer.sync_dir(&root).unwrap();
        assert!(
            observer.fired_record().is_none(),
            "root sync_dir before replace_current must not fire RootSyncDir"
        );
        assert!(!observer.root_sync_after_replace_completed());

        let _ = observer
            .create_new(&root, EntryName::Root(RootName::CurrentTmp))
            .unwrap();
        observer
            .replace_current(&root, EntryName::Root(RootName::CurrentTmp), &root)
            .unwrap();
        assert!(observer.replace_current_completed());
        assert!(observer.fired_record().is_none());

        observer.sync_dir(&root).unwrap();
        assert!(observer.root_sync_after_replace_completed());
        let rec = observer
            .fired_record()
            .expect("root sync_dir after replace_current must fire");
        assert_eq!(rec.method, "sync_dir");
        assert_eq!(rec.dir, RelPath::root());
    }

    #[test]
    fn export_bounds_enforce_exact_ceiling_and_nonempty_invariants() {
        assert!(check_export_bounds(4096, 4096, MAX_EXPORTED_WORLDS).is_ok());
        assert!(check_export_bounds(4097, 4097, MAX_EXPORTED_WORLDS).is_err());
        assert!(check_export_bounds(0, 0, MAX_EXPORTED_WORLDS).is_err());
        assert!(check_export_bounds(2, 1, MAX_EXPORTED_WORLDS).is_err());
    }
}
