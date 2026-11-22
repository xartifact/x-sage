//! Real `Store` + real `OsFileSystem` over real files: process-level qualification driver.
//!
//! This binary is a **test harness**, not product code. It consumes the checked
//! [`OsFileSystem::for_root`] constructor exactly as a caller would; it never re-implements
//! policy, never forces a durability claim, and never fabricates a model.
//!
//! Every mode prints structured `KEY=VALUE` lines (propagating stdout write/flush errors rather
//! than ignoring them) and then either exits or blocks, so a parent process can kill it at a
//! physically meaningful moment rather than at a modelled one.
//!
//! Modes:
//!
//! ```text
//! host-probe <root>
//!     Verify OsFileSystem::for_root returns DurabilityBasis::Documented and report host facts.
//! seed <root> --genesis-baseline <path> [--expect-missing-root | --expect-existing-empty]
//!     Bootstrap + base-2 commit; capture the 2 Genesis rows BEFORE committing v2 into <path>
//!     (outside the KB, create_new / wx, root-bound, bounded checksummed framing).
//! retry <root>
//!     Exact retry of base-2 through a pure delegating observer; assert zero named mutating/sync
//!     calls, unchanged CURRENT, exact recorded RequestOutcomeEvent, and AttemptHistory.
//! unacked <root>
//!     Open Ready(2) unarmed, verify inert setup, explicitly arm for candidate-3, block INSIDE
//!     replace_current after the rename succeeds and BEFORE returning to the store.
//! post-ack <root>
//!     Open Ready(2) unarmed, verify inert setup, explicitly arm for candidate-3, commit to outer
//!     Committed(3), require Durable view AND successful root sync_dir AFTER replace_current
//!     before printing ACK3=3 and blocking.
//! reopen <root> --genesis-baseline <path> --candidate3-outcome <none|unknown|durable>
//!     Fresh open of an existing root; reconstruct the root-bound Genesis baseline (fail if missing,
//!     never recreate), compare raw retained_entries() directly against
//!     fixture::expected_rows(&genesis, head), drive oracle::attempts::AttemptHistory from
//!     observed attempts and actual persisted G0 outcomes for BOTH requests 2 & 3, and read the
//!     complete historical blob union (rules + v2..=head declared blobs including superseded A1/B1).
//! lock-holder <root>
//!     Open Ready(2), print lock identity, block holding the real OS lock.
//! lock-refuser <root>
//!     open_existing must refuse with OpenStep::Lock / WouldBlock / NoEffect (exit 42).
//! silent-hold
//!     Pure parent-control helper (no KB/policy): prints one stderr diagnostic and blocks without
//!     ever emitting a READY line, for testing the parent's absolute deadline + kill/reap path.
//! exit-normal
//!     Pure parent-control helper (no KB/policy): exits 0 immediately, for testing kill guards.
//! ready-then-trailing
//!     Pure parent-control helper (no KB/policy): emits PHASE=one, then trailing stdout and stderr
//!     lines, and blocks until killed, proving full post-ready transcript capture.
//! ```
//!
//! Exit codes:
//!   0  success
//!   3  assertion / contract failure inside the driver
//!   42 exclusive-lock refusal (`OpenStep::Lock` + `WouldBlock` + `NoEffect` only)
//!   44 any other open failure
//!   45 host/path is not qualified for a product handle
#![forbid(unsafe_code)]

// Read-only shared fixture also exports P1-6 modeled-root helpers (`ROOT`, `expected_history_a1`)
// not needed by this real-OS driver; `expected_rows`, `expected_outcome`, `carrier`, `RULES`,
// `kb`, `PRINCIPAL`, `BYTES`, and `ENTRIES` are all actively called in this binary.
#[allow(dead_code)]
#[path = "../../tests/support/p16_fixture.rs"]
mod fixture;

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use sage_db_store::commit::RecordedOutcome;
use sage_db_store::fs::{
    Capabilities, CreateDir, Each, Family, FileSystem, FsError, FsErrorKind, HashReport, Published,
    WriteOutcome,
};
use sage_db_store::handle::HandleOutcome;
use sage_db_store::init::bootstrap;
use sage_db_store::lease::AttemptLedger;
use sage_db_store::names::{EntryName, RootName};
use sage_db_store::open::{OpenRefusal, OpenStep, OutcomeLookup, open_existing};
use sage_db_store::os::{DurabilityBasis, OsDir, OsFileSystem, OsLock, OsReadFile, OsWriteFile};
use sage_db_store::view::ViewDurability;
use sage_db_store_tests::oracle::attempts::{
    AttemptHistory, AttemptOutcome, Commit, LogicalRequest, World, find_version_collision,
};
use sage_db_types::framing::Limits;
use sage_db_types::ledger::SharedLedger;
use sage_db_types::persist::{RequestOutcomeEvent, StateEntry};
use sage_db_types::query::RunToCompletion;

const EXIT_OK: i32 = 0;
const EXIT_DRIVER_ASSERT: i32 = 3;
const EXIT_LOCK_REFUSAL: i32 = 42;
const EXIT_OPEN_FAILURE: i32 = 44;
const EXIT_HOST_NOT_QUALIFIED: i32 = 45;

/// Test-only bounded transport framing magic for Genesis preservation baselines (NOT a Sage format).
const BASELINE_MAGIC: &str = "P2OSGEN1";
const MAX_BASELINE_BYTES: usize = 4 * 1024 * 1024;
const MAX_BASELINE_ROWS: usize = 64;
const EXPECTED_GENESIS_ROWS: usize = 2;

// ---------------------------------------------------------------------------
// Observer, arming, and ordered publish/root-sync proof (H3)
// ---------------------------------------------------------------------------

/// Named seam calls that would constitute a *mutation* if repeated by an exact retry.
///
/// The claim under test is narrow and precise: an exact retry of an already-committed request
/// performs no named mutating or sync operation and does not rewrite `CURRENT`. It is **not** a
/// claim of zero I/O, zero allocation or zero reads — the store must read to recognise the retry.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct MutatingCalls {
    create_root: u32,
    open_family_create: u32,
    sync_parent_of: u32,
    sync_dir: u32,
    create_new: u32,
    write: u32,
    sync_file: u32,
    sync_entry: u32,
    publish_no_overwrite: u32,
    replace_current: u32,
    remove: u32,
}

impl MutatingCalls {
    fn total(self) -> u32 {
        self.create_root
            + self.open_family_create
            + self.sync_parent_of
            + self.sync_dir
            + self.create_new
            + self.write
            + self.sync_file
            + self.sync_entry
            + self.publish_no_overwrite
            + self.replace_current
            + self.remove
    }

    fn describe(self) -> String {
        format!(
            "create_root={} open_family_create={} sync_parent_of={} sync_dir={} create_new={} write={} sync_file={} sync_entry={} publish={} replace_current={} remove={}",
            self.create_root,
            self.open_family_create,
            self.sync_parent_of,
            self.sync_dir,
            self.create_new,
            self.write,
            self.sync_file,
            self.sync_entry,
            self.publish_no_overwrite,
            self.replace_current,
            self.remove
        )
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
enum Barrier {
    /// Never blocks inside the observer.
    #[default]
    None,
    /// Block after a successful `replace_current` on the bound root and BEFORE returning to Store.
    AfterReplaceCurrent,
}

/// Physical record of the armed boundary operation that fired inside the observer.
#[derive(Clone, PartialEq, Eq, Debug)]
struct FiredOpRecord {
    method: &'static str,
    dir_path: PathBuf,
    staged_name: String,
    replace_current_completed: bool,
    root_sync_after_replace: bool,
}

/// Pure ordered tracker for publish / root-sync facts on one bound KB root.
///
/// Kept free of `OsDir` so the exact production-harness state transitions can be tested directly
/// by host-independent unit negatives (missing root sync, family-only sync, wrong root path,
/// inverted order, unarmed setup, failed inner calls).
#[derive(Clone, PartialEq, Eq, Debug)]
struct PublishTracker {
    bound_root: PathBuf,
    armed: bool,
    barrier: Barrier,
    replace_current_completed: bool,
    replace_current_ok_count: u32,
    root_sync_before_replace_count: u32,
    root_sync_after_replace: bool,
    root_sync_after_replace_count: u32,
    non_root_sync_after_replace_count: u32,
    fired: Option<FiredOpRecord>,
}

impl PublishTracker {
    fn new(bound_root: PathBuf) -> Self {
        Self {
            bound_root,
            armed: false,
            barrier: Barrier::None,
            replace_current_completed: false,
            replace_current_ok_count: 0,
            root_sync_before_replace_count: 0,
            root_sync_after_replace: false,
            root_sync_after_replace_count: 0,
            non_root_sync_after_replace_count: 0,
            fired: None,
        }
    }

    fn is_bound_root(&self, dir_path: &Path, family: Option<Family>) -> bool {
        family.is_none() && dir_path == self.bound_root.as_path()
    }

    /// Explicitly reset and arm AFTER Ready(2) setup has completed.
    ///
    /// Refuses if already armed or if any prior fired record exists, so setup-phase calls can never
    /// masquerade as candidate-3 publish activity.
    fn reset_and_arm(&mut self, barrier: Barrier) -> Result<(), String> {
        if self.armed {
            return Err("observer is already armed; must start unarmed through setup".into());
        }
        if let Some(prev) = &self.fired {
            return Err(format!(
                "observer already holds a fired record before arming: {prev:?}"
            ));
        }
        self.replace_current_completed = false;
        self.replace_current_ok_count = 0;
        self.root_sync_before_replace_count = 0;
        self.root_sync_after_replace = false;
        self.root_sync_after_replace_count = 0;
        self.non_root_sync_after_replace_count = 0;
        self.fired = None;
        self.barrier = barrier;
        self.armed = true;
        Ok(())
    }

    /// Record the outcome of an inner `replace_current` call.
    ///
    /// Returns `Some(FiredOpRecord)` if and only if the observer is armed with
    /// `Barrier::AfterReplaceCurrent` and the inner call succeeded on the bound root.
    fn record_replace_current(
        &mut self,
        succeeded: bool,
        root_dir_path: &Path,
        root_dir_family: Option<Family>,
        staged_name: &str,
    ) -> Option<FiredOpRecord> {
        if !succeeded || !self.armed || !self.is_bound_root(root_dir_path, root_dir_family) {
            return None;
        }
        self.replace_current_completed = true;
        self.replace_current_ok_count += 1;
        if self.barrier == Barrier::AfterReplaceCurrent && self.fired.is_none() {
            let rec = FiredOpRecord {
                method: "replace_current",
                dir_path: root_dir_path.to_path_buf(),
                staged_name: staged_name.to_owned(),
                replace_current_completed: true,
                root_sync_after_replace: self.root_sync_after_replace,
            };
            self.fired = Some(rec.clone());
            return Some(rec);
        }
        None
    }

    /// Record the outcome of an inner `sync_dir` call.
    ///
    /// Only a **successful** sync of the **bound root** (`family == None && path == bound_root`)
    /// observed **after** a successful armed `replace_current` sets `root_sync_after_replace`.
    fn record_sync_dir(&mut self, succeeded: bool, dir_path: &Path, family: Option<Family>) {
        if !succeeded || !self.armed {
            return;
        }
        if self.is_bound_root(dir_path, family) {
            if self.replace_current_completed {
                self.root_sync_after_replace = true;
                self.root_sync_after_replace_count += 1;
            } else {
                self.root_sync_before_replace_count += 1;
            }
        } else if self.replace_current_completed {
            self.non_root_sync_after_replace_count += 1;
        }
    }
}

/// Validate the physical post-ACK3 preconditions BEFORE emitting `ACK3=3`.
///
/// Requires a `Durable` view, explicit arming, a completed `replace_current` on the bound root,
/// and a subsequent successful `sync_dir` on that same bound root (never merely a cumulative
/// `sync_dir` count over family directories or a pre-replace root sync).
fn validate_post_ack_proof(durable_view: bool, tracker: &PublishTracker) -> Result<(), String> {
    if !durable_view {
        return Err("post-ACK3 requires a Durable store view before emitting ACK3".into());
    }
    if !tracker.armed {
        return Err("post-ACK3 requires observer to have been explicitly armed after setup".into());
    }
    if !tracker.replace_current_completed || tracker.replace_current_ok_count == 0 {
        return Err("post-ACK3 requires a completed replace_current on the bound root".into());
    }
    if !tracker.root_sync_after_replace || tracker.root_sync_after_replace_count == 0 {
        return Err(format!(
            "post-ACK3 requires a successful ROOT sync_dir AFTER replace_current \
             (root_sync_before_replace={}, non_root_sync_after_replace={})",
            tracker.root_sync_before_replace_count, tracker.non_root_sync_after_replace_count
        ));
    }
    Ok(())
}

/// A **pure delegating** observer over the real backend.
///
/// It forwards every call unchanged and records counters + ordered root-sync facts only. It cannot
/// alter an effect, suppress an error, or widen a capability: `capabilities()` is the inner
/// backend's, unsullied. Always constructed **unarmed**; callers explicitly arm after Ready(2).
#[derive(Clone)]
struct CountingFs {
    inner: Rc<OsFileSystem>,
    calls: Rc<RefCell<MutatingCalls>>,
    tracker: Rc<RefCell<PublishTracker>>,
}

impl CountingFs {
    fn new(inner: Rc<OsFileSystem>, root: &Path) -> Self {
        let bound_root = inner
            .validated_root()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| root.to_path_buf());
        Self {
            inner,
            calls: Rc::new(RefCell::new(MutatingCalls::default())),
            tracker: Rc::new(RefCell::new(PublishTracker::new(bound_root))),
        }
    }

    fn calls(&self) -> MutatingCalls {
        *self.calls.borrow()
    }

    fn tracker_snapshot(&self) -> PublishTracker {
        self.tracker.borrow().clone()
    }

    fn reset_and_arm(&self, barrier: Barrier) -> Result<(), String> {
        self.tracker.borrow_mut().reset_and_arm(barrier)
    }

    fn count(&self, f: impl FnOnce(&mut MutatingCalls)) {
        f(&mut self.calls.borrow_mut());
    }
}

impl FileSystem for CountingFs {
    type Dir = OsDir;
    type WriteFile = OsWriteFile;
    type ReadFile = OsReadFile;
    type Lock = OsLock;

    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    fn open_root(&self, abs: &Path) -> Result<Self::Dir, FsError> {
        self.inner.open_root(abs)
    }

