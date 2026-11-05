//! The Android device tools: `android.devices.list` and `android.deploy`.
//!
//! Both use exactly the operator's configured Android SDK; the server never
//! downloads or installs one. Listing reads `adb devices -l`. A deploy is an
//! outward effect journaled like any other: the journaled command names the
//! export and the device, and the process boundary installs that export's
//! APK, launches it and proves it runs, phase by phase.

use std::collections::BTreeMap;
use std::time::Duration;

use godot_mcp_bridge::protocol::mutation::MutationStatus;
use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::contracts::{GodotValue, OutcomeError};
use serde_json::{Map, Value};

use super::{KernelToolCall, SessionHandler, SessionKind, integer, invalid, outcome_error};
use super::{export::not_this_callers, required_string};
use crate::effect::boundary::{CancelSignal, EditorMutation};
use crate::effect::gateway::EditorEffectIntent;
use crate::engine_plane::EngineRuntime;
use crate::engine_plane::runtime::journal_transport;
use crate::engine_plane::session::android::{DeployRequest, list_devices, valid_serial};
use crate::journal::OperationFamily;
use crate::router::{RouteEffect, RouteReport, RouteRequest};

/// The default and the longest wait for the deployed app's process.
pub(super) const DEFAULT_RUNNING_TIMEOUT_MS: u64 = 15_000;
pub(super) const MIN_RUNNING_TIMEOUT_MS: u64 = 1_000;
pub(super) const MAX_RUNNING_TIMEOUT_MS: u64 = 60_000;
/// The default and the most logcat lines a deploy reads.
pub(super) const DEFAULT_LOGCAT_LINES: u64 = 200;
pub(super) const MAX_LOGCAT_LINES: u64 = 1_000;
/// How long a deploy's phases may take together: listing, install, launch,
/// the running proof and logcat, each with its own deadline.
const DEPLOY_TIMEOUT: Duration = Duration::from_secs(60 + 600 + 120 + 60 + 60);

/// One JSON value as a typed Godot value, for error details.
fn godot_value(value: &Value) -> GodotValue {
    match value {
        Value::Null => GodotValue::Null,
        Value::Bool(flag) => GodotValue::Bool(*flag),
        Value::Number(number) => number.as_i64().map_or_else(
            || GodotValue::Float(number.as_f64().unwrap_or_default()),
            GodotValue::Integer,
        ),
        Value::String(text) => GodotValue::String(text.clone()),
        Value::Array(items) => GodotValue::Array(items.iter().map(godot_value).collect()),
        Value::Object(entries) => GodotValue::Dictionary(
            entries
                .iter()
                .map(|(key, value)| (key.clone(), godot_value(value)))
                .collect(),
        ),
    }
}

/// The answer of one routed deploy: its report when every phase succeeded,
/// or the error naming the phase that did not, with every phase's report in
/// its details.
fn deploy_answer(report: &RouteReport) -> Result<Value, OutcomeError> {
    if report.replayed() {
        return crate::editor::tools::editor_answer(report);
    }
    let outcome = report
        .editor_outcome()
        .ok_or_else(|| outcome_error(ErrorKind::Internal, "the deploy returned no answer"))?;
    let transaction_id = report.transaction_id().unwrap_or_default().to_owned();
    if outcome.status == MutationStatus::Committed && outcome.error.is_none() {
        let mut answer = match &outcome.payload {
            Value::Object(object) => object.clone(),
            _ => Map::new(),
        };
        answer.insert("transaction_id".to_owned(), Value::String(transaction_id));
        answer.insert(
            "mutation_status".to_owned(),
            Value::String(outcome.status.as_str().to_owned()),
        );
        answer.insert("replayed".to_owned(), Value::Bool(false));
        return Ok(Value::Object(answer));
    }
    let (kind, message) = outcome.error.clone().unwrap_or((
        ErrorKind::Internal,
        "the deploy reported no outcome".to_owned(),
    ));
    let kind = if outcome.status == MutationStatus::Unresolved {
        ErrorKind::Uncertain
    } else {
        kind
    };
    let mut details = BTreeMap::new();
    details.insert(
        "transaction_id".to_owned(),
        GodotValue::String(transaction_id),
    );
    details.insert(
        "mutation_status".to_owned(),
        GodotValue::String(outcome.status.as_str().to_owned()),
    );
    if let Value::Object(payload) = &outcome.payload {
        for key in [
            "export",
            "serial",
            "package",
            "component",
            "installed",
            "pid",
            "failed_phase",
            "phases",
            "logcat",
        ] {
            if let Some(value) = payload.get(key) {
                details.insert(key.to_owned(), godot_value(value));
            }
        }
    }
    Err(OutcomeError::new(kind, message, false, details))
}

