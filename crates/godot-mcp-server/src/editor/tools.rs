//! The live-editor tools.
//!
//! Reads (`editor.status.get`, `editor.scene.tree.get`, `editor.types.get`)
//! ask the connected plugin directly; they change nothing in the editor.
//! Mutations (`editor.scene.open`, `editor.selection.set`, `editor.undo`,
//! `editor.redo`, `editor.addon.reload`) are journaled `editor_runtime`
//! transactions routed through the outcome router: idempotent replay, the
//! write block, effect-lane admission, a policy permit re-checked at the editor
//! boundary, and a terminal record derived from the plugin's three-state
//! answer. An answer that never came back is reported as unresolved and never
//! sent again.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use godot_mcp_bridge::protocol::envelope::OperationClass;
use godot_mcp_bridge::protocol::mutation::MutationStatus;
use godot_mcp_domain::ErrorKind;
pub(crate) use godot_mcp_domain::catalog::EditorOperation;
use godot_mcp_domain::contracts::{GodotValue, OutcomeError};
use godot_mcp_domain::manifests::canonical_manifest;
use godot_mcp_domain::safe_root::SafeRoot;
use serde_json::{Map, Value, json};

use super::bridge::{BridgeAnswer, BridgeFailure, EditorBridge};
use crate::effect::WritePrecondition;
use crate::effect::boundary::EditorMutation;
use crate::effect::gateway::EditorEffectIntent;
use crate::idempotency::TransportAuthMode;
use crate::idempotency::replay::replay_outcome;
use crate::journal::{OperationFamily, TransportMode};
use crate::kernel::{AccessContext, KernelToolCall, KernelToolHandler, ServiceKernel};
use crate::router::{
    EnginePresence, OutcomeRouter, PlaneObservation, ProtocolFidelity, RouteEffect, RouteReport,
    RouteRequest, RouterIdentity, RuntimePresence, WorkspaceFreshness,
};
use crate::workspace::WorkspaceCoordinator;

/// How long an editor read or mutation may take.
const EDITOR_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest node path or class name accepted.
const MAX_NAME_BYTES: usize = 1024;
/// Most nodes one selection names.
const MAX_SELECTION: usize = 256;

fn outcome_error(kind: ErrorKind, message: impl Into<String>) -> OutcomeError {
    OutcomeError::new(kind, message, false, BTreeMap::new())
}

/// The refusal an editor tool answers while no editor is connected.
fn not_connected() -> OutcomeError {
    let mut details = BTreeMap::new();
    details.insert(
        crate::scheduler::PLANE_DETAIL_KEY.to_owned(),
        GodotValue::String("editor".to_owned()),
    );
    OutcomeError::new(
        ErrorKind::Unavailable,
        "no Godot editor running the godot_mcp plugin is connected; install it with \
         editor.addon.install, enable it, and open the project in the editor",
        true,
        details,
    )
}

/// What each live-editor operation asks of the bridge, beside the catalog
/// descriptor its operation serves.
trait EditorCall: Copy {
    fn bridge_method(self) -> &'static str;
    fn is_read(self) -> bool;
    fn input_schema(self) -> Value;
}

