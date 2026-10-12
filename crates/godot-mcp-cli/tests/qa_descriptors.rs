//! Descriptor-inventory gate for Todo 1.
//!
//! Asserts every `qa/tasks/1.json` .. `qa/tasks/37.json` exists, schema-validates
//! (via the descriptor validator), has non-empty command/assertion fields, and
//! that `qa/tasks/34.json` contains its fixed transaction IDs and full commands
//! with no angle-bracket transaction token or command-reuse shorthand. Also
//! proves each planted corrupt-descriptor fixture is rejected by name, and that
//! the cancelled-Todo status is both honoured (Todos 20-25 own zero commands and
//! `qa-task` runs nothing) and fail-closed (a cancelled descriptor with residual
//! commands, a missing reason, or cancellation fields on an active descriptor is
//! rejected by the validator and by the `scripts/qa_task.py` runner).

use std::path::{Path, PathBuf};
use std::process::Command;

use godot_mcp_cli::corrupt::{self, CorruptFixture};
use godot_mcp_cli::descriptors::{self, DescriptorStatus, contains_angle_bracket_token};

/// Every Todo the maintainer cancelled on 2026-07-28 with the editor plane.
const CANCELLED_TASKS: &[u32] = &[20, 21, 22, 23, 24, 25];

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root above crates/godot-mcp-cli")
        .to_path_buf()
}

#[test]
fn all_37_descriptors_present_and_valid() {
    let root = workspace_root();
    descriptors::validate_all(&root)
        .expect("all 37 qa/tasks descriptors must exist, validate, and be non-empty");
}

#[test]
fn missing_descriptor_fixture_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let root = corrupt::build(CorruptFixture::MissingDescriptor, tmp.path()).unwrap();
    assert!(
        descriptors::validate_all(&root).is_err(),
        "an emptied qa/tasks/17.json must fail the inventory validator"
    );
}

#[test]
fn task34_angle_bracket_fixture_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let root = corrupt::build(CorruptFixture::Task34AngleBracket, tmp.path()).unwrap();
    assert!(
        descriptors::validate_all(&root).is_err(),
        "a Todo-34 angle-bracket <transaction-id> token must fail the inventory validator"
    );
}

#[test]
fn task34_ambient_invocation_fixture_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let root = corrupt::build(CorruptFixture::Task34AmbientInvocation, tmp.path()).unwrap();
    assert!(
        descriptors::validate_all(&root).is_err(),
        "a Task-34 repair command invoking ambient-PATH `godot-mcp transaction` must fail the inventory validator"
    );
}

#[test]
fn committed_task34_obeys_packaged_binary_contract() {
    // The committed qa/tasks/34.json must resolve the frozen host-package
    // manifest and invoke the qa-host-invoke resolver on every repair command,
    // and must never invoke an ambient-PATH `godot-mcp transaction ...`.
    let root = workspace_root();
    let dir = descriptors::qa_tasks_dir(&root);
    let d = descriptors::load(&dir, 34).expect("committed qa/tasks/34.json loads");
    for c in d.happy.iter().chain(d.failure.iter()) {
        assert!(
            !c.command.contains("godot-mcp transaction"),
            "committed task 34 must not invoke ambient-PATH `godot-mcp transaction`: {}",
            c.command
        );
        if c.command.contains("transaction repair") {
            assert!(
                c.command.contains(descriptors::TASK34_RESOLVER_BIN),
                "repair command must invoke the packaged-binary resolver: {}",
                c.command
            );
            assert!(
                c.command
                    .contains(descriptors::TASK34_HOST_PACKAGE_MANIFEST),
                "repair command must resolve the host-package manifest: {}",
                c.command
            );
        }
    }
}

