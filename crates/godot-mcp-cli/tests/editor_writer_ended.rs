//! Settling an editor mutation the editor never answered, once the editor
//! process it was sent to has ended: `transaction repair --operation
//! acknowledge-safe-state` on a `writer-ended` proof.
//!
//! Every record is written through the Task-34 fixture journal into a fresh
//! project, and every repair runs the `godot-mcp` binary, so what is exercised
//! is the command's own authorization order end to end. The editor process a
//! record names is observed live, which this build does on Linux and macOS.

#![cfg(all(feature = "test-hooks", any(target_os = "linux", target_os = "macos")))]

use std::path::{Path, PathBuf};
use std::time::Duration;

use assert_cmd::Command;
use godot_mcp_engine::supervision::procfs;
use godot_mcp_server::test_support::{
    Task34EditorProcess, Task34EffectKind, Task34FixtureJournal, Task34Proof, Task34RecordSpec,
};
use serde_json::{Value, json};

/// A project whose journal holds the records a test writes, and a separate
/// directory for the proof files it hands the command.
struct Project {
    root: tempfile::TempDir,
    proofs: tempfile::TempDir,
    object_identity: String,
    generation: u64,
}

impl Project {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("project directory");
        std::fs::write(root.path().join("project.godot"), "config_version=5\n")
            .expect("project.godot");
        let held = Task34FixtureJournal::open(root.path())
            .expect("hold the project")
            .held()
            .clone();
        Self {
            root,
            proofs: tempfile::tempdir().expect("proof directory"),
            object_identity: held.object_identity,
            generation: held.generation,
        }
    }

    fn root(&self) -> &Path {
        self.root.path()
    }

    /// The journal, held only for as long as the caller needs it: the command
    /// takes the same lease.
    fn journal(&self) -> Task34FixtureJournal {
        Task34FixtureJournal::open(self.root()).expect("hold the project")
    }

    /// Journal `editor.script.run` as the server leaves one it sent to
    /// `editor_process` and never heard back from: quarantined, scoped to the
    /// whole project.
    fn quarantine_unanswered(
        &self,
        transaction_id: &str,
        editor_process: Option<Task34EditorProcess>,
    ) {
        self.journal()
            .write_quarantined(&Task34RecordSpec {
                transaction_id: transaction_id.to_owned(),
                effect_kind: Task34EffectKind::EditorRuntime,
                tool_name: "editor.script.run".to_owned(),
                touched_paths: vec!["*".to_owned()],
                pre_hashes: Vec::new(),
                intended_post_hashes: Vec::new(),
                proof: Task34Proof::EditorRuntime {
                    bridge_reachable: false,
                    receipt_transaction_id: None,
                    receipt_action_id: None,
                    live_state_probe: None,
                    editor_process,
                },
                required_process_transaction: None,
                terminal_reason: "the editor mutation is unresolved (the editor did not answer \
                                  before the deadline); whether it wrote any project file is not \
                                  decidable"
                    .to_owned(),
            })
            .expect("journal the unanswered mutation");
    }

    fn record(&self, transaction_id: &str) -> Value {
        self.journal()
            .record_json(transaction_id)
            .expect("the transaction record")
    }

    fn quarantined(&self) -> Vec<String> {
        self.journal().quarantined().expect("the quarantine list")
    }

    /// A proof file claiming that the editor of `transaction_id` ended, with
    /// `extra` fields added or replaced.
    fn claim(&self, transaction_id: &str, extra: &Value) -> PathBuf {
        let mut claim = json!({
            "effect_kind": "editor_runtime",
            "transaction_id": transaction_id,
            "project_object_identity": self.object_identity,
            "project_generation": self.generation,
            "bridge_reachable": false,
            "claimed_disposition": "writer-ended",
        });
        if let (Some(claim), Some(extra)) = (claim.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                claim.insert(key.clone(), value.clone());
            }
        }
        let path = self.proofs.path().join(format!("{transaction_id}.json"));
        std::fs::write(&path, serde_json::to_vec_pretty(&claim).expect("json")).expect("proof");
        path
    }

    fn repair(
        &self,
        transaction_id: &str,
        operation: &str,
        proof: &Path,
        dry_run: bool,
    ) -> assert_cmd::assert::Assert {
        let mut command = godot_mcp();
        command
            .args(["transaction", "repair", "--project"])
            .arg(self.root())
            .args(["--transaction-id", transaction_id, "--operation", operation])
            .arg("--proof-file")
            .arg(proof);
        if dry_run {
            command.arg("--dry-run");
        }
        command.assert()
    }

    /// What a server starting on the project reports on stderr. Its stdin is
    /// empty, so it stops once its startup is done, when no client
    /// initializes a session.
    fn startup_report(&self) -> String {
        let output = godot_mcp()
            .args(["serve", "--project"])
            .arg(self.root())
            .timeout(Duration::from_secs(120))
            .output()
            .expect("run the server");
        let stderr = String::from_utf8(output.stderr).expect("utf-8 stderr");
        assert!(
            stderr.contains("connection closed: initialize request"),
            "the server finished its startup and stopped at the missing client: {stderr}"
        );
        stderr
    }
}

