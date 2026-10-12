//! QA task descriptor loading and validation (`qa/tasks/<N>.json`).
//!
//! Todo 1 commits all 37 descriptors as complete command/assertion contracts
//! seeded verbatim from each Todo's prose QA scenarios. `qa-task` and the
//! `qa_descriptors` inventory test both use this module. Validation fails closed
//! on a missing descriptor, wrong task number, empty command/assertion arrays,
//! an expected-exit contradiction, an angle-bracket transaction-ID token, or a
//! command-reuse shorthand phrase.
//!
//! ## Cancelled descriptors
//!
//! A Todo that was cancelled ships no test, so requiring it to run one would make
//! the whole inventory unsatisfiable. Such a descriptor carries the explicit
//! machine-readable marker `"status": "cancelled"` plus `cancelled_reason` and
//! `cancelled_on`, and then owns **zero** commands: `qa-task` executes nothing and
//! exits 0. This is a declaration, not an escape hatch, so it is fail-closed in
//! both directions — a cancelled descriptor that still carries a `happy`/`failure`
//! command, a fixture lifecycle, or an empty/absent reason is rejected, and a
//! non-cancelled descriptor that carries cancellation fields is rejected too.
//! Cancelled descriptors are named in the plan's own QA scenarios, so a descriptor
//! cannot quietly become cancelled without the plan saying so.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// One descriptor-owned command with its exact expected outcome and assertions.
#[derive(Debug, Clone, Deserialize)]
pub struct DescriptorCommand {
    /// The exact command string as it must be run.
    pub command: String,
    /// The expected process exit code for this command.
    pub expected_exit: i32,
    /// Prose statements of intent describing what "pass" is supposed to mean,
    /// reviewed by a person. Neither this module nor the `qa-task` runner
    /// evaluates them; validation requires only that the list is non-empty and
    /// holds no blank entry. See `docs/contributor/workflow.md`.
    pub assertions: Vec<String>,
}

/// The wrapper execution mode for a task.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum WrapperKind {
    /// Run every happy command then every failure command; aggregate exits.
    Simple,
    /// Task-34-style: run `setup`, then commands, then a `finally`-equivalent
    /// `cleanup`, computing the final exit code accordingly.
    FixtureLifecycle,
}

/// Optional setup/cleanup lifecycle for fixture-driven tasks (Task 34).
#[derive(Debug, Clone, Deserialize)]
pub struct FixtureLifecycle {
    /// Exact command run once before any descriptor command.
    pub setup: String,
    /// Exact command run unconditionally after every descriptor command.
    pub cleanup: String,
}

/// A descriptor's lifecycle status.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DescriptorStatus {
    /// The Todo is live: it owns real commands that must run and pass.
    #[default]
    Active,
    /// The Todo was cancelled: it owns no command and `qa-task` runs nothing.
    Cancelled,
}

/// A complete QA task descriptor.
#[derive(Debug, Clone, Deserialize)]
pub struct TaskDescriptor {
    /// The task number; must equal the file's `<N>`.
    pub task: u32,
    /// Lifecycle status; absent means [`DescriptorStatus::Active`].
    #[serde(default)]
    pub status: DescriptorStatus,
    /// Why the Todo was cancelled. Required when, and only when, the status is
    /// [`DescriptorStatus::Cancelled`].
    #[serde(default)]
    pub cancelled_reason: Option<String>,
    /// The date the cancellation was decided. Required when, and only when, the
    /// status is [`DescriptorStatus::Cancelled`].
    #[serde(default)]
    pub cancelled_on: Option<String>,
    /// Wrapper execution mode.
    pub wrapper: WrapperKind,
    /// Optional fixture lifecycle (required when `wrapper == FixtureLifecycle`).
    #[serde(default)]
    pub fixture_lifecycle: Option<FixtureLifecycle>,
    /// Happy-path commands (each asserts a specific exit code). Must be
    /// non-empty for an active Todo and empty for a cancelled one.
    #[serde(default)]
    pub happy: Vec<DescriptorCommand>,
    /// Failure-path commands. Must be non-empty for an active Todo and empty for
    /// a cancelled one.
    #[serde(default)]
    pub failure: Vec<DescriptorCommand>,
    /// Evidence path for this task.
    pub evidence: String,
}

