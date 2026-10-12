//! What the live process observations see of the bundled supervisor while it
//! waits at its exec barrier: the barrier marker in its argument vector, the
//! nonce in its environment, its process group and its executable. These are
//! exactly the facts the weak locator authenticates a supervisor by after a
//! restart, observed on the binary this build ships rather than on a system
//! program — macOS withholds the environment of its own signed system binaries
//! from other processes, and never of an ordinary one.

#![cfg(any(target_os = "linux", target_os = "macos", windows))]

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use godot_mcp_engine::supervision::boundary::BARRIER_ARGV_MARKER;
use godot_mcp_engine::supervision::{gate, procfs};

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn a_waiting_barrier_is_observed_with_its_marker_environment_group_and_executable() {
    let scratch = tempfile::tempdir().expect("scratch directory");
    let gate_path = scratch.path().join("gate");
    gate::create(&gate_path).expect("a closed gate");
    let supervisor = PathBuf::from(env!("CARGO_BIN_EXE_godot-mcp-supervisor"));
    let nonce = "qa-observation-0123456789abcdef";
    let mut barrier = Command::new(&supervisor)
        .arg("barrier")
        .arg(BARRIER_ARGV_MARKER)
        .arg("--gate")
        .arg(&gate_path)
        .arg("--")
        .arg("/bin/sleep")
        .arg("1")
        .env("QA_OBSERVATION_NONCE", nonce)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the supervisor's barrier");
    let pid = barrier.id();

    let deadline = Instant::now() + Duration::from_secs(10);
    let observed = loop {
        let observed = procfs::process_identity(pid)
            .expect("observable")
            .expect("the barrier is alive while its gate is closed");
        if observed.is_gated_barrier() {
            break observed;
        }
        assert!(
            Instant::now() < deadline,
            "the barrier never showed its marker: {observed:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    };
    assert!(observed.start_time.is_some());
    let own = procfs::process_identity(std::process::id())
        .expect("observable")
        .expect("this process is alive");
    assert_eq!(observed.process_group, own.process_group);
    assert!(procfs::live_pids().expect("enumerable").contains(&pid));
    assert!(
        procfs::environment_carries(pid, "QA_OBSERVATION_NONCE", nonce).expect("readable"),
        "the supervisor's environment is readable by the user who started it"
    );
    assert!(
        !procfs::environment_carries(pid, "QA_OBSERVATION_NONCE", "qa-observation-other")
            .expect("readable")
    );
    let running = procfs::executable(pid)
        .expect("observable")
        .expect("the barrier is alive");
    assert_eq!(
        std::fs::canonicalize(running).expect("resolvable"),
        std::fs::canonicalize(&supervisor).expect("resolvable")
    );

    procfs::kill_pid(pid).expect("signalled");
    while procfs::process_identity(pid).expect("observable").is_some() {
        assert!(
            Instant::now() < deadline,
            "a killed barrier is still reported alive"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!barrier.wait().expect("reaped").success());
}

/// Windows has no process groups and this build reads no other process's
/// environment there: a waiting barrier is known by its marker, its creation
/// time and its executable, which is what the Job Object launch waits on before
/// it records the gated child.
#[cfg(windows)]
#[test]
fn a_waiting_windows_barrier_is_observed_with_its_marker_and_executable() {
    let scratch = tempfile::tempdir().expect("scratch directory");
    let gate_path = scratch.path().join("gate");
    gate::create(&gate_path).expect("a closed gate");
    let supervisor = PathBuf::from(env!("CARGO_BIN_EXE_godot-mcp-supervisor"));
    let mut barrier = Command::new(&supervisor)
        .arg("barrier")
        .arg(BARRIER_ARGV_MARKER)
        .arg("--gate")
        .arg(&gate_path)
        .arg("--")
        .arg("cmd.exe")
        .arg("/d")
        .arg("/c")
        .arg("exit 0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the supervisor's barrier");
    let pid = barrier.id();
    let observed = gate::wait_for_barrier(pid, Duration::from_secs(10))
        .expect("observable")
        .expect("the barrier is alive while its gate is closed");
    assert!(observed.start_time.is_some());
    assert_eq!(observed.process_group, None);
    assert!(procfs::live_pids().expect("enumerable").contains(&pid));
    let running = procfs::executable(pid)
        .expect("observable")
        .expect("the barrier is alive");
    assert_eq!(
        std::fs::canonicalize(running).expect("resolvable"),
        std::fs::canonicalize(&supervisor).expect("resolvable")
    );

    procfs::kill_pid(pid).expect("terminated");
    let deadline = Instant::now() + Duration::from_secs(10);
    while procfs::process_identity(pid).expect("observable").is_some() {
        assert!(
            Instant::now() < deadline,
            "a terminated barrier is still reported alive"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!barrier.wait().expect("waited").success());
}