    fn create_root(&self, abs: &Path) -> Result<Self::Dir, FsError> {
        self.count(|c| c.create_root += 1);
        self.inner.create_root(abs)
    }

    fn sync_parent_of(&self, abs: &Path) -> Result<(), FsError> {
        self.count(|c| c.sync_parent_of += 1);
        self.inner.sync_parent_of(abs)
    }

    fn open_family(
        &self,
        root: &Self::Dir,
        family: Family,
        create: CreateDir,
    ) -> Result<Self::Dir, FsError> {
        if create == CreateDir::IfMissing {
            self.count(|c| c.open_family_create += 1);
        }
        self.inner.open_family(root, family, create)
    }

    fn sync_dir(&self, dir: &Self::Dir) -> Result<(), FsError> {
        self.count(|c| c.sync_dir += 1);
        let result = self.inner.sync_dir(dir);
        self.tracker
            .borrow_mut()
            .record_sync_dir(result.is_ok(), dir.path(), dir.family());
        result
    }

    fn device_id(&self, dir: &Self::Dir) -> Result<u64, FsError> {
        self.inner.device_id(dir)
    }

    fn try_lock_exclusive(&self, root: &Self::Dir) -> Result<Self::Lock, FsError> {
        self.inner.try_lock_exclusive(root)
    }

    fn create_new(&self, dir: &Self::Dir, name: EntryName) -> Result<Self::WriteFile, FsError> {
        self.count(|c| c.create_new += 1);
        self.inner.create_new(dir, name)
    }

    fn write(&self, file: &mut Self::WriteFile, buf: &[u8]) -> Result<WriteOutcome, FsError> {
        self.count(|c| c.write += 1);
        self.inner.write(file, buf)
    }

    fn sync_file(&self, file: &Self::WriteFile) -> Result<(), FsError> {
        self.count(|c| c.sync_file += 1);
        self.inner.sync_file(file)
    }

    fn sync_entry(&self, dir: &Self::Dir, name: EntryName) -> Result<(), FsError> {
        self.count(|c| c.sync_entry += 1);
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
        self.count(|c| c.publish_no_overwrite += 1);
        self.inner
            .publish_no_overwrite(staged_dir, staged, dst_dir, dst, expected, ledger)
    }

    fn replace_current(
        &self,
        staged_dir: &Self::Dir,
        staged: EntryName,
        root: &Self::Dir,
    ) -> Result<(), FsError> {
        self.count(|c| c.replace_current += 1);
        let staged_name = staged.to_file_name();
        let result = self.inner.replace_current(staged_dir, staged, root);
        let fired = self.tracker.borrow_mut().record_replace_current(
            result.is_ok(),
            root.path(),
            root.family(),
            &staged_name,
        );
        // The barrier sits BETWEEN the successful rename and the store's own root sync/outer ACK,
        // so SIGKILL here leaves the exact unacknowledged publish-window state under test.
        if let Some(rec) = fired {
            emit("BARRIER=after_replace_current");
            emit(&format!("FIRED_METHOD={}", rec.method));
            emit(&format!("FIRED_DIR={}", rec.dir_path.display()));
            emit(&format!("FIRED_STAGED={}", rec.staged_name));
            emit(&format!(
                "REPLACE_CURRENT_COMPLETED={}",
                rec.replace_current_completed
            ));
            emit(&format!(
                "ROOT_SYNC_AFTER_REPLACE={}",
                rec.root_sync_after_replace
            ));
            announce_steady();
        }
        result
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
        self.count(|c| c.remove += 1);
        self.inner.remove(dir, name)
    }
}

// ---------------------------------------------------------------------------
// Test-only bounded Genesis baseline transport framing (H1)
// ---------------------------------------------------------------------------

/// Root-bound Genesis preservation baseline captured BEFORE committing version 2.
///
/// Transported outside the KB in the parent's artifact directory; NOT a new production format.
#[derive(Clone, PartialEq, Eq, Debug)]
struct GenesisBaseline {
    root_bytes: Vec<u8>,
    rows: Vec<StateEntry>,
}

fn bad_data(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

fn hex(bytes: &[u8]) -> String {
    const LUT: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(LUT[(b >> 4) as usize] as char);
        out.push(LUT[(b & 0x0f) as usize] as char);
    }
    out
}

fn hex_or_dash(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        "-".to_owned()
    } else {
        hex(bytes)
    }
}