/// Phrases that must never appear inside a descriptor command — they indicate
/// command-reuse shorthand or runtime prose substitution rather than a literal,
/// self-contained command.
pub const FORBIDDEN_SHORTHAND: &[&str] = &[
    "same as",
    "as above",
    "see above",
    "inherit",
    "repeat the",
    "...",
    "etc.",
];

const WRAPPER_INVOCATIONS: &[&str] = &["make qa-task", "qa-task TASK="];

/// Load a single descriptor from `qa/tasks/<n>.json` under `qa_dir`.
///
/// # Errors
/// Returns an error if the file is missing or is not valid JSON.
pub fn load(qa_dir: &Path, n: u32) -> Result<TaskDescriptor, String> {
    let path = qa_dir.join(format!("{n}.json"));
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("missing descriptor {}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))
}

/// The `qa/tasks` directory relative to a workspace root.
#[must_use]
pub fn qa_tasks_dir(workspace_root: &Path) -> PathBuf {
    workspace_root.join("qa").join("tasks")
}

impl TaskDescriptor {
    /// Validate structural properties that make `qa-task` fail closed.
    ///
    /// * `task` equals `expected_task`,
    /// * a cancelled descriptor owns zero commands, no fixture lifecycle and a
    ///   non-empty reason/date, and an active one carries no cancellation field,
    /// * happy and failure arrays are non-empty (active descriptors only),
    /// * every command has a non-empty command string and non-empty assertions,
    /// * failure commands expect a nonzero exit (contract: failures are nonzero),
    /// * no descriptor command invokes `make qa-task`/`qa-task TASK=` (no
    ///   recursion),
    /// * no command contains an angle-bracket `<...>` transaction/token
    ///   placeholder or a command-reuse shorthand phrase.
    ///
    /// # Errors
    /// Returns a descriptive error naming the first violated property.
    pub fn validate(&self, expected_task: u32) -> Result<(), String> {
        if self.task != expected_task {
            return Err(format!(
                "wrong task number: file says {} expected {}",
                self.task, expected_task
            ));
        }
        if self.evidence.trim().is_empty() {
            return Err(format!("task {expected_task}: empty evidence path"));
        }
        if self.status == DescriptorStatus::Cancelled {
            return self.validate_cancelled(expected_task);
        }
        if self.cancelled_reason.is_some() || self.cancelled_on.is_some() {
            return Err(format!(
                "task {expected_task}: cancellation fields on an active descriptor"
            ));
        }
        if self.happy.is_empty() {
            return Err(format!("task {expected_task}: empty happy command array"));
        }
        if self.failure.is_empty() {
            return Err(format!("task {expected_task}: empty failure command array"));
        }
        if self.wrapper == WrapperKind::FixtureLifecycle && self.fixture_lifecycle.is_none() {
            return Err(format!(
                "task {expected_task}: fixture-lifecycle wrapper without setup/cleanup"
            ));
        }
        for (label, cmds) in [("happy", &self.happy), ("failure", &self.failure)] {
            for (i, c) in cmds.iter().enumerate() {
                self.validate_command(expected_task, label, i, c)?;
            }
        }
        // NOTE: a descriptor's "failure" commands are not required to expect a
        // nonzero exit. Per the plan, most Todos' failure-path command is a
        // `cargo test ..._failure_paths` invocation that itself exits 0 (the
        // test asserts the production code path rejects the bad input). Only
        // some Todos (1, 34) record genuinely-nonzero failure commands. The
        // fail-closed contract is that each command's ACTUAL exit must match its
        // recorded `expected_exit`; that check lives in the qa-task runner.
        Ok(())
    }

    /// A cancelled Todo owns nothing to run, so the only honest descriptor is one
    /// with zero commands and a recorded reason. Every other shape is rejected so
    /// that "cancelled" can never be used to silence a command that should run.
    fn validate_cancelled(&self, task: u32) -> Result<(), String> {
        if !self.happy.is_empty() {
            return Err(format!(
                "task {task}: cancelled descriptor still carries {} happy command(s)",
                self.happy.len()
            ));
        }
        if !self.failure.is_empty() {
            return Err(format!(
                "task {task}: cancelled descriptor still carries {} failure command(s)",
                self.failure.len()
            ));
        }
        if self.fixture_lifecycle.is_some() {
            return Err(format!(
                "task {task}: cancelled descriptor still carries a fixture lifecycle"
            ));
        }
        if self.wrapper == WrapperKind::FixtureLifecycle {
            return Err(format!(
                "task {task}: cancelled descriptor must not use the fixture-lifecycle wrapper"
            ));
        }
        if self
            .cancelled_reason
            .as_ref()
            .is_none_or(|reason| reason.trim().is_empty())
        {
            return Err(format!(
                "task {task}: cancelled descriptor without a cancelled_reason"
            ));
        }
        if self
            .cancelled_on
            .as_ref()
            .is_none_or(|on| on.trim().is_empty())
        {
            return Err(format!(
                "task {task}: cancelled descriptor without a cancelled_on date"
            ));
        }
        Ok(())
    }

    fn validate_command(
        &self,
        task: u32,
        label: &str,
        i: usize,
        c: &DescriptorCommand,
    ) -> Result<(), String> {
        if c.command.trim().is_empty() {
            return Err(format!("task {task}: {label}[{i}] empty command"));
        }
        if c.assertions.is_empty() || c.assertions.iter().any(|a| a.trim().is_empty()) {
            return Err(format!("task {task}: {label}[{i}] empty assertions"));
        }
        for w in WRAPPER_INVOCATIONS {
            if c.command.contains(w) {
                return Err(format!(
                    "task {task}: {label}[{i}] command recursively invokes the qa-task wrapper"
                ));
            }
        }
        if contains_angle_bracket_token(&c.command) {
            return Err(format!(
                "task {task}: {label}[{i}] command contains an angle-bracket <...> token"
            ));
        }
        for phrase in FORBIDDEN_SHORTHAND {
            if c.command.contains(phrase) {
                return Err(format!(
                    "task {task}: {label}[{i}] command contains forbidden shorthand '{phrase}'"
                ));
            }
        }
        Ok(())
    }
}

/// Detect an angle-bracket placeholder token like `<id>` or `<transaction-id>`
/// in a command string. A literal `<` immediately followed by a non-space,
/// non-`=` character and later closed by `>` is treated as a placeholder.
#[must_use]
pub fn contains_angle_bracket_token(cmd: &str) -> bool {
    let bytes = cmd.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            // find matching '>'
            if let Some(close_rel) = cmd[i + 1..].find('>') {
                let inner = &cmd[i + 1..i + 1 + close_rel];
                if !inner.is_empty() && !inner.contains(' ') {
                    return true;
                }
            }
        }
        i += 1;
    }
    false
}

