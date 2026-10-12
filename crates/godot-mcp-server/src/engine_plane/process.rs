use std::time::Duration;

use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::contracts::OutcomeError;
use godot_mcp_engine::supervision::journal::{
    OperationFamily as EngineOperationFamily, ProjectIdentityClaim, TransactionEnvelope, hex_sha256,
};
use godot_mcp_engine::supervision::protocol::{LaunchRequest, OutputCaps};
use godot_mcp_engine::validation::diagnostic::Diagnostic;
use godot_mcp_engine::validation::outcome::Observed;
use godot_mcp_engine::validation::run::run_verdict;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::public::{execution_value, streams_value};
use super::runtime::{EngineRuntime, engine_transport, journal_transport};
use super::{outcome_error, process_supervisor_binary};
use crate::audit::RequestAudit;
use crate::effect::boundary::ProcessOutputExcerpt;
use crate::effect::{ProcessEffectIntent, ProcessLaunchRequest, ProcessOutputMetadata};
use crate::idempotency::replay::replay_outcome;
use crate::journal::OperationFamily;
use crate::journal::TypedProof;
use crate::kernel::AccessContext;
use crate::router::{RouteEffect, RouteReport, RouteRequest};

pub(super) struct ProcessExecution {
    pub(super) tool: &'static str,
    pub(super) arguments: Value,
    pub(super) argv: Vec<String>,
    pub(super) deadline: Duration,
    pub(super) family: OperationFamily,
    pub(super) required_capabilities: &'static [&'static str],
}

pub(super) enum ProcessExecutionResult {
    Executed(Box<ProcessExecutionReport>),
    Replayed(Value),
}

pub(super) struct ProcessExecutionReport {
    pub(super) transaction_id: String,
    pub(super) proof: TypedProof,
    pub(super) observed: Observed,
    pub(super) diagnostics: Vec<Diagnostic>,
    pub(super) output: ProcessOutputMetadata,
    pub(super) excerpt: ProcessOutputExcerpt,
    pub(super) boundary_kind: &'static str,
    pub(super) boundary_strength: &'static str,
    /// The workspace snapshot the route's read-your-writes barrier published,
    /// when the run's effect changed project state.
    pub(super) workspace: Option<Value>,
}

impl ProcessExecutionReport {
    /// The redacted execution proof for this report.
    pub(super) fn execution(&self) -> Value {
        execution_value(&self.proof, self.boundary_kind, self.boundary_strength)
    }
}

impl ProcessExecutionResult {
    /// The public result of one project run.
    ///
    /// A run that started and ended is a completed call even when the engine
    /// failed: the domain verdict travels in `classification`, beside the exit
    /// facts, the parsed engine diagnostics and bounded excerpts of both
    /// streams. The private boundary identity, PID and start time never appear.
    pub(super) fn into_run_value(self) -> Value {
        match self {
            Self::Executed(report) => {
                let verdict = run_verdict(
                    &report.observed,
                    &report.diagnostics,
                    &report.excerpt.stdout.text,
                );
                let (stdout, stderr) = streams_value(report.output, &report.excerpt);
                json!({
                    "transaction_id": report.transaction_id,
                    "classification": verdict.classification.as_str(),
                    "reason": verdict.reason,
                    "exit_code": report.observed.exit_code,
                    "signal": report.observed.signal,
                    "timed_out": report.observed.timed_out,
                    "diagnostics": report.diagnostics,
                    "diagnostics_truncated": report.output.diagnostics_truncated(),
                    "stdout": stdout,
                    "stderr": stderr,
                    "execution": report.execution(),
                    "workspace": report.workspace,
                })
            }
            Self::Replayed(value) => value,
        }
    }
}

