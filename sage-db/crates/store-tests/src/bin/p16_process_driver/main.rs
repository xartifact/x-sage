//! Real SIGKILL and fresh-process recovery over exported MODELED durable state.
//! This binary is a bounded test harness; it does not certify OS/device power-loss durability.
#![forbid(unsafe_code)]

#[allow(dead_code)]
#[path = "../../../tests/support/p16_fixture.rs"]
mod fixture;
mod producer;
#[allow(dead_code)]
mod snapshot;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use producer::{Boundary, MAX_EXPORTED_WORLDS, check_export_bounds, run_producer};
use sage_db_format::current::decode_current;
use sage_db_format::digest::raw_sha256;
use sage_db_format::manifest::read_manifest;
use sage_db_store::fs::{
    Capabilities, CreateDir, Each, Family, FileSystem, FsError, HashReport, Published, WriteOutcome,
};
use sage_db_store::history::{HistoryBudget, HistoryIncomplete, HistoryOutcome};
use sage_db_store::lease::AttemptLedger;
use sage_db_store::names::EntryName;
use sage_db_store::open::{Opened, OutcomeLookup, open_existing};
use sage_db_store::view::{ViewBasis, ViewDurability};
use sage_db_store_tests::model::RelPath;
use sage_db_store_tests::model::fs_model::{ModelCapabilities, ModelFileSystem};
use sage_db_store_tests::oracle::attempts::{
    AttemptHistory, AttemptOutcome, Commit, LogicalRequest, World, find_version_collision,
};
use sage_db_types::framing::{FormatOutcome, Limits};
use sage_db_types::identity::KbVersion;
use sage_db_types::ledger::SharedLedger;
use sage_db_types::persist::{RecordKey, StateEntry};
use sage_db_types::query::RunToCompletion;

const CHILD_DEADLINE: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct SeamMutSyncCounts {
    create_root: usize,
    open_family_create: usize,
    sync_parent_of: usize,
    sync_dir: usize,
    create_new: usize,
    write: usize,
    sync_file: usize,
    sync_entry: usize,
    publish_no_overwrite: usize,
    replace_current: usize,
    remove: usize,
}

#[derive(Clone)]
struct RecoveryObserverFs {
    inner: Rc<ModelFileSystem>,
    counts: Rc<RefCell<SeamMutSyncCounts>>,
}

impl RecoveryObserverFs {
    fn new(inner: Rc<ModelFileSystem>) -> Self {
        Self {
            inner,
            counts: Rc::new(RefCell::new(SeamMutSyncCounts::default())),
        }
    }

    fn counts(&self) -> SeamMutSyncCounts {
        *self.counts.borrow()
    }
}

impl FileSystem for RecoveryObserverFs {
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
        self.counts.borrow_mut().create_root += 1;
        self.inner.create_root(abs)
    }

    fn sync_parent_of(&self, abs: &Path) -> Result<(), FsError> {
        self.counts.borrow_mut().sync_parent_of += 1;
        self.inner.sync_parent_of(abs)
    }

    fn open_family(
        &self,
        root: &Self::Dir,
        family: Family,
        create: CreateDir,
    ) -> Result<Self::Dir, FsError> {
        if create == CreateDir::IfMissing {
            self.counts.borrow_mut().open_family_create += 1;
        }
        self.inner.open_family(root, family, create)
    }

    fn sync_dir(&self, dir: &Self::Dir) -> Result<(), FsError> {
        self.counts.borrow_mut().sync_dir += 1;
        self.inner.sync_dir(dir)
    }

    fn device_id(&self, dir: &Self::Dir) -> Result<u64, FsError> {
        self.inner.device_id(dir)
    }

    fn try_lock_exclusive(&self, root: &Self::Dir) -> Result<Self::Lock, FsError> {
        self.inner.try_lock_exclusive(root)
    }

    fn create_new(&self, dir: &Self::Dir, name: EntryName) -> Result<Self::WriteFile, FsError> {
        self.counts.borrow_mut().create_new += 1;
        self.inner.create_new(dir, name)
    }

    fn write(&self, file: &mut Self::WriteFile, buf: &[u8]) -> Result<WriteOutcome, FsError> {
        self.counts.borrow_mut().write += 1;
        self.inner.write(file, buf)
    }

    fn sync_file(&self, file: &Self::WriteFile) -> Result<(), FsError> {
        self.counts.borrow_mut().sync_file += 1;
        self.inner.sync_file(file)
    }

    fn sync_entry(&self, dir: &Self::Dir, name: EntryName) -> Result<(), FsError> {
        self.counts.borrow_mut().sync_entry += 1;
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
        self.counts.borrow_mut().publish_no_overwrite += 1;
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

fn build_observed_world(head: u64) -> World {
    let req2 = LogicalRequest::new(fixture::PRINCIPAL, fixture::request(2), [2u8; 32]);
    let req3 = LogicalRequest::new(fixture::PRINCIPAL, fixture::request(3), [3u8; 32]);
    match head {
        0 => World {
            base_version: 1,
            commits: vec![Commit {
                version: 0,
                request: req2,
            }],
        },
        1 => World::empty(1),
        2 => World {
            base_version: 1,
            commits: vec![Commit {
                version: 2,
                request: req2,
            }],
        },
        3 => World {
            base_version: 1,
            commits: vec![
                Commit {
                    version: 2,
                    request: req2,
                },
                Commit {
                    version: 3,
                    request: req3,
                },
            ],
        },
        _ => {
            let mut commits = vec![
                Commit {
                    version: 2,
                    request: req2,
                },
                Commit {
                    version: 3,
                    request: req3,
                },
            ];
            for v in 4..=head.min(16) {
                commits.push(Commit {
                    version: v,
                    request: LogicalRequest::new(
                        fixture::PRINCIPAL,
                        &format!("unexpected-{v}"),
                        [0xff; 32],
                    ),
                });
            }
            World {
                base_version: 1,
                commits,
            }
        }
    }
}

fn validate_sequence_oracle(acked: bool, head: u64) -> Result<(), String> {
    let req2 = LogicalRequest::new(fixture::PRINCIPAL, fixture::request(2), [2u8; 32]);
    let req3 = LogicalRequest::new(fixture::PRINCIPAL, fixture::request(3), [3u8; 32]);
    let mut history = AttemptHistory::default();
    history.record(
        "attempt-base-2",
        req2,
        AttemptOutcome::Durable { version: 2 },
    );
    let cand3_outcome = if acked {
        AttemptOutcome::Durable { version: 3 }
    } else {
        AttemptOutcome::Unknown
    };
    history.record("attempt-candidate-3", req3, cand3_outcome);

    let observed_world = build_observed_world(head);
    if let Some(collision) = find_version_collision(&observed_world) {
        return Err(format!("observed world version collision: {collision:?}"));
    }
    let accepted = history
        .accepts(1, &observed_world)
        .map_err(|e| format!("AttemptHistory::accepts error: {e:?}"))?;
    if !accepted {
        return Err(format!(
            "AttemptHistory rejected observed world (acked={acked}, head={head})"
        ));
    }
    Ok(())
}

fn validate_retained_rows(
    actual_rows: &[StateEntry],
    genesis_reference: &[StateEntry],
    head: u64,
) -> Result<(), String> {
    if !matches!(head, 2 | 3) {
        return Err(format!(
            "validate_retained_rows requires head 2 or 3, got {head}"
        ));
    }
    let mut seen = BTreeSet::new();
    for (idx, row) in actual_rows.iter().enumerate() {
        if !seen.insert((row.kind, row.key.as_slice())) {
            return Err(format!(
                "duplicate current identity in actual retained rows at index {idx}: kind={}, key={}",
                row.kind,
                snapshot::hex(&row.key)
            ));
        }
    }
    let expected = fixture::expected_rows(genesis_reference, head);
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
                "retained row mismatch at index {idx} (head={head}): actual kind={} rev={} key={} body_len={} vs expected kind={} rev={} key={} body_len={}",
                actual.kind,
                actual.revision,
                snapshot::hex(&actual.key),
                actual.body.len(),
                exp.kind,
                exp.revision,
                snapshot::hex(&exp.key),
                exp.body.len(),
            ));
        }
    }
    Ok(())
}

