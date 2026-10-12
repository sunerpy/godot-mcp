//! A launch through a Windows Job Object, end to end on a real Windows host:
//! the job exists before any process, the barrier starts inside it and waits
//! at its closed gate, the released program's exit status comes back, the job
//! is provably empty afterwards, and a job reopened from its recorded identity
//! alone sees the same. A second launch that outlives its deadline is ended by
//! terminating the whole job.

#![cfg(windows)]

use std::path::PathBuf;
use std::time::Duration;

use godot_mcp_engine::supervision::boundary::{BoundaryProvider, ContainmentState, SpawnPlan};
use godot_mcp_engine::supervision::job_object::JobObjectProvider;

fn plan(scratch: &std::path::Path, arguments: &[&str]) -> SpawnPlan {
    let system_root = std::env::var("SystemRoot").expect("SystemRoot is set on Windows");
    SpawnPlan {
        program: PathBuf::from(&system_root).join("System32").join("cmd.exe"),
        arguments: arguments
            .iter()
            .map(|argument| (*argument).to_owned())
            .collect(),
        environment: vec![("SystemRoot".to_owned(), system_root)],
        work_dir: scratch.to_path_buf(),
        stdout_path: scratch.join("stdout.txt"),
        stderr_path: scratch.join("stderr.txt"),
        gate_path: scratch.join("gate"),
        barrier_binary: PathBuf::from(env!("CARGO_BIN_EXE_godot-mcp-supervisor")),
        join_cgroup: None,
    }
}

#[test]
fn a_job_object_contains_a_gated_launch_and_is_provably_empty_after_it_exits() {
    let provider = JobObjectProvider::new("godot-mcp-qa".to_owned());
    provider.available().expect("Job Objects exist on Windows");
    let boundary = provider
        .create("00000000-0000-4000-8000-0000000007a1")
        .expect("the job is created before any process");
    assert_eq!(
        boundary.state().expect("an empty job is inspectable"),
        ContainmentState::Empty
    );

    let scratch = tempfile::tempdir().expect("scratch directory");
    let plan = plan(scratch.path(), &["/d", "/c", "echo contained & exit /b 3"]);
    let mut anchor = boundary
        .create_anchor(&plan)
        .expect("the barrier starts inside the job");
    assert!(
        boundary
            .confirm_membership(anchor.pid)
            .expect("membership is inspectable"),
        "the barrier is a member of the job"
    );
    let child = boundary
        .create_gated_child(&mut anchor)
        .expect("the gated child is the anchor itself");
    assert_eq!(child.pid, anchor.pid);
    assert!(
        !boundary.members().expect("enumerable").is_empty(),
        "a waiting barrier is a live member"
    );

    boundary.release(&mut anchor).expect("the gate opens");
    let outcome = boundary
        .wait(&mut anchor, Duration::from_secs(60))
        .expect("the launch is waited for");
    assert_eq!(outcome.exit_code, Some(3), "{outcome:?}");
    assert!(!outcome.timed_out);
    let stdout = std::fs::read_to_string(&plan.stdout_path).expect("captured stdout");
    assert!(stdout.contains("contained"), "{stdout:?}");

    assert_eq!(
        boundary.state().expect("inspectable"),
        ContainmentState::Empty,
        "nothing outlives the released program"
    );
    let reopened = provider
        .reopen(boundary.identity())
        .expect("the recorded job is inspectable")
        .expect("the job still exists while this process holds it");
    assert_eq!(
        reopened.state().expect("inspectable"),
        ContainmentState::Empty
    );
    boundary.close().expect("an empty job closes");
}

#[test]
fn a_launch_that_outlives_its_deadline_is_ended_with_its_whole_job() {
    let provider = JobObjectProvider::new("godot-mcp-qa".to_owned());
    let boundary = provider
        .create("00000000-0000-4000-8000-0000000007a2")
        .expect("the job is created");
    let scratch = tempfile::tempdir().expect("scratch directory");
    // `ping` waits about a second per echo: long enough to outlive the deadline.
    // It is named by its path, because the launch's environment carries no PATH.
    let ping = format!(
        r"{}\System32\PING.EXE -n 120 127.0.0.1 >nul",
        std::env::var("SystemRoot").expect("SystemRoot is set on Windows")
    );
    let plan = plan(scratch.path(), &["/d", "/c", &ping]);
    let mut anchor = boundary.create_anchor(&plan).expect("the barrier starts");
    boundary
        .create_gated_child(&mut anchor)
        .expect("the gated child");
    boundary.release(&mut anchor).expect("the gate opens");
    let outcome = boundary
        .wait(&mut anchor, Duration::from_secs(2))
        .expect("the launch is waited for");
    assert!(outcome.timed_out, "{outcome:?}");
    boundary
        .terminate_all()
        .expect("terminating the job ends every member");
    assert_eq!(
        boundary.state().expect("inspectable"),
        ContainmentState::Empty,
        "the program and its children ended with the job"
    );
}

/// Every process in a session shares one namespace of Job Object names, and
/// `CreateJobObjectW` opens an existing job of the same name instead of
/// failing. A launch handed another launch's job would count that launch's
/// members as its own, and terminating either would end both, so creating a
/// job whose name is taken is refused.
#[test]
fn a_launch_never_joins_a_job_another_launch_created() {
    let provider = JobObjectProvider::new("godot-mcp-qa".to_owned());
    let first = provider
        .create("00000000-0000-4000-8000-0000000007a3")
        .expect("the first launch creates its job");
    assert!(
        provider
            .create("00000000-0000-4000-8000-0000000007a3")
            .is_err(),
        "a second launch must never be handed the first one's job"
    );
    assert_eq!(
        first.state().expect("the first job is inspectable"),
        ContainmentState::Empty
    );
    first.close().expect("an empty job closes");
}
