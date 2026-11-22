//! Reproducible process probes on this host; modeled crash transport and real OS locks are distinct.
//! No product Store is opened against an asserted or overridden host durability capability.

#[cfg(unix)]
#[test]
fn real_killed_producers_and_fresh_process_recovery_of_modeled_worlds() {
    use std::{
        fs,
        process::Command,
        time::{SystemTime, UNIX_EPOCH},
    };
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let artifacts =
        std::env::temp_dir().join(format!("p16-process-ci-{}-{nonce}", std::process::id()));
    assert!(!artifacts.exists(), "never overwrite a prior run");
    fs::create_dir(&artifacts)
        .expect("create fresh empty artifact directory before launching the driver");
    let output = Command::new(env!("CARGO_BIN_EXE_p16_process_driver"))
        .arg("suite")
        .arg("--artifact-dir")
        .arg(&artifacts)
        .output()
        .expect("start real process driver");
    // Keep raw outer-driver output too, including failures. Each child has its own raw artifacts.
    fs::write(artifacts.join("outer.stdout.log"), &output.stdout).expect("capture driver stdout");
    fs::write(artifacts.join("outer.stderr.log"), &output.stderr).expect("capture driver stderr");
    fs::write(
        artifacts.join("outer.exit.txt"),
        format!(
            "success={} code={:?}\n",
            output.status.success(),
            output.status.code()
        ),
    )
    .expect("capture driver exit");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "process suite failed; artifacts={}\n{stdout}\n{stderr}",
        artifacts.display()
    );
    assert!(
        stdout.contains("cases=8 ") && stdout.contains("failed=0 status=PASS"),
        "missing successful finite-suite result: {stdout}"
    );
    assert!(artifacts.join("results.json").is_file());
    assert!(artifacts.join("results.tsv").is_file());
    println!("P16_PROCESS_ARTIFACT_DIR={}\n{stdout}", artifacts.display());
}

#[cfg(unix)]
#[test]
fn actual_os_lock_contention_and_release_after_holder_death() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_p16_oslock"))
        .output()
        .expect("start OS-lock seam probe");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "OS-lock probe failed:\n{stdout}\n{stderr}"
    );
    assert!(stdout.contains("ROOT_OSLOCK_PASS=true"));
    assert!(stdout.contains("ACK_DEADLINE_FAILS=true"));
    assert!(stdout.contains("CHILD_B_KILLED_SIGNAL=9"));
    println!("P16_HOST_LOCK_ONLY=true\n{stdout}");
}