fn parse_strict_kv(text: &str, expected_keys: &[&str]) -> Result<BTreeMap<String, String>, String> {
    if text.is_empty() || !text.ends_with('\n') {
        return Err("kv payload must be non-empty and newline-terminated".into());
    }
    let mut map = BTreeMap::new();
    for (line_no, line) in text.lines().enumerate() {
        if line.is_empty() || line.trim() != line {
            return Err(format!(
                "kv line {} is empty or has leading/trailing whitespace",
                line_no + 1
            ));
        }
        let Some((k, v)) = line.split_once('=') else {
            return Err(format!("kv line {} missing '=': {line:?}", line_no + 1));
        };
        if k.is_empty() || v.is_empty() {
            return Err(format!(
                "kv line {} has empty key or value: {line:?}",
                line_no + 1
            ));
        }
        if map.insert(k.to_owned(), v.to_owned()).is_some() {
            return Err(format!("duplicate kv key {k:?}"));
        }
    }
    for &key in expected_keys {
        if !map.contains_key(key) {
            return Err(format!("missing required kv key {key:?}"));
        }
    }
    if map.len() != expected_keys.len() {
        let expected_set: BTreeSet<&str> = expected_keys.iter().copied().collect();
        let extras: Vec<_> = map
            .keys()
            .filter(|k| !expected_set.contains(k.as_str()))
            .cloned()
            .collect();
        return Err(format!("unexpected kv keys: {extras:?}"));
    }
    Ok(map)
}

fn parse_strict_space_kv_line(
    line: &str,
    prefix: &str,
    expected_keys: &[&str],
) -> Result<BTreeMap<String, String>, String> {
    let rest = line
        .strip_prefix(prefix)
        .ok_or_else(|| format!("line missing prefix {prefix:?}: {line:?}"))?;
    let mut map = BTreeMap::new();
    for token in rest.split(' ') {
        if token.is_empty() {
            return Err(format!("empty token in line {line:?}"));
        }
        let Some((k, v)) = token.split_once('=') else {
            return Err(format!("token missing '=' in line {line:?}: {token:?}"));
        };
        if k.is_empty() || v.is_empty() {
            return Err(format!("empty key/val in token {token:?}"));
        }
        if map.insert(k.to_owned(), v.to_owned()).is_some() {
            return Err(format!("duplicate token key {k:?} in {line:?}"));
        }
    }
    for &key in expected_keys {
        if !map.contains_key(key) {
            return Err(format!("missing key {key:?} in line {line:?}"));
        }
    }
    if map.len() != expected_keys.len() {
        return Err(format!("unexpected keys in line {line:?}"));
    }
    Ok(map)
}

fn parse_bool(s: &str, field: &str) -> Result<bool, String> {
    match s {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(format!("invalid bool for {field}: {other:?}")),
    }
}

fn parse_usize(s: &str, field: &str) -> Result<usize, String> {
    s.parse::<usize>()
        .map_err(|e| format!("invalid usize for {field}: {s:?} ({e})"))
}

fn parse_u64(s: &str, field: &str) -> Result<u64, String> {
    s.parse::<u64>()
        .map_err(|e| format!("invalid u64 for {field}: {s:?} ({e})"))
}

fn parse_u32(s: &str, field: &str) -> Result<u32, String> {
    s.parse::<u32>()
        .map_err(|e| format!("invalid u32 for {field}: {s:?} ({e})"))
}

fn validate_killed_status(status: &ExitStatus) -> Result<i32, String> {
    if status.success() {
        return Err(format!("producer exited with success={status:?}"));
    }
    if let Some(code) = status.code() {
        return Err(format!(
            "producer exited normally with code={code} instead of SIGKILL"
        ));
    }
    match status.signal() {
        Some(9) => Ok(9),
        other => Err(format!(
            "producer terminated by signal {other:?} instead of SIGKILL (9)"
        )),
    }
}