#[test]
fn committed_cancelled_descriptors_own_zero_commands() {
    let dir = descriptors::qa_tasks_dir(&workspace_root());
    for &n in CANCELLED_TASKS {
        let d = descriptors::load(&dir, n).expect("committed cancelled descriptor loads");
        assert_eq!(
            d.status,
            DescriptorStatus::Cancelled,
            "qa/tasks/{n}.json must declare the machine-readable cancelled status"
        );
        assert!(
            d.happy.is_empty() && d.failure.is_empty(),
            "cancelled task {n} must own zero commands, so no test that does not exist is invoked"
        );
        let reason = d.cancelled_reason.as_deref().unwrap_or_default();
        assert!(
            reason.contains("decisions.md"),
            "cancelled task {n} must cite the archived maintainer decision"
        );
        assert_eq!(
            d.cancelled_on.as_deref(),
            Some("2026-07-28"),
            "cancelled task {n} must record the cancellation date"
        );
        assert!(
            d.evidence.contains(&format!("task-{n}-")),
            "cancelled task {n} must keep its auditable evidence path"
        );
    }
}

#[test]
fn every_non_cancelled_descriptor_still_owns_commands() {
    let dir = descriptors::qa_tasks_dir(&workspace_root());
    for n in 1..=descriptors::TASK_COUNT {
        if CANCELLED_TASKS.contains(&n) {
            continue;
        }
        let d = descriptors::load(&dir, n).expect("committed descriptor loads");
        assert_eq!(
            d.status,
            DescriptorStatus::Active,
            "task {n} is not cancelled and must not claim the cancelled status"
        );
        assert!(
            !d.happy.is_empty() && !d.failure.is_empty(),
            "active task {n} keeps its mandatory mechanical commands"
        );
    }
}

#[test]
fn cancelled_descriptor_with_residual_commands_is_rejected() {
    let dir = descriptors::qa_tasks_dir(&workspace_root());
    let mut value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("20.json")).unwrap()).unwrap();
    value["happy"] = serde_json::json!([{
        "command": "cargo test -p godot-mcp-bridge --test editor_plugin_harness",
        "expected_exit": 0,
        "assertions": ["a test that does not exist"]
    }]);
    let residual: descriptors::TaskDescriptor = serde_json::from_value(value).unwrap();
    let error = residual
        .validate(20)
        .expect_err("a cancelled descriptor with a residual command must be rejected");
    assert!(
        error.contains("still carries"),
        "the rejection must name the residual command: {error}"
    );
}

#[test]
fn cancelled_descriptor_without_a_reason_is_rejected() {
    let dir = descriptors::qa_tasks_dir(&workspace_root());
    let mut value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("21.json")).unwrap()).unwrap();
    value["cancelled_reason"] = serde_json::json!("   ");
    let blank: descriptors::TaskDescriptor = serde_json::from_value(value).unwrap();
    assert!(
        blank.validate(21).is_err(),
        "a cancelled descriptor with a blank reason must be rejected"
    );
}

#[test]
fn active_descriptor_carrying_cancellation_fields_is_rejected() {
    let dir = descriptors::qa_tasks_dir(&workspace_root());
    let mut value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("19.json")).unwrap()).unwrap();
    value["cancelled_reason"] = serde_json::json!("not actually cancelled");
    let mislabelled: descriptors::TaskDescriptor = serde_json::from_value(value).unwrap();
    assert!(
        mislabelled.validate(19).is_err(),
        "an active descriptor must not carry cancellation fields"
    );
}

/// Build a throwaway tree holding only `scripts/qa_task.py` and `qa/tasks`, so
/// the runner can be exercised against a mutated descriptor without touching the
/// committed one.
fn runner_sandbox(descriptor: u32, value: &serde_json::Value) -> tempfile::TempDir {
    let root = workspace_root();
    let tmp = tempfile::tempdir().expect("temp dir");
    std::fs::create_dir_all(tmp.path().join("scripts")).unwrap();
    std::fs::create_dir_all(tmp.path().join("qa/tasks")).unwrap();
    std::fs::copy(
        root.join("scripts/qa_task.py"),
        tmp.path().join("scripts/qa_task.py"),
    )
    .unwrap();
    std::fs::write(
        tmp.path().join(format!("qa/tasks/{descriptor}.json")),
        serde_json::to_string_pretty(value).unwrap(),
    )
    .unwrap();
    tmp
}

fn run_runner(tmp: &tempfile::TempDir, descriptor: u32) -> std::process::Output {
    run_runner_with_attempt_dir(tmp, descriptor, None)
}