fn godot_mcp() -> Command {
    let mut command = Command::cargo_bin("godot-mcp").expect("godot-mcp binary");
    for (name, _) in std::env::vars().filter(|(name, _)| name.starts_with("GODOT_MCP_")) {
        command.env_remove(name);
    }
    command
}

fn success_stdout(assertion: assert_cmd::assert::Assert) -> String {
    let assertion = assertion.success();
    String::from_utf8(assertion.get_output().stdout.clone()).expect("utf-8 report")
}

fn failure_stderr(assertion: assert_cmd::assert::Assert) -> String {
    let assertion = assertion.failure();
    String::from_utf8(assertion.get_output().stderr.clone()).expect("utf-8 stderr")
}

fn report_value<'a>(report: &'a str, key: &str) -> &'a str {
    report
        .lines()
        .find_map(|line| {
            let (candidate, value) = line.split_once(' ')?;
            (candidate == key).then_some(value)
        })
        .unwrap_or_else(|| panic!("the report has no {key:?} field: {report}"))
}

/// This test process, as an editor process record names it.
fn own_process() -> Task34EditorProcess {
    Task34EditorProcess {
        pid: std::process::id(),
        start_time: procfs::process_identity(std::process::id())
            .expect("this process is observable")
            .expect("this process is live")
            .start_time
            .expect("a start time is reported"),
    }
}

/// An editor process that has ended: a reaped child's.
fn ended_process() -> Task34EditorProcess {
    let mut child = std::process::Command::new("true")
        .spawn()
        .expect("a short-lived child");
    let pid = child.id();
    child.wait().expect("the child exits");
    Task34EditorProcess {
        pid,
        start_time: "1".to_owned(),
    }
}

#[test]
fn an_ended_editor_settles_its_unanswered_request_as_uncertain() {
    let project = Project::new();
    let own = own_process();
    let cases = [
        // No live process has the editor's id.
        ("00000000-0000-4000-8000-0000000000e1", ended_process()),
        // The editor's id now names a later process.
        (
            "00000000-0000-4000-8000-0000000000e2",
            Task34EditorProcess {
                pid: own.pid,
                start_time: format!("{}0", own.start_time),
            },
        ),
    ];
    for (transaction_id, editor) in &cases {
        project.quarantine_unanswered(transaction_id, Some(editor.clone()));
    }
    assert!(
        project.startup_report().contains("write gate closed"),
        "the unanswered mutations block writes"
    );

    for (transaction_id, editor) in &cases {
        let claim = project.claim(transaction_id, &json!({}));
        let before = project.record(transaction_id);

        let report = success_stdout(project.repair(transaction_id, "reconcile", &claim, false));
        assert_eq!(report_value(&report, "proven-disposition"), "writer-ended");
        assert_eq!(report_value(&report, "transition"), "none");
        assert_eq!(
            project.record(transaction_id),
            before,
            "reconcile changes nothing"
        );

        let report =
            success_stdout(project.repair(transaction_id, "acknowledge-safe-state", &claim, true));
        assert_eq!(report_value(&report, "transition"), "none");
        assert!(
            report_value(&report, "detail")
                .starts_with("dry run: would transition to committed_fsynced"),
            "{report}"
        );
        assert_eq!(
            project.record(transaction_id),
            before,
            "a dry run changes nothing"
        );

        let report =
            success_stdout(project.repair(transaction_id, "acknowledge-safe-state", &claim, false));
        assert_eq!(report_value(&report, "proven-disposition"), "writer-ended");
        assert_eq!(report_value(&report, "phase-before"), "quarantined");
        assert_eq!(report_value(&report, "phase-after"), "committed_fsynced");
        assert_eq!(report_value(&report, "transition"), "written");
        assert!(
            report_value(&report, "detail").contains(&format!("pid {}", editor.pid)),
            "the report names the editor that ended: {report}"
        );
        let record = project.record(transaction_id);
        assert_eq!(record["phase"], "committed_fsynced");
        assert_eq!(
            record["terminal_outcome"],
            json!({ "verdict": "failed", "code": "uncertain" }),
            "a replay still answers that the result is unknown"
        );
        assert_eq!(
            record["proof"]["editor_process"],
            json!({ "pid": editor.pid, "start_time": editor.start_time }),
            "the ended editor stays on record"
        );
    }
    assert!(project.quarantined().is_empty());
    assert!(
        !project.startup_report().contains("write gate closed"),
        "writes resume"
    );
}