fn validate_clean_exit_status(status: &ExitStatus, role: &str) -> Result<i32, String> {
    if let Some(sig) = status.signal() {
        return Err(format!("{role} terminated unexpectedly by signal {sig}"));
    }
    match status.code() {
        Some(0) if status.success() => Ok(0),
        Some(code) => Err(format!("{role} exited with non-zero code={code}")),
        None => Err(format!("{role} ended with neither code nor signal")),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_recover(
    boundary: Boundary,
    world_path: &Path,
    genesis_path: &Path,
    world_index: usize,
    acked: bool,
) -> Result<(), String> {
    let state = snapshot::read(world_path)
        .map_err(|e| format!("read world snapshot {}: {e}", world_path.display()))?;
    let genesis_reference = snapshot::read_rows(genesis_path)
        .map_err(|e| format!("read genesis reference {}: {e}", genesis_path.display()))?;

    let model_fs = Rc::new(
        ModelFileSystem::from_durable_state(
            ModelCapabilities::full(),
            Path::new(fixture::ROOT),
            state,
        )
        .map_err(|e| format!("from_durable_state failed: {e}"))?,
    );
    if model_fs.model().has_unflushed() {
        return Err("restored ModelFileSystem unexpectedly reports unflushed changes".into());
    }

    let recovery_fs = RecoveryObserverFs::new(Rc::clone(&model_fs));
    let ctl = RunToCompletion;
    let mut ledger = AttemptLedger::open(fixture::BYTES, fixture::ENTRIES, &ctl)
        .map_err(|e| format!("open AttemptLedger: {e:?}"))?;

    let opened = open_existing(
        recovery_fs.clone(),
        Path::new(fixture::ROOT),
        Some(fixture::kb()),
        &Limits::V1,
        &mut ledger,
    )
    .map_err(|e| format!("open_existing failed: {e:?}"))?;

    if opened.durability() != ViewDurability::Durable {
        return Err(format!(
            "opened durability {:?} != Durable",
            opened.durability()
        ));
    }

    let mut store = match opened {
        Opened::Ready(store) => store,
        Opened::ReadOnly { step, cause, .. } => {
            return Err(format!(
                "valid completed-op snapshot opened ReadOnly instead of Ready: step={step:?}, cause={cause:?}"
            ));
        }
    };

    if store.is_fenced() {
        return Err("recovered Ready store must not be fenced".into());
    }
    if store.authority_undecided() {
        return Err("recovered Ready store must not have authority_undecided".into());
    }
    if store.is_terminal_corrupt() {
        return Err("recovered Ready store must not be terminal_corrupt".into());
    }
    if !store.coverage_complete() {
        return Err("recovered Ready store must report coverage_complete=true".into());
    }

    let head = store.head().ok_or("recovered Ready store has head=None")?;
    if !boundary.allowed_head(head) {
        return Err(format!(
            "recovered head={head} is illegal for boundary {}",
            boundary.as_str()
        ));
    }

    let actual_rows: Vec<StateEntry> = store.retained_entries().cloned().collect();
    validate_retained_rows(&actual_rows, &genesis_reference, head)?;
    validate_sequence_oracle(acked, head)?;

    let view = store.view().ok_or("recovered Ready store has view=None")?;
    if view.durability() != ViewDurability::Durable {
        return Err(format!(
            "view durability {:?} != Durable",
            view.durability()
        ));
    }
    if view.head().get() != head {
        return Err(format!(
            "view head {} != store.head() {}",
            view.head().get(),
            head
        ));
    }
    if view.entries() != actual_rows.as_slice() {
        return Err("view.entries() differs from store.retained_entries()".into());
    }

    let (current_bytes, current_visible_matches) = {
        let model = model_fs.model();
        let ino = model
            .inode_of(&RelPath::root(), "CURRENT")
            .ok_or("restored model missing CURRENT")?;
        let dur = model
            .durable_bytes(ino)
            .ok_or("restored model CURRENT missing durable bytes")?
            .to_vec();
        let vis_eq = model.visible_bytes(ino) == Some(dur.as_slice());
        (dur, vis_eq)
    };
    if !current_visible_matches {
        return Err("CURRENT visible bytes differ from durable bytes".into());
    }
    let decoded_current = match decode_current(&current_bytes, &Limits::V1, &mut ledger) {
        FormatOutcome::Ok(c) => c,
        other => {
            return Err(format!(
                "decode_current failed on restored CURRENT: {other:?}"
            ));
        }
    };
    if decoded_current.kb_uuid != fixture::kb() {
        return Err(format!(
            "CURRENT kb_uuid {:?} != {:?}",
            decoded_current.kb_uuid,
            fixture::kb()
        ));
    }
    if decoded_current.version != head {
        return Err(format!(
            "CURRENT version {} != recovered head {}",
            decoded_current.version, head
        ));
    }

    let manifests_dir = RelPath::join(&RelPath::root(), Family::Manifests.as_str());
    let manifest_name = decoded_current.manifest_name();
    let (manifest_bytes, manifest_visible_matches) = {
        let model = model_fs.model();
        let ino = model
            .inode_of(&manifests_dir, manifest_name)
            .ok_or_else(|| format!("restored model missing manifest {manifest_name}"))?;
        let dur = model
            .durable_bytes(ino)
            .ok_or_else(|| format!("manifest {manifest_name} missing durable bytes"))?
            .to_vec();
        let vis_eq = model.visible_bytes(ino) == Some(dur.as_slice());
        (dur, vis_eq)
    };
    if !manifest_visible_matches {
        return Err("manifest visible bytes differ from durable bytes".into());
    }
    let actual_manifest_sha256 = raw_sha256(&manifest_bytes);
    if decoded_current.manifest_sha256 != actual_manifest_sha256 {
        return Err("CURRENT manifest_sha256 disagrees with manifest file SHA-256".into());
    }
    let manifest_summary = match read_manifest(&manifest_bytes, &Limits::V1, &mut ledger) {
        FormatOutcome::Ok(m) => m,
        other => return Err(format!("read_manifest failed: {other:?}")),
    };
    if manifest_summary.header.version != head {
        return Err(format!(
            "manifest header version {} != head {}",
            manifest_summary.header.version, head
        ));
    }
    if manifest_summary.header.kb_uuid != fixture::kb() {
        return Err("manifest header kb_uuid mismatch".into());
    }

    let expected_basis = ViewBasis {
        kb_uuid: fixture::kb(),
        version: KbVersion::new(head),
        manifest_sha256: actual_manifest_sha256,
    };
    if view.basis() != expected_basis {
        return Err(format!(
            "view.basis() {:?} != expected actual CURRENT/manifest basis {:?}",
            view.basis(),
            expected_basis
        ));
    }

    match store.outcome(fixture::PRINCIPAL, fixture::request(2)) {
        OutcomeLookup::Found {
            outcome,
            original_version,
            basis,
        } => {
            if original_version != 2 {
                return Err(format!(
                    "G0 base-2 original_version={original_version} != 2"
                ));
            }
            if basis != expected_basis {
                return Err(format!("G0 base-2 basis {basis:?} != {expected_basis:?}"));
            }
            if outcome.event() != &fixture::expected_outcome(2) {
                return Err("G0 base-2 outcome event != fixture::expected_outcome(2)".into());
            }
        }
        other => {
            return Err(format!("G0 base-2 outcome expected Found, got {other:?}"));
        }
    }

    let g0_req3_label = match (head, store.outcome(fixture::PRINCIPAL, fixture::request(3))) {
        (2, OutcomeLookup::Absent { basis }) => {
            if basis != expected_basis {
                return Err(format!(
                    "G0 candidate-3 Absent basis {basis:?} != {expected_basis:?}"
                ));
            }
            "Absent"
        }
        (
            3,
            OutcomeLookup::Found {
                outcome,
                original_version,
                basis,
            },
        ) => {
            if original_version != 3 {
                return Err(format!(
                    "G0 candidate-3 original_version={original_version} != 3"
                ));
            }
            if basis != expected_basis {
                return Err(format!(
                    "G0 candidate-3 Found basis {basis:?} != {expected_basis:?}"
                ));
            }
            if outcome.event() != &fixture::expected_outcome(3) {
                return Err("G0 candidate-3 outcome event != fixture::expected_outcome(3)".into());
            }
            "Found"
        }
        (h, other) => {
            return Err(format!(
                "G0 candidate-3 outcome mismatch at head={h}: got {other:?}"
            ));
        }
    };

    let old_pin = store
        .published_pin()
        .ok_or("recovered Ready store missing published_pin")?;
    let old_pin_head = old_pin.head();
    let old_pin_id = *old_pin.id();
    let old_pin_reserved = old_pin.reserved_bytes();
    let old_pin_entries = old_pin.entries().to_vec();
    let old_pin_ptr = old_pin.entries().as_ptr() as usize;

    let counts_before = recovery_fs.counts();
    store
        .verify(&Limits::V1, &mut ledger)
        .map_err(|e| format!("store.verify failed: {e:?}"))?;
    let counts_after_verify = recovery_fs.counts();
    if counts_after_verify != counts_before {
        return Err(format!(
            "store.verify executed mutating/sync ops: before={counts_before:?}, after={counts_after_verify:?}"
        ));
    }
    if old_pin.head() != old_pin_head
        || *old_pin.id() != old_pin_id
        || old_pin.reserved_bytes() != old_pin_reserved
        || old_pin.entries() != old_pin_entries.as_slice()
        || old_pin.entries().as_ptr() as usize != old_pin_ptr
    {
        return Err("old_pin mutated during store.verify".into());
    }
    if store
        .view()
        .as_ref()
        .map(|v| (v.durability(), v.basis(), v.head()))
        != Some((
            ViewDurability::Durable,
            expected_basis,
            KbVersion::new(head),
        ))
    {
        return Err("store.view() changed after store.verify".into());
    }

    let a1_identity = sage_db_store::fold::RecordIdentity::Identified {
        key: RecordKey::term(1),
    };
    let scanned_events = match store.history(
        &a1_identity,
        1,
        HistoryBudget::new(1024),
        &Limits::V1,
        &mut ledger,
    ) {
        HistoryOutcome::Found(hit) => {
            if hit.source_version != 2 {
                return Err(format!(
                    "history A1 rev1 source_version={} != 2",
                    hit.source_version
                ));
            }
            if hit.image.record() != &fixture::expected_history_a1() {
                return Err("history A1 rev1 record != fixture::expected_history_a1()".into());
            }
            if hit.durability != ViewDurability::Durable {
                return Err(format!(
                    "history A1 rev1 durability {:?} != Durable",
                    hit.durability
                ));
            }
            if hit.basis != expected_basis {
                return Err(format!(
                    "history A1 rev1 basis {:?} != {expected_basis:?}",
                    hit.basis
                ));
            }
            if hit.scanned_events == 0 {
                return Err("history A1 rev1 reported scanned_events == 0".into());
            }
            hit.scanned_events
        }
        HistoryOutcome::Absent { .. } => {
            return Err("history A1 rev1 unexpectedly returned Absent".into());
        }
        HistoryOutcome::Incomplete {
            reason,
            scanned_events,
        } => {
            return Err(format!(
                "history A1 rev1 unexpectedly Incomplete: {reason:?}, scanned={scanned_events}"
            ));
        }
        HistoryOutcome::Error(err) => {
            return Err(format!("history A1 rev1 returned Error: {err:?}"));
        }
    };

    match store.history(
        &a1_identity,
        1,
        HistoryBudget::new(0),
        &Limits::V1,
        &mut ledger,
    ) {
        HistoryOutcome::Incomplete {
            reason: HistoryIncomplete::EventLimit { limit: 0 },
            scanned_events: 0,
        } => {}
        HistoryOutcome::Incomplete {
            reason,
            scanned_events,
        } => {
            return Err(format!(
                "zero-budget history returned wrong Incomplete reason={reason:?}, scanned={scanned_events}"
            ));
        }
        HistoryOutcome::Found(_) => {
            return Err("zero-budget history unexpectedly returned Found".into());
        }
        HistoryOutcome::Absent { .. } => {
            return Err("zero-budget history unexpectedly returned Absent".into());
        }
        HistoryOutcome::Error(err) => {
            return Err(format!("zero-budget history returned Error: {err:?}"));
        }
    }

    let counts_after_history = recovery_fs.counts();
    if counts_after_history != counts_before {
        return Err(format!(
            "store.history executed mutating/sync ops: before={counts_before:?}, after={counts_after_history:?}"
        ));
    }

    drop(store);
    if old_pin.head() != old_pin_head
        || *old_pin.id() != old_pin_id
        || old_pin.reserved_bytes() != old_pin_reserved
        || old_pin.entries() != old_pin_entries.as_slice()
        || old_pin.entries().as_ptr() as usize != old_pin_ptr
    {
        return Err("old_pin mutated after Store drop".into());
    }

    let pid = std::process::id();
    let basis_hex = snapshot::hex(&actual_manifest_sha256);
    println!(
        "RECOVER_OK recovery_pid={pid} boundary={} world_index={world_index} head={head} \
         retained_rows={} g0_req2=Found g0_req3={g0_req3_label} basis_manifest_sha256={basis_hex} \
         verify_mut_sync_delta=0 history_a1_source_version=2 scanned_events={scanned_events}",
        boundary.as_str(),
        actual_rows.len()
    );
    Ok(())
}

struct ProducerCapture {
    producer_pid: u32,
    signal: i32,
    ack_emitted: bool,
    ready_sampled: usize,
    ready_worlds: usize,
    meta: BTreeMap<String, String>,
}

fn write_text_create_new(path: &Path, content: &str) -> Result<(), String> {
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| format!("create {}: {e}", path.display()))?;
    f.write_all(content.as_bytes())
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    f.sync_all()
        .map_err(|e| format!("sync {}: {e}", path.display()))?;
    Ok(())
}

fn join_string_thread(handle: std::thread::JoinHandle<String>, label: &str) -> String {
    match handle.join() {
        Ok(s) => s,
        Err(_) => format!("<{label} thread panicked>"),
    }
}

fn spawn_producer_and_kill(
    exe: &Path,
    boundary: Boundary,
    case_dir: &Path,
) -> Result<ProducerCapture, String> {
    fs::create_dir_all(case_dir)
        .map_err(|e| format!("create case dir {}: {e}", case_dir.display()))?;

    let mut child = Command::new(exe)
        .arg("producer")
        .arg("--boundary")
        .arg(boundary.as_str())
        .arg("--case-dir")
        .arg(case_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn producer for {}: {e}", boundary.as_str()))?;

    let producer_pid = child.id();
    let child_stdin = child.stdin.take().ok_or("missing producer stdin pipe")?;
    let child_stdout = child.stdout.take().ok_or("missing producer stdout pipe")?;
    let child_stderr = child.stderr.take().ok_or("missing producer stderr pipe")?;

    let (tx_line, rx_line) = mpsc::channel::<Option<Result<String, String>>>();
    let stdout_handle = std::thread::spawn(move || {
        let mut reader = BufReader::new(child_stdout);
        let mut raw = String::new();
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    let _ = tx_line.send(None);
                    break;
                }
                Ok(_) => {
                    raw.push_str(&line);
                    let trimmed = line.trim_end_matches(['\r', '\n']).to_owned();
                    let _ = tx_line.send(Some(Ok(trimmed)));
                }
                Err(e) => {
                    let _ = tx_line.send(Some(Err(format!("producer stdout read error: {e}"))));
                    break;
                }
            }
        }
        raw
    });

    let stderr_handle = std::thread::spawn(move || {
        let mut reader = child_stderr;
        let mut buf = String::new();
        let _ = reader.read_to_string(&mut buf);
        buf
    });

    let start = Instant::now();
    let mut saw_ack = false;
    let mut ready_kv: Option<BTreeMap<String, String>> = None;
    let mut loop_error: Option<String> = None;

    loop {
        let elapsed = start.elapsed();
        if elapsed >= CHILD_DEADLINE {
            loop_error = Some(format!(
                "deadline {:?} exceeded waiting for producer READY ({})",
                CHILD_DEADLINE,
                boundary.as_str()
            ));
            break;
        }
        match rx_line.recv_timeout(CHILD_DEADLINE - elapsed) {
            Ok(Some(Ok(line))) => {
                if line == "ACK_TARGET=3" {
                    if !boundary.is_after_ack() {
                        loop_error = Some(format!(
                            "unacked boundary {} emitted ACK_TARGET=3",
                            boundary.as_str()
                        ));
                        break;
                    }
                    if saw_ack {
                        loop_error = Some("after_ack emitted duplicate ACK_TARGET=3".into());
                        break;
                    }
                    saw_ack = true;
                } else if line.starts_with("READY ") {
                    match parse_strict_space_kv_line(
                        &line,
                        "READY ",
                        &["boundary", "pid", "sampled", "worlds", "ack"],
                    ) {
                        Ok(kv) => {
                            if boundary.is_after_ack() && !saw_ack {
                                loop_error =
                                    Some("after_ack emitted READY before ACK_TARGET=3".into());
                            } else {
                                ready_kv = Some(kv);
                            }
                        }
                        Err(e) => {
                            loop_error = Some(format!("malformed READY line: {e}"));
                        }
                    }
                    break;
                } else {
                    loop_error = Some(format!(
                        "unexpected producer stdout line for {}: {line:?}",
                        boundary.as_str()
                    ));
                    break;
                }
            }
            Ok(Some(Err(err))) => {
                loop_error = Some(err);
                break;
            }
            Ok(None) => {
                loop_error = Some(format!(
                    "producer closed stdout before READY ({})",
                    boundary.as_str()
                ));
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                loop_error = Some(format!(
                    "deadline {:?} expired waiting for producer READY ({})",
                    CHILD_DEADLINE,
                    boundary.as_str()
                ));
                break;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                loop_error = Some(format!(
                    "producer stdout channel disconnected before READY ({})",
                    boundary.as_str()
                ));
                break;
            }
        }
    }

    let kill_res = child.kill();
    let status = child
        .wait()
        .map_err(|e| format!("wait on producer {}: {e}", boundary.as_str()))?;
    drop(child_stdin);

    let raw_stdout = join_string_thread(stdout_handle, "producer stdout");
    let raw_stderr = join_string_thread(stderr_handle, "producer stderr");

    write_text_create_new(&case_dir.join("producer.stdout.log"), &raw_stdout)?;
    write_text_create_new(&case_dir.join("producer.stderr.log"), &raw_stderr)?;
    let exit_text = format!(
        "pid={producer_pid}\nsuccess={}\ncode={}\nsignal={}\nkill_invoked_ok={}\n",
        status.success(),
        status
            .code()
            .map_or_else(|| "none".to_owned(), |c| c.to_string()),
        status
            .signal()
            .map_or_else(|| "none".to_owned(), |s| s.to_string()),
        kill_res.is_ok(),
    );
    write_text_create_new(&case_dir.join("producer.exit.kv"), &exit_text)?;

    if let Some(err) = loop_error {
        return Err(err);
    }
    if let Err(e) = kill_res {
        return Err(format!("SIGKILL failed on producer {producer_pid}: {e}"));
    }
    let signal = validate_killed_status(&status)?;
    if !boundary.is_after_ack() && raw_stdout.contains("ACK_TARGET=") {
        return Err(format!(
            "unacked boundary {} raw stdout contained ACK_TARGET",
            boundary.as_str()
        ));
    }
    let ready = ready_kv.ok_or("missing READY payload")?;

    if ready["boundary"] != boundary.as_str() {
        return Err(format!(
            "READY boundary {:?} != {:?}",
            ready["boundary"],
            boundary.as_str()
        ));
    }
    let ready_pid = parse_u32(&ready["pid"], "READY.pid")?;
    if ready_pid != producer_pid {
        return Err(format!(
            "READY pid {ready_pid} != spawned producer_pid {producer_pid}"
        ));
    }
    let ready_sampled = parse_usize(&ready["sampled"], "READY.sampled")?;
    let ready_worlds = parse_usize(&ready["worlds"], "READY.worlds")?;
    let ready_ack = parse_u64(&ready["ack"], "READY.ack")?;
    let expected_ack = if boundary.is_after_ack() { 3 } else { 0 };
    if ready_ack != expected_ack {
        return Err(format!("READY ack={ready_ack} != expected {expected_ack}"));
    }

    let meta_raw =
        fs::read_to_string(case_dir.join("meta.kv")).map_err(|e| format!("read meta.kv: {e}"))?;
    let meta = parse_strict_kv(
        &meta_raw,
        &[
            "boundary",
            "producer_pid",
            "armed_after_version",
            "ack_emitted",
            "ack_version",
            "op_method",
            "op_dir_hex",
            "op_name",
            "replace_current_completed",
            "root_sync_after_replace_completed",
            "sampled",
            "worlds_len",
            "genesis_rows",
        ],
    )?;

    if meta["boundary"] != boundary.as_str() {
        return Err(format!(
            "meta.kv boundary {:?} != {:?}",
            meta["boundary"],
            boundary.as_str()
        ));
    }
    if parse_u32(&meta["producer_pid"], "meta.producer_pid")? != producer_pid {
        return Err("meta.kv producer_pid mismatch".into());
    }
    if parse_u64(&meta["armed_after_version"], "meta.armed_after_version")? != 2 {
        return Err("meta.kv armed_after_version must be 2".into());
    }
    let meta_ack_emitted = parse_bool(&meta["ack_emitted"], "meta.ack_emitted")?;
    let meta_ack_version = parse_u64(&meta["ack_version"], "meta.ack_version")?;
    if meta_ack_emitted != boundary.is_after_ack() || meta_ack_version != expected_ack {
        return Err(format!(
            "meta.kv ack mismatch: ack_emitted={meta_ack_emitted}, ack_version={meta_ack_version}"
        ));
    }
    let meta_sampled = parse_usize(&meta["sampled"], "meta.sampled")?;
    let meta_worlds_len = parse_usize(&meta["worlds_len"], "meta.worlds_len")?;
    check_export_bounds(meta_sampled, meta_worlds_len, MAX_EXPORTED_WORLDS)?;
    if meta_sampled != ready_sampled || meta_worlds_len != ready_worlds {
        return Err("meta.kv sampled/worlds_len disagrees with READY".into());
    }

    let replace_done = parse_bool(
        &meta["replace_current_completed"],
        "meta.replace_current_completed",
    )?;
    let root_sync_done = parse_bool(
        &meta["root_sync_after_replace_completed"],
        "meta.root_sync_after_replace_completed",
    )?;
    match boundary {
        Boundary::FirstCandidateWrite
        | Boundary::FirstSyncFile
        | Boundary::FirstBlobPublish
        | Boundary::TransactionPublish
        | Boundary::ManifestPublish => {
            if replace_done || root_sync_done {
                return Err(format!(
                    "pre-replace boundary {} unexpectedly recorded replace_done={replace_done} root_sync_done={root_sync_done}",
                    boundary.as_str()
                ));
            }
        }
        Boundary::ReplaceCurrent => {
            if !replace_done || root_sync_done {
                return Err(format!(
                    "replace_current boundary recorded replace_done={replace_done} root_sync_done={root_sync_done}"
                ));
            }
        }
        Boundary::RootSyncDir | Boundary::AfterAck => {
            if !replace_done || !root_sync_done {
                return Err(format!(
                    "post-root-sync boundary {} missing completion flags: replace_done={replace_done} root_sync_done={root_sync_done}",
                    boundary.as_str()
                ));
            }
        }
    }

    let mut world_file_count = 0usize;
    for entry in
        fs::read_dir(case_dir).map_err(|e| format!("read_dir {}: {e}", case_dir.display()))?
    {
        let entry = entry.map_err(|e| format!("dir entry: {e}"))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("world-") && name.ends_with(".snap") {
            world_file_count += 1;
        }
    }
    if world_file_count != meta_worlds_len {
        return Err(format!(
            "world file count {world_file_count} != meta worlds_len {meta_worlds_len}"
        ));
    }

    Ok(ProducerCapture {
        producer_pid,
        signal,
        ack_emitted: saw_ack,
        ready_sampled,
        ready_worlds,
        meta,
    })
}