impl EditorCall for EditorOperation {
    fn bridge_method(self) -> &'static str {
        match self {
            Self::StatusGet => "editor.state",
            Self::SceneTreeGet => "editor.scene.tree",
            Self::TypesGet => "editor.types.class",
            Self::SceneOpen => "editor.scene.open",
            Self::SelectionSet => "editor.selection.set",
            Self::Undo => "editor.history.undo",
            Self::Redo => "editor.history.redo",
            Self::AddonReload => "editor.addon.reload",
        }
    }

    fn is_read(self) -> bool {
        matches!(self, Self::StatusGet | Self::SceneTreeGet | Self::TypesGet)
    }

    fn input_schema(self) -> Value {
        let scene = json!({
            "type": "string",
            "minLength": 1,
            "maxLength": 1024,
            "description": "An open text scene, project-relative or res://; the scene being \
                            edited when omitted."
        });
        let (properties, required): (Value, &[&str]) = match self {
            Self::StatusGet | Self::AddonReload => (json!({}), &[]),
            Self::SceneTreeGet => (json!({ "scene": scene }), &[]),
            Self::TypesGet => (
                json!({
                    "class": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 256,
                        "description": "Engine class name, e.g. CharacterBody2D."
                    }
                }),
                &["class"],
            ),
            Self::SceneOpen => (
                json!({
                    "path": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 1024,
                        "description": "Project-relative or res:// path of a text scene."
                    },
                    "reload": {
                        "type": "boolean",
                        "description": "Reload an already open scene from its file; refused \
                                        while the editor holds changes the file does not."
                    }
                }),
                &["path"],
            ),
            Self::SelectionSet => (
                json!({
                    "scene": scene,
                    "nodes": {
                        "type": "array",
                        "maxItems": MAX_SELECTION,
                        "items": { "type": "string", "minLength": 1, "maxLength": MAX_NAME_BYTES },
                        "description": "Node paths relative to the scene root (`.` is the root); \
                                        empty clears the selection."
                    }
                }),
                &["nodes"],
            ),
            Self::Undo | Self::Redo => (
                json!({
                    "scene": scene,
                    "save": {
                        "type": "boolean",
                        "description": "Also save the scene afterwards (default false)."
                    }
                }),
                &[],
            ),
        };
        let mut required: Vec<&str> = required.to_vec();
        if !self.is_read() {
            // The handler refuses a change without one before the bridge is asked.
            required.push("idempotencyId");
        }
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": properties,
            "required": required,
        })
    }
}

/// The input schema of one live-editor tool.
pub(crate) fn input_schema(operation: EditorOperation) -> Map<String, Value> {
    operation
        .input_schema()
        .as_object()
        .cloned()
        .expect("editor schemas are objects")
}

/// The runtime behind every live-editor tool.
pub(crate) struct EditorToolRuntime {
    pub(crate) bridge: Arc<EditorBridge>,
    pub(crate) router: Arc<OutcomeRouter>,
    pub(crate) workspace: Arc<WorkspaceCoordinator>,
    pub(crate) root: SafeRoot,
}

/// Register every live-editor tool the kernel's profile lists.
pub(crate) fn register_editor_tools(
    kernel: &mut ServiceKernel,
    runtime: Arc<EditorToolRuntime>,
) -> Result<(), OutcomeError> {
    let manifest = canonical_manifest(kernel.profile());
    for &operation in EditorOperation::ALL {
        if manifest.tools.contains(operation.tool_name()) {
            kernel.register_tool_handler(
                operation.key(),
                Arc::new(EditorHandler {
                    operation,
                    runtime: Arc::clone(&runtime),
                }),
            )?;
        }
    }
    Ok(())
}

struct EditorHandler {
    operation: EditorOperation,
    runtime: Arc<EditorToolRuntime>,
}

fn res_path(raw: &str) -> Result<String, OutcomeError> {
    let relative = raw.strip_prefix("res://").unwrap_or(raw);
    if relative.is_empty()
        || relative.starts_with('/')
        || relative.contains('\\')
        || relative
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(outcome_error(
            ErrorKind::InvalidInput,
            format!("`{raw}` must be a project-relative path"),
        ));
    }
    Ok(format!("res://{relative}"))
}

fn scene_path(raw: &str) -> Result<String, OutcomeError> {
    let path = res_path(raw)?;
    if !path.ends_with(".tscn") {
        return Err(outcome_error(
            ErrorKind::InvalidInput,
            format!("`{raw}` must name a .tscn text scene"),
        ));
    }
    Ok(path)
}

fn optional_scene(arguments: &Map<String, Value>) -> Result<Option<String>, OutcomeError> {
    match arguments.get("scene") {
        None => Ok(None),
        Some(Value::String(raw)) => scene_path(raw).map(Some),
        Some(_) => Err(outcome_error(
            ErrorKind::InvalidInput,
            "`scene` must be a string",
        )),
    }
}

impl EditorToolRuntime {
    async fn ask(&self, method: &str, payload: Value) -> Result<Value, OutcomeError> {
        let bridge = Arc::clone(&self.bridge);
        let method = method.to_owned();
        let answer = tokio::task::spawn_blocking(move || {
            bridge.call(
                &method,
                OperationClass::Read,
                &payload,
                "editor-read",
                EDITOR_TIMEOUT,
            )
        })
        .await
        .map_err(|error| outcome_error(ErrorKind::Internal, error.to_string()))?;
        match answer {
            Ok(BridgeAnswer::Result { payload, .. }) => Ok(payload),
            Ok(BridgeAnswer::Error { kind, message, .. }) => Err(outcome_error(kind, message)),
            Err(BridgeFailure::NotConnected) => Err(not_connected()),
            Err(failure @ BridgeFailure::Unsupported(_)) => {
                Err(outcome_error(ErrorKind::Unsupported, failure.message()))
            }
            Err(failure) => Err(outcome_error(ErrorKind::Unavailable, failure.message())),
        }
    }