impl EngineRuntime {
    pub(super) async fn execute_process(
        &self,
        execution: ProcessExecution,
        access: &AccessContext,
        idempotency_id: Option<&str>,
        audit: RequestAudit,
        cancellation: CancellationToken,
    ) -> Result<ProcessExecutionResult, OutcomeError> {
        if let Some(idempotency_id) = idempotency_id {
            let replay_request = RouteRequest {
                tool: execution.tool.to_owned(),
                idempotency_id: idempotency_id.to_owned(),
                transaction_id: None,
                path: String::new(),
                bytes: Vec::new(),
                arguments: execution.arguments.clone(),
                observation: self.route_observation(),
                cancellation: cancellation.clone(),
                queued_at_unix_ms: access.observed_at_unix_ms(),
                identity: Some(Self::route_identity(access)),
                effect: None,
                audit: Some(audit.clone()),
            };
            if let Some(report) = self
                .router
                .recorded_replay(&replay_request)
                .map_err(|refusal| refusal.to_outcome())?
                && let Some(result) = recorded_replay_result(&report)?
            {
                return Ok(ProcessExecutionResult::Replayed(result));
            }
        }
        let resolved = self.resolve_engine().await?;
        if let Some(missing) = execution
            .required_capabilities
            .iter()
            .find(|flag| !resolved.binding.capabilities.contains(**flag))
        {
            return Err(outcome_error(
                ErrorKind::Unsupported,
                format!("configured Godot build does not advertise {missing}"),
            ));
        }

        let outer_id = self
            .router
            .next_transaction_id(execution.tool, access.observed_at_unix_ms());
        let inner_id = self
            .router
            .next_transaction_id("supervised-launch", access.observed_at_unix_ms());
        let principal = access.mode_principal().to_journal_string();
        let session = Some(access.connection_session().to_owned());
        let effect_id = idempotency_id
            .map(str::to_owned)
            .unwrap_or_else(|| format!("engine-{outer_id}"));
        let intent = ProcessEffectIntent {
            transaction_id: outer_id.clone(),
            tool_name: execution.tool.to_owned(),
            operation_family: execution.family,
            idempotency_id: effect_id.clone(),
            transport_mode: journal_transport(access.transport_mode()),
            principal: Some(principal.clone()),
            session: session.clone(),
            arguments: execution.arguments.clone(),
            required_process_transaction: inner_id.clone(),
            opened_at_unix_ms: access.observed_at_unix_ms(),
        };
        let canonical_bytes = serde_json::to_vec(&execution.arguments)
            .map_err(|error| outcome_error(ErrorKind::Internal, error.to_string()))?;
        let environment = resolved.binding.environment.clone();
        let request = ProcessLaunchRequest {
            launch: LaunchRequest {
                envelope: TransactionEnvelope {
                    transaction_id: inner_id,
                    project: ProjectIdentityClaim {
                        object_identity: self.held.object_identity.clone(),
                        root_object: self.held.root_object.clone(),
                        file_object: self.held.file_object.clone(),
                        generation: self.held.generation,
                    },
                    transport_mode: engine_transport(access.transport_mode()),
                    principal: Some(principal),
                    session,
                    operation_family: engine_family(execution.family),
                    tool_name: execution.tool.to_owned(),
                    idempotency_id: format!("launch-{effect_id}"),
                    canonical_request_hash: hex_sha256(&canonical_bytes),
                    canonical_request_bytes: canonical_bytes.len() as u64,
                    opened_at_unix_ms: access.observed_at_unix_ms(),
                },
                trust: resolved.binding.trust,
                program: resolved.binding.program,
                arguments: execution.argv,
                binary_sha256: resolved.binding.binary_sha256,
                supervisor_binary: process_supervisor_binary(),
                project_root_display: self.project_root.clone(),
                environment,
                caps: OutputCaps {
                    stdout_bytes: 1_048_576,
                    stderr_bytes: 1_048_576,
                },
                deadline: execution.deadline,
                work_dir: self.state_dir.join("engine-launches"),
                evidence_path: None,
            },
        };
        let route_request = RouteRequest {
            tool: execution.tool.to_owned(),
            idempotency_id: intent.idempotency_id.clone(),
            transaction_id: Some(outer_id.clone()),
            path: String::new(),
            bytes: Vec::new(),
            arguments: execution.arguments,
            observation: self.route_observation(),
            cancellation,
            queued_at_unix_ms: access.observed_at_unix_ms(),
            identity: Some(Self::route_identity(access)),
            effect: Some(RouteEffect::Process {
                intent,
                request: Box::new(request),
            }),
            audit: Some(audit),
        };
        let report = self
            .router
            .route(&route_request)
            .await
            .map_err(|refusal| refusal.to_outcome())?;
        if let Some(result) = recorded_replay_result(&report)? {
            return Ok(ProcessExecutionResult::Replayed(result));
        }
        let outcome = report.process_outcome().cloned().ok_or_else(|| {
            outcome_error(ErrorKind::Internal, "process route returned no outcome")
        })?;
        Ok(ProcessExecutionResult::Executed(Box::new(
            ProcessExecutionReport {
                transaction_id: outer_id,
                proof: outcome.proof,
                observed: outcome.observed,
                diagnostics: outcome.diagnostics,
                output: outcome.output,
                excerpt: outcome.excerpt,
                boundary_kind: outcome.boundary_kind,
                boundary_strength: outcome.boundary_strength,
                workspace: report.workspace().cloned(),
            },
        )))
    }
}