fn spawn_and_wait_child_with_deadline(
    mut child: Child,
    deadline: Duration,
    label: &str,
) -> Result<(u32, ExitStatus, String, String, bool), String> {
    let pid = child.id();
    let stdout = child.stdout.take().ok_or("missing child stdout")?;
    let stderr = child.stderr.take().ok_or("missing child stderr")?;

    let (tx_done, rx_done) = mpsc::channel::<()>();
    let out_handle = std::thread::spawn(move || {
        let mut reader = stdout;
        let mut buf = String::new();
        let _ = reader.read_to_string(&mut buf);
        let _ = tx_done.send(());
        buf
    });
    let err_handle = std::thread::spawn(move || {
        let mut reader = stderr;
        let mut buf = String::new();
        let _ = reader.read_to_string(&mut buf);
        buf
    });

    let timed_out = matches!(
        rx_done.recv_timeout(deadline),
        Err(mpsc::RecvTimeoutError::Timeout)
    );
    if timed_out {
        let _ = child.kill();
    }
    let status = child
        .wait()
        .map_err(|e| format!("wait on {label} (pid={pid}): {e}"))?;
    let raw_stdout = join_string_thread(out_handle, "recovery stdout");
    let raw_stderr = join_string_thread(err_handle, "recovery stderr");
    Ok((pid, status, raw_stdout, raw_stderr, timed_out))
}