    fn observation(&self) -> PlaneObservation {
        PlaneObservation::new(
            self.router.plane_availability(),
            EnginePresence::EditorConnected,
            RuntimePresence::NoInstance,
            ProtocolFidelity::Full,
            if self.workspace.current().coherence().is_coherent() {
                WorkspaceFreshness::Fresh
            } else {
                WorkspaceFreshness::StaleIndex
            },
        )
    }

    /// Journal and send one live-editor mutation through the outcome router.
    #[allow(clippy::too_many_arguments)]
    async fn mutate(
        &self,
        tool: &str,
        method: &str,
        payload: Value,
        files: Vec<(String, WritePrecondition)>,
        arguments: &Value,
        call: &KernelToolCall,
        idempotency_id: &str,
    ) -> Result<Value, OutcomeError> {
        let access = &call.access;
        let transaction_id = self
            .router
            .next_transaction_id(tool, access.observed_at_unix_ms());
        let mut payload = payload;
        if let Value::Object(object) = &mut payload {
            object.insert(
                "transaction_id".to_owned(),
                Value::String(transaction_id.clone()),
            );
        }
        let intent = EditorEffectIntent {
            transaction_id: transaction_id.clone(),
            tool_name: tool.to_owned(),
            operation_family: OperationFamily::EditorMutation,
            idempotency_id: idempotency_id.to_owned(),
            transport_mode: journal_transport(access.transport_mode()),
            principal: Some(access.mode_principal().to_journal_string()),
            session: Some(access.connection_session().to_owned()),
            arguments: arguments.clone(),
            files,
            opened_at_unix_ms: access.observed_at_unix_ms(),
        };
        let request = RouteRequest {
            tool: tool.to_owned(),
            idempotency_id: idempotency_id.to_owned(),
            transaction_id: Some(transaction_id),
            path: String::new(),
            bytes: Vec::new(),
            arguments: arguments.clone(),
            observation: self.observation(),
            cancellation: call.cancellation.clone(),
            queued_at_unix_ms: access.observed_at_unix_ms(),
            identity: Some(route_identity(access)),
            effect: Some(RouteEffect::Editor {
                intent,
                mutation: Box::new(EditorMutation {
                    action: method.to_owned(),
                    payload: payload.to_string(),
                    idempotency_id: idempotency_id.to_owned(),
                    timeout: EDITOR_TIMEOUT,
                    cancellation: crate::effect::boundary::CancelSignal::default(),
                }),
            }),
            audit: Some(call.audit.clone()),
        };
        let report = self
            .router
            .route(&request)
            .await
            .map_err(|refusal| refusal.to_outcome())?;
        editor_answer(&report)
    }

    /// The edited scene's path, asked from the editor.
    async fn edited_scene(&self) -> Result<String, OutcomeError> {
        let state = self.ask("editor.state", json!({})).await?;
        state
            .get("edited_scene")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| outcome_error(ErrorKind::NotFound, "no scene is being edited"))
    }

    fn disk_precondition(&self, res_path: &str) -> Result<WritePrecondition, OutcomeError> {
        let relative = res_path.strip_prefix("res://").unwrap_or(res_path);
        match self.root.read(std::path::Path::new(relative)) {
            Ok(bytes) => Ok(WritePrecondition::Sha256(
                godot_mcp_engine::supervision::journal::hex_sha256(&bytes),
            )),
            Err(_) => Ok(WritePrecondition::Absent),
        }
    }
}

fn route_identity(access: &AccessContext) -> RouterIdentity {
    RouterIdentity {
        transport_mode: access.transport_mode(),
        principal: access.mode_principal().to_journal_string(),
        session: access.connection_session().to_owned(),
    }
}

