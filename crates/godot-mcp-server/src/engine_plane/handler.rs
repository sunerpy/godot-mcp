use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::contracts::OutcomeError;
use godot_mcp_domain::manifests::canonical_manifest;
use godot_mcp_engine::canonical::request::CanonicalOperation;
use godot_mcp_engine::supervision::journal::{
    OperationFamily as EngineOperationFamily, ProjectIdentityClaim, TransactionEnvelope,
    TransportMode as EngineTransportMode, hex_sha256,
};
use godot_mcp_engine::validation::import::import_verdict;
use godot_mcp_engine::validation::project_relative_path;
use godot_mcp_engine::validation::runner::{EngineRunner, RunError, TransactionSource};
use godot_mcp_engine::validation::script::{
    REQUIRED_CAPABILITIES as SCRIPT_CHECK_CAPABILITIES, ScriptCheckReport, ScriptCheckRequest,
    check_script, unsupported_report,
};
use godot_mcp_engine::validation::suite::{TestRunRequest, run_tests};
use serde_json::{Value, json};

use super::outcome_error;
use super::process::{ProcessExecution, ProcessExecutionReport, ProcessExecutionResult};
use super::request::{EngineOperation, EngineRequest};
use super::runtime::{EngineResolution, EngineRuntime};
use crate::journal::OperationFamily;
use crate::kernel::{AccessContext, KernelToolCall, KernelToolHandler, ServiceKernel};
use crate::scheduler::OperationDeadline;

pub(crate) fn register_engine_tools(
    kernel: &mut ServiceKernel,
    runtime: Arc<EngineRuntime>,
) -> Result<BTreeSet<String>, OutcomeError> {
    let manifest = canonical_manifest(kernel.profile());
    let mut registered = BTreeSet::new();
    for &operation in EngineOperation::ALL {
        let name = operation.tool_name();
        if !manifest.tools.contains(name) {
            continue;
        }
        kernel.register_tool_handler(
            operation.key(),
            Arc::new(EngineHandler {
                operation,
                runtime: Arc::clone(&runtime),
            }),
        )?;
        registered.insert(name.to_owned());
    }
    Ok(registered)
}

struct EngineHandler {
    operation: EngineOperation,
    runtime: Arc<EngineRuntime>,
}

fn effective_process_deadline(
    request: &EngineRequest,
    operation_deadline: OperationDeadline,
) -> Option<Duration> {
    match request {
        EngineRequest::ImportProject { deadline_ms, .. }
        | EngineRequest::RunProject { deadline_ms, .. }
        | EngineRequest::RunTests { deadline_ms, .. }
        | EngineRequest::CheckScript { deadline_ms, .. } => Some(
            Duration::from_millis(*deadline_ms)
                .min(Duration::from_millis(operation_deadline.run_budget_ms())),
        ),
        EngineRequest::UpdateUid(_)
        | EngineRequest::Author(_)
        | EngineRequest::UpdateResource(_)
        | EngineRequest::SaveScene(_) => None,
    }
}

struct ValidationTransactions<'a> {
    runtime: &'a EngineRuntime,
    access: &'a AccessContext,
    canonical_request: &'a Value,
    idempotency_id: Option<&'a str>,
}

impl TransactionSource for ValidationTransactions<'_> {
    fn next(&self, tool_name: &str, family: EngineOperationFamily) -> TransactionEnvelope {
        let observed_at = self.access.observed_at_unix_ms();
        let transaction_id = self
            .runtime
            .router
            .next_transaction_id(tool_name, observed_at);
        let canonical_bytes = serde_json::to_vec(self.canonical_request)
            .expect("validated engine arguments serialize to canonical JSON");
        TransactionEnvelope {
            transaction_id: transaction_id.clone(),
            project: ProjectIdentityClaim {
                object_identity: self.runtime.held.object_identity.clone(),
                root_object: self.runtime.held.root_object.clone(),
                file_object: self.runtime.held.file_object.clone(),
                generation: self.runtime.held.generation,
            },
            transport_mode: engine_transport(self.access),
            principal: Some(self.access.mode_principal().to_journal_string()),
            session: Some(self.access.connection_session().to_owned()),
            operation_family: family,
            tool_name: tool_name.to_owned(),
            idempotency_id: self
                .idempotency_id
                .map_or_else(|| format!("validation-{transaction_id}"), str::to_owned),
            canonical_request_hash: hex_sha256(&canonical_bytes),
            canonical_request_bytes: canonical_bytes.len() as u64,
            opened_at_unix_ms: observed_at,
        }
    }
}