#[derive(Clone, Debug)]
struct WorldResult {
    boundary: &'static str,
    world_index: usize,
    recovery_pid: u32,
    exit_code: Option<i32>,
    signal: Option<i32>,
    head: Option<u64>,
    retained_rows: Option<usize>,
    g0_req2: String,
    g0_req3: String,
    basis_manifest_sha256: String,
    ok: bool,
    error: Option<String>,
}

#[derive(Clone, Debug)]
struct CaseResult {
    boundary: &'static str,
    producer_pid: Option<u32>,
    producer_signal: Option<i32>,
    ack_emitted: bool,
    sampled: usize,
    worlds_len: usize,
    op_method: String,
    op_dir_hex: String,
    op_name: String,
    world_results: Vec<WorldResult>,
    ok: bool,
    error: Option<String>,
}

#[allow(clippy::too_many_arguments)]
fn run_world_recovery(
    exe: &Path,
    boundary: Boundary,
    case_dir: &Path,
    world_index: usize,
    producer_pid: u32,
    parent_pid: u32,
    acked: bool,
) -> WorldResult {
    let world_path = case_dir.join(format!("world-{world_index:04}.snap"));
    let genesis_path = case_dir.join("genesis_preservation_reference.snap");
    let stdout_log_path = case_dir.join(format!("recover-{world_index:04}.stdout.log"));
    let stderr_log_path = case_dir.join(format!("recover-{world_index:04}.stderr.log"));
    let exit_log_path = case_dir.join(format!("recover-{world_index:04}.exit.kv"));

    let mut res = WorldResult {
        boundary: boundary.as_str(),
        world_index,
        recovery_pid: 0,
        exit_code: None,
        signal: None,
        head: None,
        retained_rows: None,
        g0_req2: String::new(),
        g0_req3: String::new(),
        basis_manifest_sha256: String::new(),
        ok: false,
        error: None,
    };

    let child = match Command::new(exe)
        .arg("recover")
        .arg("--boundary")
        .arg(boundary.as_str())
        .arg("--world")
        .arg(&world_path)
        .arg("--genesis")
        .arg(&genesis_path)
        .arg("--world-index")
        .arg(world_index.to_string())
        .arg("--acked")
        .arg(if acked { "true" } else { "false" })
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            res.error = Some(format!("spawn recover world {world_index}: {e}"));
            return res;
        }
    };

    let (recovery_pid, status, raw_stdout, raw_stderr, timed_out) =
        match spawn_and_wait_child_with_deadline(child, CHILD_DEADLINE, "recover") {
            Ok(v) => v,
            Err(e) => {
                res.error = Some(e);
                return res;
            }
        };

    res.recovery_pid = recovery_pid;
    res.exit_code = status.code();
    res.signal = status.signal();

    let mut write_errors = Vec::new();
    if let Err(e) = write_text_create_new(&stdout_log_path, &raw_stdout) {
        write_errors.push(e);
    }
    if let Err(e) = write_text_create_new(&stderr_log_path, &raw_stderr) {
        write_errors.push(e);
    }
    let exit_text = format!(
        "recovery_pid={recovery_pid}\nsuccess={}\ncode={}\nsignal={}\ntimed_out={timed_out}\n",
        status.success(),
        status
            .code()
            .map_or_else(|| "none".to_owned(), |c| c.to_string()),
        status
            .signal()
            .map_or_else(|| "none".to_owned(), |s| s.to_string()),
    );
    if let Err(e) = write_text_create_new(&exit_log_path, &exit_text) {
        write_errors.push(e);
    }
    if !write_errors.is_empty() {
        res.error = Some(format!(
            "recovery artifact write failed: {}",
            write_errors.join("; ")
        ));
        return res;
    }

    if timed_out {
        res.error = Some(format!(
            "recover world {world_index} timed out after {:?}",
            CHILD_DEADLINE
        ));
        return res;
    }
    if recovery_pid == producer_pid || recovery_pid == parent_pid {
        res.error = Some(format!(
            "recovery_pid {recovery_pid} is not distinct from producer_pid={producer_pid} and parent_pid={parent_pid}"
        ));
        return res;
    }
    if let Err(e) = validate_clean_exit_status(&status, "recover") {
        res.error = Some(format!("{e}; stderr={}", raw_stderr.trim()));
        return res;
    }

    let line = raw_stdout.trim_end_matches(['\r', '\n']);
    if raw_stdout.lines().count() != 1 {
        res.error = Some(format!(
            "recover stdout must be exactly 1 line, got {:?}",
            raw_stdout
        ));
        return res;
    }
    let parsed = match parse_strict_space_kv_line(
        line,
        "RECOVER_OK ",
        &[
            "recovery_pid",
            "boundary",
            "world_index",
            "head",
            "retained_rows",
            "g0_req2",
            "g0_req3",
            "basis_manifest_sha256",
            "verify_mut_sync_delta",
            "history_a1_source_version",
            "scanned_events",
        ],
    ) {
        Ok(m) => m,
        Err(e) => {
            res.error = Some(format!("malformed RECOVER_OK: {e}"));
            return res;
        }
    };

    if parsed["boundary"] != boundary.as_str() {
        res.error = Some("RECOVER_OK boundary mismatch".into());
        return res;
    }
    if parse_u32(&parsed["recovery_pid"], "recovery_pid").ok() != Some(recovery_pid) {
        res.error = Some("RECOVER_OK pid mismatch".into());
        return res;
    }
    if parse_usize(&parsed["world_index"], "world_index").ok() != Some(world_index) {
        res.error = Some("RECOVER_OK world_index mismatch".into());
        return res;
    }
    let head = match parse_u64(&parsed["head"], "head") {
        Ok(h) => h,
        Err(e) => {
            res.error = Some(e);
            return res;
        }
    };
    let rows = match parse_usize(&parsed["retained_rows"], "retained_rows") {
        Ok(r) => r,
        Err(e) => {
            res.error = Some(e);
            return res;
        }
    };
    if parsed["verify_mut_sync_delta"] != "0" {
        res.error = Some(format!(
            "RECOVER_OK verify_mut_sync_delta={} != 0",
            parsed["verify_mut_sync_delta"]
        ));
        return res;
    }
    if parsed["history_a1_source_version"] != "2" {
        res.error = Some(format!(
            "RECOVER_OK history_a1_source_version={} != 2",
            parsed["history_a1_source_version"]
        ));
        return res;
    }
    match parse_u64(&parsed["scanned_events"], "scanned_events") {
        Ok(n) if n > 0 => {}
        Ok(0) => {
            res.error = Some("RECOVER_OK scanned_events == 0".into());
            return res;
        }
        Ok(_) => unreachable!(),
        Err(e) => {
            res.error = Some(e);
            return res;
        }
    }
    if parsed["g0_req2"] != "Found" {
        res.error = Some(format!("RECOVER_OK g0_req2={} != Found", parsed["g0_req2"]));
        return res;
    }
    let expected_g0_req3 = if head == 2 { "Absent" } else { "Found" };
    if parsed["g0_req3"] != expected_g0_req3 {
        res.error = Some(format!(
            "RECOVER_OK g0_req3={} != {expected_g0_req3} at head={head}",
            parsed["g0_req3"]
        ));
        return res;
    }
    res.head = Some(head);
    res.retained_rows = Some(rows);
    res.g0_req2 = parsed["g0_req2"].clone();
    res.g0_req3 = parsed["g0_req3"].clone();
    res.basis_manifest_sha256 = parsed["basis_manifest_sha256"].clone();
    res.ok = true;
    res
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn write_suite_reports(artifact_dir: &Path, cases: &[CaseResult]) -> Result<(), String> {
    let mut tsv = String::from(
        "boundary\tproducer_pid\tproducer_signal\tack_emitted\tsampled\tworlds_len\t\
         world_index\trecovery_pid\texit_code\trecovery_signal\thead\tretained_rows\t\
         g0_req2\tg0_req3\tbasis_manifest_sha256\tstatus\terror\n",
    );
    for c in cases {
        if c.world_results.is_empty() {
            tsv.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}\t{}\t-\t-\t-\t-\t-\t-\t-\t-\t-\t{}\t{}\n",
                c.boundary,
                c.producer_pid
                    .map_or_else(|| "-".to_owned(), |p| p.to_string()),
                c.producer_signal
                    .map_or_else(|| "-".to_owned(), |s| s.to_string()),
                c.ack_emitted,
                c.sampled,
                c.worlds_len,
                if c.ok { "PASS" } else { "FAIL" },
                c.error.as_deref().unwrap_or("-"),
            ));
        } else {
            for w in &c.world_results {
                tsv.push_str(&format!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                    c.boundary,
                    c.producer_pid
                        .map_or_else(|| "-".to_owned(), |p| p.to_string()),
                    c.producer_signal
                        .map_or_else(|| "-".to_owned(), |s| s.to_string()),
                    c.ack_emitted,
                    c.sampled,
                    c.worlds_len,
                    w.world_index,
                    w.recovery_pid,
                    w.exit_code
                        .map_or_else(|| "-".to_owned(), |x| x.to_string()),
                    w.signal.map_or_else(|| "-".to_owned(), |s| s.to_string()),
                    w.head.map_or_else(|| "-".to_owned(), |h| h.to_string()),
                    w.retained_rows
                        .map_or_else(|| "-".to_owned(), |r| r.to_string()),
                    if w.g0_req2.is_empty() {
                        "-"
                    } else {
                        &w.g0_req2
                    },
                    if w.g0_req3.is_empty() {
                        "-"
                    } else {
                        &w.g0_req3
                    },
                    if w.basis_manifest_sha256.is_empty() {
                        "-"
                    } else {
                        &w.basis_manifest_sha256
                    },
                    if w.ok { "PASS" } else { "FAIL" },
                    w.error.as_deref().unwrap_or("-"),
                ));
            }
        }
    }
    write_text_create_new(&artifact_dir.join("results.tsv"), &tsv)?;

    let total_worlds: usize = cases.iter().map(|c| c.world_results.len()).sum();
    let failed_cases = cases.iter().filter(|c| !c.ok).count();
    let mut json = String::new();
    json.push_str("{\n");
    json.push_str(
        "  \"scope_note\": \"Simulated ModelFileSystem DurableState exported before real process SIGKILL; genuine fresh-process recovery of modeled state, NOT OS/device power-loss durability, cross-process model lock, all-input, or allocation proof.\",\n",
    );
    json.push_str(&format!("  \"cases_count\": {},\n", cases.len()));
    json.push_str(&format!("  \"worlds_total\": {},\n", total_worlds));
    json.push_str(&format!("  \"failed_cases\": {},\n", failed_cases));
    json.push_str("  \"cases\": [\n");
    for (ci, c) in cases.iter().enumerate() {
        json.push_str("    {\n");
        json.push_str(&format!(
            "      \"boundary\": {},\n",
            json_escape(c.boundary)
        ));
        json.push_str(&format!(
            "      \"producer_pid\": {},\n",
            c.producer_pid
                .map_or_else(|| "null".to_owned(), |p| p.to_string())
        ));
        json.push_str(&format!(
            "      \"producer_signal\": {},\n",
            c.producer_signal
                .map_or_else(|| "null".to_owned(), |s| s.to_string())
        ));
        json.push_str(&format!("      \"ack_emitted\": {},\n", c.ack_emitted));
        json.push_str(&format!("      \"sampled\": {},\n", c.sampled));
        json.push_str(&format!("      \"worlds_len\": {},\n", c.worlds_len));
        json.push_str(&format!(
            "      \"op_method\": {},\n",
            json_escape(&c.op_method)
        ));
        json.push_str(&format!(
            "      \"op_dir_hex\": {},\n",
            json_escape(&c.op_dir_hex)
        ));
        json.push_str(&format!(
            "      \"op_name\": {},\n",
            json_escape(&c.op_name)
        ));
        json.push_str(&format!("      \"ok\": {},\n", c.ok));
        json.push_str(&format!(
            "      \"error\": {},\n",
            c.error
                .as_deref()
                .map_or_else(|| "null".to_owned(), json_escape)
        ));
        json.push_str("      \"worlds\": [\n");
        for (wi, w) in c.world_results.iter().enumerate() {
            json.push_str("        {\n");
            json.push_str(&format!("          \"world_index\": {},\n", w.world_index));
            json.push_str(&format!(
                "          \"recovery_pid\": {},\n",
                w.recovery_pid
            ));
            json.push_str(&format!(
                "          \"exit_code\": {},\n",
                w.exit_code
                    .map_or_else(|| "null".to_owned(), |v| v.to_string())
            ));
            json.push_str(&format!(
                "          \"signal\": {},\n",
                w.signal
                    .map_or_else(|| "null".to_owned(), |v| v.to_string())
            ));
            json.push_str(&format!(
                "          \"head\": {},\n",
                w.head.map_or_else(|| "null".to_owned(), |v| v.to_string())
            ));
            json.push_str(&format!(
                "          \"retained_rows\": {},\n",
                w.retained_rows
                    .map_or_else(|| "null".to_owned(), |v| v.to_string())
            ));
            json.push_str(&format!(
                "          \"g0_req2\": {},\n",
                json_escape(&w.g0_req2)
            ));
            json.push_str(&format!(
                "          \"g0_req3\": {},\n",
                json_escape(&w.g0_req3)
            ));
            json.push_str(&format!(
                "          \"basis_manifest_sha256\": {},\n",
                json_escape(&w.basis_manifest_sha256)
            ));
            json.push_str(&format!("          \"ok\": {},\n", w.ok));
            json.push_str(&format!(
                "          \"error\": {}\n",
                w.error
                    .as_deref()
                    .map_or_else(|| "null".to_owned(), json_escape)
            ));
            json.push_str(if wi + 1 == c.world_results.len() {
                "        }\n"
            } else {
                "        },\n"
            });
        }
        json.push_str("      ]\n");
        json.push_str(if ci + 1 == cases.len() {
            "    }\n"
        } else {
            "    },\n"
        });
    }
    json.push_str("  ]\n");
    json.push_str("}\n");
    write_text_create_new(&artifact_dir.join("results.json"), &json)?;
    Ok(())
}