fn run_runner_with_attempt_dir(
    tmp: &tempfile::TempDir,
    descriptor: u32,
    attempt_dir: Option<&Path>,
) -> std::process::Output {
    let mut command = Command::new("python3");
    command
        .arg(tmp.path().join("scripts/qa_task.py"))
        .arg(descriptor.to_string());
    match attempt_dir {
        Some(dir) => {
            command.env("QA_ATTEMPT_DIR", dir);
        }
        None => {
            command.env_remove("QA_ATTEMPT_DIR");
        }
    }
    command.output().expect("the qa-task runner is invocable")
}

/// An active descriptor whose two commands are trivially cheap, so a test can
/// exercise the evidence check without running a real gate.
fn trivial_active_descriptor(evidence: &str) -> serde_json::Value {
    serde_json::json!({
        "task": 19,
        "wrapper": "simple",
        "evidence": evidence,
        "happy": [{ "command": "true", "expected_exit": 0, "assertions": ["exits zero"] }],
        "failure": [{ "command": "false", "expected_exit": 1, "assertions": ["exits one"] }]
    })
}

#[test]
fn runner_rejects_a_descriptor_whose_repo_relative_evidence_is_missing() {
    let value = trivial_active_descriptor("docs/reports/this-evidence-file-does-not-exist.md");
    let tmp = runner_sandbox(19, &value);
    let output = run_runner(&tmp, 19);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "a descriptor naming evidence that does not exist must fail: {stderr}"
    );
    assert!(
        stderr.contains("docs/reports/this-evidence-file-does-not-exist.md")
            && stderr.contains("does not exist"),
        "the rejection must name the missing evidence path: {stderr}"
    );
    assert!(
        !stderr.contains("qa-task: run:"),
        "the evidence check must fail before any command runs: {stderr}"
    );
}

// The descriptor's command is a POSIX `true`; qa-task runs on the Unix gate hosts.
#[cfg(unix)]
#[test]
fn runner_accepts_a_descriptor_whose_repo_relative_evidence_exists() {
    let value = trivial_active_descriptor("evidence/present.txt");
    let tmp = runner_sandbox(19, &value);
    std::fs::create_dir_all(tmp.path().join("evidence")).unwrap();
    std::fs::write(tmp.path().join("evidence/present.txt"), b"observed\n").unwrap();
    let output = run_runner(&tmp, 19);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "evidence that exists must not fail the task: {stderr}"
    );
    assert!(
        stderr.contains("[PASS] evidence exists"),
        "the runner must report the evidence it verified: {stderr}"
    );
}

#[test]
fn runner_rejects_attempt_dir_evidence_that_is_missing_once_the_env_names_a_directory() {
    let value = trivial_active_descriptor("<attemptDir>/task-19-absent.json");
    let tmp = runner_sandbox(19, &value);
    let attempt = tempfile::tempdir().expect("attempt dir");
    let output = run_runner_with_attempt_dir(&tmp, 19, Some(attempt.path()));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "a resolvable attempt-directory evidence path that is absent must fail: {stderr}"
    );
    assert!(
        stderr.contains("task-19-absent.json") && stderr.contains("does not exist"),
        "the rejection must name the resolved evidence path: {stderr}"
    );
    assert!(
        !stderr.contains("qa-task: run:"),
        "the evidence check must fail before any command runs: {stderr}"
    );
}

// The descriptor's command is a POSIX `true`; qa-task runs on the Unix gate hosts.
#[cfg(unix)]
#[test]
fn runner_accepts_attempt_dir_evidence_that_exists() {
    let value = trivial_active_descriptor("<attemptDir>/task-19-present.json");
    let tmp = runner_sandbox(19, &value);
    let attempt = tempfile::tempdir().expect("attempt dir");
    std::fs::write(attempt.path().join("task-19-present.json"), b"{}\n").unwrap();
    let output = run_runner_with_attempt_dir(&tmp, 19, Some(attempt.path()));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "attempt-directory evidence that exists must not fail the task: {stderr}"
    );
    assert!(
        stderr.contains("[PASS] evidence exists"),
        "the runner must report the evidence it verified: {stderr}"
    );
}