fn recorded_replay_result(report: &RouteReport) -> Result<Option<Value>, OutcomeError> {
    if !report.replayed() {
        return Ok(None);
    }
    let status = report.recorded_status().ok_or_else(|| {
        outcome_error(
            ErrorKind::Internal,
            "replayed process route returned no recorded status",
        )
    })?;
    let transaction_id = report.transaction_id().ok_or_else(|| {
        outcome_error(
            ErrorKind::Internal,
            "replayed process route returned no transaction identifier",
        )
    })?;
    let verdict = report.recorded_verdict().ok_or_else(|| {
        outcome_error(
            ErrorKind::Unavailable,
            "replayed process route returned no typed terminal outcome",
        )
    })?;
    replay_outcome(transaction_id, status, report.recorded_result(), verdict).map(Some)
}

const fn engine_family(family: OperationFamily) -> EngineOperationFamily {
    match family {
        OperationFamily::ProjectMutation => EngineOperationFamily::ProjectMutation,
        OperationFamily::EditorMutation => EngineOperationFamily::EditorMutation,
        OperationFamily::RuntimeExecution => EngineOperationFamily::RuntimeExecution,
        OperationFamily::DestructiveExport => EngineOperationFamily::DestructiveExport,
        OperationFamily::DiagnosticProbe => EngineOperationFamily::DiagnosticProbe,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::{BoundaryMembership, ReleaseStatus};

    fn report(output: ProcessOutputMetadata) -> ProcessExecutionReport {
        ProcessExecutionReport {
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
            diagnostics: Vec::new(),
            output,
            excerpt: ProcessOutputExcerpt::default(),
            boundary_kind: "unix_locator",
            boundary_strength: "weak",
            workspace: None,
        }
    }

    #[test]
    fn noisy_process_response_discloses_truncation() {
        let value = ProcessExecutionResult::Executed(Box::new(report(ProcessOutputMetadata {
            stdout: crate::effect::boundary::ProcessStreamOutputMetadata {
                retained_bytes: 1_048_576,
                total_bytes: 2_190_073,
                truncated: true,
            },
            stderr: crate::effect::boundary::ProcessStreamOutputMetadata::default(),
        })))
        .into_run_value();

        assert_eq!(
            value["diagnostics_truncated"],
            json!(true),
            "a noisy process response must disclose known output truncation: {value}"
        );
        assert_eq!(value["stdout"]["retained_bytes"], json!(1_048_576));
        assert_eq!(value["stdout"]["total_bytes"], json!(2_190_073));
        assert_eq!(value["stdout"]["truncated"], json!(true));
    }

    #[test]
    fn quiet_process_response_discloses_complete_output() {
        let value = ProcessExecutionResult::Executed(Box::new(report(ProcessOutputMetadata {
            stdout: crate::effect::boundary::ProcessStreamOutputMetadata {
                retained_bytes: 73,
                total_bytes: 73,
                truncated: false,
            },
            stderr: crate::effect::boundary::ProcessStreamOutputMetadata::default(),
        })))
        .into_run_value();

        assert_eq!(
            value["diagnostics_truncated"],
            json!(false),
            "a quiet process response must explicitly report that output was not truncated: {value}"
        );
        assert_eq!(value["stdout"]["retained_bytes"], json!(73));
        assert_eq!(value["stdout"]["total_bytes"], json!(73));
        assert_eq!(value["stdout"]["truncated"], json!(false));
    }
}