#[test]
fn an_editor_that_still_runs_or_was_never_identified_keeps_its_request_quarantined() {
    let project = Project::new();
    let own = own_process();
    let running = "00000000-0000-4000-8000-0000000000f1";
    let unidentified = "00000000-0000-4000-8000-0000000000f2";
    let ended = "00000000-0000-4000-8000-0000000000f3";
    project.quarantine_unanswered(running, Some(own.clone()));
    project.quarantine_unanswered(unidentified, None);
    let ended_editor = ended_process();
    project.quarantine_unanswered(ended, Some(ended_editor.clone()));
    let before: Vec<Value> = [running, unidentified, ended]
        .iter()
        .map(|transaction_id| project.record(transaction_id))
        .collect();

    // The editor still runs: this test process is the editor the record names.
    let claim = project.claim(running, &json!({}));
    let still_running = format!(
        "the editor that ran this request is still running (pid {}): wait for it to answer, \
         or close it",
        own.pid
    );
    let stderr = failure_stderr(project.repair(running, "acknowledge-safe-state", &claim, false));
    assert!(stderr.contains(&still_running), "{stderr}");
    let report = success_stdout(project.repair(running, "reconcile", &claim, false));
    assert_eq!(report_value(&report, "proven-disposition"), "none");
    assert!(
        report_value(&report, "detail").contains(&still_running),
        "{report}"
    );

    // No editor process was recorded.
    let claim = project.claim(unidentified, &json!({}));
    let stderr =
        failure_stderr(project.repair(unidentified, "acknowledge-safe-state", &claim, false));
    assert!(stderr.contains("recorded no editor process"), "{stderr}");

    // The proof file names another editor than the one recorded.
    let claim = project.claim(
        ended,
        &json!({ "editor_process": { "pid": 1, "start_time": "1" } }),
    );
    let stderr = failure_stderr(project.repair(ended, "acknowledge-safe-state", &claim, false));
    assert!(
        stderr.contains(&format!(
            "the proof file claims editor process 1 started at 1, and transaction {ended} \
             recorded {} started at {}",
            ended_editor.pid, ended_editor.start_time
        )),
        "{stderr}"
    );

    // A writer-ended claim says the editor never answered.
    let claim = project.claim(ended, &json!({ "bridge_reachable": true }));
    let stderr = failure_stderr(project.repair(ended, "acknowledge-safe-state", &claim, false));
    assert!(stderr.contains("bridge_reachable"), "{stderr}");
    let claim = project.claim(ended, &json!({ "bridge_receipt": "editor.script.run" }));
    let stderr = failure_stderr(project.repair(ended, "acknowledge-safe-state", &claim, false));
    assert!(stderr.contains("bridge_receipt"), "{stderr}");

    // The end of the editor proves no pre-state.
    let claim = project.claim(ended, &json!({}));
    let stderr = failure_stderr(project.repair(ended, "compensate", &claim, false));
    assert!(
        stderr.contains("compensate requires a pre-state proof"),
        "{stderr}"
    );

    for (transaction_id, before) in [running, unidentified, ended].iter().zip(&before) {
        assert_eq!(
            &project.record(transaction_id),
            before,
            "{transaction_id} is unchanged"
        );
    }
    assert_eq!(project.quarantined().len(), 3);
}
