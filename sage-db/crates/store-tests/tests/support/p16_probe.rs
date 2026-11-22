// Shared fault-injection probe for the P1-6 matrix targets.
//
// ROOT derivative: all thirteen points emit facts; WriteFile facts follow the handle returned by real create_new.
// Each test target compiles this module separately, hence the `dead_code` allowance below; every item is
// exercised by at least one target.

#![allow(dead_code)]

use sage_db_store::fs::{
    Capabilities, CreateDir, Each, Family, FileSystem, FsError, HashReport, Published, WriteOutcome,
};
use sage_db_store::lease::AttemptLedger;
use sage_db_store::names::EntryName;
use sage_db_store_tests::faults::{FaultPoint, FaultSchedule, Injection};
use sage_db_store_tests::model::RelPath;
use sage_db_store_tests::model::fs_model::ModelFileSystem;
use sage_db_types::framing::Limits;
use sage_db_types::query::RunToCompletion;
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::path::Path;
use std::rc::Rc;

use crate::p16_trace;

/// A finite ledger for decoding the visible CURRENT during classification.
fn probe_ledger() -> sage_db_store::lease::AttemptLedger<'static> {
    static CONTROL: RunToCompletion = RunToCompletion;
    AttemptLedger::open(1 << 26, 1 << 20, &CONTROL).expect("a finite ledger opens")
}

type InstalledFact = (String, String, String, Option<u64>);

pub(crate) struct Probe {
    pub(crate) inner: Rc<ModelFileSystem>,
    point: FaultPoint,
    nth: usize,
    injection: Injection,
    counting: Rc<Cell<bool>>,
    quota_remaining: Rc<Cell<Option<usize>>>,
    pub(crate) quota_denials: Rc<Cell<usize>>,
    pub(crate) quota_written: Rc<Cell<usize>>,
    seen: Rc<RefCell<usize>>,
    trace: p16_trace::Trace,
    files: Rc<RefCell<BTreeMap<u64, (sage_db_store_tests::model::fs_model::DirHandle, String)>>>,
    pub(crate) installed_at: Rc<RefCell<Option<InstalledFact>>>,
    /// Factual (method, real dir path, optional entry name) log from `dir_path`.
    pub(crate) facts: Rc<RefCell<Vec<(String, String, String)>>>,
    /// Candidate version the visible CURRENT must show before the injection is installed.
    require_visible_version: Rc<Cell<Option<u64>>>,
    /// Whether the injection was installed (i.e. the preconditions were observed).
    pub(crate) installed: Rc<Cell<bool>>,
}

impl Clone for Probe {
    fn clone(&self) -> Self {
        Self {
            inner: Rc::clone(&self.inner),
            point: self.point,
            nth: self.nth,
            injection: self.injection,
            counting: Rc::clone(&self.counting),
            quota_remaining: Rc::clone(&self.quota_remaining),
            quota_denials: Rc::clone(&self.quota_denials),
            quota_written: Rc::clone(&self.quota_written),
            seen: Rc::clone(&self.seen),
            trace: self.trace.clone(),
            files: Rc::clone(&self.files),
            installed_at: Rc::clone(&self.installed_at),
            facts: Rc::clone(&self.facts),
            require_visible_version: Rc::clone(&self.require_visible_version),
            installed: Rc::clone(&self.installed),
        }
    }
}

impl Probe {
    pub(crate) fn new(
        inner: Rc<ModelFileSystem>,
        point: FaultPoint,
        nth: usize,
        injection: Injection,
    ) -> Self {
        Self {
            inner,
            point,
            nth,
            injection,
            counting: Rc::new(Cell::new(false)),
            quota_remaining: Rc::new(Cell::new(None)),
            quota_denials: Rc::new(Cell::new(0)),
            quota_written: Rc::new(Cell::new(0)),
            seen: Rc::new(RefCell::new(0)),
            trace: p16_trace::Trace::new(),
            files: Rc::new(RefCell::new(BTreeMap::new())),
            installed_at: Rc::new(RefCell::new(None)),
            facts: Rc::new(RefCell::new(Vec::new())),
            require_visible_version: Rc::new(Cell::new(None)),
            installed: Rc::new(Cell::new(false)),
        }
    }