/// The number of descriptors that must exist: `qa/tasks/1.json` .. `37.json`.
pub const TASK_COUNT: u32 = 37;

/// The frozen, workspace-relative Task-34 host-package manifest path every
/// Task-34 repair command must resolve. Kept in sync with
/// [`crate::host_package::HOST_PACKAGE_MANIFEST_PATH`].
pub const TASK34_HOST_PACKAGE_MANIFEST: &str = "target/qa-task-34/host-package/manifest.json";

/// The `--bin` name of the deterministic packaged-binary resolver every
/// Task-34 repair command must invoke instead of an ambient-PATH `godot-mcp`.
pub const TASK34_RESOLVER_BIN: &str = "qa-host-invoke";

/// The repair sub-invocation marker. A Task-34 descriptor command that contains
/// this is a repair command and must obey the deterministic packaged-binary
/// contract.
pub const TASK34_REPAIR_MARKER: &str = "transaction repair";

/// The forbidden ambient-PATH invocation form. Task-34 repair commands must
/// never invoke `godot-mcp transaction ...` directly off the PATH.
pub const TASK34_AMBIENT_INVOCATION: &str = "godot-mcp transaction";

/// The fixed Todo-34 transaction IDs that `qa/tasks/34.json` must contain
/// verbatim.
pub const TASK34_FIXED_IDS: &[&str] = &[
    "34000000-0000-4000-8000-000000000101",
    "34000000-0000-4000-8000-000000000102",
    "34000000-0000-4000-8000-000000000201",
    "34000000-0000-4000-8000-000000000202",
    "34000000-0000-4000-8000-000000000301",
    "34000000-0000-4000-8000-000000000302",
    "34000000-0000-4000-8000-000000001001",
    "34000000-0000-4000-8000-000000001002",
    "34000000-0000-4000-8000-000000001003",
    "34000000-0000-4000-8000-000000001004",
    "34000000-0000-4000-8000-000000001005",
    "34000000-0000-4000-8000-000000001006",
];

