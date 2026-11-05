//! `script.run`: caller-written GDScript in a supervised headless Godot
//! (ADR 0012).
//!
//! The run is one engine launch through the same journaled process route as
//! `project.run`, with the bundled host as the engine's main loop. In the
//! `project` context it runs against the held project: whatever it changes
//! stays, the workspace is rescanned after it, and the answer lists every file
//! it added, changed or removed, compared by content. In the `copy` context it
//! runs against a disposable copy the server made and imported first; the
//! copy's changes are compared the same way, reported and discarded with it.
//!
//! Nothing is undone and nothing is contained beyond the launch boundary every
//! engine run has: the code runs with the user's privileges, and may read,
//! write, start and reach whatever the user can.

use std::path::{Path, PathBuf};
use std::time::Duration;

use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::contracts::OutcomeError;
use godot_mcp_domain::safe_root::SafeRoot;
use godot_mcp_engine::disposable::copy::copy_project;
use godot_mcp_engine::disposable::exclusion::ExclusionPolicy;
use godot_mcp_engine::script_run::{
    HOST_SCRIPT, HostReport, REQUIRED_CAPABILITIES, RunFiles, read_report, source_line,
};
use godot_mcp_engine::supervision::journal::hex_sha256;
use godot_mcp_engine::validation::diagnostic::{Diagnostic, DiagnosticSeverity};
use godot_mcp_engine::validation::run::run_verdict;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::outcome_error;
use super::process::{ProcessExecution, ProcessExecutionReport, ProcessExecutionResult};
use super::public::streams_value;
use super::request::ScriptContext;
use super::runtime::EngineRuntime;
use crate::audit::RequestAudit;
use crate::journal::OperationFamily;
use crate::kernel::AccessContext;
use crate::scripts::{
    Contents, Fingerprint, MAX_COMPARED_BYTES, MAX_COMPARED_FILES, changes_between,
    contents_of_inventory, project_changes, returned_value, scripts_off, with_changes,
};

/// The public tool name every launch of a run is journaled under.
const TOOL: &str = "script.run";

/// The server's own directories in a project: its state, where a run's copy
/// is made, a bootstrap server's state and the helper area. A copy never opens
/// them, and they are not the project's files, so an answer does not list them
/// among what the copy left out.
const SERVER_DIRECTORIES: [&str; 3] = [
    godot_mcp_domain::lease::PROJECT_STATE_DIR_NAME,
    godot_mcp_domain::workspace_root::BOOTSTRAP_STATE_DIR_NAME,
    godot_mcp_engine::canonical::helper::HELPER_ROOT,
];

/// The most paths a copy's exclusion list names.
const MAX_LISTED_EXCLUSIONS: usize = 100;

/// What `script.run` was asked to do.
pub(super) struct ScriptRun {
    pub(super) source: String,
    pub(super) context: ScriptContext,
    pub(super) deadline: Duration,
}