fn run_suite(artifact_dir: &Path) -> Result<(), String> {
    if artifact_dir.exists() {
        let mut entries = fs::read_dir(artifact_dir)
            .map_err(|e| format!("read artifact_dir {}: {e}", artifact_dir.display()))?;
        if entries.next().is_some() {
            return Err(format!(
                "artifact_dir {} already exists and is non-empty; refusing to overwrite or mix artifacts",
                artifact_dir.display()
            ));
        }
    } else {
        fs::create_dir_all(artifact_dir)
            .map_err(|e| format!("create artifact_dir {}: {e}", artifact_dir.display()))?;
    }

    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let parent_pid = std::process::id();
    let mut all_cases = Vec::new();
    let mut recovery_pids = BTreeSet::new();

    println!(
        "SCOPE_NOTE kind=modeled_state_crash_replay \
         statement=\"ModelFileSystem DurableState exported before real SIGKILL; fresh-process recovery replays modeled state, NOT OS/device power-loss durability, cross-process model lock, all-input, or allocation proof.\""
    );

    for (idx, &boundary) in Boundary::ALL.iter().enumerate() {
        let case_dir = artifact_dir.join(format!("{:02}-{}", idx + 1, boundary.as_str()));
        let mut case_res = CaseResult {
            boundary: boundary.as_str(),
            producer_pid: None,
            producer_signal: None,
            ack_emitted: false,
            sampled: 0,
            worlds_len: 0,
            op_method: String::new(),
            op_dir_hex: String::new(),
            op_name: String::new(),
            world_results: Vec::new(),
            ok: false,
            error: None,
        };

        match spawn_producer_and_kill(&exe, boundary, &case_dir) {
            Ok(capture) => {
                case_res.producer_pid = Some(capture.producer_pid);
                case_res.producer_signal = Some(capture.signal);
                case_res.ack_emitted = capture.ack_emitted;
                case_res.sampled = capture.ready_sampled;
                case_res.worlds_len = capture.ready_worlds;
                case_res.op_method = capture.meta["op_method"].clone();
                case_res.op_dir_hex = capture.meta["op_dir_hex"].clone();
                case_res.op_name = capture.meta["op_name"].clone();

                let mut observed_heads = BTreeSet::new();
                let mut world_failures = 0usize;
                for w_idx in 0..capture.ready_worlds {
                    let wr = run_world_recovery(
                        &exe,
                        boundary,
                        &case_dir,
                        w_idx,
                        capture.producer_pid,
                        parent_pid,
                        boundary.is_after_ack(),
                    );
                    if !recovery_pids.insert(wr.recovery_pid) && wr.recovery_pid != 0 {
                        let mut wr_dup = wr;
                        wr_dup.ok = false;
                        wr_dup.error = Some(format!(
                            "duplicate recovery_pid {} across recovery invocations",
                            wr_dup.recovery_pid
                        ));
                        world_failures += 1;
                        println!(
                            "WORLD_RESULT boundary={} world_index={} recovery_pid={} head=- ok=false error={:?}",
                            wr_dup.boundary,
                            wr_dup.world_index,
                            wr_dup.recovery_pid,
                            wr_dup.error.as_deref().unwrap_or("")
                        );
                        case_res.world_results.push(wr_dup);
                        continue;
                    }
                    if let Some(h) = wr.head {
                        observed_heads.insert(h);
                    }
                    if !wr.ok {
                        world_failures += 1;
                    }
                    println!(
                        "WORLD_RESULT boundary={} world_index={} recovery_pid={} exit_code={} signal={} head={} retained_rows={} g0_req2={} g0_req3={} ok={}",
                        wr.boundary,
                        wr.world_index,
                        wr.recovery_pid,
                        wr.exit_code
                            .map_or_else(|| "none".to_owned(), |c| c.to_string()),
                        wr.signal
                            .map_or_else(|| "none".to_owned(), |s| s.to_string()),
                        wr.head.map_or_else(|| "none".to_owned(), |h| h.to_string()),
                        wr.retained_rows
                            .map_or_else(|| "none".to_owned(), |r| r.to_string()),
                        if wr.g0_req2.is_empty() {
                            "-"
                        } else {
                            &wr.g0_req2
                        },
                        if wr.g0_req3.is_empty() {
                            "-"
                        } else {
                            &wr.g0_req3
                        },
                        wr.ok
                    );
                    case_res.world_results.push(wr);
                }

                if world_failures > 0 {
                    case_res.error = Some(format!("{world_failures} world(s) failed recovery"));
                } else if boundary == Boundary::ReplaceCurrent
                    && (!observed_heads.contains(&2) || !observed_heads.contains(&3))
                {
                    case_res.error = Some(format!(
                        "replace_current must produce both head=2 and head=3 across crash worlds, observed {observed_heads:?}"
                    ));
                } else {
                    case_res.ok = true;
                }
            }
            Err(err) => {
                case_res.error = Some(err);
            }
        }

        println!(
            "CASE_RESULT boundary={} producer_pid={} signal={} ack={} sampled={} worlds={} op_method={} op_dir_hex={} op_name={} ok={} error={}",
            case_res.boundary,
            case_res
                .producer_pid
                .map_or_else(|| "none".to_owned(), |p| p.to_string()),
            case_res
                .producer_signal
                .map_or_else(|| "none".to_owned(), |s| s.to_string()),
            case_res.ack_emitted,
            case_res.sampled,
            case_res.worlds_len,
            if case_res.op_method.is_empty() {
                "-"
            } else {
                &case_res.op_method
            },
            if case_res.op_dir_hex.is_empty() {
                "-"
            } else {
                &case_res.op_dir_hex
            },
            if case_res.op_name.is_empty() {
                "-"
            } else {
                &case_res.op_name
            },
            case_res.ok,
            case_res.error.as_deref().unwrap_or("none")
        );
        all_cases.push(case_res);
    }

    write_suite_reports(artifact_dir, &all_cases)?;

    let total_worlds: usize = all_cases.iter().map(|c| c.world_results.len()).sum();
    let failed_cases = all_cases.iter().filter(|c| !c.ok).count();
    let status_label = if failed_cases == 0 { "PASS" } else { "FAIL" };
    println!(
        "SUITE_SUMMARY parent_pid={parent_pid} cases={} worlds={} failed={} status={status_label} artifact_dir={}",
        all_cases.len(),
        total_worlds,
        failed_cases,
        artifact_dir.display()
    );
    if failed_cases > 0 {
        return Err(format!("{failed_cases} suite case(s) failed"));
    }
    Ok(())
}