const fn journal_transport(mode: TransportAuthMode) -> TransportMode {
    match mode {
        TransportAuthMode::Stdio => TransportMode::Stdio,
        TransportAuthMode::TrustedLocal => TransportMode::TrustedLocal,
        TransportAuthMode::ProxiedOidc => TransportMode::ProxiedOidc,
    }
}

/// The tool result for one routed editor mutation.
pub(crate) fn editor_answer(report: &RouteReport) -> Result<Value, OutcomeError> {
    if report.replayed() {
        let status = report.recorded_status().ok_or_else(|| {
            outcome_error(ErrorKind::Internal, "a replay carried no recorded status")
        })?;
        let transaction_id = report
            .transaction_id()
            .ok_or_else(|| outcome_error(ErrorKind::Internal, "a replay carried no transaction"))?;
        let verdict = report.recorded_verdict().ok_or_else(|| {
            outcome_error(
                ErrorKind::Unavailable,
                "a replay carried no terminal outcome",
            )
        })?;
        let mut value = replay_outcome(transaction_id, status, report.recorded_result(), verdict)?;
        if let Value::Object(object) = &mut value {
            object.insert("replayed".to_owned(), Value::Bool(true));
        }
        return Ok(value);
    }
    let outcome = report
        .editor_outcome()
        .ok_or_else(|| outcome_error(ErrorKind::Internal, "the editor route returned no answer"))?;
    let transaction_id = report.transaction_id().unwrap_or_default().to_owned();
    let mut details = BTreeMap::new();
    details.insert(
        "transaction_id".to_owned(),
        GodotValue::String(transaction_id.clone()),
    );
    details.insert(
        "mutation_status".to_owned(),
        GodotValue::String(outcome.status.as_str().to_owned()),
    );
    match (outcome.status, &outcome.error) {
        (MutationStatus::Committed, None) => {
            let mut result = match &outcome.payload {
                Value::Object(object) => object.clone(),
                _ => Map::new(),
            };
            result.remove("transaction_id");
            result.insert("transaction_id".to_owned(), Value::String(transaction_id));
            result.insert(
                "mutation_status".to_owned(),
                Value::String(outcome.status.as_str().to_owned()),
            );
            result.insert("replayed".to_owned(), Value::Bool(false));
            result.insert(
                "workspace".to_owned(),
                report.workspace().cloned().unwrap_or(Value::Null),
            );
            Ok(Value::Object(result))
        }
        (MutationStatus::Unresolved, error) => Err(OutcomeError::new(
            ErrorKind::Uncertain,
            format!(
                "whether the editor made the change is not decidable ({}); it is recorded as \
                 unresolved and will not be sent again",
                error
                    .as_ref()
                    .map_or("no answer", |(_, message)| message.as_str())
            ),
            false,
            details,
        )),
        (_, error) => {
            let (kind, message) = error.clone().unwrap_or((
                ErrorKind::Internal,
                "the editor reported no outcome".to_owned(),
            ));
            Err(OutcomeError::new(kind, message, false, details))
        }
    }
}