fn engine_transport(access: &AccessContext) -> EngineTransportMode {
    super::runtime::engine_transport(access.transport_mode())
}

fn run_error(error: RunError) -> OutcomeError {
    outcome_error(error.error_kind(), error.to_string())
}

fn script_check_value(report: ScriptCheckReport) -> Value {
    json!({
        "script": report.script,
        "classification": report.verdict.classification.as_str(),
        "reason": report.verdict.reason,
        "diagnostics": report.diagnostics,
        "calibration": {
            "honored": report.calibration.honored,
            "detail": report.calibration.detail,
            "diagnostics": report.calibration.diagnostics,
            "exit_code": report.calibration.exit_code,
        },
        "argv": report.argv,
        "stdout": report.stdout,
        "stderr": report.stderr,
        "transaction_id": report.transaction_id,
    })
}

fn import_result_value(
    report: ProcessExecutionReport,
    expected_artifacts: &[String],
    produced: Vec<String>,
    missing: Vec<String>,
) -> Value {
    let diagnostics_truncated = report.output.diagnostics_truncated();
    let verdict = import_verdict(
        &report.observed,
        &report.diagnostics,
        expected_artifacts,
        &missing,
        diagnostics_truncated,
    );
    let execution = report.execution();
    let (stdout, stderr) = super::public::streams_value(report.output, &report.excerpt);
    json!({
        "transaction_id": report.transaction_id,
        "execution": execution,
        "classification": verdict.classification.as_str(),
        "reason": verdict.reason,
        "exit_code": report.observed.exit_code,
        "signal": report.observed.signal,
        "timed_out": report.observed.timed_out,
        "diagnostics": report.diagnostics,
        "diagnostics_truncated": diagnostics_truncated,
        "stdout": stdout,
        "stderr": stderr,
        "produced": produced,
        "missing": missing,
        "workspace": report.workspace,
    })
}

/// Add each requested asset's own import state to an import result, and
/// refuse to call an import passed while one of them is not imported from its
/// current bytes.
fn with_asset_states(
    mut value: Value,
    root: &godot_mcp_domain::safe_root::SafeRoot,
    assets: &[String],
) -> Result<Value, OutcomeError> {
    if assets.is_empty() {
        return Ok(value);
    }
    let states = assets
        .iter()
        .map(|asset| crate::authoring::import_status_value(root, asset))
        .collect::<Result<Vec<_>, _>>()?;
    let not_imported: Vec<String> = states
        .iter()
        .filter(|state| state["import"]["state"] != "imported")
        .filter_map(|state| state["path"].as_str().map(str::to_owned))
        .collect();
    if let Value::Object(object) = &mut value {
        if object.get("classification") == Some(&json!("passed")) && !not_imported.is_empty() {
            object.insert("classification".to_owned(), json!("failed"));
            object.insert(
                "reason".to_owned(),
                json!(format!(
                    "the importer ran cleanly but these assets are not imported from their \
                     current bytes: {}",
                    not_imported.join(", ")
                )),
            );
        }
        object.insert("assets".to_owned(), Value::Array(states));
    }
    Ok(value)
}

