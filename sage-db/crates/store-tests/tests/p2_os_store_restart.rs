//! Real `Store` + real `OsFileSystem` over real files: process-level qualification.
//!
//! These cases require a host whose platform policy accepts the filesystem: `linux/x86_64`, the
//! validated kernel prefix, Btrfs, no voiding mount option, and `/var/tmp` on the same `st_dev` as
//! `/opt/xartifact/x-sage/data/kb` (checked via read-only `stat` only; nothing ever writes to
//! production data). They are `#[ignore]` so non-qualifying hosts still compile and run the pure
//! controls, while the durable-Store claim is only ever earned on a qualifying host.
//!
//! Run on the qualified target with:
//!
//! ```text
//! cargo test -p sage-db-store-tests --test p2_os_store_restart -- --ignored --nocapture --test-threads=1
//! ```
//!
//! On a non-qualifying host the ignored cases **fail** rather than skip.
//!
//! Every child process is wrapped in a kill+reap RAII guard on every exit path, waits under an
//! absolute deadline over real pipes, captures complete remaining stdout/stderr upon exit or kill
//! (never saving a truncated wait prefix as the complete child transcript), and persists raw
//! artifacts with `create_new` (`wx`) before parsing any field.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Absolute deadline for any real-OS qualification child to reach its READY line or exit.
const CHILD_DEADLINE: Duration = Duration::from_secs(60);
/// Short deadline used by pure negative controls so deadline failures finish fast.
const SHORT_TEST_DEADLINE: Duration = Duration::from_millis(250);
/// Exit code reserved exclusively for `OpenStep::Lock` / `WouldBlock` / `NoEffect`.
const EXIT_LOCK_REFUSAL: i32 = 42;
/// Exit code emitted by the driver when `OsFileSystem::for_root` refuses the host/path.
const EXIT_HOST_NOT_QUALIFIED: i32 = 45;
/// Exit code emitted by the driver on internal assertion/contract failure.
const EXIT_DRIVER_ASSERT: i32 = 3;

const SCRATCH_PARENT: &str = "/var/tmp";
const PROD_KB_PATH: &str = "/opt/xartifact/x-sage/data/kb";

static NONCE_SEQ: AtomicU64 = AtomicU64::new(1);

fn next_nonce() -> String {
    let seq = NONCE_SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    format!("{}-{nanos}-{seq}", std::process::id())
}

/// Complete, untruncated output and exit status of one child process.
#[derive(Debug)]
struct ChildCapture {
    pid: u32,
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

#[derive(Debug)]
enum WaitFailure {
    DeadlineExpired { seen: Vec<String> },
    PrematureEof { seen: Vec<String> },
}

impl WaitFailure {
    fn seen(&self) -> &[String] {
        match self {
            Self::DeadlineExpired { seen } | Self::PrematureEof { seen } => seen,
        }
    }
}

/// RAII child guard that kills and reaps on every path (including assertion panic unwinding)
/// while continuously draining BOTH stdout and stderr until EOF so no post-READY output is lost.
struct Guarded {
    label: String,
    pid: u32,
    child: Option<Child>,
    stdout_rx: Option<mpsc::Receiver<String>>,
    stdout_thread: Option<JoinHandle<String>>,
    stderr_thread: Option<JoinHandle<String>>,
}

impl Guarded {
    fn spawn(label: &str, args: &[&str]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_p2_os_store_driver"))
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("{label}: spawn failed: {e}"));
        let pid = child.id();
        let stdout = child.stdout.take().expect("child stdout piped");
        let stderr = child.stderr.take().expect("child stderr piped");