#[test]
fn runner_reports_attempt_dir_evidence_as_unchecked_when_the_env_is_absent() {
    // Every committed descriptor uses the `<attemptDir>/` shape, which names a
    // file outside the repository. Unresolvable is disclosed, never silent.
    let dir = descriptors::qa_tasks_dir(&workspace_root());
    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("22.json")).unwrap()).unwrap();
    let tmp = runner_sandbox(22, &value);
    let output = run_runner(&tmp, 22);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "an unresolvable attempt-directory path must not fail the task: {stderr}"
    );
    assert!(
        stderr.contains("[UNCHECKED]") && stderr.contains("QA_ATTEMPT_DIR"),
        "the runner must disclose that it could not check the evidence, and how to make it \
         checkable: {stderr}"
    );
}

#[test]
fn runner_rejects_an_unrecognised_evidence_placeholder() {
    let value = trivial_active_descriptor("<someOtherDir>/task-19.json");
    let tmp = runner_sandbox(19, &value);
    let output = run_runner(&tmp, 19);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "only `<attemptDir>` is a recognised evidence placeholder: {stderr}"
    );
    assert!(
        stderr.contains("unrecognised angle-bracket token"),
        "the rejection must say the placeholder is not recognised: {stderr}"
    );
}

#[test]
fn every_committed_descriptor_uses_a_shape_the_runner_can_classify() {
    // The runner must never silently accept an evidence entry it cannot classify.
    let dir = descriptors::qa_tasks_dir(&workspace_root());
    for n in 1..=descriptors::TASK_COUNT {
        let d = descriptors::load(&dir, n).expect("committed descriptor loads");
        let evidence = d.evidence.trim();
        assert!(
            evidence.starts_with("<attemptDir>/"),
            "task {n} evidence {evidence:?} must use the attempt-directory shape, or the \
             disclosure in docs/contributor/workflow.md no longer describes reality"
        );
        assert!(
            !contains_angle_bracket_token(&evidence["<attemptDir>/".len()..]),
            "task {n} evidence {evidence:?} must not nest a second placeholder"
        );
    }
}

#[test]
fn runner_exits_zero_and_runs_nothing_for_a_cancelled_descriptor() {
    let dir = descriptors::qa_tasks_dir(&workspace_root());
    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("22.json")).unwrap()).unwrap();
    let tmp = runner_sandbox(22, &value);
    let output = run_runner(&tmp, 22);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "a cancelled descriptor must exit 0: {stderr}"
    );
    assert!(
        !stderr.contains("qa-task: run:"),
        "a cancelled descriptor must execute zero commands: {stderr}"
    );
    assert!(
        stderr.contains("CANCELLED") && stderr.contains("0 commands executed"),
        "the runner must state the cancellation explicitly: {stderr}"
    );
}

#[test]
fn runner_rejects_a_cancelled_descriptor_with_residual_commands() {
    let dir = descriptors::qa_tasks_dir(&workspace_root());
    let mut value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("23.json")).unwrap()).unwrap();
    value["happy"] = serde_json::json!([{
        "command": "cargo test -p godot-mcp-bridge --test undoable_mutations",
        "expected_exit": 0,
        "assertions": ["a test that does not exist"]
    }]);
    let tmp = runner_sandbox(23, &value);
    let output = run_runner(&tmp, 23);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "cancellation must not become an escape hatch for leftover commands: {stderr}"
    );
    assert!(
        !stderr.contains("qa-task: run:"),
        "the rejected descriptor must not have executed anything: {stderr}"
    );
}

#[test]
fn angle_bracket_detector_is_accurate() {
    assert!(contains_angle_bracket_token(
        "godot-mcp transaction repair --transaction-id <transaction-id>"
    ));
    assert!(!contains_angle_bracket_token(
        "godot-mcp transaction repair --transaction-id 34000000-0000-4000-8000-000000000101"
    ));
    // A shell redirection `>` alone or a comparison is not a placeholder token.
    assert!(!contains_angle_bracket_token(
        "cargo test -- --exact x 2> log.txt"
    ));
}