fn flag_value(args: &[String], flag: &str) -> Result<String, String> {
    let pos = args
        .iter()
        .position(|a| a == flag)
        .ok_or_else(|| format!("missing required flag {flag}"))?;
    args.get(pos + 1)
        .cloned()
        .ok_or_else(|| format!("missing value for flag {flag}"))
}

fn optional_flag_value(args: &[String], flag: &str) -> Result<Option<String>, String> {
    match args.iter().position(|a| a == flag) {
        Some(pos) => args
            .get(pos + 1)
            .cloned()
            .map(Some)
            .ok_or_else(|| format!("missing value for flag {flag}")),
        None => Ok(None),
    }
}

fn run_cli() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map_or("suite", String::as_str);
    match cmd {
        "producer" => {
            let b_str = flag_value(&args, "--boundary")?;
            let boundary =
                Boundary::parse(&b_str).ok_or_else(|| format!("unknown boundary {b_str:?}"))?;
            let case_dir = PathBuf::from(flag_value(&args, "--case-dir")?);
            run_producer(boundary, &case_dir)
        }
        "recover" => {
            let b_str = flag_value(&args, "--boundary")?;
            let boundary =
                Boundary::parse(&b_str).ok_or_else(|| format!("unknown boundary {b_str:?}"))?;
            let world = PathBuf::from(flag_value(&args, "--world")?);
            let genesis = PathBuf::from(flag_value(&args, "--genesis")?);
            let world_index = parse_usize(&flag_value(&args, "--world-index")?, "world-index")?;
            let acked = parse_bool(&flag_value(&args, "--acked")?, "acked")?;
            run_recover(boundary, &world, &genesis, world_index, acked)
        }
        "suite" => {
            let dir = match optional_flag_value(&args, "--artifact-dir")? {
                Some(p) => PathBuf::from(p),
                None => std::env::temp_dir().join(format!("p16-suite-{}", std::process::id())),
            };
            run_suite(&dir)
        }
        other => Err(format!("unknown subcommand {other:?}")),
    }
}