    pub(crate) fn arm_from_here(&self, require_visible_version: Option<u64>) {
        *self.seen.borrow_mut() = 0;
        self.facts.borrow_mut().clear();
        self.require_visible_version.set(require_visible_version);
        self.counting.set(true);
    }

    pub(crate) fn arm_write_quota(&self, bytes: usize) {
        self.arm_from_here(None);
        self.quota_remaining.set(Some(bytes));
    }

    /// The visible `CURRENT` version, decoded from the model's VISIBLE bytes.
    pub(crate) fn visible_current_version(&self) -> Option<u64> {
        let model = self.inner.model();
        let ino = model.inode_of(&RelPath::root(), "CURRENT")?;
        let bytes = model.visible_bytes(ino)?.to_vec();
        drop(model);
        let mut l = probe_ledger();
        match sage_db_format::current::decode_current(&bytes, &Limits::V1, &mut l) {
            sage_db_types::framing::FormatOutcome::Ok(current) => Some(current.version),
            _ => None,
        }
    }

    /// Log the FACTUAL path of a directory handle, straight from the model.
    pub(crate) fn fact(
        &self,
        method: &str,
        dir: Option<&sage_db_store_tests::model::fs_model::DirHandle>,
        name: &str,
    ) -> String {
        let path = dir
            .and_then(|handle| self.inner.dir_path(*handle))
            .map(|p| p.as_str().to_owned())
            .unwrap_or_default();
        self.facts
            .borrow_mut()
            .push((method.to_owned(), path.clone(), name.to_owned()));
        path
    }

    fn tick(&self, point: FaultPoint) {
        if point != self.point || !self.counting.get() {
            return;
        }
        let mut seen = self.seen.borrow_mut();
        *seen += 1;
        if self.installed.get() || *seen < self.nth {
            return;
        }
        // Classification, not a guessed ordinal: the step only legitimately fires once the visible CURRENT
        // already names the candidate version, i.e. replace_current has taken effect. If this occurrence comes
        // earlier, it is NOT the boundary: withhold rather than install at the wrong step, and keep looking.
        if let Some(required) = self.require_visible_version.get() {
            let facts = self.facts.borrow();
            let last = facts.last().expect("current operation fact");
            if point != FaultPoint::SyncDir
                || !last.1.is_empty()
                || self.visible_current_version() != Some(required)
            {
                return;
            }
        }
        let last = self
            .facts
            .borrow()
            .last()
            .cloned()
            .expect("every scheduled point emits a fact");
        *self.installed_at.borrow_mut() =
            Some((last.0, last.1, last.2, self.visible_current_version()));
        *self.inner.faults_handle().borrow_mut() =
            FaultSchedule::none().at(self.point, self.injection, 1);
        self.installed.set(true);
    }
}