        let (tx, rx) = mpsc::channel::<String>();
        let stdout_thread = std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut full = String::new();
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        full.push_str(&line);
                        let trimmed = line.trim_end_matches(&['\n', '\r'][..]).to_owned();
                        // Ignore receiver drop: keep draining to EOF until child dies so the
                        // complete child transcript up to process death is preserved.
                        let _ = tx.send(trimmed);
                    }
                }
            }
            full
        });
        let stderr_thread = std::thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut full = String::new();
            let _ = reader.read_to_string(&mut full);
            full
        });

        Self {
            label: label.to_owned(),
            pid,
            child: Some(child),
            stdout_rx: Some(rx),
            stdout_thread: Some(stdout_thread),
            stderr_thread: Some(stderr_thread),
        }
    }

    fn pid(&self) -> u32 {
        self.pid
    }

    fn take_stdin(&mut self) -> Option<std::process::ChildStdin> {
        self.child.as_mut().and_then(|c| c.stdin.take())
    }

    /// Wait over the real stdout pipe until an exact line equals `needle`, under an absolute
    /// deadline. Does NOT discard subsequent stdout/stderr; call `kill_and_collect` afterwards
    /// to obtain the complete transcript up to child death.
    fn wait_for_line_with_deadline(
        &mut self,
        needle: &str,
        deadline: Duration,
    ) -> Result<Vec<String>, WaitFailure> {
        let rx = self
            .stdout_rx
            .as_ref()
            .expect("stdout receiver active before collect");
        let end = Instant::now() + deadline;
        let mut seen = Vec::new();
        loop {
            let remaining = end.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(WaitFailure::DeadlineExpired { seen });
            }
            match rx.recv_timeout(remaining) {
                Ok(line) => {
                    let matched = line == needle;
                    seen.push(line);
                    if matched {
                        return Ok(seen);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    return Err(WaitFailure::DeadlineExpired { seen });
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(WaitFailure::PrematureEof { seen });
                }
            }
        }
    }

    fn join_streams(&mut self) -> (String, String) {
        // Drop the line receiver first; the stdout thread keeps draining to EOF regardless.
        let _ = self.stdout_rx.take();
        let stdout = self
            .stdout_thread
            .take()
            .map(|h| h.join().expect("stdout reader thread panicked"))
            .unwrap_or_default();
        let stderr = self
            .stderr_thread
            .take()
            .map(|h| h.join().expect("stderr reader thread panicked"))
            .unwrap_or_default();
        (stdout, stderr)
    }

    /// SIGKILL, reap, and drain ALL remaining stdout/stderr until EOF.
    fn kill_and_collect(&mut self) -> Option<ChildCapture> {
        let mut child = self.child.take()?;
        let _ = child.kill();
        let status = child
            .wait()
            .unwrap_or_else(|e| panic!("{}: reap after kill failed: {e}", self.label));
        let (stdout, stderr) = self.join_streams();
        Some(ChildCapture {
            pid: self.pid,
            status,
            stdout,
            stderr,
        })
    }

    /// Wait for a child to exit under an absolute deadline, capturing complete stdout/stderr.
    /// On deadline expiry, kills and reaps the child and returns its captured output alongside
    /// the timeout failure.
    fn wait_exit_with_deadline(
        &mut self,
        deadline: Duration,
    ) -> Result<ChildCapture, (WaitFailure, ChildCapture)> {
        let end = Instant::now() + deadline;
        loop {
            let poll = {
                let child = self.child.as_mut().expect("child present until reaped");
                child
                    .try_wait()
                    .unwrap_or_else(|e| panic!("{}: try_wait failed: {e}", self.label))
            };
            if let Some(status) = poll {
                let _ = self.child.take();
                let (stdout, stderr) = self.join_streams();
                return Ok(ChildCapture {
                    pid: self.pid,
                    status,
                    stdout,
                    stderr,
                });
            }
            if Instant::now() >= end {
                let capture = self
                    .kill_and_collect()
                    .expect("child present on deadline expiry");
                let seen = capture.stdout.lines().map(str::to_owned).collect();
                return Err((WaitFailure::DeadlineExpired { seen }, capture));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for Guarded {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // Every exit path, including panic unwinding, kills and reaps the child.
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = self.stdout_rx.take();
        if let Some(handle) = self.stdout_thread.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.stderr_thread.take() {
            let _ = handle.join();
        }
    }
}

/// Write a new artifact file with `create_new(true)` (`wx`): never overwrites prior output.
fn write_new_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(contents)?;
    file.sync_all()?;
    Ok(())
}

/// Persist one child's complete raw artifacts BEFORE parsing anything.
fn save_capture(artifacts: &Path, name: &str, capture: &ChildCapture) {
    write_new_file(
        &artifacts.join(format!("{name}.stdout.log")),
        capture.stdout.as_bytes(),
    )
    .unwrap_or_else(|e| panic!("write {name}.stdout.log: {e}"));
    write_new_file(
        &artifacts.join(format!("{name}.stderr.log")),
        capture.stderr.as_bytes(),
    )
    .unwrap_or_else(|e| panic!("write {name}.stderr.log: {e}"));
    let exit_text = format!(
        "pid={}\nsuccess={}\ncode={:?}\nsignal={:?}\n",
        capture.pid,
        capture.status.success(),
        capture.status.code(),
        capture.status.signal()
    );
    write_new_file(
        &artifacts.join(format!("{name}.exit.kv")),
        exit_text.as_bytes(),
    )
    .unwrap_or_else(|e| panic!("write {name}.exit.kv: {e}"));
}

/// Verify that `status` represents death by SIGKILL(9) — rejecting normal exits and other signals.
fn verify_killed_by_sigkill(status: ExitStatus) -> Result<(), String> {
    if status.success() {
        return Err(format!(
            "killed child unexpectedly succeeded: code={:?} signal={:?}",
            status.code(),
            status.signal()
        ));
    }
    if let Some(code) = status.code() {
        return Err(format!(
            "expected SIGKILL(9), but child exited normally with code {code}"
        ));
    }
    match status.signal() {
        Some(9) => Ok(()),
        other => Err(format!(
            "expected SIGKILL(9), but child terminated with signal {other:?}"
        )),
    }
}

/// Parse complete stdout into structured `KEY=VALUE` lines, preserving order and rejecting
/// malformed lines or conflicting duplicate keys.
struct ParsedOutput {
    ordered: Vec<(String, String)>,
    map: BTreeMap<String, String>,
}

impl ParsedOutput {
    fn parse(stdout: &str) -> Result<Self, String> {
        let mut ordered = Vec::new();
        let mut map = BTreeMap::new();
        for (idx, line) in stdout.lines().enumerate() {
            let Some((k, v)) = line.split_once('=') else {
                return Err(format!("line {} is not KEY=VALUE: {line:?}", idx + 1));
            };
            if k.is_empty() {
                return Err(format!("line {} has empty key: {line:?}", idx + 1));
            }
            if let Some(prev) = map.insert(k.to_owned(), v.to_owned())
                && prev != v
            {
                return Err(format!(
                    "conflicting duplicate key {k:?}: prev={prev:?} new={v:?}"
                ));
            }
            ordered.push((k.to_owned(), v.to_owned()));
        }
        Ok(Self { ordered, map })
    }

    fn require(&self, key: &str) -> &str {
        self.map
            .get(key)
            .map(String::as_str)
            .unwrap_or_else(|| panic!("missing required output key {key:?}"))
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.map.get(key).map(String::as_str)
    }

    fn require_eq(&self, key: &str, expected: &str) {
        let actual = self.require(key);
        assert_eq!(
            actual, expected,
            "key {key:?}: expected {expected:?}, got {actual:?}"
        );
    }

    fn index_of(&self, key: &str, value: &str) -> Option<usize> {
        self.ordered
            .iter()
            .position(|(k, v)| k == key && v == value)
    }
}

fn assert_pid_matches(parsed: &ParsedOutput, expected_pid: u32) {
    let reported: u32 = parsed
        .require("PID")
        .parse()
        .expect("PID must parse as u32");
    assert_eq!(
        reported, expected_pid,
        "child-reported PID disagrees with OS-spawned PID"
    );
}

/// Nonced private KB root under `/var/tmp` (for real-OS ignored target tests).
///
/// When `precreate_empty` is false, the parent directory is created and the KB root itself is left
/// absent (missing-root bootstrap). When true, the empty KB root directory is created beforehand
/// (existing-empty-root bootstrap supported by platform R2).
fn target_nonced_root(tag: &str, precreate_empty: bool) -> PathBuf {
    let base = Path::new(SCRATCH_PARENT).join(format!("p2-os-store-{tag}-{}", next_nonce()));
    fs::create_dir(&base).unwrap_or_else(|e| panic!("create {}: {e}", base.display()));
    let root = base.join("kb");
    if precreate_empty {
        fs::create_dir(&root)
            .unwrap_or_else(|e| panic!("create empty root {}: {e}", root.display()));
        assert_eq!(
            fs::read_dir(&root).expect("read empty root").count(),
            0,
            "pre-created root must be empty"
        );
    }
    root
}

fn target_artifact_dir(tag: &str) -> PathBuf {
    assert_eq!(
        std::env::consts::OS,
        "linux",
        "ignored real-OS qualification cases require linux/x86_64 (got {}/{})",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    assert_eq!(
        std::env::consts::ARCH,
        "x86_64",
        "ignored real-OS qualification cases require linux/x86_64 (got {}/{})",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    let base =
        Path::new(SCRATCH_PARENT).join(format!("p2-os-store-artifacts-{tag}-{}", next_nonce()));
    fs::create_dir(&base).unwrap_or_else(|e| panic!("create artifact dir {}: {e}", base.display()));
    base
}

/// Nonced artifact directory under `std::env::temp_dir()` for host-independent pure controls.
fn pure_artifact_dir(tag: &str) -> PathBuf {
    let base =
        std::env::temp_dir().join(format!("p2-os-store-pure-artifacts-{tag}-{}", next_nonce()));
    fs::create_dir(&base)
        .unwrap_or_else(|e| panic!("create pure artifact dir {}: {e}", base.display()));
    base
}

fn run_to_exit(artifacts: &Path, name: &str, args: &[&str]) -> ChildCapture {
    let mut guarded = Guarded::spawn(name, args);
    match guarded.wait_exit_with_deadline(CHILD_DEADLINE) {
        Ok(capture) => {
            save_capture(artifacts, name, &capture);
            capture
        }
        Err((failure, capture)) => {
            save_capture(artifacts, name, &capture);
            panic!(
                "{name}: wait_exit failed ({failure:?}); artifacts={}\nstdout:\n{}\nstderr:\n{}",
                artifacts.display(),
                capture.stdout,
                capture.stderr
            );
        }
    }
}

/// Wait for `needle` over the child pipe, kill and reap on failure (preserving raw artifacts),
/// and return the seen prefix on success (caller MUST later call `kill_and_collect` and validate
/// the COMPLETE captured transcript, never treating `seen` as the complete transcript).
fn wait_ready_or_fail(
    guarded: &mut Guarded,
    needle: &str,
    deadline: Duration,
    artifacts: &Path,
    name: &str,
) -> Result<Vec<String>, (WaitFailure, ChildCapture)> {
    match guarded.wait_for_line_with_deadline(needle, deadline) {
        Ok(seen) => Ok(seen),
        Err(failure) => {
            let capture = guarded
                .kill_and_collect()
                .expect("child present on wait failure");
            save_capture(artifacts, name, &capture);
            Err((failure, capture))
        }
    }
}

/// Record and assert all target host qualification facts BEFORE any durable Store claim.
///
/// Fails (never skips) on any non-qualifying host. Production KB path is ONLY `stat`ed read-only.
fn require_qualified_host(artifacts: &Path) {
    let probe_path = Path::new(SCRATCH_PARENT).join(format!("p2-os-store-probe-{}", next_nonce()));
    let probe = run_to_exit(
        artifacts,
        "host-probe",
        &["host-probe", probe_path.to_str().expect("utf8 probe path")],
    );

    assert_eq!(
        std::env::consts::OS,
        "linux",
        "qualified target must be linux (got {}); probe stderr:\n{}",
        std::env::consts::OS,
        probe.stderr
    );
    assert_eq!(
        std::env::consts::ARCH,
        "x86_64",
        "qualified target must be x86_64 (got {}); probe stderr:\n{}",
        std::env::consts::ARCH,
        probe.stderr
    );

    let kernel_release = fs::read_to_string("/proc/sys/kernel/osrelease")
        .unwrap_or_else(|e| panic!("read /proc/sys/kernel/osrelease: {e}"))
        .trim()
        .to_owned();
    assert!(
        !kernel_release.is_empty(),
        "empty /proc/sys/kernel/osrelease"
    );

    // Read-only stat ONLY on scratch parent and production KB path; zero writes to production data.
    let scratch_meta = fs::metadata(SCRATCH_PARENT)
        .unwrap_or_else(|e| panic!("stat {SCRATCH_PARENT} failed: {e}"));
    let prod_meta =
        fs::metadata(PROD_KB_PATH).unwrap_or_else(|e| panic!("stat {PROD_KB_PATH} failed: {e}"));
    let scratch_dev = scratch_meta.dev();
    let prod_dev = prod_meta.dev();

    let summary = format!(
        "target_os={}\ntarget_arch={}\nkernel_release={kernel_release}\n\
         scratch_parent={SCRATCH_PARENT}\nscratch_parent_dev={scratch_dev}\n\
         prod_kb_path={PROD_KB_PATH}\nprod_kb_dev={prod_dev}\n\
         same_dev={}\nprobe_exit={:?}\n",
        std::env::consts::OS,
        std::env::consts::ARCH,
        scratch_dev == prod_dev,
        probe.status.code(),
    );
    write_new_file(&artifacts.join("host-qualification.kv"), summary.as_bytes())
        .expect("write host-qualification.kv");

    assert_eq!(
        scratch_dev, prod_dev,
        "scratch parent {SCRATCH_PARENT} (st_dev={scratch_dev}) must share st_dev with \
         {PROD_KB_PATH} (st_dev={prod_dev})"
    );
    assert_eq!(
        probe.status.code(),
        Some(0),
        "host-probe refused this host (exit {:?}):\nstdout:\n{}\nstderr:\n{}",
        probe.status.code(),
        probe.stdout,
        probe.stderr
    );
    let parsed = ParsedOutput::parse(&probe.stdout).expect("parse host-probe stdout");
    assert_pid_matches(&parsed, probe.pid);
    parsed.require_eq("POLICY", "for_root/Documented");
    parsed.require_eq("TARGET_OS", "linux");
    parsed.require_eq("TARGET_ARCH", "x86_64");
    parsed.require_eq("KERNEL_RELEASE", &kernel_release);
    parsed.require_eq("HOST_PROBE", "PASS");
}

fn assert_seed_ok(capture: &ChildCapture, baseline: &Path, expected_shape: &str) -> ParsedOutput {
    assert_eq!(
        capture.status.code(),
        Some(0),
        "seed failed:\nstdout:\n{}\nstderr:\n{}",
        capture.stdout,
        capture.stderr
    );
    let parsed = ParsedOutput::parse(&capture.stdout).expect("parse seed stdout");
    assert_pid_matches(&parsed, capture.pid);
    parsed.require_eq("POLICY", "for_root/Documented");
    parsed.require_eq("SEED_ROOT_SHAPE", expected_shape);
    parsed.require_eq("GENESIS_ROWS", "2");
    assert_eq!(
        parsed.require("GENESIS_DIGEST").len(),
        64,
        "GENESIS_DIGEST must be 64-char sha256 hex"
    );
    assert_eq!(
        parsed.require("GENESIS_BASELINE"),
        baseline.to_str().expect("utf8 baseline"),
        "seed must record the exact external baseline path"
    );
    assert!(baseline.is_file(), "genesis baseline file must exist");
    parsed.require_eq("ACK2", "2");
    parsed.require_eq("HEAD", "2");
    parsed.require_eq("SEED", "PASS");
    parsed
}

fn assert_reopen_ok(
    capture: &ChildCapture,
    expected_candidate3: &str,
    allowed_heads: &[u64],
) -> (u64, ParsedOutput) {
    assert_eq!(
        capture.status.code(),
        Some(0),
        "reopen failed:\nstdout:\n{}\nstderr:\n{}",
        capture.stdout,
        capture.stderr
    );
    let parsed = ParsedOutput::parse(&capture.stdout).expect("parse reopen stdout");
    assert_pid_matches(&parsed, capture.pid);
    parsed.require_eq("POLICY", "for_root/Documented");
    parsed.require_eq("REOPEN_DURABILITY", "Durable");
    let head: u64 = parsed
        .require("REOPEN_HEAD")
        .parse()
        .expect("REOPEN_HEAD must be u64");
    assert!(
        allowed_heads.contains(&head),
        "REOPEN_HEAD={head} not in allowed set {allowed_heads:?}"
    );
    parsed.require_eq("ROWS_MATCH_EXPECTED", "true");
    parsed.require_eq("GENESIS_PRESERVED_EXACT", "true");
    parsed.require_eq("OUTCOME2_ORIGINAL_VERSION", "2");
    parsed.require_eq("OUTCOME2_MATCHES_FIXTURE", "true");
    if head == 2 {
        parsed.require_eq("OUTCOME3_STATE", "Absent");
        assert!(
            parsed.get("OUTCOME3_ORIGINAL_VERSION").is_none(),
            "head 2 must not report an outcome(3) version"
        );
        parsed.require_eq("BLOB_COVERAGE_SCOPE", "historical_union_genesis_through_v2");
        parsed.require_eq("BLOBS_VERIFIED_COUNT", "4");
    } else {
        parsed.require_eq("OUTCOME3_STATE", "Found");
        parsed.require_eq("OUTCOME3_ORIGINAL_VERSION", "3");
        parsed.require_eq("OUTCOME3_MATCHES_FIXTURE", "true");
        parsed.require_eq("BLOB_COVERAGE_SCOPE", "historical_union_genesis_through_v3");
        parsed.require_eq("BLOBS_VERIFIED_COUNT", "7");
    }
    parsed.require_eq("ORACLE_CANDIDATE3_STATUS", expected_candidate3);
    parsed.require_eq("ORACLE_ACCEPTED", "true");
    parsed.require_eq("REOPEN", "PASS");
    (head, parsed)
}

// ---------------------------------------------------------------------------
// Real-OS qualification cases (#[ignore]; FAIL on non-qualifying hosts)
// ---------------------------------------------------------------------------

#[test]
#[ignore = "requires a policy-qualified host (linux/x86_64, validated kernel, Btrfs, /var/tmp same st_dev as KB)"]
fn real_os_store_seed_retry_and_whole_state_recovery() {
    let artifacts = target_artifact_dir("seed-retry");
    require_qualified_host(&artifacts);

    let root = target_nonced_root("seed", false);
    let baseline = artifacts.join("seed.genesis-baseline.v1");
    assert!(
        !baseline.starts_with(&root),
        "baseline must live outside KB"
    );

    let seed = run_to_exit(
        &artifacts,
        "seed",
        &[
            "seed",
            root.to_str().expect("utf8 root"),
            "--genesis-baseline",
            baseline.to_str().expect("utf8 baseline"),
            "--expect-missing-root",
        ],
    );
    let _ = assert_seed_ok(&seed, &baseline, "missing_root");

    let retry = run_to_exit(
        &artifacts,
        "retry",
        &["retry", root.to_str().expect("utf8 root")],
    );
    assert_ne!(retry.pid, seed.pid, "retry must run in a fresh PID");
    assert_eq!(
        retry.status.code(),
        Some(0),
        "retry failed:\nstdout:\n{}\nstderr:\n{}",
        retry.stdout,
        retry.stderr
    );
    let retry_kv = ParsedOutput::parse(&retry.stdout).expect("parse retry stdout");
    assert_pid_matches(&retry_kv, retry.pid);
    retry_kv.require_eq("RETRY_MUTATIONS", "0");
    retry_kv.require_eq("RETRY_OUTCOME_MATCHES_FIXTURE", "true");
    retry_kv.require_eq("RETRY_ORACLE_ACCEPTED", "true");
    retry_kv.require_eq("CURRENT_UNCHANGED", "true");
    retry_kv.require_eq("HEAD_AFTER_RETRY", "2");
    retry_kv.require_eq("RETRY", "PASS");

    let reopen = run_to_exit(
        &artifacts,
        "reopen",
        &[
            "reopen",
            root.to_str().expect("utf8 root"),
            "--genesis-baseline",
            baseline.to_str().expect("utf8 baseline"),
            "--candidate3-outcome",
            "none",
        ],
    );
    assert_ne!(reopen.pid, seed.pid, "reopen must run in a fresh PID");
    assert_ne!(reopen.pid, retry.pid, "reopen must differ from retry PID");
    let (head, _) = assert_reopen_ok(&reopen, "none", &[2]);
    assert_eq!(head, 2);

    println!("P2_OS_SEED_RETRY_ARTIFACTS={}", artifacts.display());
}

#[test]
#[ignore = "requires a policy-qualified host (linux/x86_64, validated kernel, Btrfs, /var/tmp same st_dev as KB)"]
fn real_os_store_existing_empty_root_bootstrap_and_recovery() {
    let artifacts = target_artifact_dir("existing-empty");
    require_qualified_host(&artifacts);

    // Pre-create an EMPTY KB root directory; platform R2 `create_root` returns `AlreadyExists`
    // and `init::bootstrap` opens the existing root instead.
    let root = target_nonced_root("existing-empty", true);
    let baseline = artifacts.join("existing-empty.genesis-baseline.v1");
    assert!(
        !baseline.starts_with(&root),
        "baseline must live outside KB"
    );

    let seed = run_to_exit(
        &artifacts,
        "existing-empty-seed",
        &[
            "seed",
            root.to_str().expect("utf8 root"),
            "--genesis-baseline",
            baseline.to_str().expect("utf8 baseline"),
            "--expect-existing-empty",
        ],
    );
    let _ = assert_seed_ok(&seed, &baseline, "existing_empty");

    let retry = run_to_exit(
        &artifacts,
        "existing-empty-retry",
        &["retry", root.to_str().expect("utf8 root")],
    );
    assert_ne!(retry.pid, seed.pid);
    assert_eq!(retry.status.code(), Some(0));
    let retry_kv = ParsedOutput::parse(&retry.stdout).expect("parse existing-empty retry");
    retry_kv.require_eq("RETRY_MUTATIONS", "0");
    retry_kv.require_eq("RETRY_OUTCOME_MATCHES_FIXTURE", "true");
    retry_kv.require_eq("RETRY_ORACLE_ACCEPTED", "true");
    retry_kv.require_eq("RETRY", "PASS");

    let reopen = run_to_exit(
        &artifacts,
        "existing-empty-reopen",
        &[
            "reopen",
            root.to_str().expect("utf8 root"),
            "--genesis-baseline",
            baseline.to_str().expect("utf8 baseline"),
            "--candidate3-outcome",
            "none",
        ],
    );
    assert_ne!(reopen.pid, seed.pid);
    assert_ne!(reopen.pid, retry.pid);
    let (head, _) = assert_reopen_ok(&reopen, "none", &[2]);
    assert_eq!(head, 2);

    println!("P2_OS_EXISTING_EMPTY_ARTIFACTS={}", artifacts.display());
}

#[test]
#[ignore = "requires a policy-qualified host (linux/x86_64, validated kernel, Btrfs, /var/tmp same st_dev as KB)"]
fn real_os_store_unacked_kill_recovers_old_whole_or_new_whole() {
    let artifacts = target_artifact_dir("unacked");
    require_qualified_host(&artifacts);

    let root = target_nonced_root("unacked-seed", false);
    let baseline = artifacts.join("unacked.genesis-baseline.v1");
    let seed = run_to_exit(
        &artifacts,
        "unacked-seed",
        &[
            "seed",
            root.to_str().expect("utf8 root"),
            "--genesis-baseline",
            baseline.to_str().expect("utf8 baseline"),
            "--expect-missing-root",
        ],
    );
    let _ = assert_seed_ok(&seed, &baseline, "missing_root");

    // Producer opens Ready(2) unarmed, arms for candidate-3, and blocks INSIDE replace_current
    // after the rename succeeds and BEFORE returning to Store (no root sync, no outer ACK).
    let mut guarded = Guarded::spawn(
        "unacked-producer",
        &["unacked", root.to_str().expect("utf8 root")],
    );
    let producer_pid = guarded.pid();
    assert_ne!(producer_pid, seed.pid);
    let _ready_prefix = wait_ready_or_fail(
        &mut guarded,
        "STATE=steady",
        CHILD_DEADLINE,
        &artifacts,
        "unacked-producer",
    )
    .unwrap_or_else(|(failure, capture)| {
        panic!(
            "unacked-producer did not reach STATE=steady ({failure:?}):\nstdout:\n{}\nstderr:\n{}",
            capture.stdout, capture.stderr
        )
    });

    // SIGKILL, reap, and drain ALL remaining stdout/stderr until child death.
    let capture = guarded.kill_and_collect().expect("kill producer");
    save_capture(&artifacts, "unacked-producer", &capture);
    verify_killed_by_sigkill(capture.status).expect("unacked producer must die by SIGKILL(9)");

    // Validate the COMPLETE stdout transcript up to process death (never a truncated prefix).
    let kv = ParsedOutput::parse(&capture.stdout).expect("parse complete unacked-producer stdout");
    assert_pid_matches(&kv, producer_pid);
    kv.require_eq("PRE_ARM_ARMED", "false");
    kv.require_eq("PRE_ARM_FIRED", "none");
    kv.require_eq("PRE_ARM_REPLACE_CURRENT", "0");
    kv.require_eq("ARMED", "barrier_after_replace_current");
    kv.require_eq("BARRIER", "after_replace_current");
    kv.require_eq("FIRED_METHOD", "replace_current");
    kv.require_eq("REPLACE_CURRENT_COMPLETED", "true");
    kv.require_eq("ROOT_SYNC_AFTER_REPLACE", "false");
    kv.require_eq("STATE", "steady");
    assert!(
        kv.get("ACK3").is_none() && !capture.stdout.contains("ACK3"),
        "unacked producer must never emit ACK3 across its entire lifetime:\n{}",
        capture.stdout
    );

    // Fresh PID recovers the same real files; candidate-3 status is Unknown.
    let reopen = run_to_exit(
        &artifacts,
        "unacked-reopen",
        &[
            "reopen",
            root.to_str().expect("utf8 root"),
            "--genesis-baseline",
            baseline.to_str().expect("utf8 baseline"),
            "--candidate3-outcome",
            "unknown",
        ],
    );
    assert_ne!(
        reopen.pid, producer_pid,
        "recovery must run in a fresh PID distinct from producer"
    );
    assert_ne!(reopen.pid, seed.pid);
    let (head, _) = assert_reopen_ok(&reopen, "unknown", &[2, 3]);
    assert!(head == 2 || head == 3, "head must be 2 or 3, got {head}");

    println!("P2_OS_UNACKED_ARTIFACTS={}", artifacts.display());
}

#[test]
#[ignore = "requires a policy-qualified host (linux/x86_64, validated kernel, Btrfs, /var/tmp same st_dev as KB)"]
fn real_os_store_acked_kill_keeps_new_whole_state() {
    let artifacts = target_artifact_dir("post-ack");
    require_qualified_host(&artifacts);

    let root = target_nonced_root("post-ack-seed", false);
    let baseline = artifacts.join("post-ack.genesis-baseline.v1");
    let seed = run_to_exit(
        &artifacts,
        "post-ack-seed",
        &[
            "seed",
            root.to_str().expect("utf8 root"),
            "--genesis-baseline",
            baseline.to_str().expect("utf8 baseline"),
            "--expect-missing-root",
        ],
    );
    let _ = assert_seed_ok(&seed, &baseline, "missing_root");

    let mut guarded = Guarded::spawn(
        "post-ack-producer",
        &["post-ack", root.to_str().expect("utf8 root")],
    );
    let producer_pid = guarded.pid();
    assert_ne!(producer_pid, seed.pid);
    let _ready_prefix = wait_ready_or_fail(
        &mut guarded,
        "STATE=post_ack_steady",
        CHILD_DEADLINE,
        &artifacts,
        "post-ack-producer",
    )
    .unwrap_or_else(|(failure, capture)| {
        panic!(
            "post-ack-producer did not reach STATE=post_ack_steady ({failure:?}):\nstdout:\n{}\nstderr:\n{}",
            capture.stdout, capture.stderr
        )
    });

    let capture = guarded.kill_and_collect().expect("kill producer");
    save_capture(&artifacts, "post-ack-producer", &capture);
    verify_killed_by_sigkill(capture.status).expect("post-ack producer must die by SIGKILL(9)");

    let kv = ParsedOutput::parse(&capture.stdout).expect("parse complete post-ack-producer stdout");
    assert_pid_matches(&kv, producer_pid);
    kv.require_eq("PRE_ARM_ARMED", "false");
    kv.require_eq("PRE_ARM_FIRED", "none");
    kv.require_eq("PRE_ARM_REPLACE_CURRENT", "0");
    kv.require_eq("REPLACE_CURRENT_COMPLETED", "true");
    kv.require_eq("ROOT_SYNC_AFTER_REPLACE", "true");
    let root_sync_count: u32 = kv
        .require("ROOT_SYNC_AFTER_REPLACE_COUNT")
        .parse()
        .expect("ROOT_SYNC_AFTER_REPLACE_COUNT must be u32");
    assert!(
        root_sync_count >= 1,
        "post-ACK3 must record at least one ROOT sync_dir AFTER replace_current"
    );
    kv.require_eq("ACK3", "3");
    kv.require_eq("STATE", "post_ack_steady");

    // Verify causal emission order: root sync proof BEFORE ACK3, ACK3 BEFORE steady barrier.
    let idx_root_sync = kv
        .index_of("ROOT_SYNC_AFTER_REPLACE", "true")
        .expect("ROOT_SYNC_AFTER_REPLACE line");
    let idx_ack3 = kv.index_of("ACK3", "3").expect("ACK3 line");
    let idx_steady = kv
        .index_of("STATE", "post_ack_steady")
        .expect("STATE=post_ack_steady line");
    assert!(
        idx_root_sync < idx_ack3 && idx_ack3 < idx_steady,
        "expected ROOT_SYNC_AFTER_REPLACE before ACK3 before STATE=post_ack_steady"
    );

    // Fresh PID recovers the same real files; candidate-3 status is Durable(3) — never lost.
    let reopen = run_to_exit(
        &artifacts,
        "post-ack-reopen",
        &[
            "reopen",
            root.to_str().expect("utf8 root"),
            "--genesis-baseline",
            baseline.to_str().expect("utf8 baseline"),
            "--candidate3-outcome",
            "durable",
        ],
    );
    assert_ne!(
        reopen.pid, producer_pid,
        "recovery must run in a fresh PID distinct from producer"
    );
    assert_ne!(reopen.pid, seed.pid);
    let (head, _) = assert_reopen_ok(&reopen, "durable", &[3]);
    assert_eq!(head, 3);

    println!("P2_OS_POST_ACK_ARTIFACTS={}", artifacts.display());
}

#[test]
#[ignore = "requires a policy-qualified host (linux/x86_64, validated kernel, Btrfs, /var/tmp same st_dev as KB)"]
fn real_os_store_second_process_lock_refusal_and_release_after_holder_death() {
    let artifacts = target_artifact_dir("lock");
    require_qualified_host(&artifacts);

    let root = target_nonced_root("lock-seed", false);
    let baseline = artifacts.join("lock.genesis-baseline.v1");
    let seed = run_to_exit(
        &artifacts,
        "lock-seed",
        &[
            "seed",
            root.to_str().expect("utf8 root"),
            "--genesis-baseline",
            baseline.to_str().expect("utf8 baseline"),
            "--expect-missing-root",
        ],
    );
    let _ = assert_seed_ok(&seed, &baseline, "missing_root");

    let mut holder = Guarded::spawn(
        "lock-holder",
        &["lock-holder", root.to_str().expect("utf8")],
    );
    let holder_pid = holder.pid();
    assert_ne!(holder_pid, seed.pid);
    let _ = wait_ready_or_fail(
        &mut holder,
        "STATE=steady",
        CHILD_DEADLINE,
        &artifacts,
        "lock-holder",
    )
    .unwrap_or_else(|(failure, capture)| {
        panic!(
            "lock-holder did not reach STATE=steady ({failure:?}):\nstdout:\n{}\nstderr:\n{}",
            capture.stdout, capture.stderr
        )
    });

    let refuser = run_to_exit(
        &artifacts,
        "lock-refuser",
        &["lock-refuser", root.to_str().expect("utf8 root")],
    );
    assert_ne!(refuser.pid, holder_pid);
    assert_eq!(
        refuser.status.code(),
        Some(EXIT_LOCK_REFUSAL),
        "second process was not refused with dedicated lock exit code 42:\nstdout:\n{}\nstderr:\n{}",
        refuser.stdout,
        refuser.stderr
    );
    let refuser_kv = ParsedOutput::parse(&refuser.stdout).expect("parse lock-refuser stdout");
    assert_pid_matches(&refuser_kv, refuser.pid);
    refuser_kv.require_eq("LOCK_REFUSAL_STEP", "Lock");
    refuser_kv.require_eq("LOCK_REFUSAL_KIND", "WouldBlock");
    refuser_kv.require_eq("LOCK_REFUSAL_EFFECT", "NoEffect");
    refuser_kv.require_eq("REFUSE", "PASS");

    let holder_capture = holder.kill_and_collect().expect("kill holder");
    save_capture(&artifacts, "lock-holder", &holder_capture);
    verify_killed_by_sigkill(holder_capture.status).expect("holder must die by SIGKILL(9)");
    let holder_kv = ParsedOutput::parse(&holder_capture.stdout).expect("parse lock-holder stdout");
    assert_pid_matches(&holder_kv, holder_pid);
    holder_kv.require_eq("HOLDER_HEAD", "2");
    holder_kv.require_eq("LOCKHELD_PID", &holder_pid.to_string());
    holder_kv.require_eq("LOCKHELD", "true");

    let reopen = run_to_exit(
        &artifacts,
        "lock-reopen",
        &[
            "reopen",
            root.to_str().expect("utf8 root"),
            "--genesis-baseline",
            baseline.to_str().expect("utf8 baseline"),
            "--candidate3-outcome",
            "none",
        ],
    );
    assert_ne!(reopen.pid, holder_pid);
    assert_ne!(reopen.pid, refuser.pid);
    let (head, _) = assert_reopen_ok(&reopen, "none", &[2]);
    assert_eq!(head, 2);

    println!("P2_OS_LOCK_ARTIFACTS={}", artifacts.display());
}

// ---------------------------------------------------------------------------
// Pure controls: compile and run on any host; make NO OS durability claim.
// ---------------------------------------------------------------------------

#[test]
fn silent_child_deadline_triggers_kill_reap_and_preserves_raw_output() {
    let artifacts = pure_artifact_dir("silent-deadline");
    let mut guarded = Guarded::spawn("silent-child", &["silent-hold"]);
    // Confirm the child has started and flushed its stderr diagnostic before timing out on READY.
    let startup = guarded
        .wait_for_line_with_deadline("SILENT_CHILD_STARTED=true", Duration::from_secs(5))
        .expect("silent-hold must start");
    assert_eq!(startup, vec!["SILENT_CHILD_STARTED=true".to_owned()]);

    let started = Instant::now();
    let (failure, capture) = wait_ready_or_fail(
        &mut guarded,
        "STATE=steady",
        SHORT_TEST_DEADLINE,
        &artifacts,
        "silent-child",
    )
    .expect_err("silent child must fail with deadline expiry");

    assert!(
        started.elapsed() < Duration::from_secs(5),
        "short test deadline took too long: {:?}",
        started.elapsed()
    );
    assert!(
        matches!(failure, WaitFailure::DeadlineExpired { .. }) && failure.seen().is_empty(),
        "expected DeadlineExpired with no post-start lines, got {failure:?}"
    );
    verify_killed_by_sigkill(capture.status).expect("deadline expiry must SIGKILL and reap child");
    assert!(
        capture.stdout.contains("SILENT_CHILD_STARTED=true")
            && !capture.stdout.contains("STATE=steady"),
        "raw stdout must preserve pre-timeout output without READY; got: {:?}",
        capture.stdout
    );
    assert!(
        capture
            .stderr
            .contains("SILENT_CHILD_DIAG=blocking_without_ready"),
        "raw failure stderr must be preserved; got: {:?}",
        capture.stderr
    );
    // Verify raw artifacts were saved to disk before failure handling.
    let saved_stdout =
        fs::read_to_string(artifacts.join("silent-child.stdout.log")).expect("read saved stdout");
    let saved_stderr =
        fs::read_to_string(artifacts.join("silent-child.stderr.log")).expect("read saved stderr");
    let saved_exit =
        fs::read_to_string(artifacts.join("silent-child.exit.kv")).expect("read saved exit");
    assert!(saved_stdout.contains("SILENT_CHILD_STARTED=true"));
    assert!(saved_stderr.contains("SILENT_CHILD_DIAG=blocking_without_ready"));
    assert!(saved_exit.contains("signal=Some(9)"));
}

#[test]
fn killed_status_guard_rejects_normal_exit_and_wrong_signal() {
    let artifacts = pure_artifact_dir("kill-status");

    // 1. Normal exit (code 0) must be rejected.
    let normal = run_to_exit(&artifacts, "exit-normal", &["exit-normal"]);
    assert_eq!(normal.status.code(), Some(0));
    let err = verify_killed_by_sigkill(normal.status)
        .expect_err("normal exit 0 must not satisfy SIGKILL guard");
    assert!(
        err.contains("succeeded") || err.contains("exited normally"),
        "{err}"
    );

    // 2. Nonzero normal exit (driver assert/policy refusal) must be rejected.
    let nonzero = run_to_exit(
        &artifacts,
        "bad-mode",
        &["unknown-mode-for-test", "/tmp/unused"],
    );
    assert_eq!(nonzero.status.code(), Some(EXIT_DRIVER_ASSERT));
    let err = verify_killed_by_sigkill(nonzero.status)
        .expect_err("nonzero normal exit must not satisfy SIGKILL guard");
    assert!(err.contains("exited normally"), "{err}");

    // 3. Wrong signal (SIGTERM = 15) on a real child must be rejected.
    let mut term_child = Guarded::spawn("sigterm-child", &["silent-hold"]);
    let term_pid = term_child.pid().to_string();
    let kill_status = Command::new("kill")
        .args(["-TERM", &term_pid])
        .status()
        .expect("invoke kill -TERM");
    assert!(kill_status.success(), "kill -TERM failed");
    let term_capture = term_child
        .wait_exit_with_deadline(Duration::from_secs(5))
        .expect("reap SIGTERM child");
    save_capture(&artifacts, "sigterm-child", &term_capture);
    assert_eq!(term_capture.status.signal(), Some(15));
    let err = verify_killed_by_sigkill(term_capture.status)
        .expect_err("SIGTERM(15) must not satisfy SIGKILL(9) guard");
    assert!(err.contains("Some(15)"), "{err}");

    // 4. Synthetic wrong signal check via ExitStatusExt::from_raw(15) as well.
    let raw_sigterm = ExitStatus::from_raw(15);
    assert!(verify_killed_by_sigkill(raw_sigterm).is_err());

    // 5. Real SIGKILL(9) satisfies the guard.
    let mut kill_child = Guarded::spawn("sigkill-child", &["silent-hold"]);
    let kill_capture = kill_child.kill_and_collect().expect("kill child");
    save_capture(&artifacts, "sigkill-child", &kill_capture);
    verify_killed_by_sigkill(kill_capture.status).expect("real SIGKILL(9) must pass");
}

#[test]
fn guarded_child_is_reaped_on_drop_and_double_wait() {
    // Dropping a still-running silent child must kill and reap cleanly without hanging.
    let active = Guarded::spawn("drop-reap", &["silent-hold"]);
    drop(active);

    // Waiting to exit and then dropping must not panic or double-wait.
    let mut normal = Guarded::spawn("double-wait", &["exit-normal"]);
    let capture = normal
        .wait_exit_with_deadline(Duration::from_secs(5))
        .expect("exit-normal exits cleanly");
    assert_eq!(capture.status.code(), Some(0));
    assert!(normal.kill_and_collect().is_none());
    drop(normal);
}

#[test]
fn child_stdin_pipe_close_exits_silent_hold_cleanly() {
    let mut guarded = Guarded::spawn("stdin-close", &["silent-hold"]);
    let stdin = guarded.take_stdin();
    drop(stdin);
    let capture = guarded
        .wait_exit_with_deadline(Duration::from_secs(5))
        .expect("closing stdin must let silent-hold exit cleanly without hanging");
    assert_eq!(capture.status.code(), Some(0));
}

#[test]
fn full_child_output_is_captured_after_ready_and_kill() {
    let artifacts = pure_artifact_dir("full-transcript");
    let mut guarded = Guarded::spawn("trailing-child", &["ready-then-trailing"]);

    // Stop waiting at PHASE=one; the child also prints AFTER_PHASE_ONE and TRAILING_STDERR.
    let prefix = wait_ready_or_fail(
        &mut guarded,
        "PHASE=one",
        Duration::from_secs(5),
        &artifacts,
        "trailing-child",
    )
    .expect("child reached PHASE=one");
    assert_eq!(prefix.last().map(String::as_str), Some("PHASE=one"));

    // Allow the child to flush its post-PHASE=one line (already emitted right after PHASE=one).
    std::thread::sleep(Duration::from_millis(50));
    let capture = guarded.kill_and_collect().expect("kill trailing-child");
    save_capture(&artifacts, "trailing-child", &capture);
    verify_killed_by_sigkill(capture.status).expect("SIGKILL");

    // Full captured transcript MUST contain lines printed AFTER wait_for_line returned, plus stderr.
    assert!(
        capture
            .stdout
            .contains("AFTER_PHASE_ONE=captured_in_full_transcript"),
        "post-READY stdout was truncated:\n{}",
        capture.stdout
    );
    assert!(
        capture.stderr.contains("TRAILING_STDERR=captured"),
        "stderr was lost on kill:\n{}",
        capture.stderr
    );
}

#[test]
fn missing_baseline_fails_reopen_without_recreating_it() {
    let artifacts = pure_artifact_dir("missing-baseline");
    let missing_baseline = artifacts.join("does-not-exist.genesis-baseline.v1");
    assert!(!missing_baseline.exists());

    let fake_root = artifacts.join("kb");
    let capture = run_to_exit(
        &artifacts,
        "reopen-missing-baseline",
        &[
            "reopen",
            fake_root.to_str().expect("utf8 root"),
            "--genesis-baseline",
            missing_baseline.to_str().expect("utf8 baseline"),
            "--candidate3-outcome",
            "none",
        ],
    );
    assert_eq!(
        capture.status.code(),
        Some(EXIT_DRIVER_ASSERT),
        "reopen without baseline must fail with EXIT_DRIVER_ASSERT:\n{}\n{}",
        capture.stdout,
        capture.stderr
    );
    assert!(
        capture
            .stderr
            .contains("missing or unreadable genesis baseline"),
        "unexpected stderr:\n{}",
        capture.stderr
    );
    assert!(
        !missing_baseline.exists(),
        "missing baseline must NEVER be recreated"
    );
}

#[test]
fn non_qualifying_host_probe_refuses_explicitly() {
    let artifacts = pure_artifact_dir("host-probe-control");
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    {
        let probe = run_to_exit(
            &artifacts,
            "non-linux-probe",
            &["host-probe", "/tmp/unused"],
        );
        assert_eq!(
            probe.status.code(),
            Some(EXIT_HOST_NOT_QUALIFIED),
            "non-qualifying host must fail with EXIT_HOST_NOT_QUALIFIED (45):\n{}\n{}",
            probe.stdout,
            probe.stderr
        );
        assert!(
            probe.stderr.contains("UnsupportedTarget"),
            "expected UnsupportedTarget refusal:\n{}",
            probe.stderr
        );
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        // On Linux/x86_64, a relative root must still be refused by for_root with exit 45.
        let probe = run_to_exit(
            &artifacts,
            "relative-root-probe",
            &["host-probe", "relative/kb"],
        );
        assert_eq!(
            probe.status.code(),
            Some(EXIT_HOST_NOT_QUALIFIED),
            "relative root must fail with EXIT_HOST_NOT_QUALIFIED (45):\n{}\n{}",
            probe.stdout,
            probe.stderr
        );
        assert!(probe.stderr.contains("RelativeRoot"), "{}", probe.stderr);
    }
}

#[test]
fn artifact_save_refuses_overwrite_and_deadline_bounds() {
    assert!(CHILD_DEADLINE >= Duration::from_secs(5));
    assert!(CHILD_DEADLINE <= Duration::from_secs(300));
    assert!(SHORT_TEST_DEADLINE < Duration::from_secs(2));

    let dir = pure_artifact_dir("wx-check");
    let file = dir.join("once.txt");
    write_new_file(&file, b"first\n").expect("create_new first write");
    let err = write_new_file(&file, b"second\n").expect_err("wx must refuse overwrite");
    assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(fs::read_to_string(&file).expect("read"), "first\n");
}
