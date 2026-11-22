//! A test-local observer around the model filesystem.
//!
//! It counts the seam operations that could make `verify` more than read-only: writing, publishing, replacing CURRENT, syncing a
//! file or a directory. The counts are the measurement; comparing final values cannot distinguish "did nothing" from "wrote the
//! same bytes back".

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use sage_db_store::fs::{
    Capabilities, CreateDir, Each, Family, FileSystem, FsError, HashReport, Published, WriteOutcome,
};
use sage_db_store::names::EntryName;
use sage_db_store_tests::faults::{FaultPoint, FaultSchedule};
use sage_db_store_tests::model::fs_model::ModelFileSystem;
use sage_db_types::ledger::SharedLedger;

/// Mutating/syncing seam operations observed.
#[derive(Default, Debug)]
pub(crate) struct SeamCounts {
    /// `create_new` calls.
    pub(crate) create_new: usize,
    /// `write` calls.
    pub(crate) write: usize,
    /// `publish_no_overwrite` calls.
    pub(crate) publish: usize,
    /// `replace_current` calls.
    pub(crate) replace_current: usize,
    /// `sync_entry` calls (a file-content flush).
    pub(crate) sync_entry: usize,
    /// `sync_dir` calls.
    pub(crate) sync_dir: usize,
    /// `sync_file` calls.
    pub(crate) sync_file: usize,
    /// `remove` calls.
    pub(crate) remove: usize,
    /// `sync_parent_of` calls.
    pub(crate) sync_parent_of: usize,
}

impl SeamCounts {
    /// Every counted operation at once, for a single "nothing happened" assertion.
    pub(crate) fn total(&self) -> usize {
        self.create_new
            + self.write
            + self.publish
            + self.replace_current
            + self.sync_entry
            + self.sync_dir
            + self.sync_file
            + self.remove
            + self.sync_parent_of
    }
}

#[derive(Clone)]
struct ScheduledFault {
    point: FaultPoint,
    occurrence: usize,
    schedule: FaultSchedule,
}

/// A filesystem that forwards everything and counts the mutating/syncing subset.
#[derive(Clone)]
pub(crate) struct ObservingFs {
    inner: Rc<ModelFileSystem>,
    counts: Rc<RefCell<SeamCounts>>,
    scheduled: Option<ScheduledFault>,
}

impl ObservingFs {
    pub(crate) fn new(inner: Rc<ModelFileSystem>) -> Self {
        Self {
            inner,
            counts: Rc::new(RefCell::new(SeamCounts::default())),
            scheduled: None,
        }
    }

    /// Arm the model's fault schedule just before forwarding the selected call.
    #[allow(dead_code)]
    pub(crate) fn fault_at(
        mut self,
        point: FaultPoint,
        occurrence: usize,
        schedule: FaultSchedule,
    ) -> Self {
        assert!(occurrence > 0);
        self.scheduled = Some(ScheduledFault {
            point,
            occurrence,
            schedule,
        });
        self
    }

    fn record(&self, point: FaultPoint, counter: fn(&mut SeamCounts) -> &mut usize) {
        let mut counts = self.counts.borrow_mut();
        let count = counter(&mut counts);
        *count += 1;
        if let Some(fault) = &self.scheduled
            && fault.point == point
            && fault.occurrence == *count
        {
            *self.inner.faults_handle().borrow_mut() = fault.schedule.clone();
        }
    }

    pub(crate) fn counts(&self) -> Rc<RefCell<SeamCounts>> {
        Rc::clone(&self.counts)
    }
}

impl FileSystem for ObservingFs {
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
        self.counts.borrow_mut().create_new += 1;
        self.inner.create_root(abs)
    }
    fn sync_parent_of(&self, abs: &Path) -> Result<(), FsError> {
        self.record(FaultPoint::SyncParentOf, |counts| {
            &mut counts.sync_parent_of
        });
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
        self.record(FaultPoint::SyncDir, |counts| &mut counts.sync_dir);
        self.inner.sync_dir(dir)
    }
    fn device_id(&self, dir: &Self::Dir) -> Result<u64, FsError> {
        self.inner.device_id(dir)
    }
    fn try_lock_exclusive(&self, root: &Self::Dir) -> Result<Self::Lock, FsError> {
        self.inner.try_lock_exclusive(root)
    }
    fn create_new(&self, dir: &Self::Dir, name: EntryName) -> Result<Self::WriteFile, FsError> {
        self.record(FaultPoint::CreateNew, |counts| &mut counts.create_new);
        self.inner.create_new(dir, name)
    }
    fn write(&self, file: &mut Self::WriteFile, buf: &[u8]) -> Result<WriteOutcome, FsError> {
        self.record(FaultPoint::Write, |counts| &mut counts.write);
        self.inner.write(file, buf)
    }
    fn sync_file(&self, file: &Self::WriteFile) -> Result<(), FsError> {
        self.record(FaultPoint::SyncFile, |counts| &mut counts.sync_file);
        self.inner.sync_file(file)
    }
    fn sync_entry(&self, dir: &Self::Dir, name: EntryName) -> Result<(), FsError> {
        self.record(FaultPoint::SyncEntry, |counts| &mut counts.sync_entry);
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
        self.counts.borrow_mut().publish += 1;
        self.inner
            .publish_no_overwrite(staged_dir, staged, dst_dir, dst, expected, ledger)
    }
    fn replace_current(
        &self,
        staged_dir: &Self::Dir,
        staged: EntryName,
        root: &Self::Dir,
    ) -> Result<(), FsError> {
        self.counts.borrow_mut().replace_current += 1;
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
        self.counts.borrow_mut().remove += 1;
        self.inner.remove(dir, name)
    }
}