#[async_trait]
impl KernelToolHandler for EditorHandler {
    async fn call(&self, call: KernelToolCall) -> Result<Value, OutcomeError> {
        if call.cancellation.is_cancelled() {
            return Err(outcome_error(
                ErrorKind::Cancelled,
                "the request was cancelled before it started",
            ));
        }
        let runtime = self.runtime.as_ref();
        let arguments = &call.arguments;
        let operation = self.operation;
        if operation.is_read() {
            return match operation {
                EditorOperation::StatusGet => {
                    let status = runtime.bridge.status();
                    let mut answer = status.to_value();
                    if status.connected {
                        let state = runtime.ask("editor.state", json!({})).await?;
                        answer["editor"] = state;
                    } else {
                        answer["editor"] = Value::Null;
                    }
                    Ok(answer)
                }
                EditorOperation::SceneTreeGet => {
                    let scene = optional_scene(arguments)?;
                    runtime
                        .ask(
                            operation.bridge_method(),
                            json!({ "scene": scene.unwrap_or_default() }),
                        )
                        .await
                }
                _ => {
                    let class = arguments
                        .get("class")
                        .and_then(Value::as_str)
                        .filter(|class| !class.is_empty() && class.len() <= 256)
                        .ok_or_else(|| {
                            outcome_error(ErrorKind::InvalidInput, "`class` names an engine class")
                        })?;
                    runtime
                        .ask(operation.bridge_method(), json!({ "class": class }))
                        .await
                }
            };
        }
        let tool = operation.tool_name();
        let idempotency_id = call.idempotency_id.clone().ok_or_else(|| {
            outcome_error(
                ErrorKind::InvalidInput,
                format!("{tool} requires `idempotencyId`, so a retried call can never apply twice"),
            )
        })?;
        if !runtime.bridge.status().connected {
            return Err(not_connected());
        }
        let canonical = Value::Object(arguments.clone());
        let (payload, files) = match operation {
            EditorOperation::SceneOpen => {
                let path = scene_path(arguments.get("path").and_then(Value::as_str).ok_or_else(
                    || outcome_error(ErrorKind::InvalidInput, "`path` is required"),
                )?)?;
                let reload = match arguments.get("reload") {
                    None => false,
                    Some(Value::Bool(reload)) => *reload,
                    Some(_) => {
                        return Err(outcome_error(
                            ErrorKind::InvalidInput,
                            "`reload` must be a boolean",
                        ));
                    }
                };
                (json!({ "path": path, "reload": reload }), Vec::new())
            }
            EditorOperation::SelectionSet => {
                let scene = optional_scene(arguments)?;
                let nodes: Vec<String> = arguments
                    .get("nodes")
                    .and_then(Value::as_array)
                    .ok_or_else(|| outcome_error(ErrorKind::InvalidInput, "`nodes` is required"))?
                    .iter()
                    .map(|node| {
                        node.as_str()
                            .filter(|node| !node.is_empty() && node.len() <= MAX_NAME_BYTES)
                            .map(str::to_owned)
                            .ok_or_else(|| {
                                outcome_error(
                                    ErrorKind::InvalidInput,
                                    "every node is a non-empty node path",
                                )
                            })
                    })
                    .collect::<Result<_, _>>()?;
                if nodes.len() > MAX_SELECTION {
                    return Err(outcome_error(
                        ErrorKind::InvalidInput,
                        format!("at most {MAX_SELECTION} nodes may be selected"),
                    ));
                }
                (
                    json!({ "scene": scene.unwrap_or_default(), "nodes": nodes }),
                    Vec::new(),
                )
            }
            EditorOperation::Undo | EditorOperation::Redo => {
                let scene = optional_scene(arguments)?;
                let save = match arguments.get("save") {
                    None => false,
                    Some(Value::Bool(save)) => *save,
                    Some(_) => {
                        return Err(outcome_error(
                            ErrorKind::InvalidInput,
                            "`save` must be a boolean",
                        ));
                    }
                };
                // A save writes the scene file, so that file is in the
                // transaction's scope with the content it holds now.
                let files = if save {
                    let path = match &scene {
                        Some(scene) => scene.clone(),
                        None => runtime.edited_scene().await?,
                    };
                    let relative = path.trim_start_matches("res://").to_owned();
                    vec![(relative, runtime.disk_precondition(&path)?)]
                } else {
                    Vec::new()
                };
                (
                    json!({ "scene": scene.unwrap_or_default(), "save": save }),
                    files,
                )
            }
            EditorOperation::AddonReload => (json!({}), Vec::new()),
            EditorOperation::StatusGet
            | EditorOperation::SceneTreeGet
            | EditorOperation::TypesGet => unreachable!("reads are answered above"),
        };
        runtime
            .mutate(
                tool,
                operation.bridge_method(),
                payload,
                files,
                &canonical,
                &call,
                &idempotency_id,
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An editor tool's schema requires an idempotency id exactly when the
    /// tool is a mutation, which is when its handler refuses a call without
    /// one: every operation that is not a read.
    #[test]
    fn exactly_the_editor_mutations_require_an_idempotency_id() {
        for &operation in EditorOperation::ALL {
            assert_eq!(
                operation.is_read(),
                !operation.descriptor().is_mutation(),
                "{}",
                operation.tool_name()
            );
            let schema = crate::kernel::tool_schema(
                operation.tool_name(),
                crate::core::CoreLimits::default(),
            );
            assert_eq!(
                schema["required"]
                    .as_array()
                    .expect("a required list")
                    .contains(&json!("idempotencyId")),
                operation.descriptor().is_mutation(),
                "{}",
                operation.tool_name()
            );
        }
    }
}