fn unhex_or_dash(s: &str) -> io::Result<Vec<u8>> {
    if s == "-" {
        return Ok(Vec::new());
    }
    if s.is_empty() || !s.len().is_multiple_of(2) || s.len() > MAX_BASELINE_BYTES * 2 {
        return Err(bad_data("invalid hex length"));
    }
    let raw = s.as_bytes();
    let mut out = Vec::with_capacity(raw.len() / 2);
    for pair in raw.as_chunks::<2>().0 {
        let hi = decode_nibble(pair[0])?;
        let lo = decode_nibble(pair[1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn decode_nibble(byte: u8) -> io::Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(bad_data("non-canonical hex digit")),
    }
}

fn encode_genesis_baseline(root_bytes: &[u8], rows: &[StateEntry]) -> io::Result<Vec<u8>> {
    if root_bytes.is_empty() {
        return Err(bad_data("baseline root binding must not be empty"));
    }
    if rows.len() != EXPECTED_GENESIS_ROWS {
        return Err(bad_data("genesis baseline must contain exactly 2 rows"));
    }
    let mut seen = BTreeSet::new();
    for row in rows {
        if !seen.insert((row.kind, row.key.as_slice())) {
            return Err(bad_data("duplicate row identity in genesis baseline"));
        }
    }
    let mut out = Vec::new();
    writeln!(out, "{BASELINE_MAGIC}")?;
    writeln!(out, "ROOT {} {}", root_bytes.len(), hex_or_dash(root_bytes))?;
    writeln!(out, "ROWS {}", rows.len())?;
    for row in rows {
        writeln!(
            out,
            "R {} {} {} {} {} {}",
            row.kind,
            row.revision,
            row.key.len(),
            hex_or_dash(&row.key),
            row.body.len(),
            hex_or_dash(&row.body)
        )?;
    }
    if out.len() > MAX_BASELINE_BYTES {
        return Err(bad_data("genesis baseline exceeds transport byte ceiling"));
    }
    let digest = sage_db_format::digest::raw_sha256(&out);
    writeln!(out, "END {}", hex(&digest))?;
    Ok(out)
}

fn decode_genesis_baseline(bytes: &[u8]) -> io::Result<GenesisBaseline> {
    if bytes.len() > MAX_BASELINE_BYTES + 80 || !bytes.ends_with(b"\n") {
        return Err(bad_data(
            "baseline exceeds ceiling or misses trailing newline",
        ));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| bad_data("baseline utf8"))?;
    let cut = text
        .rfind("END ")
        .ok_or_else(|| bad_data("missing END footer"))?;
    let footer = &text[cut..];
    let expected_footer = format!(
        "END {}\n",
        hex(&sage_db_format::digest::raw_sha256(&bytes[..cut]))
    );
    if footer != expected_footer {
        return Err(bad_data("baseline checksum mismatch or trailing data"));
    }

    let mut lines = text[..cut].lines();
    if lines.next() != Some(BASELINE_MAGIC) {
        return Err(bad_data("baseline magic mismatch"));
    }

    let root_line = lines.next().ok_or_else(|| bad_data("missing ROOT line"))?;
    let root_parts: Vec<&str> = root_line.split(' ').collect();
    if root_parts.len() != 3 || root_parts[0] != "ROOT" {
        return Err(bad_data("malformed ROOT line"));
    }
    let root_len: usize = root_parts[1]
        .parse()
        .map_err(|_| bad_data("invalid ROOT length"))?;
    let root_bytes = unhex_or_dash(root_parts[2])?;
    if root_bytes.is_empty() || root_bytes.len() != root_len {
        return Err(bad_data("ROOT length mismatch"));
    }

    let count_line = lines.next().ok_or_else(|| bad_data("missing ROWS line"))?;
    let count_parts: Vec<&str> = count_line.split(' ').collect();
    if count_parts.len() != 2 || count_parts[0] != "ROWS" {
        return Err(bad_data("malformed ROWS line"));
    }
    let row_count: usize = count_parts[1]
        .parse()
        .map_err(|_| bad_data("invalid ROWS count"))?;
    if row_count > MAX_BASELINE_ROWS || row_count != EXPECTED_GENESIS_ROWS {
        return Err(bad_data("genesis baseline row count must be exactly 2"));
    }

    let mut rows = Vec::with_capacity(row_count);
    let mut seen = BTreeSet::new();
    for _ in 0..row_count {
        let line = lines.next().ok_or_else(|| bad_data("truncated row list"))?;
        let parts: Vec<&str> = line.split(' ').collect();
        if parts.len() != 7 || parts[0] != "R" {
            return Err(bad_data("malformed row entry"));
        }
        let kind: u16 = parts[1].parse().map_err(|_| bad_data("invalid row kind"))?;
        let revision: u64 = parts[2]
            .parse()
            .map_err(|_| bad_data("invalid row revision"))?;
        let key_len: usize = parts[3]
            .parse()
            .map_err(|_| bad_data("invalid row key length"))?;
        let key = unhex_or_dash(parts[4])?;
        if key.len() != key_len {
            return Err(bad_data("row key length mismatch"));
        }
        let body_len: usize = parts[5]
            .parse()
            .map_err(|_| bad_data("invalid row body length"))?;
        let body = unhex_or_dash(parts[6])?;
        if body.len() != body_len {
            return Err(bad_data("row body length mismatch"));
        }
        if !seen.insert((kind, key.clone())) {
            return Err(bad_data("duplicate row identity in baseline"));
        }
        rows.push(StateEntry {
            kind,
            key,
            revision,
            body,
        });
    }
    if lines.next().is_some() {
        return Err(bad_data("unexpected extra lines in baseline"));
    }

    Ok(GenesisBaseline { root_bytes, rows })
}

fn path_bytes(path: &Path) -> &[u8] {
    path.as_os_str().as_encoded_bytes()
}

fn ensure_outside_kb(baseline_path: &Path, root: &Path) -> Result<(), String> {
    if baseline_path.starts_with(root) {
        return Err(format!(
            "genesis baseline {} must live outside KB root {}",
            baseline_path.display(),
            root.display()
        ));
    }
    Ok(())
}

/// Write a new baseline file with `create_new(true)` (`wx`), never overwriting an existing file.
fn write_genesis_baseline(path: &Path, root: &Path, rows: &[StateEntry]) -> io::Result<()> {
    ensure_outside_kb(path, root).map_err(|e| bad_data(&e))?;
    let bytes = encode_genesis_baseline(path_bytes(root), rows)?;
    let mut out = OpenOptions::new().write(true).create_new(true).open(path)?;
    out.write_all(&bytes)?;
    out.sync_all()?;
    Ok(())
}

/// Read and validate a baseline file bound to `expected_root`. Never recreates if missing.
fn read_genesis_baseline(path: &Path, expected_root: &Path) -> Result<Vec<StateEntry>, String> {
    ensure_outside_kb(path, expected_root)?;
    let file = File::open(path).map_err(|e| {
        format!(
            "missing or unreadable genesis baseline {}: {e}",
            path.display()
        )
    })?;
    let mut bytes = Vec::new();
    file.take((MAX_BASELINE_BYTES + 81) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("read genesis baseline {}: {e}", path.display()))?;
    let decoded = decode_genesis_baseline(&bytes)
        .map_err(|e| format!("decode genesis baseline {}: {e}", path.display()))?;
    if decoded.root_bytes.as_slice() != path_bytes(expected_root) {
        return Err(format!(
            "genesis baseline {} is bound to a different root than {}",
            path.display(),
            expected_root.display()
        ));
    }
    Ok(decoded.rows)
}

// ---------------------------------------------------------------------------
// Whole-state raw row comparison (H1)
// ---------------------------------------------------------------------------

/// Compare raw recovered `StateEntry` rows directly against `fixture::expected_rows(genesis, head)`.
///
/// Never sorts, deduplicates, or key-searches `actual_rows`. Full `StateEntry` equality covers
/// `kind`, `key`, `revision`, and `body` (including all audit fields and opaque extensions).
fn validate_retained_rows(
    actual_rows: &[StateEntry],
    genesis_baseline: &[StateEntry],
    head: u64,
) -> Result<usize, String> {
    if genesis_baseline.len() != EXPECTED_GENESIS_ROWS {
        return Err(format!(
            "captured genesis baseline must have exactly {EXPECTED_GENESIS_ROWS} rows, got {}",
            genesis_baseline.len()
        ));
    }
    if !matches!(head, 2 | 3) {
        return Err(format!(
            "validate_retained_rows requires head 2 or 3, got {head}"
        ));
    }
    let mut seen = BTreeSet::new();
    for (idx, row) in actual_rows.iter().enumerate() {
        if !seen.insert((row.kind, row.key.as_slice())) {
            return Err(format!(
                "duplicate current identity in actual retained rows at index {idx}: kind={} key={}",
                row.kind,
                hex(&row.key)
            ));
        }
    }
    let expected = fixture::expected_rows(genesis_baseline, head);
    if actual_rows.len() != expected.len() {
        return Err(format!(
            "retained row count mismatch for head={head}: actual={}, expected={}",
            actual_rows.len(),
            expected.len()
        ));
    }
    for (idx, (actual, exp)) in actual_rows.iter().zip(expected.iter()).enumerate() {
        if actual != exp {
            return Err(format!(
                "retained row mismatch at index {idx} (head={head}): \
                 actual(kind={} rev={} key={} body_len={} body_sha={}) != \
                 expected(kind={} rev={} key={} body_len={} body_sha={})",
                actual.kind,
                actual.revision,
                hex(&actual.key),
                actual.body.len(),
                hex(&sage_db_format::digest::raw_sha256(&actual.body)),
                exp.kind,
                exp.revision,
                hex(&exp.key),
                exp.body.len(),
                hex(&sage_db_format::digest::raw_sha256(&exp.body)),
            ));
        }
    }
    // Explicit preservation check: both captured Genesis rows survive byte-identically.
    for (g_idx, g_row) in genesis_baseline.iter().enumerate() {
        if !actual_rows.iter().any(|r| r == g_row) {
            return Err(format!(
                "captured genesis row {g_idx} (kind={} key={}) was not preserved byte-identically",
                g_row.kind,
                hex(&g_row.key)
            ));
        }
    }
    Ok(expected.len())
}

// ---------------------------------------------------------------------------
// Oracle driving and full G0 outcome verification (H2)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Candidate3Status {
    /// Only base-2 was submitted (seed/retry/lock/existing-empty timelines).
    NotSubmitted,
    /// Candidate-3 was killed inside the observer callback before outer ACK.
    Unknown,
    /// Candidate-3 completed root sync and outer `Committed(3)` ACK before kill.
    Durable3,
}

impl Candidate3Status {
    fn parse(s: &str) -> Result<Self, String> {
        match s {
            "none" => Ok(Self::NotSubmitted),
            "unknown" => Ok(Self::Unknown),
            "durable" => Ok(Self::Durable3),
            other => Err(format!(
                "invalid --candidate3-outcome {other:?}; expected none|unknown|durable"
            )),
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::NotSubmitted => "none",
            Self::Unknown => "unknown",
            Self::Durable3 => "durable",
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
enum ObservedOutcome {
    Found {
        original_version: u64,
        event: RequestOutcomeEvent,
    },
    Absent,
    Incomplete(String),
}

fn logical_request_from_carrier(version: u64) -> LogicalRequest {
    let carrier = fixture::carrier(version);
    LogicalRequest::new(
        &carrier.principal_id,
        &carrier.request_id,
        carrier.payload_digest,
    )
}

fn logical_request_from_persisted(event: &RequestOutcomeEvent) -> Result<LogicalRequest, String> {
    if event.principal_id.len() > sage_db_format::field::MAX_PRINCIPAL_ID_BYTES as usize {
        return Err(format!(
            "persisted principal_id exceeds bound: {}",
            event.principal_id.len()
        ));
    }
    if event.request_id.len() > sage_db_format::field::MAX_REQUEST_ID_BYTES as usize {
        return Err(format!(
            "persisted request_id exceeds bound: {}",
            event.request_id.len()
        ));
    }
    Ok(LogicalRequest::new(
        &event.principal_id,
        &event.request_id,
        event.payload_digest,
    ))
}

fn build_attempt_history(status: Candidate3Status) -> AttemptHistory {
    let mut history = AttemptHistory::default();
    history.record(
        "attempt-base-2",
        logical_request_from_carrier(2),
        AttemptOutcome::Durable { version: 2 },
    );
    match status {
        Candidate3Status::NotSubmitted => {}
        Candidate3Status::Unknown => {
            history.record(
                "attempt-candidate-3",
                logical_request_from_carrier(3),
                AttemptOutcome::Unknown,
            );
        }
        Candidate3Status::Durable3 => {
            history.record(
                "attempt-candidate-3",
                logical_request_from_carrier(3),
                AttemptOutcome::Durable { version: 3 },
            );
        }
    }
    history
}

/// Validate recovered G0 outcomes (requests 2 & 3) AND drive existing `oracle::attempts`.
///
/// The recovered `World` is derived from ACTUAL persisted outcome records (`original_version`,
/// `principal_id`, `request_id`, `payload_digest`) reconciled with the observed `head` — never
/// constant-present from `head` alone and never using fabricated sentinel versions.
fn validate_recovered_outcomes_and_oracle(
    candidate3: Candidate3Status,
    observed_head: u64,
    obs2: &ObservedOutcome,
    obs3: &ObservedOutcome,
) -> Result<World, String> {
    let mut commits = Vec::new();
    match obs2 {
        ObservedOutcome::Found {
            original_version,
            event,
        } => {
            if *original_version != 2 {
                return Err(format!(
                    "outcome(2) original_version must be 2, got {original_version}"
                ));
            }
            let expected2 = fixture::expected_outcome(2);
            if event != &expected2 {
                return Err(format!(
                    "recovered outcome(2) differs from fixture::expected_outcome(2): \
                     actual={event:?} expected={expected2:?}"
                ));
            }
            if event.commit_version != *original_version {
                return Err(format!(
                    "outcome(2) commit_version {} disagrees with original_version {}",
                    event.commit_version, original_version
                ));
            }
            commits.push(Commit {
                version: *original_version,
                request: logical_request_from_persisted(event)?,
            });
        }
        ObservedOutcome::Absent => {
            // Do not short-circuit before running the oracle: let the oracle also witness a world
            // missing base-2 so lost ACK2 is rejected by AttemptHistory as well.
        }
        ObservedOutcome::Incomplete(reason) => {
            return Err(format!("outcome(2) lookup was Incomplete: {reason}"));
        }
    }

    match obs3 {
        ObservedOutcome::Found {
            original_version,
            event,
        } => {
            if observed_head == 2 {
                return Err(format!(
                    "outcome(3) must be Absent at head 2, but was Found at version {original_version}"
                ));
            }
            if *original_version != 3 {
                return Err(format!(
                    "outcome(3) original_version must be 3, got {original_version}"
                ));
            }
            let expected3 = fixture::expected_outcome(3);
            if event != &expected3 {
                return Err(format!(
                    "recovered outcome(3) differs from fixture::expected_outcome(3): \
                     actual={event:?} expected={expected3:?}"
                ));
            }
            if event.commit_version != *original_version {
                return Err(format!(
                    "outcome(3) commit_version {} disagrees with original_version {}",
                    event.commit_version, original_version
                ));
            }
            commits.push(Commit {
                version: *original_version,
                request: logical_request_from_persisted(event)?,
            });
        }
        ObservedOutcome::Absent => {
            if observed_head != 2 && observed_head != 1 {
                return Err(format!(
                    "outcome(3) may be Absent only at head 2, got Absent at head {observed_head}"
                ));
            }
        }
        ObservedOutcome::Incomplete(reason) => {
            return Err(format!("outcome(3) lookup was Incomplete: {reason}"));
        }
    }

    let world = World {
        base_version: 1,
        commits,
    };
    if let Some(collision) = find_version_collision(&world) {
        return Err(format!(
            "recovered world has version collision: {collision:?}"
        ));
    }
    if !world.versions_are_contiguous() {
        return Err(format!(
            "recovered world versions are not contiguous from base 1: {world:?}"
        ));
    }
    let derived_head = world
        .commits
        .last()
        .map(|c| c.version)
        .unwrap_or(world.base_version);
    if derived_head != observed_head {
        return Err(format!(
            "observed head {observed_head} disagrees with persisted outcome world head {derived_head}"
        ));
    }

    let history = build_attempt_history(candidate3);
    let accepted = history
        .accepts(1, &world)
        .map_err(|e| format!("AttemptHistory::accepts error: {e:?}"))?;
    if !accepted {
        return Err(format!(
            "AttemptHistory rejected recovered world (candidate3={:?}, head={observed_head}, commits={:?})",
            candidate3, world.commits
        ));
    }
    Ok(world)
}

/// Validate an exact retry's returned `(version, recorded_event)` against both the fixture event
/// and `oracle::attempts::AttemptHistory`.
fn validate_exact_retry(version: u64, event: &RequestOutcomeEvent) -> Result<(), String> {
    if version != 2 {
        return Err(format!(
            "exact retry must report original version 2, got {version}"
        ));
    }
    let expected = fixture::expected_outcome(2);
    if event != &expected {
        return Err(format!(
            "exact retry recorded event differs from fixture::expected_outcome(2): \
             actual={event:?} expected={expected:?}"
        ));
    }
    let mut history = AttemptHistory::default();
    history.record(
        "attempt-base-2",
        logical_request_from_carrier(2),
        AttemptOutcome::Durable { version: 2 },
    );
    history.record(
        "attempt-retry-2",
        logical_request_from_persisted(event)?,
        AttemptOutcome::Durable { version },
    );
    let worlds = history
        .legal_worlds(1)
        .map_err(|e| format!("AttemptHistory rejected exact retry history: {e:?}"))?;
    let world = World {
        base_version: 1,
        commits: vec![Commit {
            version,
            request: logical_request_from_persisted(event)?,
        }],
    };
    if worlds.len() != 1 || worlds[0] != world {
        return Err(format!(
            "exact retry history produced unexpected legal worlds: {worlds:?}"
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Whole historical blob union (rules + v2..=head declared blobs incl. superseded A1 & B1)
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, Eq, Debug)]
struct ExpectedBlob {
    label: String,
    sha256: [u8; 32],
    bytes: Vec<u8>,
}

/// Complete union of committed blobs from Genesis rules through `head` (`2..=head`).
///
/// For `head == 2`: rules + A1 + B1 + result(2) (4 unique blobs).
/// For `head == 3`: rules + superseded A1 + carried B1 + result(2) + A2 + C1 + result(3) (7 unique blobs).
fn declared_blobs_union(head: u64) -> Result<Vec<ExpectedBlob>, String> {
    if !matches!(head, 2 | 3) {
        return Err(format!(
            "declared_blobs_union requires head 2 or 3, got {head}"
        ));
    }
    let mut out: Vec<ExpectedBlob> = Vec::new();
    let mut push_unique = |label: String, sha256: [u8; 32], bytes: Vec<u8>| -> Result<(), String> {
        let computed = sage_db_format::digest::raw_sha256(&bytes);
        if computed != sha256 {
            return Err(format!(
                "fixture blob {label} digest mismatch: declared={} computed={}",
                hex(&sha256),
                hex(&computed)
            ));
        }
        if let Some(existing) = out.iter().find(|b| b.sha256 == sha256) {
            if existing.bytes != bytes {
                return Err(format!("fixture blob digest collision for {label}"));
            }
            return Ok(());
        }
        out.push(ExpectedBlob {
            label,
            sha256,
            bytes,
        });
        Ok(())
    };

    push_unique(
        "genesis_rules".to_owned(),
        sage_db_format::digest::raw_sha256(fixture::RULES),
        fixture::RULES.to_vec(),
    )?;

    for version in 2..=head {
        for (index, blob) in fixture::carrier(version).blobs.into_iter().enumerate() {
            let bytes = match blob.source {
                sage_db_types::prepared::BlobSource::Provide(bytes) => bytes,
                other => {
                    return Err(format!(
                        "fixture v{version} blob {index} is not supplied inline: {other:?}"
                    ));
                }
            };
            if blob.declared_len != bytes.len() as u64 {
                return Err(format!(
                    "fixture v{version} blob {index} declared_len={} != actual={}",
                    blob.declared_len,
                    bytes.len()
                ));
            }
            push_unique(format!("v{version}_{index}"), blob.sha256, bytes)?;
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Plumbing (checked stdout emission, fail, real_fs)
// ---------------------------------------------------------------------------

/// Print one line and flush stdout, propagating I/O errors instead of ignoring them.
fn try_emit(line: &str) -> io::Result<()> {
    let mut out = io::stdout().lock();
    writeln!(out, "{line}")?;
    out.flush()
}

fn emit(line: &str) {
    if let Err(err) = try_emit(line) {
        let _ = writeln!(io::stderr().lock(), "DRIVER_STDOUT_ERROR: {err}");
        let _ = io::stderr().flush();
        std::process::exit(EXIT_DRIVER_ASSERT);
    }
}

/// Block until SIGKILL. Reads stdin so parent pipe close is also a clean exit path.
fn block_until_killed() -> ! {
    let stdin = io::stdin();
    let mut line = String::new();
    loop {
        line.clear();
        match stdin.lock().read_line(&mut line) {
            Ok(0) | Err(_) => std::process::exit(EXIT_OK),
            Ok(_) => {}
        }
    }
}

fn announce_steady() -> ! {
    emit("STATE=steady");
    block_until_killed()
}

fn fail(code: i32, message: &str) -> ! {
    let _ = writeln!(io::stderr().lock(), "DRIVER_FAILURE: {message}");
    let _ = io::stderr().flush();
    std::process::exit(code);
}

fn ledger() -> AttemptLedger<'static> {
    static CONTROL: RunToCompletion = RunToCompletion;
    AttemptLedger::open(fixture::BYTES, fixture::ENTRIES, &CONTROL)
        .unwrap_or_else(|e| fail(EXIT_DRIVER_ASSERT, &format!("open AttemptLedger: {e:?}")))
}

/// Build the real backend for `root`, or fail with the distinct not-qualified code (45).
fn real_fs(root: &Path) -> OsFileSystem {
    match OsFileSystem::for_root(root) {
        Ok(fs) => {
            if fs.durability() != DurabilityBasis::Documented {
                fail(
                    EXIT_HOST_NOT_QUALIFIED,
                    "for_root returned a backend that does not claim Documented durability",
                );
            }
            emit("POLICY=for_root/Documented");
            fs
        }
        Err(error) => fail(
            EXIT_HOST_NOT_QUALIFIED,
            &format!("OsFileSystem::for_root refused this host/path: {error:?}"),
        ),
    }
}

/// Canonical, order-preserving digest of a row set (no sorting, no deduplication).
fn rows_digest(rows: &[StateEntry]) -> [u8; 32] {
    let mut framed = Vec::new();
    framed.extend_from_slice(&(rows.len() as u64).to_le_bytes());
    for row in rows {
        framed.extend_from_slice(&row.kind.to_le_bytes());
        framed.extend_from_slice(&(row.key.len() as u64).to_le_bytes());
        framed.extend_from_slice(&row.key);
        framed.extend_from_slice(&row.revision.to_le_bytes());
        framed.extend_from_slice(&(row.body.len() as u64).to_le_bytes());
        framed.extend_from_slice(&row.body);
    }
    sage_db_format::digest::raw_sha256(&framed)
}

fn current_bytes(root: &Path) -> Option<Vec<u8>> {
    fs::read(root.join(RootName::Current.as_str())).ok()
}

fn current_identity(root: &Path) -> String {
    match current_bytes(root) {
        None => "absent".to_owned(),
        Some(bytes) => format!(
            "len={} sha256={}",
            bytes.len(),
            hex(&sage_db_format::digest::raw_sha256(&bytes))
        ),
    }
}

fn lookup_observed_outcome(
    store: &sage_db_store::handle::Store<CountingFs>,
    version: u64,
    expected_head: u64,
) -> Result<ObservedOutcome, String> {
    let carrier = fixture::carrier(version);
    match store.outcome(&carrier.principal_id, &carrier.request_id) {
        OutcomeLookup::Found {
            outcome,
            original_version,
            basis,
        } => {
            if basis.kb_uuid != fixture::kb() {
                return Err(format!(
                    "outcome({version}) basis kb_uuid mismatch: {:?}",
                    basis.kb_uuid
                ));
            }
            if basis.version.get() != expected_head {
                return Err(format!(
                    "outcome({version}) basis version {} != observed head {expected_head}",
                    basis.version.get()
                ));
            }
            Ok(ObservedOutcome::Found {
                original_version,
                event: outcome.event().clone(),
            })
        }
        OutcomeLookup::Absent { basis } => {
            if basis.kb_uuid != fixture::kb() {
                return Err(format!(
                    "outcome({version}) absent basis kb_uuid mismatch: {:?}",
                    basis.kb_uuid
                ));
            }
            if basis.version.get() != expected_head {
                return Err(format!(
                    "outcome({version}) absent basis version {} != observed head {expected_head}",
                    basis.version.get()
                ));
            }
            Ok(ObservedOutcome::Absent)
        }
        OutcomeLookup::Incomplete(reason) => Ok(ObservedOutcome::Incomplete(format!("{reason:?}"))),
    }
}

// ---------------------------------------------------------------------------
// Modes
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SeedRootExpectation {
    Unspecified,
    MissingRoot,
    ExistingEmpty,
}

fn mode_host_probe(root: &Path) {
    emit(&format!("TARGET_OS={}", std::env::consts::OS));
    emit(&format!("TARGET_ARCH={}", std::env::consts::ARCH));
    if let Ok(release) = fs::read_to_string("/proc/sys/kernel/osrelease") {
        emit(&format!("KERNEL_RELEASE={}", release.trim()));
    }
    let fs = real_fs(root);
    if let Some(validated) = fs.validated_root() {
        emit(&format!("VALIDATED_ROOT={}", validated.display()));
    }
    if let Some(bound) = fs.bound_directory() {
        emit(&format!("BOUND_DIR={}", bound.display()));
    }
    emit("HOST_PROBE=PASS");
}

fn mode_seed(root: &Path, baseline_path: &Path, expectation: SeedRootExpectation) {
    if let Err(err) = ensure_outside_kb(baseline_path, root) {
        fail(EXIT_DRIVER_ASSERT, &err);
    }
    match expectation {
        SeedRootExpectation::Unspecified => {}
        SeedRootExpectation::MissingRoot => {
            if root.exists() {
                fail(
                    EXIT_DRIVER_ASSERT,
                    &format!("--expect-missing-root violated: {} exists", root.display()),
                );
            }
        }
        SeedRootExpectation::ExistingEmpty => {
            let meta = fs::symlink_metadata(root).unwrap_or_else(|e| {
                fail(
                    EXIT_DRIVER_ASSERT,
                    &format!("--expect-existing-empty root missing: {e}"),
                )
            });
            if !meta.is_dir() {
                fail(
                    EXIT_DRIVER_ASSERT,
                    "--expect-existing-empty path is not a directory",
                );
            }
            let count = fs::read_dir(root)
                .unwrap_or_else(|e| fail(EXIT_DRIVER_ASSERT, &format!("read_dir root: {e}")))
                .count();
            if count != 0 {
                fail(
                    EXIT_DRIVER_ASSERT,
                    &format!("--expect-existing-empty root has {count} entries"),
                );
            }
        }
    }

    let fs = real_fs(root);
    match expectation {
        SeedRootExpectation::Unspecified => {}
        SeedRootExpectation::MissingRoot => {
            if fs.validated_root() != Some(root) || fs.bound_directory() != root.parent() {
                fail(
                    EXIT_DRIVER_ASSERT,
                    &format!(
                        "missing-root for_root binding unexpected: validated={:?} bound={:?}",
                        fs.validated_root(),
                        fs.bound_directory()
                    ),
                );
            }
            emit("SEED_ROOT_SHAPE=missing_root");
        }
        SeedRootExpectation::ExistingEmpty => {
            if fs.validated_root() != Some(root) || fs.bound_directory() != Some(root) {
                fail(
                    EXIT_DRIVER_ASSERT,
                    &format!(
                        "existing-empty for_root binding unexpected: validated={:?} bound={:?}",
                        fs.validated_root(),
                        fs.bound_directory()
                    ),
                );
            }
            emit("SEED_ROOT_SHAPE=existing_empty");
        }
    }

    let inner = Rc::new(fs);
    let observed = CountingFs::new(Rc::clone(&inner), root);
    let mut ledger = ledger();

    let mut store = bootstrap(
        observed.clone(),
        root,
        CreateDir::IfMissing,
        fixture::kb(),
        fixture::RULES,
        &Limits::V1,
        &mut ledger,
    )
    .unwrap_or_else(|e| fail(EXIT_DRIVER_ASSERT, &format!("bootstrap: {e:?}")));
    if store.head() != Some(1) {
        fail(
            EXIT_DRIVER_ASSERT,
            &format!("genesis head must be 1, got {:?}", store.head()),
        );
    }

    // H1: capture actual Genesis rows BEFORE committing version 2; exactly 2 rows, transported
    // outside the KB using test-only bounded checksummed framing (`wx` / `create_new`).
    let genesis: Vec<StateEntry> = store.retained_entries().cloned().collect();
    if genesis.len() != EXPECTED_GENESIS_ROWS {
        fail(
            EXIT_DRIVER_ASSERT,
            &format!(
                "captured genesis row count must be {EXPECTED_GENESIS_ROWS}, got {}",
                genesis.len()
            ),
        );
    }
    write_genesis_baseline(baseline_path, root, &genesis)
        .unwrap_or_else(|e| fail(EXIT_DRIVER_ASSERT, &format!("write genesis baseline: {e}")));
    emit(&format!("GENESIS_ROWS={}", genesis.len()));
    emit(&format!("GENESIS_DIGEST={}", hex(&rows_digest(&genesis))));
    emit(&format!("GENESIS_BASELINE={}", baseline_path.display()));

    match store.commit(fixture::carrier(2), &Limits::V1, &mut ledger) {
        HandleOutcome::Committed {
            version: 2,
            recorded: RecordedOutcome::Outcome(shared),
        } => {
            let expected2 = fixture::expected_outcome(2);
            if shared.event() != &expected2 {
                fail(
                    EXIT_DRIVER_ASSERT,
                    &format!(
                        "base-2 committed outcome differs from fixture::expected_outcome(2): {:?}",
                        shared.event()
                    ),
                );
            }
        }
        other => fail(
            EXIT_DRIVER_ASSERT,
            &format!("base-2 must commit with RecordedOutcome::Outcome, got {other:?}"),
        ),
    }
    if store.head() != Some(2) {
        fail(
            EXIT_DRIVER_ASSERT,
            &format!("expected head 2, got {:?}", store.head()),
        );
    }
    let view = store
        .view()
        .unwrap_or_else(|| fail(EXIT_DRIVER_ASSERT, "no view after commit"));
    if view.durability() != ViewDurability::Durable {
        fail(EXIT_DRIVER_ASSERT, "ACK2 must be on a Durable view");
    }
    emit("ACK2=2");
    emit("HEAD=2");
    emit(&format!("CURRENT_AFTER_ACK2={}", current_identity(root)));
    emit("SEED=PASS");
}

fn mode_retry(root: &Path) {
    let fs = real_fs(root);
    let inner = Rc::new(fs);
    let observed = CountingFs::new(Rc::clone(&inner), root);
    let mut ledger = ledger();

    let opened = open_existing(
        observed.clone(),
        root,
        Some(fixture::kb()),
        &Limits::V1,
        &mut ledger,
    )
    .unwrap_or_else(|e| fail(EXIT_OPEN_FAILURE, &format!("open_existing: {e:?}")));
    let durability = opened.durability();
    if durability != ViewDurability::Durable {
        fail(EXIT_DRIVER_ASSERT, "retry must open a Durable root");
    }
    let mut store = opened.into_store();
    if store.head() != Some(2) {
        fail(
            EXIT_DRIVER_ASSERT,
            &format!("retry expected head 2, got {:?}", store.head()),
        );
    }
    emit("OPEN_HEAD=2");
    emit(&format!("OPEN_DURABILITY={durability:?}"));

    let before_calls = observed.calls();
    let before_current = current_bytes(root);

    // Exact retry of base-2: must return AlreadyCommitted(2) with exact full RequestOutcomeEvent,
    // accepted by AttemptHistory, with zero named mutating/sync calls and unchanged CURRENT bytes.
    let outcome = store.commit(fixture::carrier(2), &Limits::V1, &mut ledger);
    match outcome {
        HandleOutcome::AlreadyCommitted {
            version,
            recorded: RecordedOutcome::Outcome(shared),
        } => {
            if let Err(err) = validate_exact_retry(version, shared.event()) {
                fail(EXIT_DRIVER_ASSERT, &err);
            }
        }
        other => fail(
            EXIT_DRIVER_ASSERT,
            &format!("exact retry must answer AlreadyCommitted(2) with Outcome, got {other:?}"),
        ),
    }

    let after_calls = observed.calls();
    let delta = MutatingCalls {
        create_root: after_calls.create_root - before_calls.create_root,
        open_family_create: after_calls.open_family_create - before_calls.open_family_create,
        sync_parent_of: after_calls.sync_parent_of - before_calls.sync_parent_of,
        sync_dir: after_calls.sync_dir - before_calls.sync_dir,
        create_new: after_calls.create_new - before_calls.create_new,
        write: after_calls.write - before_calls.write,
        sync_file: after_calls.sync_file - before_calls.sync_file,
        sync_entry: after_calls.sync_entry - before_calls.sync_entry,
        publish_no_overwrite: after_calls.publish_no_overwrite - before_calls.publish_no_overwrite,
        replace_current: after_calls.replace_current - before_calls.replace_current,
        remove: after_calls.remove - before_calls.remove,
    };
    emit(&format!("RETRY_DELTA={}", delta.describe()));
    if delta.total() != 0 {
        fail(
            EXIT_DRIVER_ASSERT,
            &format!(
                "exact retry performed named mutating/sync calls: {}",
                delta.describe()
            ),
        );
    }
    let after_current = current_bytes(root);
    if before_current != after_current {
        fail(
            EXIT_DRIVER_ASSERT,
            "exact retry changed the durable CURRENT bytes",
        );
    }
    if store.head() != Some(2) {
        fail(
            EXIT_DRIVER_ASSERT,
            &format!("head after retry must remain 2, got {:?}", store.head()),
        );
    }
    emit("RETRY_MUTATIONS=0");
    emit("RETRY_OUTCOME_MATCHES_FIXTURE=true");
    emit("RETRY_ORACLE_ACCEPTED=true");
    emit("CURRENT_UNCHANGED=true");
    emit("HEAD_AFTER_RETRY=2");
    emit("RETRY=PASS");
}

fn verify_unarmed_setup(observed: &CountingFs) {
    let pre_calls = observed.calls();
    let pre_trace = observed.tracker_snapshot();
    if pre_trace.armed {
        fail(
            EXIT_DRIVER_ASSERT,
            "observer must remain unarmed during Ready(2) setup",
        );
    }
    if pre_trace.fired.is_some() {
        fail(
            EXIT_DRIVER_ASSERT,
            &format!(
                "observer fired during Ready(2) setup before arming: {:?}",
                pre_trace.fired
            ),
        );
    }
    if pre_calls.replace_current != 0 {
        fail(
            EXIT_DRIVER_ASSERT,
            &format!(
                "Ready(2) setup unexpectedly called replace_current {} times",
                pre_calls.replace_current
            ),
        );
    }
    if pre_trace.replace_current_completed || pre_trace.root_sync_after_replace {
        fail(
            EXIT_DRIVER_ASSERT,
            "publish flags must be false before arming for candidate-3",
        );
    }
    emit("PRE_ARM_ARMED=false");
    emit("PRE_ARM_FIRED=none");
    emit(&format!(
        "PRE_ARM_REPLACE_CURRENT={}",
        pre_calls.replace_current
    ));
    emit(&format!("PRE_ARM_CALLS={}", pre_calls.describe()));
}

fn mode_unacked(root: &Path) {
    let fs = real_fs(root);
    let inner = Rc::new(fs);
    // Always constructed UNARMED; armed only after Ready(2) setup is proven.
    let observed = CountingFs::new(Rc::clone(&inner), root);
    let mut ledger = ledger();

    let opened = open_existing(
        observed.clone(),
        root,
        Some(fixture::kb()),
        &Limits::V1,
        &mut ledger,
    )
    .unwrap_or_else(|e| fail(EXIT_OPEN_FAILURE, &format!("open_existing: {e:?}")));
    if opened.durability() != ViewDurability::Durable {
        fail(EXIT_DRIVER_ASSERT, "producer must open Ready(2)");
    }
    let mut store = opened.into_store();
    if store.head() != Some(2) {
        fail(
            EXIT_DRIVER_ASSERT,
            &format!("producer must see head 2, got {:?}", store.head()),
        );
    }

    verify_unarmed_setup(&observed);

    observed
        .reset_and_arm(Barrier::AfterReplaceCurrent)
        .unwrap_or_else(|e| fail(EXIT_DRIVER_ASSERT, &e));
    let armed_trace = observed.tracker_snapshot();
    if !armed_trace.armed || armed_trace.fired.is_some() {
        fail(EXIT_DRIVER_ASSERT, "post-arm invariant failed");
    }

    emit("ARMED=barrier_after_replace_current");
    emit("BASE=2");
    emit("TARGET=3");

    let outcome = store.commit(fixture::carrier(3), &Limits::V1, &mut ledger);
    // Unreachable when the barrier fires: SIGKILL happens inside replace_current before return.
    fail(
        EXIT_DRIVER_ASSERT,
        &format!("unacked producer returned without hitting the barrier: {outcome:?}"),
    );
}

fn mode_post_ack(root: &Path) {
    let fs = real_fs(root);
    let inner = Rc::new(fs);
    // Always constructed UNARMED; armed only after Ready(2) setup is proven.
    let observed = CountingFs::new(Rc::clone(&inner), root);
    let mut ledger = ledger();

    let opened = open_existing(
        observed.clone(),
        root,
        Some(fixture::kb()),
        &Limits::V1,
        &mut ledger,
    )
    .unwrap_or_else(|e| fail(EXIT_OPEN_FAILURE, &format!("open_existing: {e:?}")));
    if opened.durability() != ViewDurability::Durable {
        fail(EXIT_DRIVER_ASSERT, "producer must open Ready(2)");
    }
    let mut store = opened.into_store();
    if store.head() != Some(2) {
        fail(
            EXIT_DRIVER_ASSERT,
            &format!("producer must see head 2, got {:?}", store.head()),
        );
    }

    verify_unarmed_setup(&observed);

    observed
        .reset_and_arm(Barrier::None)
        .unwrap_or_else(|e| fail(EXIT_DRIVER_ASSERT, &e));
    emit("ARMED=post_ack_observer");
    emit("BASE=2");
    emit("TARGET=3");

    match store.commit(fixture::carrier(3), &Limits::V1, &mut ledger) {
        HandleOutcome::Committed {
            version: 3,
            recorded: RecordedOutcome::Outcome(shared),
        } => {
            let expected3 = fixture::expected_outcome(3);
            if shared.event() != &expected3 {
                fail(
                    EXIT_DRIVER_ASSERT,
                    &format!(
                        "candidate-3 committed outcome differs from fixture::expected_outcome(3): {:?}",
                        shared.event()
                    ),
                );
            }
        }
        other => fail(
            EXIT_DRIVER_ASSERT,
            &format!("candidate-3 must commit with RecordedOutcome::Outcome, got {other:?}"),
        ),
    }
    if store.head() != Some(3) {
        fail(
            EXIT_DRIVER_ASSERT,
            &format!("after ACK3 expected head 3, got {:?}", store.head()),
        );
    }

    // H3: require Durable view + completed replace_current + successful ROOT sync_dir AFTER
    // replace_current BEFORE printing ACK3=3. Never rely on cumulative sync_dir count.
    let durable_view = store
        .view()
        .is_some_and(|v| v.durability() == ViewDurability::Durable);
    let trace = observed.tracker_snapshot();
    if let Err(err) = validate_post_ack_proof(durable_view, &trace) {
        fail(EXIT_DRIVER_ASSERT, &err);
    }
    let calls = observed.calls();
    emit("REPLACE_CURRENT_COMPLETED=true");
    emit("ROOT_SYNC_AFTER_REPLACE=true");
    emit(&format!(
        "ROOT_SYNC_AFTER_REPLACE_COUNT={}",
        trace.root_sync_after_replace_count
    ));
    emit("ACK3=3");
    emit(&format!("POST_ACK_CALLS={}", calls.describe()));
    emit(&format!("CURRENT_AFTER_ACK3={}", current_identity(root)));
    emit("STATE=post_ack_steady");
    block_until_killed();
}

fn mode_reopen(root: &Path, baseline_path: &Path, candidate3: Candidate3Status) {
    // Load and bind the Genesis baseline BEFORE opening: missing baseline must fail immediately
    // and never be recreated, even on a non-qualifying host.
    let genesis =
        read_genesis_baseline(baseline_path, root).unwrap_or_else(|e| fail(EXIT_DRIVER_ASSERT, &e));

    let fs = real_fs(root);
    let inner = Rc::new(fs);
    let observed = CountingFs::new(Rc::clone(&inner), root);
    let mut ledger = ledger();

    let opened = open_existing(
        observed,
        root,
        Some(fixture::kb()),
        &Limits::V1,
        &mut ledger,
    )
    .unwrap_or_else(|e| fail(EXIT_OPEN_FAILURE, &format!("reopen: {e:?}")));
    let durability = opened.durability();
    if durability != ViewDurability::Durable {
        fail(EXIT_DRIVER_ASSERT, "reopen must produce a Durable root");
    }
    let mut store = opened.into_store();
    let head = store
        .head()
        .unwrap_or_else(|| fail(EXIT_DRIVER_ASSERT, "reopen saw no head"));
    emit(&format!("REOPEN_DURABILITY={durability:?}"));
    emit(&format!("REOPEN_HEAD={head}"));

    // H2: read actual persisted G0 outcomes for BOTH requests 2 & 3, reconcile with head,
    // and drive existing oracle::attempts::AttemptHistory.
    let obs2 =
        lookup_observed_outcome(&store, 2, head).unwrap_or_else(|e| fail(EXIT_DRIVER_ASSERT, &e));
    let obs3 =
        lookup_observed_outcome(&store, 3, head).unwrap_or_else(|e| fail(EXIT_DRIVER_ASSERT, &e));
    let _world = validate_recovered_outcomes_and_oracle(candidate3, head, &obs2, &obs3)
        .unwrap_or_else(|e| fail(EXIT_DRIVER_ASSERT, &e));

    emit("OUTCOME2_ORIGINAL_VERSION=2");
    emit("OUTCOME2_MATCHES_FIXTURE=true");
    match obs3 {
        ObservedOutcome::Absent => {
            emit("OUTCOME3_STATE=Absent");
        }
        ObservedOutcome::Found {
            original_version, ..
        } => {
            emit("OUTCOME3_STATE=Found");
            emit(&format!("OUTCOME3_ORIGINAL_VERSION={original_version}"));
            emit("OUTCOME3_MATCHES_FIXTURE=true");
        }
        ObservedOutcome::Incomplete(reason) => {
            fail(
                EXIT_DRIVER_ASSERT,
                &format!("outcome(3) incomplete: {reason}"),
            );
        }
    }
    emit(&format!("ORACLE_CANDIDATE3_STATUS={}", candidate3.as_str()));
    emit("ORACLE_ACCEPTED=true");

    // H1: compare raw retained_entries() directly against fixture::expected_rows(&genesis, head).
    // No sorting, deduplication, or key-search on the actual side.
    let actual_rows: Vec<StateEntry> = store.retained_entries().cloned().collect();
    let expected_count = validate_retained_rows(&actual_rows, &genesis, head)
        .unwrap_or_else(|e| fail(EXIT_DRIVER_ASSERT, &e));
    emit(&format!("ROWS={}", actual_rows.len()));
    emit(&format!("EXPECTED_ROWS={expected_count}"));
    emit(&format!("ROWS_DIGEST={}", hex(&rows_digest(&actual_rows))));
    emit("ROWS_MATCH_EXPECTED=true");
    emit("GENESIS_PRESERVED_EXACT=true");

    // Historical blob union: rules + every declared blob from v2..=head (including superseded A1
    // and carried-forward B1 when head == 3), read through Store::read_committed_blob.
    let blobs = declared_blobs_union(head).unwrap_or_else(|e| fail(EXIT_DRIVER_ASSERT, &e));
    for expected in &blobs {
        let blob = store
            .read_committed_blob(expected.sha256, expected.bytes.len() as u64, &mut ledger)
            .unwrap_or_else(|e| {
                fail(
                    EXIT_DRIVER_ASSERT,
                    &format!("blob {} unreadable: {e:?}", expected.label),
                )
            });
        if blob.as_bytes() != expected.bytes.as_slice() {
            fail(
                EXIT_DRIVER_ASSERT,
                &format!("blob {} bytes differ from the fixture", expected.label),
            );
        }
        emit(&format!(
            "BLOB_{}=len:{}_sha256:{}",
            expected.label,
            expected.bytes.len(),
            hex(&expected.sha256)
        ));
    }
    emit(&format!(
        "BLOB_COVERAGE_SCOPE=historical_union_genesis_through_v{head}"
    ));
    emit(&format!("BLOBS_VERIFIED_COUNT={}", blobs.len()));
    emit("REOPEN=PASS");
}

fn mode_lock_holder(root: &Path) {
    let fs = real_fs(root);
    let inner = Rc::new(fs);
    let observed = CountingFs::new(Rc::clone(&inner), root);
    let mut ledger = ledger();

    let opened = open_existing(
        observed,
        root,
        Some(fixture::kb()),
        &Limits::V1,
        &mut ledger,
    )
    .unwrap_or_else(|e| fail(EXIT_OPEN_FAILURE, &format!("holder open: {e:?}")));
    if opened.durability() != ViewDurability::Durable {
        fail(EXIT_DRIVER_ASSERT, "holder must open Ready(2)");
    }
    let head = opened
        .store()
        .head()
        .unwrap_or_else(|| fail(EXIT_DRIVER_ASSERT, "holder saw no head"));
    if head != 2 {
        fail(
            EXIT_DRIVER_ASSERT,
            &format!("holder expected head 2, got {head}"),
        );
    }
    emit(&format!("HOLDER_HEAD={head}"));
    emit(&format!("LOCKHELD_PID={}", std::process::id()));
    emit("LOCKHELD=true");
    announce_steady();
}

fn mode_lock_refuser(root: &Path) {
    let fs = real_fs(root);
    let inner = Rc::new(fs);
    let observed = CountingFs::new(Rc::clone(&inner), root);
    let mut ledger = ledger();

    match open_existing(
        observed,
        root,
        Some(fixture::kb()),
        &Limits::V1,
        &mut ledger,
    ) {
        Ok(_) => fail(
            EXIT_OPEN_FAILURE,
            "second process opened a root whose lock is held elsewhere",
        ),
        Err(OpenRefusal::Filesystem { step, error }) => {
            // Only this exact class gets exit 42: the lock step, WouldBlock, NoEffect.
            let exact = matches!(step, OpenStep::Lock)
                && error.kind() == FsErrorKind::WouldBlock
                && error.effect() == sage_db_store::fs::Effect::NoEffect;
            if !exact {
                fail(
                    EXIT_OPEN_FAILURE,
                    &format!(
                        "lock refusal was not the exact class: step={step:?} kind={:?} effect={:?}",
                        error.kind(),
                        error.effect()
                    ),
                );
            }
            emit("LOCK_REFUSAL_STEP=Lock");
            emit("LOCK_REFUSAL_KIND=WouldBlock");
            emit("LOCK_REFUSAL_EFFECT=NoEffect");
            emit("REFUSE=PASS");
            std::process::exit(EXIT_LOCK_REFUSAL);
        }
        Err(other) => fail(
            EXIT_OPEN_FAILURE,
            &format!("unexpected open refusal: {other:?}"),
        ),
    }
}

fn mode_silent_hold() -> ! {
    let _ = writeln!(
        io::stderr().lock(),
        "SILENT_CHILD_DIAG=blocking_without_ready"
    );
    let _ = io::stderr().flush();
    emit("SILENT_CHILD_STARTED=true");
    block_until_killed()
}

fn mode_ready_then_trailing() -> ! {
    emit("PHASE=one");
    emit("AFTER_PHASE_ONE=captured_in_full_transcript");
    let _ = writeln!(io::stderr().lock(), "TRAILING_STDERR=captured");
    let _ = io::stderr().flush();
    block_until_killed()
}

// ---------------------------------------------------------------------------
// Explicit CLI parsing
// ---------------------------------------------------------------------------

struct CliOptions {
    baseline: Option<PathBuf>,
    candidate3: Option<Candidate3Status>,
    seed_expectation: SeedRootExpectation,
}

fn parse_flags(args: &[String]) -> Result<CliOptions, String> {
    let mut opts = CliOptions {
        baseline: None,
        candidate3: None,
        seed_expectation: SeedRootExpectation::Unspecified,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--genesis-baseline" => {
                let val = args
                    .get(i + 1)
                    .ok_or_else(|| "--genesis-baseline requires a path".to_owned())?;
                opts.baseline = Some(PathBuf::from(val));
                i += 2;
            }
            "--candidate3-outcome" => {
                let val = args.get(i + 1).ok_or_else(|| {
                    "--candidate3-outcome requires none|unknown|durable".to_owned()
                })?;
                opts.candidate3 = Some(Candidate3Status::parse(val)?);
                i += 2;
            }
            "--expect-missing-root" => {
                opts.seed_expectation = SeedRootExpectation::MissingRoot;
                i += 1;
            }
            "--expect-existing-empty" => {
                opts.seed_expectation = SeedRootExpectation::ExistingEmpty;
                i += 1;
            }
            other => return Err(format!("unknown CLI flag {other:?}")),
        }
    }
    Ok(opts)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("");

    // Pure parent-control modes that do not touch any KB or platform policy.
    match mode {
        "silent-hold" => mode_silent_hold(),
        "exit-normal" => {
            emit(&format!("PID={}", std::process::id()));
            emit("MODE=exit-normal");
            emit("EXIT_NORMAL=true");
            std::process::exit(EXIT_OK);
        }
        "ready-then-trailing" => {
            emit(&format!("PID={}", std::process::id()));
            emit("MODE=ready-then-trailing");
            mode_ready_then_trailing();
        }
        _ => {}
    }

    let root = args
        .get(2)
        .map(PathBuf::from)
        .unwrap_or_else(|| fail(EXIT_DRIVER_ASSERT, "missing <root> argument"));
    let opts = parse_flags(&args[3..]).unwrap_or_else(|e| fail(EXIT_DRIVER_ASSERT, &e));

    emit(&format!("PID={}", std::process::id()));
    emit(&format!("MODE={mode}"));
    emit(&format!("ROOT={}", root.display()));

    match mode {
        "host-probe" => mode_host_probe(&root),
        "seed" => {
            let baseline = opts.baseline.unwrap_or_else(|| {
                fail(
                    EXIT_DRIVER_ASSERT,
                    "seed requires --genesis-baseline <path>",
                )
            });
            mode_seed(&root, &baseline, opts.seed_expectation);
        }
        "retry" => mode_retry(&root),
        "unacked" => mode_unacked(&root),
        "post-ack" => mode_post_ack(&root),
        "reopen" => {
            let baseline = opts.baseline.unwrap_or_else(|| {
                fail(
                    EXIT_DRIVER_ASSERT,
                    "reopen requires --genesis-baseline <path>",
                )
            });
            let candidate3 = opts.candidate3.unwrap_or_else(|| {
                fail(
                    EXIT_DRIVER_ASSERT,
                    "reopen requires --candidate3-outcome <none|unknown|durable>",
                )
            });
            mode_reopen(&root, &baseline, candidate3);
        }
        "lock-holder" => mode_lock_holder(&root),
        "lock-refuser" => mode_lock_refuser(&root),
        other => fail(EXIT_DRIVER_ASSERT, &format!("unknown mode {other:?}")),
    }
    std::process::exit(EXIT_OK);
}

// ---------------------------------------------------------------------------
// Load-bearing pure unit controls (run everywhere; make no OS durability claim)
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    fn sample_genesis_rows() -> Vec<StateEntry> {
        vec![
            StateEntry {
                kind: 0,
                key: b"kb-meta".to_vec(),
                revision: 1,
                body: b"genesis-kb-metadata-body-bytes".to_vec(),
            },
            StateEntry {
                kind: 15,
                key: b"rule-cfg".to_vec(),
                revision: 1,
                body: b"genesis-rule-config-body-bytes".to_vec(),
            },
        ]
    }

    fn found_outcome(version: u64, original_version: u64) -> ObservedOutcome {
        ObservedOutcome::Found {
            original_version,
            event: fixture::expected_outcome(version),
        }
    }

    // ---- H3: PublishTracker + validate_post_ack_proof ----

    #[test]
    fn post_ack_proof_accepts_durable_view_with_root_sync_after_replace() {
        let root = PathBuf::from("/var/tmp/p2-os-unit/kb");
        let mut tracker = PublishTracker::new(root.clone());
        tracker.reset_and_arm(Barrier::None).expect("arm");
        assert!(
            tracker
                .record_replace_current(true, &root, None, "CURRENT.staged")
                .is_none()
        );
        tracker.record_sync_dir(true, &root, None);
        assert!(validate_post_ack_proof(true, &tracker).is_ok());
    }

    #[test]
    fn post_ack_proof_rejects_mutant_missing_root_sync_even_with_family_syncs() {
        let root = PathBuf::from("/var/tmp/p2-os-unit/kb");
        let manifests = root.join("manifests");
        let other_root = PathBuf::from("/var/tmp/p2-os-unit/other");
        let mut tracker = PublishTracker::new(root.clone());
        tracker.reset_and_arm(Barrier::None).expect("arm");
        tracker.record_replace_current(true, &root, None, "CURRENT.staged");

        // Twelve non-root / wrong-path / failed sync_dir calls must NOT satisfy root sync.
        for _ in 0..10 {
            tracker.record_sync_dir(true, &manifests, Some(Family::Manifests));
        }
        tracker.record_sync_dir(true, &other_root, None);
        tracker.record_sync_dir(false, &root, None);

        let err =
            validate_post_ack_proof(true, &tracker).expect_err("must reject missing root sync");
        assert!(err.contains("ROOT sync_dir AFTER replace_current"), "{err}");
    }

    #[test]
    fn post_ack_proof_rejects_mutant_inverted_order_root_sync_before_replace() {
        let root = PathBuf::from("/var/tmp/p2-os-unit/kb");
        let mut tracker = PublishTracker::new(root.clone());
        tracker.reset_and_arm(Barrier::None).expect("arm");

        // Inverted order: root sync happens BEFORE replace_current, none after.
        tracker.record_sync_dir(true, &root, None);
        tracker.record_replace_current(true, &root, None, "CURRENT.staged");

        assert_eq!(tracker.root_sync_before_replace_count, 1);
        assert!(!tracker.root_sync_after_replace);
        let err = validate_post_ack_proof(true, &tracker).expect_err("must reject inverted order");
        assert!(err.contains("root_sync_before_replace=1"), "{err}");
    }

    #[test]
    fn post_ack_proof_rejects_unarmed_failed_replace_and_non_durable_view() {
        let root = PathBuf::from("/var/tmp/p2-os-unit/kb");
        let mut tracker = PublishTracker::new(root.clone());
        // Calls while unarmed are ignored.
        tracker.record_replace_current(true, &root, None, "CURRENT.staged");
        tracker.record_sync_dir(true, &root, None);
        assert!(validate_post_ack_proof(true, &tracker).is_err());

        // Armed, but replace_current failed.
        tracker.reset_and_arm(Barrier::None).expect("arm");
        tracker.record_replace_current(false, &root, None, "CURRENT.staged");
        tracker.record_sync_dir(true, &root, None);
        assert!(validate_post_ack_proof(true, &tracker).is_err());

        // Armed, replace + root sync succeeded, but view is NOT durable.
        tracker.record_replace_current(true, &root, None, "CURRENT.staged");
        tracker.record_sync_dir(true, &root, None);
        assert!(validate_post_ack_proof(false, &tracker).is_err());
        assert!(validate_post_ack_proof(true, &tracker).is_ok());

        // Re-arming an already-armed tracker is refused.
        assert!(tracker.reset_and_arm(Barrier::AfterReplaceCurrent).is_err());
    }

    #[test]
    fn unacked_barrier_fires_only_when_armed_and_records_root_sync_false() {
        let root = PathBuf::from("/var/tmp/p2-os-unit/kb");
        let mut tracker = PublishTracker::new(root.clone());
        // Unarmed setup call must not fire.
        assert!(
            tracker
                .record_replace_current(true, &root, None, "CURRENT.staged")
                .is_none()
        );
        assert!(tracker.fired.is_none());

        tracker
            .reset_and_arm(Barrier::AfterReplaceCurrent)
            .expect("arm");
        let rec = tracker
            .record_replace_current(true, &root, None, "CURRENT.staged")
            .expect("armed replace_current must fire");
        assert_eq!(rec.method, "replace_current");
        assert_eq!(rec.dir_path, root);
        assert!(rec.replace_current_completed);
        assert!(!rec.root_sync_after_replace);
    }

    // ---- H1: Genesis baseline framing + validate_retained_rows ----

    #[test]
    fn genesis_baseline_framing_roundtrips_and_rejects_corruption_and_wrong_root() {
        let root = PathBuf::from("/var/tmp/p2-os-unit/kb");
        let genesis = sample_genesis_rows();
        let bytes = encode_genesis_baseline(path_bytes(&root), &genesis).expect("encode");
        let decoded = decode_genesis_baseline(&bytes).expect("decode");
        assert_eq!(decoded.root_bytes, path_bytes(&root));
        assert_eq!(decoded.rows, genesis);

        // Wrong row count (1 or 3) refused.
        assert!(encode_genesis_baseline(path_bytes(&root), &genesis[..1]).is_err());
        let mut three = genesis.clone();
        three.push(StateEntry {
            kind: 1,
            key: b"extra".to_vec(),
            revision: 1,
            body: b"x".to_vec(),
        });
        assert!(encode_genesis_baseline(path_bytes(&root), &three).is_err());

        // Mutated byte -> checksum mismatch.
        let mut bad = bytes.clone();
        bad[0] ^= 1;
        assert!(decode_genesis_baseline(&bad).is_err());

        // Trailing junk -> rejected.
        let mut trailing = bytes.clone();
        trailing.extend_from_slice(b"junk\n");
        assert!(decode_genesis_baseline(&trailing).is_err());

        // Root binding check via temp file (using std::env::temp_dir, never inside KB).
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "p2-os-unit-baseline-{}-{nonce}.v1",
            std::process::id()
        ));
        write_genesis_baseline(&path, &root, &genesis).expect("write wx");
        // Overwrite attempt must fail (create_new / wx).
        assert!(write_genesis_baseline(&path, &root, &genesis).is_err());
        // Matching root succeeds.
        assert_eq!(read_genesis_baseline(&path, &root).expect("read"), genesis);
        // Different root fails.
        assert!(read_genesis_baseline(&path, Path::new("/var/tmp/p2-os-unit/other")).is_err());
        // Inside-KB path rejected.
        assert!(write_genesis_baseline(&root.join("baseline"), &root, &genesis).is_err());
        let _ = fs::remove_file(&path);
        // Missing file fails and is never recreated.
        assert!(read_genesis_baseline(&path, &root).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn retained_rows_validator_accepts_exact_and_rejects_wrong_missing_extra_duplicate_and_unsorted()
     {
        let genesis = sample_genesis_rows();
        let expected2 = fixture::expected_rows(&genesis, 2);
        let expected3 = fixture::expected_rows(&genesis, 3);
        assert_eq!(
            validate_retained_rows(&expected2, &genesis, 2),
            Ok(expected2.len())
        );
        assert_eq!(
            validate_retained_rows(&expected3, &genesis, 3),
            Ok(expected3.len())
        );

        // Mutant: wrong body byte (covers audit/extensions inside body).
        let mut wrong_body = expected3.clone();
        wrong_body[2].body[0] ^= 0xff;
        assert!(validate_retained_rows(&wrong_body, &genesis, 3).is_err());

        // Mutant: wrong revision on Term 1 (rev 1 instead of rev 2 at head 3).
        let mut wrong_rev = expected3.clone();
        let term1 = wrong_rev
            .iter_mut()
            .find(|r| r.kind == 1 && r.revision == 2)
            .expect("term 1 rev 2");
        term1.revision = 1;
        assert!(validate_retained_rows(&wrong_rev, &genesis, 3).is_err());

        // Mutant: missing row (A2 + C1 without B1 — exact H1 review counterexample).
        let mut missing_row = expected3.clone();
        missing_row.pop();
        assert!(validate_retained_rows(&missing_row, &genesis, 3).is_err());

        // Mutant: extra row.
        let mut extra_row = expected3.clone();
        extra_row.push(StateEntry {
            kind: 1,
            key: vec![1, 0, 99, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            revision: 1,
            body: b"extra".to_vec(),
        });
        assert!(validate_retained_rows(&extra_row, &genesis, 3).is_err());

        // Mutant: duplicate row identity.
        let mut dup_row = expected2.clone();
        dup_row[1] = dup_row[0].clone();
        assert!(validate_retained_rows(&dup_row, &genesis, 2).is_err());

        // Mutant: swapped order (proves no silent sorting on actual side).
        let mut swapped = expected3.clone();
        swapped.swap(0, 1);
        assert!(validate_retained_rows(&swapped, &genesis, 3).is_err());

        // Mutant: genesis baseline with wrong row count.
        assert!(validate_retained_rows(&expected2, &genesis[..1], 2).is_err());
    }

    // ---- H2: G0 outcomes + AttemptHistory oracle negatives ----

    #[test]
    fn oracle_and_g0_outcomes_accept_legal_worlds_and_reject_mutants() {
        let ok2 = found_outcome(2, 2);
        let ok3 = found_outcome(3, 3);

        // Positive controls across all three candidate-3 statuses.
        assert!(
            validate_recovered_outcomes_and_oracle(
                Candidate3Status::NotSubmitted,
                2,
                &ok2,
                &ObservedOutcome::Absent,
            )
            .is_ok()
        );
        assert!(
            validate_recovered_outcomes_and_oracle(
                Candidate3Status::Unknown,
                2,
                &ok2,
                &ObservedOutcome::Absent,
            )
            .is_ok()
        );
        assert!(
            validate_recovered_outcomes_and_oracle(Candidate3Status::Unknown, 3, &ok2, &ok3)
                .is_ok()
        );
        assert!(
            validate_recovered_outcomes_and_oracle(Candidate3Status::Durable3, 3, &ok2, &ok3)
                .is_ok()
        );

        // Mutant: wrong original_version on outcome(2) (e.g. 3 instead of 2).
        let wrong_ver2 = found_outcome(2, 3);
        assert!(
            validate_recovered_outcomes_and_oracle(
                Candidate3Status::Unknown,
                3,
                &wrong_ver2,
                &ok3,
            )
            .is_err()
        );

        // Mutant: wrong original_version on outcome(3) (e.g. 2 instead of 3).
        let wrong_ver3 = found_outcome(3, 2);
        assert!(
            validate_recovered_outcomes_and_oracle(
                Candidate3Status::Durable3,
                3,
                &ok2,
                &wrong_ver3,
            )
            .is_err()
        );

        // Mutant: corrupted payload digest inside persisted outcome(2).
        let mut bad_payload2 = fixture::expected_outcome(2);
        bad_payload2.payload_digest[0] ^= 0x55;
        assert!(
            validate_recovered_outcomes_and_oracle(
                Candidate3Status::NotSubmitted,
                2,
                &ObservedOutcome::Found {
                    original_version: 2,
                    event: bad_payload2,
                },
                &ObservedOutcome::Absent,
            )
            .is_err()
        );

        // Mutant: lost ACK2 (head 1, both outcomes absent) rejected under EVERY timeline.
        for status in [
            Candidate3Status::NotSubmitted,
            Candidate3Status::Unknown,
            Candidate3Status::Durable3,
        ] {
            let err = validate_recovered_outcomes_and_oracle(
                status,
                1,
                &ObservedOutcome::Absent,
                &ObservedOutcome::Absent,
            )
            .expect_err("lost ACK2 must be rejected by AttemptHistory");
            assert!(err.contains("AttemptHistory rejected"), "{err}");
        }

        // Mutant: lost ACK3 (head 2 after Durable3) rejected by AttemptHistory.
        let err = validate_recovered_outcomes_and_oracle(
            Candidate3Status::Durable3,
            2,
            &ok2,
            &ObservedOutcome::Absent,
        )
        .expect_err("lost ACK3 must be rejected by AttemptHistory");
        assert!(err.contains("AttemptHistory rejected"), "{err}");

        // Mutant: head 3 with missing outcome(3) rejected (A8 closure).
        assert!(
            validate_recovered_outcomes_and_oracle(
                Candidate3Status::Unknown,
                3,
                &ok2,
                &ObservedOutcome::Absent,
            )
            .is_err()
        );

        // Mutant: head 2 with present outcome(3) rejected (mixed state).
        assert!(
            validate_recovered_outcomes_and_oracle(Candidate3Status::Unknown, 2, &ok2, &ok3)
                .is_err()
        );

        // Mutant: unsubmitted candidate-3 cannot appear at head 3.
        assert!(
            validate_recovered_outcomes_and_oracle(Candidate3Status::NotSubmitted, 3, &ok2, &ok3)
                .is_err()
        );
    }

    #[test]
    fn exact_retry_validator_checks_version_full_event_and_oracle() {
        let ev2 = fixture::expected_outcome(2);
        assert!(validate_exact_retry(2, &ev2).is_ok());
        assert!(validate_exact_retry(3, &ev2).is_err());

        let mut mutated = ev2;
        mutated.result_ref[0] ^= 1;
        assert!(validate_exact_retry(2, &mutated).is_err());
    }

    #[test]
    fn historical_blob_union_includes_rules_superseded_a1_b1_and_v3_blobs() {
        let v2 = declared_blobs_union(2).expect("v2 union");
        assert_eq!(v2.len(), 4);
        assert!(v2.iter().any(|b| b.bytes == fixture::RULES));
        assert!(v2.iter().any(|b| b.bytes == fixture::body(1, 1)));
        assert!(v2.iter().any(|b| b.bytes == fixture::body(2, 1)));
        assert!(v2.iter().any(|b| b.bytes == fixture::result(2)));

        let v3 = declared_blobs_union(3).expect("v3 union");
        assert_eq!(v3.len(), 7);
        assert!(v3.iter().any(|b| b.bytes == fixture::RULES));
        // Superseded A1 and carried-forward B1 from v2 are both in the v3 historical union.
        assert!(v3.iter().any(|b| b.bytes == fixture::body(1, 1)));
        assert!(v3.iter().any(|b| b.bytes == fixture::body(2, 1)));
        assert!(v3.iter().any(|b| b.bytes == fixture::result(2)));
        // Plus v3's own A2, C1, and result(3).
        assert!(v3.iter().any(|b| b.bytes == fixture::body(1, 2)));
        assert!(v3.iter().any(|b| b.bytes == fixture::body(3, 1)));
        assert!(v3.iter().any(|b| b.bytes == fixture::result(3)));
    }

    #[test]
    fn hex_and_unhex_are_canonical() {
        assert_eq!(hex(&[0x00, 0x0a, 0xff]), "000aff");
        assert_eq!(unhex_or_dash("000aff").unwrap(), vec![0x00, 0x0a, 0xff]);
        assert_eq!(unhex_or_dash("-").unwrap(), Vec::<u8>::new());
        assert!(unhex_or_dash("000AFF").is_err());
        assert!(unhex_or_dash("0").is_err());
        assert!(unhex_or_dash("").is_err());
    }
}