#[async_trait]
impl KernelToolHandler for EngineHandler {
    async fn call(&self, call: KernelToolCall) -> Result<Value, OutcomeError> {
        if call.cancellation.is_cancelled() {
            return Err(outcome_error(
                ErrorKind::Unavailable,
                "the engine request was cancelled before execution",
            ));
        }
        call.progress
            .report(0.0, Some(1.0), Some("validating the engine request"))
            .await;
        let idempotency_id = call.idempotency_id.clone();
        let cancellation = call.cancellation.clone();
        let canonical_arguments = Value::Object(call.arguments.clone());
        let request = EngineRequest::parse(self.operation, call.arguments)?;
        let process_deadline =
            effective_process_deadline(&request, self.runtime.operation_deadline);
        let project = self.runtime.project_root.display().to_string();
        let result = match request {
            EngineRequest::ImportProject {
                mut expected_artifacts,
                assets,
                deadline_ms: _,
            } => {
                // Each reported asset must leave its `.import` sidecar behind.
                for asset in &assets {
                    let sidecar = format!("{asset}.import");
                    if !expected_artifacts.contains(&sidecar) {
                        expected_artifacts.push(sidecar);
                    }
                }
                let execution = self
                    .runtime
                    .execute_process(
                        ProcessExecution {
                            tool: EngineOperation::ImportProject.tool_name(),
                            arguments: canonical_arguments,
                            argv: vec![
                                "--headless".to_owned(),
                                "--path".to_owned(),
                                project,
                                "--import".to_owned(),
                            ],
                            deadline: process_deadline.expect("import is a process operation"),
                            family: OperationFamily::ProjectMutation,
                            required_capabilities: &["--headless", "--path", "--import"],
                        },
                        &call.access,
                        idempotency_id.as_deref(),
                        call.audit.clone(),
                        cancellation.clone(),
                    )
                    .await?;
                let root = self.runtime.root()?;
                let (produced, missing): (Vec<_>, Vec<_>) = expected_artifacts
                    .iter()
                    .cloned()
                    .partition(|path| root.read(Path::new(project_relative_path(path))).is_ok());
                match execution {
                    ProcessExecutionResult::Executed(report) => {
                        let value =
                            import_result_value(*report, &expected_artifacts, produced, missing);
                        with_asset_states(value, root, &assets)
                    }
                    ProcessExecutionResult::Replayed(value) => Ok(value),
                }
            }
            EngineRequest::RunProject {
                main_scene,
                headless,
                deadline_ms: _,
            } => {
                let mut argv = Vec::with_capacity(5);
                if headless {
                    argv.push("--headless".to_owned());
                }
                argv.extend(["--path".to_owned(), project]);
                if let Some(scene) = main_scene {
                    argv.push(scene);
                }
                self.runtime
                    .execute_process(
                        ProcessExecution {
                            tool: EngineOperation::RunProject.tool_name(),
                            arguments: canonical_arguments,
                            argv,
                            deadline: process_deadline.expect("run is a process operation"),
                            family: OperationFamily::RuntimeExecution,
                            required_capabilities: &["--path"],
                        },
                        &call.access,
                        idempotency_id.as_deref(),
                        call.audit.clone(),
                        cancellation.clone(),
                    )
                    .await
                    .map(ProcessExecutionResult::into_run_value)
            }
            EngineRequest::RunTests {
                selection,
                deadline_ms: _,
            } => {
                let resolved = self.runtime.resolve_engine().await?;
                let transactions = ValidationTransactions {
                    runtime: &self.runtime,
                    access: &call.access,
                    canonical_request: &canonical_arguments,
                    idempotency_id: idempotency_id.as_deref(),
                };
                let runner = EngineRunner::new(
                    self.runtime.journal.as_ref(),
                    resolved.provider.as_ref(),
                    &transactions,
                    resolved.binding,
                );
                let report = run_tests(
                    &runner,
                    &TestRunRequest {
                        selection,
                        deadline: process_deadline.expect("test is a process operation"),
                    },
                )
                .map_err(run_error)?;
                Ok(json!({
                    "classification": report.verdict.classification.as_str(),
                    "reason": report.verdict.reason,
                    "framework": report.framework.map(|framework| framework.as_str()),
                    "summary": report.summary,
                    "diagnostics": report.diagnostics,
                    "detection": report.detection,
                    "argv": report.argv,
                    "stdout": report.stdout,
                    "stderr": report.stderr,
                    "transaction_id": report.transaction_id,
                }))
            }
            EngineRequest::CheckScript {
                script,
                deadline_ms: _,
            } => {
                let request = ScriptCheckRequest {
                    script,
                    deadline: process_deadline.expect("script check is a process operation"),
                };
                let resolved = match self.runtime.resolve_engine_for_operation().await? {
                    EngineResolution::Ready(resolved) => *resolved,
                    EngineResolution::MissingCapability {
                        capability,
                        message: _,
                    } if SCRIPT_CHECK_CAPABILITIES.contains(&capability.as_str()) => {
                        return Ok(script_check_value(unsupported_report(
                            &request,
                            &capability,
                        )));
                    }
                    EngineResolution::MissingCapability { message, .. } => {
                        return Err(outcome_error(ErrorKind::Unsupported, message));
                    }
                };
                let transactions = ValidationTransactions {
                    runtime: &self.runtime,
                    access: &call.access,
                    canonical_request: &canonical_arguments,
                    idempotency_id: idempotency_id.as_deref(),
                };
                let runner = EngineRunner::new(
                    self.runtime.journal.as_ref(),
                    resolved.provider.as_ref(),
                    &transactions,
                    resolved.binding,
                );
                let report = check_script(&runner, &request).map_err(run_error)?;
                Ok(script_check_value(report))
            }
            EngineRequest::Author(author) => {
                self.runtime
                    .execute_authored(
                        author,
                        canonical_arguments,
                        &call.access,
                        idempotency_id.as_deref(),
                        call.audit.clone(),
                        cancellation.clone(),
                    )
                    .await
            }
            EngineRequest::UpdateUid(arguments) => {
                self.runtime
                    .execute_canonical(
                        EngineOperation::UpdateUid.tool_name(),
                        arguments.into_request(CanonicalOperation::RegisterUid),
                        &call.access,
                        idempotency_id.as_deref(),
                        call.audit.clone(),
                        cancellation.clone(),
                    )
                    .await
            }
            EngineRequest::UpdateResource(arguments) => {
                self.runtime
                    .execute_canonical(
                        EngineOperation::UpdateResource.tool_name(),
                        arguments.into_request(CanonicalOperation::UpdateResource),
                        &call.access,
                        idempotency_id.as_deref(),
                        call.audit.clone(),
                        cancellation.clone(),
                    )
                    .await
            }
            EngineRequest::SaveScene(arguments) => {
                self.runtime
                    .execute_canonical(
                        EngineOperation::SaveScene.tool_name(),
                        arguments.into_request(CanonicalOperation::UpdateResource),
                        &call.access,
                        idempotency_id.as_deref(),
                        call.audit.clone(),
                        cancellation,
                    )
                    .await
            }
        };
        call.progress.report(1.0, Some(1.0), None).await;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::{BoundaryMembership, ReleaseStatus, TypedProof};
    use godot_mcp_engine::validation::diagnostic::parse_engine_diagnostics;
    use godot_mcp_engine::validation::outcome::Observed;

    fn operation_deadline(milliseconds: u64) -> OperationDeadline {
        OperationDeadline::new(milliseconds).expect("positive operation deadline")
    }

    /// Every catalog tool that needs a Godot process is served by exactly one
    /// process-backed component: the engine for headless runs, the live
    /// editor for what the connected editor answers, or the managed sessions
    /// for what a game or editor session answers.
    #[test]
    fn engine_bindings_and_the_catalog_agree() {
        use godot_mcp_domain::catalog::{HandlerKey, TOOL_CATALOG, ToolNeeds};

        for tool in TOOL_CATALOG
            .iter()
            .filter(|tool| tool.needs != ToolNeeds::ProjectFiles)
        {
            assert!(
                matches!(
                    tool.handler,
                    Some(HandlerKey::Engine(_) | HandlerKey::Editor(_) | HandlerKey::Session(_))
                ),
                "catalog tool {} needs a Godot process but is served by {:?}",
                tool.name,
                tool.handler
            );
        }
        for &operation in EngineOperation::ALL {
            assert_ne!(
                operation.descriptor().needs,
                ToolNeeds::ProjectFiles,
                "{} is an engine operation, so it needs a Godot process",
                operation.tool_name()
            );
        }
    }

    #[test]
    fn larger_process_deadline_is_clamped_to_scheduler_operation_budget() {
        let request = EngineRequest::RunProject {
            main_scene: None,
            headless: true,
            deadline_ms: 3_000,
        };

        assert_eq!(
            effective_process_deadline(&request, operation_deadline(100)),
            Some(Duration::from_millis(100))
        );
    }

    #[test]
    fn smaller_process_deadline_is_not_expanded_by_scheduler_operation_budget() {
        let request = EngineRequest::RunProject {
            main_scene: None,
            headless: true,
            deadline_ms: 50,
        };

        assert_eq!(
            effective_process_deadline(&request, operation_deadline(100)),
            Some(Duration::from_millis(50))
        );
    }

    #[test]
    fn every_process_operation_uses_the_scheduler_operation_budget() {
        let cases = [
            (
                "project.import.run",
                EngineRequest::ImportProject {
                    expected_artifacts: Vec::new(),
                    assets: Vec::new(),
                    deadline_ms: 3_000,
                },
            ),
            (
                "project.run",
                EngineRequest::RunProject {
                    main_scene: None,
                    headless: true,
                    deadline_ms: 3_000,
                },
            ),
            (
                "project.test",
                EngineRequest::RunTests {
                    selection: Vec::new(),
                    deadline_ms: 3_000,
                },
            ),
        ];

        for (tool, request) in cases {
            assert_eq!(
                effective_process_deadline(&request, operation_deadline(100)),
                Some(Duration::from_millis(100)),
                "{tool} must use the scheduler operation budget"
            );
        }
    }

    #[test]
    fn production_import_handler_has_one_shared_verdict_path() {
        let source = include_str!("handler.rs");
        let shared_call = ["import_", "verdict("].concat();
        let artifact_only = ["if !produced.is_empty() && ", "missing.is_empty()"].concat();

        assert_eq!(
            source.matches(&shared_call).count(),
            1,
            "project.import.run must call the shared import verdict exactly once"
        );
        assert!(
            !source.contains(&artifact_only),
            "project.import.run must not retain an artifact-only verdict path"
        );
    }

    #[test]
    fn import_response_exposes_the_capped_diagnostics_used_by_its_verdict() {
        let diagnostics = parse_engine_diagnostics(concat!(
            "ERROR: Condition \"rl != len\" is true. Returning: ERR_FILE_CORRUPT\n",
            "   at: check_error (drivers/png/png_driver_common.cpp:57)\n"
        ));
        let expected = vec![".godot/uid_cache.bin".to_owned()];
        let report = ProcessExecutionReport {
            transaction_id: "transaction-1".to_owned(),
            proof: TypedProof::ProcessLaunch {
                boundary_identity: "boundary-1".to_owned(),
                pid: Some(42),
                start_time: Some("100".to_owned()),
                boundary_membership: BoundaryMembership::Confirmed,
                release_status: ReleaseStatus::ExitedClean,
                exit_code: Some(0),
                signal: None,
                boundary_empty_after_exit: Some(true),
            },
            observed: Observed {
                exit_code: Some(0),
                signal: None,
                timed_out: false,
            },
            diagnostics: diagnostics.clone(),
            output: crate::effect::ProcessOutputMetadata::default(),
            excerpt: crate::effect::boundary::ProcessOutputExcerpt::default(),
            boundary_kind: "unix_locator",
            boundary_strength: "weak",
            workspace: None,
        };

        let value = import_result_value(report, &expected, expected.clone(), Vec::new());

        assert_eq!(value["classification"], "failed");
        assert_eq!(value["diagnostics"], json!(diagnostics));
        assert_eq!(value["diagnostics_truncated"], false);
        assert_eq!(value["stdout"]["retained_bytes"], 0);
        assert_eq!(value["stdout"]["total_bytes"], 0);
        assert_eq!(value["stdout"]["truncated"], false);
        assert!(
            value["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("parsed engine evidence")),
            "the response must explain that parsed diagnostics drove its verdict: {value}"
        );
    }
}