impl FileSystem for Probe {
    type Dir = sage_db_store_tests::model::fs_model::DirHandle;
    type WriteFile = sage_db_store_tests::model::fs_model::WriteHandle;
    type ReadFile = sage_db_store_tests::model::fs_model::ReadHandle;
    type Lock = sage_db_store_tests::model::fs_model::LockHandle;

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
        self.fact(
            "sync_parent_of",
            None,
            abs.to_str().expect("fixture absolute path"),
        );
        self.trace.push("sync_parent_of", "", "");
        self.tick(FaultPoint::SyncParentOf);
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
        let path = self.fact("sync_dir", Some(dir), "");
        self.trace.push("sync_dir", "", &path);
        self.tick(FaultPoint::SyncDir);
        self.inner.sync_dir(dir)
    }
    fn device_id(&self, dir: &Self::Dir) -> Result<u64, FsError> {
        self.inner.device_id(dir)
    }
    fn try_lock_exclusive(&self, root: &Self::Dir) -> Result<Self::Lock, FsError> {
        self.trace.push("try_lock_exclusive", "", "");
        self.inner.try_lock_exclusive(root)
    }
    fn create_new(&self, dir: &Self::Dir, name: EntryName) -> Result<Self::WriteFile, FsError> {
        let path = self.fact("create_new", Some(dir), &name.to_file_name());
        self.trace.push("create_new", &name.to_file_name(), &path);
        self.tick(FaultPoint::CreateNew);
        let file = self.inner.create_new(dir, name)?;
        self.files
            .borrow_mut()
            .insert(file.0, (*dir, name.to_file_name()));
        Ok(file)
    }
    fn write(&self, file: &mut Self::WriteFile, buf: &[u8]) -> Result<WriteOutcome, FsError> {
        let (dir, name) = self
            .files
            .borrow()
            .get(&file.0)
            .cloned()
            .expect("real create_new mapped this write handle");
        self.fact("write", Some(&dir), &name);
        self.trace.push("write", &name, "");
        self.tick(FaultPoint::Write);
        if let Some(remaining) = self.quota_remaining.get() {
            if remaining == 0 {
                self.quota_denials.set(self.quota_denials.get() + 1);
                return Err(FsError::no_effect(sage_db_store::fs::FsErrorKind::NoSpace));
            }
            let report = self.inner.write(file, &buf[..buf.len().min(remaining)])?;
            self.quota_remaining.set(Some(remaining - report.written));
            self.quota_written
                .set(self.quota_written.get() + report.written);
            return Ok(report);
        }
        self.inner.write(file, buf)
    }
    fn sync_file(&self, file: &Self::WriteFile) -> Result<(), FsError> {
        let (dir, name) = self
            .files
            .borrow()
            .get(&file.0)
            .cloned()
            .expect("real create_new mapped this write handle");
        self.fact("sync_file", Some(&dir), &name);
        self.trace.push("sync_file", &name, "");
        self.tick(FaultPoint::SyncFile);
        self.inner.sync_file(file)
    }
    fn sync_entry(&self, dir: &Self::Dir, name: EntryName) -> Result<(), FsError> {
        let path = self.fact("sync_entry", Some(dir), &name.to_file_name());
        self.trace.push("sync_entry", &name.to_file_name(), &path);
        self.tick(FaultPoint::SyncEntry);
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
        let path = self.fact("publish_no_overwrite", Some(dst_dir), &dst.to_file_name());
        self.trace
            .push("publish_no_overwrite", &dst.to_file_name(), &path);
        self.tick(FaultPoint::Publish);
        self.inner
            .publish_no_overwrite(staged_dir, staged, dst_dir, dst, expected, ledger)
    }
    fn replace_current(
        &self,
        staged_dir: &Self::Dir,
        staged: EntryName,
        root: &Self::Dir,
    ) -> Result<(), FsError> {
        let path = self.fact("replace_current", Some(root), &staged.to_file_name());
        self.trace
            .push("replace_current", &staged.to_file_name(), &path);
        self.tick(FaultPoint::ReplaceCurrent);
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
        self.fact("read_exact_into", Some(dir), &name.to_file_name());
        self.trace.push("read_exact_into", &name.to_file_name(), "");
        self.tick(FaultPoint::ReadExact);
        self.inner.read_exact_into(dir, name, buf)
    }
    fn open_read(&self, dir: &Self::Dir, name: EntryName) -> Result<Self::ReadFile, FsError> {
        self.fact("open_read", Some(dir), &name.to_file_name());
        self.trace.push("open_read", &name.to_file_name(), "");
        self.tick(FaultPoint::OpenRead);
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
        self.fact("hash_bounded", Some(dir), &name.to_file_name());
        self.trace.push("hash_bounded", &name.to_file_name(), "");
        self.tick(FaultPoint::Hash);
        self.inner.hash_bounded(dir, name, max_len, ledger)
    }
    fn for_each_entry(
        &self,
        dir: &Self::Dir,
        ledger: &mut dyn sage_db_types::ledger::SharedLedger,
        visit: &mut dyn FnMut(&str) -> Each,
    ) -> Result<(), FsError> {
        let path = self.fact("for_each_entry", Some(dir), "");
        self.trace.push("for_each_entry", "", &path);
        self.tick(FaultPoint::Enumerate);
        self.inner.for_each_entry(dir, ledger, visit)
    }
    fn remove(&self, dir: &Self::Dir, name: EntryName) -> Result<(), FsError> {
        let path = self.fact("remove", Some(dir), &name.to_file_name());
        self.trace.push("remove", &name.to_file_name(), &path);
        self.tick(FaultPoint::Remove);
        self.inner.remove(dir, name)
    }
}