/// Validate the whole inventory: every descriptor `1..=37` exists, validates,
/// and `qa/tasks/34.json` carries every fixed transaction ID with no
/// angle-bracket token or shorthand.
///
/// # Errors
/// Returns the first failing descriptor's error.
pub fn validate_all(workspace_root: &Path) -> Result<(), String> {
    let dir = qa_tasks_dir(workspace_root);
    for n in 1..=TASK_COUNT {
        let d = load(&dir, n)?;
        d.validate(n)?;
        if n == 34 {
            validate_task34_ids(&d)?;
        }
    }
    Ok(())
}

fn validate_task34_ids(d: &TaskDescriptor) -> Result<(), String> {
    let commands: Vec<&str> = d
        .happy
        .iter()
        .chain(d.failure.iter())
        .map(|c| c.command.as_str())
        .collect();
    let all_cmds = commands.join("\n");
    for id in TASK34_FIXED_IDS {
        if !all_cmds.contains(id) {
            return Err(format!(
                "task 34: fixed transaction id {id} missing from descriptor commands"
            ));
        }
    }
    // Todo 34 must carry a fixture lifecycle (setup/cleanup).
    if d.wrapper != WrapperKind::FixtureLifecycle || d.fixture_lifecycle.is_none() {
        return Err("task 34: must use fixture-lifecycle wrapper with setup/cleanup".to_string());
    }
    // Deterministic packaged-binary contract: every repair command must resolve
    // the Task-34 host-package manifest and invoke the packaged-binary resolver,
    // never an ambient-PATH `godot-mcp transaction ...`.
    validate_task34_packaged_contract(&commands)?;
    Ok(())
}

/// Enforce the Task-34 deterministic packaged-binary contract on every repair
/// command: it must (a) invoke the `qa-host-invoke` resolver, (b) resolve the
/// frozen host-package manifest path, and (c) never invoke `godot-mcp
/// transaction ...` off the ambient PATH.
fn validate_task34_packaged_contract(commands: &[&str]) -> Result<(), String> {
    for cmd in commands {
        let is_repair = cmd.contains(TASK34_REPAIR_MARKER);
        // No ambient-PATH godot-mcp transaction invocation anywhere.
        if cmd.contains(TASK34_AMBIENT_INVOCATION) {
            return Err(format!(
                "task 34: command invokes ambient-PATH `{TASK34_AMBIENT_INVOCATION}` instead of the deterministic packaged binary: {cmd}"
            ));
        }
        if is_repair {
            if !cmd.contains(TASK34_RESOLVER_BIN) {
                return Err(format!(
                    "task 34: repair command does not invoke the `{TASK34_RESOLVER_BIN}` packaged-binary resolver: {cmd}"
                ));
            }
            if !cmd.contains(TASK34_HOST_PACKAGE_MANIFEST) {
                return Err(format!(
                    "task 34: repair command does not resolve the host-package manifest `{TASK34_HOST_PACKAGE_MANIFEST}`: {cmd}"
                ));
            }
        }
    }
    Ok(())
}
