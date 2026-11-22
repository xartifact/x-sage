//! R3: the real `OsFileSystem` lock seam — `conservative()`, failing guards, and a real ACK deadline.
//!
//! Bound to the accepted immutable b831 capture. No Store is opened, eligibility is never changed,
//! no product `open` is called, and `unsafe_code = "forbid"` holds.
//!
//! Delta from r2: the ACK read has an actual DEADLINE that FAILS (and cleans up), instead of a bare
//! `lines().next()` that could block forever on a live child that never ACKs.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use sage_db_store::fs::{Effect, FileSystem, FsErrorKind};
use sage_db_store::os::OsFileSystem;

/// How long the parent waits for the child's acquisition ACK before failing.
const ACK_DEADLINE: Duration = Duration::from_secs(10);

fn temp_root(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("p16-oslock-root-{tag}-{}", std::process::id()));
    std::fs::create_dir(&base).expect("create fresh unique temp root");
    base
}

fn assert_exact_refusal(err: &sage_db_store::fs::FsError, label: &str) {
    assert_eq!(
        err.kind(),
        FsErrorKind::WouldBlock,
        "{label}: expected WouldBlock, got {:?}",
        err.kind()
    );
    assert_eq!(err.effect(), Effect::NoEffect, "{label}: expected NoEffect");
    println!("{label}=WouldBlock/NoEffect (exact)");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("child") {
        child(Path::new(&args[2]));
    }
    if args.get(1).map(String::as_str) == Some("silent-child") {
        silent_child(Path::new(&args[2]));
    }

    // ---- the real conservative constructor, and its durability claim ----
    let fs = OsFileSystem::conservative();
    assert!(
        !fs.capabilities().durable_sync,
        "conservative() must not claim durable_sync"
    );
    println!("CONSTRUCTOR=OsFileSystem::conservative()");
    println!("DURABLE_SYNC=false (asserted, never overridden)");

    let root = temp_root("main");
    let dir = fs.open_root(&root).expect("open_root on a fresh temp dir");
    let held = fs.try_lock_exclusive(&dir).expect("parent acquires");
    println!("PARENT_LOCK=acquired");

    let exe = std::env::current_exe().expect("exe");

    // ---- child A: exact refusal, explicit code ----
    let refusing_child = Command::new(&exe)
        .arg("child")
        .arg(&root)
        .stdout(Stdio::piped())
        .spawn()
        .expect("run refusing child");
    let mut refused = KillOnDrop(refusing_child);
    let line = read_ack_with_deadline(&mut refused.0, ACK_DEADLINE)
        .expect("refusal child must respond before deadline");
    assert_eq!(line, "CHILD_REFUSED=WouldBlock/NoEffect");
    let refused_status =
        wait_exit(&mut refused.0, ACK_DEADLINE).expect("refusal child must exit before deadline");
    assert_eq!(
        refused_status.code(),
        Some(42),
        "child A must exit 42 (explicit expected-refusal), not merely nonzero"
    );
    println!("CHILD_A_REFUSED=true (exit 42, exact error)");

    // ---- release, then child B ACKs acquisition ----
    drop(held);
    println!("PARENT_RELEASED=true");

    let child_b = Command::new(&exe)
        .arg("child")
        .arg(&root)
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn holding child");
    let mut child_b = KillOnDrop(child_b);
    let ack = read_ack_with_deadline(&mut child_b.0, ACK_DEADLINE)
        .expect("child B must ACK within the deadline");
    assert_eq!(ack, "CHILD_ACQUIRED");
    println!("CHILD_B_ACK={ack}");

    // ---- contend against the live child ----
    let contend = fs
        .try_lock_exclusive(&dir)
        .expect_err("parent must be refused while child B holds the lock");
    assert_exact_refusal(&contend, "PARENT_CONTENDED_WHILE_CHILD_HOLDS");

    // ---- kill and reap ----
    child_b.0.kill().expect("kill child B");
    let status = child_b.0.wait().expect("reap child B");
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(9), "child B must die by SIGKILL");
        assert!(!status.success(), "killed child must not report success");
        println!("CHILD_B_KILLED_SIGNAL=9 (asserted)");
    }

    // ---- reacquire ----
    let again = fs
        .try_lock_exclusive(&dir)
        .expect("parent must reacquire after child B died");
    println!("PARENT_REACQUIRED=true");
    drop(again);

    // ---- the deadline itself must FAIL: a live child that never ACKs ----
    let silent = Command::new(&exe)
        .arg("silent-child")
        .arg(&root)
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn silent child");
    let mut silent = KillOnDrop(silent);
    let failed = read_ack_with_deadline(&mut silent.0, Duration::from_millis(800));
    assert!(
        failed.is_none(),
        "a child that acquires but never ACKs must trip the deadline"
    );
    // The deadline path kills and reaps the child so nothing is leaked.
    let st = silent.0.wait().expect("reap silent child");
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(st.signal(), Some(9), "deadline path must kill the child");
    }
    println!("ACK_DEADLINE_FAILS=true (child killed and reaped on timeout)");

    let _ = std::fs::remove_dir_all(&root);
    println!("ROOT_OSLOCK_PASS=true");
}

/// Read the ACK line, but FAIL after `deadline` and kill+reap the child so nothing leaks.
fn read_ack_with_deadline(child: &mut Child, deadline: Duration) -> Option<String> {
    let stdout = child.stdout.take()?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut lines = BufReader::new(stdout).lines();
        let _ = tx.send(lines.next());
    });
    match rx.recv_timeout(deadline) {
        Ok(Some(Ok(line))) => Some(line),
        Ok(_) | Err(_) => {
            // Deadline (or a closed pipe with no line): terminate and reap, then report failure.
            let _ = child.kill();
            let _ = child.wait();
            None
        }
    }
}

fn child(root: &Path) {
    let fs = OsFileSystem::conservative();
    let dir = match fs.open_root(root) {
        Ok(d) => d,
        Err(e) => {
            println!("CHILD_OPEN_ROOT_FAILED={e:?}");
            std::process::exit(43);
        }
    };
    match fs.try_lock_exclusive(&dir) {
        Ok(lock) => {
            println!("CHILD_ACQUIRED");
            use std::io::Write as _;
            std::io::stdout().flush().expect("flush ACK");
            // Hold the lock and stay alive until the parent kills us. stdin is NOT a reliable
            // barrier here: an inherited EOF makes read() return immediately and the child exits
            // normally, which would defeat the signal-9 assertion.
            std::mem::forget(lock);
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        }
        Err(e) => {
            let exact = e.kind() == FsErrorKind::WouldBlock && e.effect() == Effect::NoEffect;
            if exact {
                println!("CHILD_REFUSED=WouldBlock/NoEffect");
                std::process::exit(42);
            }
            println!("CHILD_UNEXPECTED={e:?}");
            std::process::exit(44);
        }
    }
}

/// A child that takes the lock but deliberately never ACKs, to exercise the deadline.
fn silent_child(root: &Path) {
    let fs = OsFileSystem::conservative();
    let dir = fs.open_root(root).expect("silent child opens root");
    let lock = fs
        .try_lock_exclusive(&dir)
        .expect("silent child takes the lock");
    std::mem::forget(lock);
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

// Cleanup also runs when any assertion panics, including an unexpected acquisition.
struct KillOnDrop(Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn wait_exit(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Some(status) = child.try_wait().expect("child status") {
            return Some(status);
        }
        // Deadline polling is not the acquisition/contention boundary; the pipe ACK is.
        std::thread::sleep(Duration::from_millis(5));
    }
    let _ = child.kill();
    let _ = child.wait();
    None
}