impl EngineRuntime {
    pub(super) async fn run_script(
        &self,
        run: ScriptRun,
        arguments: Value,
        access: &AccessContext,
        idempotency_id: Option<&str>,
        audit: RequestAudit,
        cancellation: CancellationToken,
    ) -> Result<Value, OutcomeError> {
        if !self.allow_scripts {
            return Err(scripts_off());
        }
        // A replay answers from the journal and never runs the code again.
        if let Some(replayed) = self.recorded_process_replay(
            TOOL,
            &arguments,
            access,
            idempotency_id,
            &audit,
            &cancellation,
        )? {
            return Ok(replay_answer(replayed));
        }
        let run_id = self
            .router
            .next_transaction_id("script-run", access.observed_at_unix_ms());
        match run.context {
            ScriptContext::Project => {
                self.run_in_project(
                    &run,
                    &run_id,
                    arguments,
                    access,
                    idempotency_id,
                    audit,
                    cancellation,
                )
                .await
            }
            ScriptContext::Copy => {
                self.run_in_copy(
                    &run,
                    &run_id,
                    arguments,
                    access,
                    idempotency_id,
                    audit,
                    cancellation,
                )
                .await
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_in_project(
        &self,
        run: &ScriptRun,
        run_id: &str,
        arguments: Value,
        access: &AccessContext,
        idempotency_id: Option<&str>,
        audit: RequestAudit,
        cancellation: CancellationToken,
    ) -> Result<Value, OutcomeError> {
        // The run writes project files the way the user's code does, beside
        // nothing else: no editor holds the project while it runs.
        let _exclusive = match &self.editor {
            Some(bridge) => Some(bridge.exclude_connections().map_err(|_| {
                outcome_error(
                    ErrorKind::Conflict,
                    "a Godot editor is connected, and a headless run in the project would write \
                     files underneath it; run the script in the editor with \
                     editor.script.run, use `context: copy`, or close the editor. Nothing was \
                     run.",
                )
            })?),
            None => None,
        };
        let root = self.root()?;
        // Read from disk now, not from the last snapshot, which may predate
        // another writer's changes.
        let before = contents_of_inventory(root, &self.workspace.rescan())?;
        let files = RunFiles::of(run_id);
        install_held(root, &files, &run.source)?;
        let execution = self
            .execute_process(
                ProcessExecution {
                    tool: TOOL,
                    arguments,
                    argv: files.host_arguments(&self.project_root),
                    deadline: run.deadline,
                    family: OperationFamily::ProjectMutation,
                    required_capabilities: REQUIRED_CAPABILITIES,
                },
                access,
                idempotency_id,
                audit,
                cancellation,
            )
            .await;
        let report = root
            .read(Path::new(&files.report))
            .ok()
            .map(|bytes| read_report(&bytes).map_err(|error| error.to_string()));
        let cleanup = remove_held(root, &files, &run.source);
        let report_value = match execution {
            Ok(ProcessExecutionResult::Replayed(value)) => return Ok(replay_answer(value)),
            Ok(ProcessExecutionResult::Executed(report_value)) => report_value,
            // The code ran and was ended: what it wrote until then is part of
            // the project now, and the refusal says so.
            Err(error) if matches!(error.kind(), ErrorKind::Timeout | ErrorKind::Cancelled) => {
                let after = contents_of_inventory(root, &self.workspace.rescan());
                return Err(with_changes(
                    error,
                    before.as_ref(),
                    after,
                    "The run was ended before run() returned",
                ));
            }
            Err(error) => return Err(error),
        };
        let after = contents_of_inventory(root, &self.workspace.rescan());
        let changes = project_changes(before.as_ref(), after);
        Ok(answer(Answer {
            context: ScriptContext::Project,
            execution: &report_value,
            report,
            changes,
            cleanup,
            excluded: Value::Null,
        }))
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_in_copy(
        &self,
        run: &ScriptRun,
        run_id: &str,
        arguments: Value,
        access: &AccessContext,
        idempotency_id: Option<&str>,
        audit: RequestAudit,
        cancellation: CancellationToken,
    ) -> Result<Value, OutcomeError> {
        let directory = self.state_dir.join("script-copies").join(run_id);
        let copy = Copy::make(&self.project_root, &directory)?;
        let result = self
            .run_against_copy(
                &copy,
                run,
                run_id,
                arguments,
                access,
                idempotency_id,
                audit,
                cancellation,
            )
            .await;
        let discarded = copy.discard();
        let mut value = result?;
        if let (Value::Object(object), Err(reason)) = (&mut value, discarded) {
            object.insert("copy_discarded".to_owned(), Value::Bool(false));
            object.insert("copy_discard_error".to_owned(), Value::String(reason));
        }
        Ok(value)
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_against_copy(
        &self,
        copy: &Copy,
        run: &ScriptRun,
        run_id: &str,
        arguments: Value,
        access: &AccessContext,
        idempotency_id: Option<&str>,
        audit: RequestAudit,
        cancellation: CancellationToken,
    ) -> Result<Value, OutcomeError> {
        // The copy holds no import cache; importing it first is what lets the
        // source use the project's script classes and imported resources.
        let mut import_arguments = arguments.clone();
        if let Value::Object(object) = &mut import_arguments {
            object.insert("step".to_owned(), Value::String("import".to_owned()));
        }
        let import_id = idempotency_id.map(|id| format!("{id}#import"));
        let imported = self
            .execute_process(
                ProcessExecution {
                    tool: TOOL,
                    arguments: import_arguments,
                    argv: vec![
                        "--headless".to_owned(),
                        "--path".to_owned(),
                        copy.root.display().to_string(),
                        "--import".to_owned(),
                    ],
                    deadline: run.deadline,
                    family: OperationFamily::RuntimeExecution,
                    required_capabilities: &["--headless", "--path", "--import"],
                },
                access,
                import_id.as_deref(),
                audit.clone(),
                cancellation.clone(),
            )
            .await?;
        if let ProcessExecutionResult::Executed(import) = &imported
            && (import.observed.timed_out || import.observed.exit_code != Some(0))
        {
            return Err(outcome_error(
                ErrorKind::Unavailable,
                format!(
                    "the copy could not be imported before the run (exit {:?}, timed out: {}); \
                     the source was not run",
                    import.observed.exit_code, import.observed.timed_out
                ),
            ));
        }
        let before = contents_of_directory(&copy.root);
        let files = RunFiles::of(run_id);
        install_private(&copy.root, &files, &run.source)?;
        let execution = self
            .execute_process(
                ProcessExecution {
                    tool: TOOL,
                    arguments,
                    argv: files.host_arguments(&copy.root),
                    deadline: run.deadline,
                    family: OperationFamily::RuntimeExecution,
                    required_capabilities: REQUIRED_CAPABILITIES,
                },
                access,
                idempotency_id,
                audit,
                cancellation,
            )
            .await;
        let report = std::fs::read(copy.root.join(&files.report))
            .ok()
            .map(|bytes| read_report(&bytes).map_err(|error| error.to_string()));
        // The run's files go with the copy.
        let cleanup = Ok(());
        let report_value = match execution? {
            ProcessExecutionResult::Replayed(value) => return Ok(replay_answer(value)),
            ProcessExecutionResult::Executed(report_value) => report_value,
        };
        let after = contents_of_directory(&copy.root);
        let changes = match (before, after) {
            (Ok(Some(before)), Ok(Some(after))) => Ok(changes_between(&before, &after)),
            (Ok(None), _) | (_, Ok(None)) => Err(format!(
                "the copy holds more than {MAX_COMPARED_FILES} files or {MAX_COMPARED_BYTES} \
                 bytes, which are not compared"
            )),
            (Err(error), _) | (_, Err(error)) => {
                Err(format!("the copy could not be read: {error}"))
            }
        };
        Ok(answer(Answer {
            context: ScriptContext::Copy,
            execution: &report_value,
            report,
            changes,
            cleanup,
            excluded: copy.excluded.clone(),
        }))
    }
}

/// The answer to a repeated request: the journal's record of the run it
/// repeats, which keeps the run's outcome but not its value or its changes.
fn replay_answer(mut recorded: Value) -> Value {
    if let Value::Object(object) = &mut recorded {
        object.insert("replayed".to_owned(), Value::Bool(true));
        object.insert(
            "result_unavailable".to_owned(),
            Value::String(
                "this request repeats one the journal already holds; the source was not run \
                 again, and the journal keeps the run's outcome, not its value or the files it \
                 changed"
                    .to_owned(),
            ),
        );
    }
    recorded
}

/// Write the host and the source of one run into the held project.
fn install_held(root: &SafeRoot, files: &RunFiles, source: &str) -> Result<(), OutcomeError> {
    let write = |path: &str, bytes: &[u8]| {
        root.create_parent_dirs(Path::new(path))
            .and_then(|()| root.cas_write(Path::new(path), None, bytes))
            .map_err(|error| {
                outcome_error(
                    ErrorKind::Unavailable,
                    format!("the run's files could not be written: {error}; nothing was run"),
                )
            })
    };
    write(&files.host, HOST_SCRIPT.as_bytes())?;
    if let Err(error) = write(&files.source, source.as_bytes()) {
        let _ = root.cas_delete(Path::new(&files.host), HOST_SCRIPT.as_bytes());
        return Err(error);
    }
    Ok(())
}

/// Remove the run's files from the held project, each only while it still
/// holds the bytes the server wrote or the host left; `Err` names what is left.
fn remove_held(root: &SafeRoot, files: &RunFiles, source: &str) -> Result<(), String> {
    let mut left = Vec::new();
    if root
        .cas_delete(Path::new(&files.host), HOST_SCRIPT.as_bytes())
        .is_err()
    {
        left.push(files.host.clone());
    }
    if root
        .cas_delete(Path::new(&files.source), source.as_bytes())
        .is_err()
    {
        left.push(files.source.clone());
    }
    if let Ok(bytes) = root.read(Path::new(&files.report))
        && root.cas_delete(Path::new(&files.report), &bytes).is_err()
    {
        left.push(files.report.clone());
    }
    if left.is_empty() {
        Ok(())
    } else {
        Err(format!("left behind: {}", left.join(", ")))
    }
}

/// Write the host and the source of one run into a copy, the server's own
/// private directory.
fn install_private(root: &Path, files: &RunFiles, source: &str) -> Result<(), OutcomeError> {
    for (path, bytes) in [
        (&files.host, HOST_SCRIPT.as_bytes()),
        (&files.source, source.as_bytes()),
    ] {
        let path = root.join(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| copy_unwritable(&error))?;
        }
        std::fs::write(&path, bytes).map_err(|error| copy_unwritable(&error))?;
    }
    Ok(())
}

fn copy_unwritable(error: &std::io::Error) -> OutcomeError {
    outcome_error(
        ErrorKind::Unavailable,
        format!("the run's files could not be written into the copy: {error}; nothing was run"),
    )
}

/// A disposable copy of the held project, made for one run.
struct Copy {
    directory: PathBuf,
    root: PathBuf,
    /// What the copy left out, as the answer lists it.
    excluded: Value,
}

impl Copy {
    fn make(project_root: &Path, directory: &Path) -> Result<Self, OutcomeError> {
        let root = directory.join("project");
        std::fs::create_dir_all(&root).map_err(|error| {
            outcome_error(
                ErrorKind::Unavailable,
                format!("the copy's directory could not be created: {error}; nothing was run"),
            )
        })?;
        let policy = SERVER_DIRECTORIES
            .into_iter()
            .fold(ExclusionPolicy::new(), ExclusionPolicy::owning_directory);
        let manifest = match copy_project(project_root, &root, &policy) {
            Ok(manifest) => manifest,
            Err(error) => {
                let _ = std::fs::remove_dir_all(directory);
                return Err(outcome_error(
                    error.error_kind(),
                    format!("the project could not be copied: {error}; nothing was run"),
                ));
            }
        };
        let listed: Vec<Value> = manifest
            .excluded
            .iter()
            .take(MAX_LISTED_EXCLUSIONS)
            .map(|entry| json!({ "path": entry.path, "reason": entry.reason.as_str() }))
            .collect();
        Ok(Self {
            directory: directory.to_path_buf(),
            root,
            excluded: json!({
                "paths": listed,
                "total": manifest.excluded.len(),
                "truncated": manifest.excluded.len() > MAX_LISTED_EXCLUSIONS,
            }),
        })
    }

    fn discard(&self) -> Result<(), String> {
        std::fs::remove_dir_all(&self.directory).map_err(|error| error.to_string())
    }
}

/// The content of every regular file of a copy, except the engine cache and
/// the server-owned helper area; `None` when it is larger than a comparison
/// reads. The copy is the server's own private directory.
fn contents_of_directory(root: &Path) -> Result<Option<Contents>, String> {
    let mut contents = Contents::new();
    let mut total = 0_u64;
    let mut pending = vec![(root.to_path_buf(), String::new())];
    while let Some((directory, prefix)) = pending.pop() {
        let entries = std::fs::read_dir(&directory).map_err(|error| error.to_string())?;
        for entry in entries {
            let entry = entry.map_err(|error| error.to_string())?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let relative = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            if relative == ".godot" || relative == godot_mcp_engine::canonical::helper::HELPER_ROOT
            {
                continue;
            }
            let kind = entry.file_type().map_err(|error| error.to_string())?;
            if kind.is_dir() {
                pending.push((entry.path(), relative));
            } else if kind.is_file() {
                let bytes = std::fs::read(entry.path()).map_err(|error| error.to_string())?;
                total += bytes.len() as u64;
                if contents.len() >= MAX_COMPARED_FILES || total > MAX_COMPARED_BYTES {
                    return Ok(None);
                }
                contents.insert(
                    relative,
                    Fingerprint {
                        sha256: hex_sha256(&bytes),
                        bytes: bytes.len() as u64,
                    },
                );
            }
        }
    }
    Ok(Some(contents))
}

struct Answer<'a> {
    context: ScriptContext,
    execution: &'a ProcessExecutionReport,
    /// The host's report: absent when it wrote none.
    report: Option<Result<HostReport, String>>,
    changes: Result<Value, String>,
    cleanup: Result<(), String>,
    excluded: Value,
}

/// One engine diagnostic as the answer carries it: with the line of the
/// caller's source when the engine named a position inside it.
fn diagnostic_value(diagnostic: &Diagnostic) -> Value {
    let mut value = serde_json::to_value(diagnostic).unwrap_or(Value::Null);
    if let (Value::Object(object), Some(line)) = (
        &mut value,
        diagnostic.engine_location.as_deref().and_then(source_line),
    ) {
        object.insert("source_line".to_owned(), json!(line));
    }
    value
}

fn answer(answer: Answer<'_>) -> Value {
    let execution = answer.execution;
    let verdict = run_verdict(
        &execution.observed,
        &execution.diagnostics,
        &execution.excerpt.stdout.text,
    );
    let errors = execution
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.severity == DiagnosticSeverity::Error);
    let (classification, reason, result, literal, unavailable) =
        match (&answer.report, execution.observed.timed_out) {
            (_, true) => (
                "timed_out",
                "the deadline ended the run before it finished; what it changed until then stays"
                    .to_owned(),
                Value::Null,
                Value::Null,
                Some("the run did not finish".to_owned()),
            ),
            (Some(Ok(HostReport::NotRun { reason })), false) => (
                "compile_error",
                format!("{reason}; run() was not called"),
                Value::Null,
                Value::Null,
                Some("run() was not called".to_owned()),
            ),
            (Some(Ok(HostReport::Returned { literal, length })), false) => {
                let (result, unavailable) = returned_value(literal.as_deref(), *length);
                if errors || execution.observed.exit_code != Some(0) {
                    (
                        "failed",
                        verdict.reason.clone(),
                        result,
                        literal.clone().map_or(Value::Null, Value::String),
                        unavailable,
                    )
                } else {
                    (
                        "passed",
                        "run() returned".to_owned(),
                        result,
                        literal.clone().map_or(Value::Null, Value::String),
                        unavailable,
                    )
                }
            }
            (Some(Err(reason)), false) => (
                "failed",
                format!("the run's report is unreadable: {reason}"),
                Value::Null,
                Value::Null,
                Some("the report is unreadable".to_owned()),
            ),
            (None, false) => (
                if errors || execution.observed.exit_code != Some(0) {
                    "failed"
                } else {
                    "inconclusive"
                },
                "the run ended without a report: run() did not return, or the code ended the \
             engine itself"
                    .to_owned(),
                Value::Null,
                Value::Null,
                Some("the run wrote no report".to_owned()),
            ),
        };
    let (stdout, stderr) = streams_value(execution.output, &execution.excerpt);
    let (changes, changes_unavailable) = match answer.changes {
        Ok(changes) => (changes, Value::Null),
        Err(reason) => (Value::Null, Value::String(reason)),
    };
    json!({
        "transaction_id": execution.transaction_id,
        "context": answer.context.as_str(),
        "classification": classification,
        "reason": reason,
        "result": result,
        "result_literal": literal,
        "result_unavailable": unavailable,
        "exit_code": execution.observed.exit_code,
        "signal": execution.observed.signal,
        "timed_out": execution.observed.timed_out,
        "diagnostics": execution.diagnostics.iter().map(diagnostic_value).collect::<Vec<_>>(),
        "diagnostics_truncated": execution.output.diagnostics_truncated(),
        "stdout": stdout,
        "stderr": stderr,
        "changes": changes,
        "changes_unavailable": changes_unavailable,
        "applied": answer.context == ScriptContext::Project,
        "rolled_back": false,
        "copy_excluded": answer.excluded,
        "cleanup_error": answer.cleanup.err(),
        "execution": execution.execution(),
        "workspace": execution.workspace,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fingerprint(sha: &str, bytes: u64) -> Fingerprint {
        Fingerprint {
            sha256: sha.to_owned(),
            bytes,
        }
    }

    #[test]
    fn a_copy_leaves_out_the_server_state_it_is_made_in() {
        let directory = tempfile::tempdir().expect("directory");
        let project = directory.path().join("game");
        for (path, text) in [
            ("project.godot", "config_version=5\n"),
            ("main.gd", "extends Node\n"),
            (".godot-mcp/journal/entry.json", "{}"),
            (".godot-mcp-bootstrap/state.json", "{}"),
            ("godot_mcp_helpers/script_run/old.host.gd", "host"),
        ] {
            let file = project.join(path);
            std::fs::create_dir_all(file.parent().expect("parent")).expect("parent");
            std::fs::write(file, text).expect("file");
        }
        let copy =
            Copy::make(&project, &project.join(".godot-mcp/script-copies/run-1")).expect("copy");
        let contents = contents_of_directory(&copy.root)
            .expect("read")
            .expect("small");
        assert_eq!(
            contents.keys().map(String::as_str).collect::<Vec<_>>(),
            ["main.gd", "project.godot"]
        );
        for directory in [".godot-mcp", ".godot-mcp-bootstrap", "godot_mcp_helpers"] {
            assert!(!copy.root.join(directory).exists(), "{directory} is copied");
        }
        assert_eq!(copy.excluded["total"], json!(0), "{}", copy.excluded);
        copy.discard().expect("discard");
        assert!(!project.join(".godot-mcp/script-copies/run-1").exists());
        assert!(project.join(".godot-mcp/journal/entry.json").is_file());
    }

    #[test]
    fn a_copy_compares_everything_but_the_engine_cache_and_the_helper_area() {
        let directory = tempfile::tempdir().expect("directory");
        let root = directory.path();
        for (path, text) in [
            ("project.godot", "config_version=5\n"),
            ("scripts/main.gd", "extends Node\n"),
            (".godot/imported/x.ctex", "cache"),
            ("godot_mcp_helpers/script_run/r/host.gd", "host"),
        ] {
            let file = root.join(path);
            std::fs::create_dir_all(file.parent().expect("parent")).expect("parent");
            std::fs::write(file, text).expect("file");
        }
        let contents = contents_of_directory(root).expect("read").expect("small");
        assert_eq!(
            contents.keys().map(String::as_str).collect::<Vec<_>>(),
            ["project.godot", "scripts/main.gd"]
        );
        assert_eq!(
            contents["scripts/main.gd"],
            fingerprint(&hex_sha256(b"extends Node\n"), 13)
        );
    }
}