impl SessionHandler {
    /// `android.devices.list`.
    pub(super) async fn android_devices(&self) -> Result<Value, OutcomeError> {
        let tools = self.sessions().android().clone();
        let dirs = self.sessions().android_dirs().clone();
        tokio::task::spawn_blocking(move || list_devices(&tools, &dirs))
            .await
            .map_err(|error| outcome_error(ErrorKind::Internal, error.to_string()))?
    }

    /// `android.deploy`.
    pub(super) async fn android_deploy(
        &self,
        call: &KernelToolCall,
    ) -> Result<Value, OutcomeError> {
        let tool = self.operation.tool_name();
        let idempotency_id = self.idempotency(call)?;
        let arguments = &call.arguments;
        let export = required_string(arguments, "export")?.to_owned();
        let serial = required_string(arguments, "serial")?.to_owned();
        if !valid_serial(&serial) {
            return Err(invalid(
                "`serial` must be one device serial as adb devices -l lists it",
            ));
        }
        let running_timeout = integer(
            arguments,
            "runningTimeoutMs",
            MIN_RUNNING_TIMEOUT_MS,
            MAX_RUNNING_TIMEOUT_MS,
        )?
        .unwrap_or(DEFAULT_RUNNING_TIMEOUT_MS);
        let logcat_lines =
            integer(arguments, "logcatLines", 1, MAX_LOGCAT_LINES)?.unwrap_or(DEFAULT_LOGCAT_LINES);

        // Everything that can refuse does so before anything is journaled or
        // sent to a device.
        self.sessions().android().adb()?;
        let session = self.sessions().session(&export)?;
        if !session.belongs_to(call.access.principal().as_str()) {
            return Err(not_this_callers(&export));
        }
        if session.kind() != SessionKind::Export {
            return Err(invalid(format!(
                "{export} is a {} session, not an export",
                session.kind().as_str()
            )));
        }
        self.sessions().deployable(&export)?;
        let request = DeployRequest {
            export: export.clone(),
            serial,
            running_timeout: Duration::from_millis(running_timeout),
            logcat_lines: u32::try_from(logcat_lines).unwrap_or(u32::MAX),
        };

        let engine = self.engine.as_ref();
        let access = &call.access;
        let observed = access.observed_at_unix_ms();
        let transaction_id = engine.router.next_transaction_id(tool, observed);
        let canonical = Value::Object(arguments.clone());
        let intent = EditorEffectIntent {
            transaction_id: transaction_id.clone(),
            tool_name: tool.to_owned(),
            operation_family: OperationFamily::RuntimeExecution,
            idempotency_id: idempotency_id.clone(),
            transport_mode: journal_transport(access.transport_mode()),
            principal: Some(access.mode_principal().to_journal_string()),
            session: Some(access.connection_session().to_owned()),
            arguments: canonical.clone(),
            files: Vec::new(),
            project_wide: false,
            observe_all: false,
            expected_revision: None,
            opened_at_unix_ms: observed,
        };
        let command = Box::new(EditorMutation {
            action: tool.to_owned(),
            payload: request.payload().to_string(),
            idempotency_id: idempotency_id.clone(),
            timeout: DEPLOY_TIMEOUT,
            cancellation: CancelSignal::of(&call.cancellation),
        });
        let route = RouteRequest {
            tool: tool.to_owned(),
            idempotency_id,
            transaction_id: Some(transaction_id),
            path: String::new(),
            bytes: Vec::new(),
            arguments: canonical,
            observation: self.observation(&export),
            cancellation: call.cancellation.clone(),
            queued_at_unix_ms: observed,
            identity: Some(EngineRuntime::route_identity(access)),
            effect: Some(RouteEffect::Device { intent, command }),
            audit: Some(call.audit.clone()),
        };
        let report = engine
            .router
            .route(&route)
            .await
            .map_err(|refusal| refusal.to_outcome())?;
        deploy_answer(&report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_json_report_becomes_typed_details() {
        let value = json!({
            "phase": "install",
            "exit": null,
            "ok": false,
            "elapsed_ms": 12,
            "ratio": 0.5,
            "lines": ["a", "b"],
        });
        let GodotValue::Dictionary(entries) = godot_value(&value) else {
            panic!("an object becomes a dictionary");
        };
        assert_eq!(entries["phase"], GodotValue::String("install".to_owned()));
        assert_eq!(entries["exit"], GodotValue::Null);
        assert_eq!(entries["ok"], GodotValue::Bool(false));
        assert_eq!(entries["elapsed_ms"], GodotValue::Integer(12));
        assert_eq!(entries["ratio"], GodotValue::Float(0.5));
        assert_eq!(
            entries["lines"],
            GodotValue::Array(vec![
                GodotValue::String("a".to_owned()),
                GodotValue::String("b".to_owned())
            ])
        );
    }
}