fn main() {
    if let Err(err) = run_cli() {
        eprintln!("DRIVER_ERROR: {err}");
        let _ = io::stderr().flush();
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sage_db_store::init::bootstrap;

    #[test]
    fn sequence_oracle_enforces_unacked_and_after_ack_obligations() {
        assert!(validate_sequence_oracle(false, 2).is_ok());
        assert!(validate_sequence_oracle(false, 3).is_ok());
        assert!(validate_sequence_oracle(false, 1).is_err());
        assert!(validate_sequence_oracle(false, 0).is_err());
        assert!(validate_sequence_oracle(false, 4).is_err());

        assert!(validate_sequence_oracle(true, 3).is_ok());
        assert!(validate_sequence_oracle(true, 2).is_err());
        assert!(validate_sequence_oracle(true, 1).is_err());
        assert!(validate_sequence_oracle(true, 0).is_err());
        assert!(validate_sequence_oracle(true, 4).is_err());
    }

    #[test]
    fn boundary_head_rules_reject_illegal_versions() {
        for b in [
            Boundary::FirstCandidateWrite,
            Boundary::FirstSyncFile,
            Boundary::FirstBlobPublish,
            Boundary::TransactionPublish,
            Boundary::ManifestPublish,
        ] {
            assert!(b.allowed_head(2));
            assert!(!b.allowed_head(3));
            assert!(!b.allowed_head(1));
        }
        assert!(Boundary::ReplaceCurrent.allowed_head(2));
        assert!(Boundary::ReplaceCurrent.allowed_head(3));
        assert!(!Boundary::ReplaceCurrent.allowed_head(1));
        for b in [Boundary::RootSyncDir, Boundary::AfterAck] {
            assert!(b.allowed_head(3));
            assert!(!b.allowed_head(2));
            assert!(!b.allowed_head(1));
        }
    }

    #[test]
    fn retained_row_validator_catches_duplicates_reordering_and_extension_tampering() {
        let model_fs = ModelFileSystem::new(ModelCapabilities::full());
        let ctl = RunToCompletion;
        let mut ledger = AttemptLedger::open(fixture::BYTES, fixture::ENTRIES, &ctl).unwrap();
        let store = bootstrap(
            model_fs,
            Path::new(fixture::ROOT),
            CreateDir::IfMissing,
            fixture::kb(),
            fixture::RULES,
            &Limits::V1,
            &mut ledger,
        )
        .unwrap();
        let genesis: Vec<StateEntry> = store.retained_entries().cloned().collect();
        let expected_v3 = fixture::expected_rows(&genesis, 3);
        assert!(validate_retained_rows(&expected_v3, &genesis, 3).is_ok());

        let mut dup = expected_v3.clone();
        let first = dup[0].clone();
        dup.push(first);
        assert!(validate_retained_rows(&dup, &genesis, 3).is_err());

        let mut reordered = expected_v3.clone();
        reordered.reverse();
        assert!(validate_retained_rows(&reordered, &genesis, 3).is_err());

        let mut tampered = expected_v3;
        let term_idx = tampered
            .iter()
            .position(|r| r.kind == 1)
            .expect("term row must exist");
        let needle = b"process.note";
        let pos = tampered[term_idx]
            .body
            .windows(needle.len())
            .position(|w| w == needle)
            .expect("process.note extension must exist in term row body");
        tampered[term_idx].body[pos] ^= 0x01;
        assert!(validate_retained_rows(&tampered, &genesis, 3).is_err());
    }

    #[test]
    fn strict_kv_and_process_status_validators_reject_defects() {
        let good_kv = "a=1\nb=two\n";
        let parsed = parse_strict_kv(good_kv, &["a", "b"]).unwrap();
        assert_eq!(parsed["a"], "1");
        assert_eq!(parsed["b"], "two");

        assert!(parse_strict_kv("a=1\nb=two", &["a", "b"]).is_err());
        assert!(parse_strict_kv("a=1\na=2\nb=two\n", &["a", "b"]).is_err());
        assert!(parse_strict_kv("a=1\n", &["a", "b"]).is_err());
        assert!(parse_strict_kv("a=1\nb=two\nc=3\n", &["a", "b"]).is_err());
        assert!(parse_strict_kv("a=\nb=two\n", &["a", "b"]).is_err());

        assert_eq!(validate_killed_status(&ExitStatus::from_raw(9)).unwrap(), 9);
        assert!(validate_killed_status(&ExitStatus::from_raw(15)).is_err());
        assert!(validate_killed_status(&ExitStatus::from_raw(0)).is_err());
        assert!(validate_killed_status(&ExitStatus::from_raw(1 << 8)).is_err());

        assert_eq!(
            validate_clean_exit_status(&ExitStatus::from_raw(0), "recover").unwrap(),
            0
        );
        assert!(validate_clean_exit_status(&ExitStatus::from_raw(1 << 8), "recover").is_err());
        assert!(validate_clean_exit_status(&ExitStatus::from_raw(9), "recover").is_err());
    }

    #[test]
    fn fixture_distinct_body_refs_and_uuid_binding_hold() {
        assert_ne!(fixture::body(1, 1), fixture::body(2, 1));
        assert_ne!(fixture::body(1, 1), fixture::body(1, 2));
        assert_ne!(fixture::body(1, 2), fixture::body(3, 1));
        assert_ne!(fixture::result(2), fixture::result(3));

        let c2 = fixture::carrier(2);
        let c3 = fixture::carrier(3);
        assert_eq!(c2.kb_uuid, fixture::kb());
        assert_eq!(c3.kb_uuid, fixture::kb());
        assert_eq!(c2.version, 2);
        assert_eq!(c3.version, 3);
        assert_eq!(c2.blobs.len(), 4);
        assert_eq!(c3.blobs.len(), 4);
        let c3_hashes: BTreeSet<[u8; 32]> = c3.blobs.iter().map(|b| b.sha256).collect();
        assert_eq!(c3_hashes.len(), 4, "candidate-3 blob refs must be distinct");
    }

    #[test]
    fn recovery_observer_counts_open_family_if_missing_only() {
        let model_fs = Rc::new(ModelFileSystem::new(ModelCapabilities::full()));
        let observer = RecoveryObserverFs::new(Rc::clone(&model_fs));
        let root = observer.create_root(Path::new(fixture::ROOT)).unwrap();
        let before = observer.counts();
        assert_eq!(before.open_family_create, 0);

        let err = observer
            .open_family(&root, Family::Blobs, CreateDir::Never)
            .unwrap_err();
        assert_eq!(err.kind(), sage_db_store::fs::FsErrorKind::NotFound);
        assert_eq!(
            observer.counts(),
            before,
            "CreateDir::Never must not increment open_family_create"
        );

        let _ = observer
            .open_family(&root, Family::Blobs, CreateDir::IfMissing)
            .unwrap();
        assert_eq!(
            observer.counts().open_family_create,
            before.open_family_create + 1
        );

        let _ = observer
            .open_family(&root, Family::Blobs, CreateDir::Never)
            .unwrap();
        assert_eq!(
            observer.counts().open_family_create,
            before.open_family_create + 1
        );
    }
}
