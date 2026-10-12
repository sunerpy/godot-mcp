//! Contract 0.2 project authoring: coherent files, GDScript, project settings
//! and assets.
//!
//! Every read answers from one coherent workspace revision: the bytes it
//! returns are proven to be the bytes that revision indexed, and a read that
//! catches the workspace mid-change refreshes it rather than mixing states.
//!
//! Every mutation is exactly one journaled `file` transaction routed through
//! the outcome router, so it inherits the whole effect path rather than
//! reimplementing any of it: idempotent replay over the durable journal, the
//! write block, effect-lane admission, a policy permit, a content precondition
//! per touched path checked before anything is journaled, compare-and-swap
//! publication through the held lease, a three-state terminal proof and the
//! read-your-writes barrier. A mutation answers only after its terminal record
//! is durable and the revision that contains it is published.
//!
//! Nothing here executes project code. A script's syntax is checked by the
//! static parser before publication; an engine verdict is `script.check`.

mod addon;
mod assets;
mod base64;
mod diff;
mod files;
mod md5;
mod patch;
mod paths;
mod scripts;
mod settings;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use godot_mcp_analysis::index::FileFingerprint;
use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::catalog::AuthoringOperation;
use godot_mcp_domain::contracts::{GodotValue, OutcomeError};
use godot_mcp_domain::manifests::canonical_manifest;
use godot_mcp_domain::safe_root::{SafeRoot, SafeRootError};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::audit::RequestAudit;
use crate::effect::{FileEffectIntent, PlannedWrite};
use crate::idempotency::TransportAuthMode;
use crate::idempotency::replay::replay_outcome;
use crate::journal::{Conclusion, OperationFamily, TransportMode};
use crate::kernel::{AccessContext, KernelToolCall, KernelToolHandler, ServiceKernel};
use crate::router::{
    EnginePresence, OutcomeRouter, PlaneObservation, ProtocolFidelity, RouteEffect, RouteReport,
    RouteRequest, RouterIdentity, RuntimePresence, WorkspaceFreshness,
};
use crate::workspace::{CommitScope, WorkspaceCoordinator, WorkspaceSnapshot};

pub(crate) use assets::import_status_value;

/// A snapshot and the bytes of each read path it indexed (`None`: absent).
type CoherentRead = (Arc<WorkspaceSnapshot>, Vec<Option<Vec<u8>>>);

/// How many times a read re-pins the workspace after catching it mid-change.
const COHERENT_READ_ATTEMPTS: usize = 3;

fn outcome_error(kind: ErrorKind, message: impl Into<String>) -> OutcomeError {
    OutcomeError::new(kind, message, false, BTreeMap::new())
}

fn outcome_error_with(
    kind: ErrorKind,
    message: impl Into<String>,
    details: impl IntoIterator<Item = (&'static str, GodotValue)>,
) -> OutcomeError {
    OutcomeError::new(
        kind,
        message,
        false,
        details
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    )
}

/// The schema of one authoring operation's arguments.
fn operation_schema(operation: AuthoringOperation) -> Value {
    match operation {
        AuthoringOperation::FilesList => files::list_schema(),
        AuthoringOperation::FileGet => files::get_schema(),
        AuthoringOperation::FileCreate => files::create_schema(),
        AuthoringOperation::FilePatch => files::patch_schema(),
        AuthoringOperation::FileMove => files::move_schema(),
        AuthoringOperation::FileDelete => files::delete_schema(),
        AuthoringOperation::ScriptCreate => scripts::create_schema(),
        AuthoringOperation::ScriptPatch => scripts::patch_schema(),
        AuthoringOperation::SettingsGet => settings::get_schema(),
        AuthoringOperation::SettingsPatch => settings::patch_schema(),
        AuthoringOperation::AssetPut => assets::put_schema(),
        AuthoringOperation::AssetDelete => assets::delete_schema(),
        AuthoringOperation::AssetStatus => assets::status_schema(),
        AuthoringOperation::AddonStatus => addon::status_schema(),
        AuthoringOperation::AddonInstall => addon::install_schema(),
        AuthoringOperation::AddonUpdate => addon::update_schema(),
        AuthoringOperation::AddonRemove => addon::remove_schema(),
    }
}

/// A bounded unified diff from `before` to `after` in the public result shape;
/// `None` names an absent side.
pub(crate) fn unified_diff_value(
    old_path: Option<&str>,
    new_path: Option<&str>,
    before: &str,
    after: &str,
) -> Value {
    diff::unified(old_path, new_path, before, after).to_value()
}

/// The strict input schema of one authoring tool.
pub(crate) fn input_schema(operation: AuthoringOperation) -> Map<String, Value> {
    operation_schema(operation)
        .as_object()
        .cloned()
        .expect("authoring schemas are objects")
}

/// Everything the authoring tools are built from, all derived from the held
/// project context.
pub(crate) struct AuthoringRuntimeConfig {
    pub(crate) root: SafeRoot,
    pub(crate) router: Arc<OutcomeRouter>,
    pub(crate) workspace: Arc<WorkspaceCoordinator>,
    pub(crate) uploads_dir: PathBuf,
    pub(crate) generation: u64,
}

/// The shared runtime behind every authoring handler.
pub(crate) struct AuthoringRuntime {
    root: SafeRoot,
    router: Arc<OutcomeRouter>,
    workspace: Arc<WorkspaceCoordinator>,
    uploads: assets::Uploads,
    generation: u64,
}

impl AuthoringRuntime {
    pub(crate) fn new(config: AuthoringRuntimeConfig) -> Self {
        Self {
            root: config.root,
            router: config.router,
            workspace: config.workspace,
            uploads: assets::Uploads::new(config.uploads_dir),
            generation: config.generation,
        }
    }

    /// Read one project file through the held lease. `None` means absent.
    fn read(&self, path: &str) -> Result<Option<Vec<u8>>, OutcomeError> {
        match self.root.read(std::path::Path::new(path)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(SafeRootError::StalePreimage { .. }) => Ok(None),
            Err(error) => Err(outcome_error(
                error.error_kind(),
                format!("{path} could not be read as project source"),
            )),
        }
    }

    /// Read `paths` together with a workspace snapshot that indexed exactly
    /// the bytes returned.
    ///
    /// A path the snapshot does not hold, or holds with other bytes, means the
    /// workspace changed after the snapshot was published; those paths are
    /// observed and the read repeats against the newer revision.
    fn coherent_read(&self, paths: &[String]) -> Result<CoherentRead, OutcomeError> {
        let mut snapshot = self.workspace.snapshot();
        for _ in 0..COHERENT_READ_ATTEMPTS {
            let contents = paths
                .iter()
                .map(|path| self.read(path))
                .collect::<Result<Vec<_>, _>>()?;
            let diverged: Vec<String> = paths
                .iter()
                .zip(&contents)
                .filter(|(path, bytes)| {
                    let indexed = snapshot.fingerprint_of(path);
                    let observed = bytes.as_deref().map(FileFingerprint::of_bytes);
                    match (indexed, observed) {
                        (None, None) => false,
                        (Some(indexed), Some(observed)) => indexed != observed.as_str(),
                        _ => true,
                    }
                })
                .map(|(path, _)| path.clone())
                .collect();
            if diverged.is_empty() {
                return Ok((snapshot, contents));
            }
            snapshot = self.workspace.observe_commit(&CommitScope::Paths(diverged));
        }
        Err(outcome_error(
            ErrorKind::Conflict,
            "the files kept changing while they were read; retry the read",
        ))
    }

    fn observation(&self) -> PlaneObservation {
        PlaneObservation::new(
            self.router.plane_availability(),
            EnginePresence::HeadlessOnly,
            RuntimePresence::NoInstance,
            ProtocolFidelity::Full,
            if self.workspace.current().coherence().is_coherent() {
                WorkspaceFreshness::Fresh
            } else {
                WorkspaceFreshness::StaleIndex
            },
        )
    }

    fn route_identity(access: &AccessContext) -> RouterIdentity {
        RouterIdentity {
            transport_mode: access.transport_mode(),
            principal: access.mode_principal().to_journal_string(),
            session: access.connection_session().to_owned(),
        }
    }

    /// The caller's own recorded result for this exact identity, when the
    /// mutation already ran.
    fn recorded(
        &self,
        tool: &str,
        arguments: &Value,
        context: &CallContext<'_>,
    ) -> Result<Option<Value>, OutcomeError> {
        let request = RouteRequest {
            tool: tool.to_owned(),
            idempotency_id: context.idempotency_id.to_owned(),
            transaction_id: None,
            path: String::new(),
            bytes: Vec::new(),
            arguments: arguments.clone(),
            observation: self.observation(),
            cancellation: context.cancellation.clone(),
            queued_at_unix_ms: context.access.observed_at_unix_ms(),
            identity: Some(Self::route_identity(context.access)),
            effect: None,
            audit: Some(context.audit.clone()),
        };
        match self
            .router
            .recorded_replay(&request)
            .map_err(|refusal| refusal.to_outcome())?
        {
            Some(report) => replayed(&report),
            None => Ok(None),
        }
    }

    /// Journal and publish one mutation through the outcome router.
    async fn commit(
        &self,
        mutation: Mutation,
        arguments: &Value,
        context: &CallContext<'_>,
    ) -> Result<Committed, OutcomeError> {
        let access = context.access;
        let transaction_id = self
            .router
            .next_transaction_id(mutation.tool, access.observed_at_unix_ms());
        let content = mutation
            .writes
            .iter()
            .find_map(|write| write.bytes.clone())
            .unwrap_or_default();
        let intent = FileEffectIntent {
            transaction_id: transaction_id.clone(),
            tool_name: mutation.tool.to_owned(),
            operation_family: OperationFamily::ProjectMutation,
            idempotency_id: context.idempotency_id.to_owned(),
            transport_mode: journal_transport(access.transport_mode()),
            principal: Some(access.mode_principal().to_journal_string()),
            session: Some(access.connection_session().to_owned()),
            arguments: arguments.clone(),
            writes: mutation.writes,
            required_process_transaction: None,
            opened_at_unix_ms: access.observed_at_unix_ms(),
            expected_revision: mutation.expected_revision,
        };
        let request = RouteRequest {
            tool: mutation.tool.to_owned(),
            idempotency_id: context.idempotency_id.to_owned(),
            transaction_id: Some(transaction_id),
            path: mutation.primary_path,
            bytes: content,
            arguments: arguments.clone(),
            observation: self.observation(),
            cancellation: context.cancellation.clone(),
            queued_at_unix_ms: access.observed_at_unix_ms(),
            identity: Some(Self::route_identity(access)),
            effect: Some(RouteEffect::File(intent)),
            audit: Some(context.audit.clone()),
        };
        let report = self
            .router
            .route(&request)
            .await
            .map_err(|refusal| refusal.to_outcome())?;
        if let Some(value) = replayed(&report)? {
            return Ok(Committed::Replayed(value));
        }
        let outcome = report.effect_outcome().ok_or_else(|| {
            outcome_error(ErrorKind::Internal, "the file route returned no outcome")
        })?;
        if outcome.conclusion != Conclusion::ProvenPostState {
            return Err(outcome_error_with(
                ErrorKind::Conflict,
                format!(
                    "the change did not land and the files still hold their prior state: {}",
                    outcome.reason
                ),
                [
                    (
                        "transaction_id",
                        GodotValue::String(outcome.transaction_id.clone()),
                    ),
                    (
                        "conclusion",
                        GodotValue::String(outcome.conclusion.as_str().to_owned()),
                    ),
                ],
            ));
        }
        Ok(Committed::Applied {
            transaction_id: outcome.transaction_id.clone(),
            phase: outcome.phase.as_str().to_owned(),
            conclusion: outcome.conclusion.as_str().to_owned(),
            workspace: report.workspace().cloned().unwrap_or(Value::Null),
        })
    }
}

fn replayed(report: &RouteReport) -> Result<Option<Value>, OutcomeError> {
    if !report.replayed() {
        return Ok(None);
    }
    let status = report.recorded_status().ok_or_else(|| {
        outcome_error(
            ErrorKind::Internal,
            "a replayed mutation returned no recorded status",
        )
    })?;
    let transaction_id = report.transaction_id().ok_or_else(|| {
        outcome_error(
            ErrorKind::Internal,
            "a replayed mutation returned no transaction identifier",
        )
    })?;
    let verdict = report.recorded_verdict().ok_or_else(|| {
        outcome_error(
            ErrorKind::Unavailable,
            "a replayed mutation returned no typed terminal outcome",
        )
    })?;
    let mut value = replay_outcome(transaction_id, status, report.recorded_result(), verdict)?;
    if let Value::Object(object) = &mut value {
        object.insert("replayed".to_owned(), Value::Bool(true));
    }
    Ok(Some(value))
}

const fn journal_transport(mode: TransportAuthMode) -> TransportMode {
    match mode {
        TransportAuthMode::Stdio => TransportMode::Stdio,
        TransportAuthMode::TrustedLocal => TransportMode::TrustedLocal,
        TransportAuthMode::ProxiedOidc => TransportMode::ProxiedOidc,
    }
}

/// The per-call facts a mutation is journaled under.
struct CallContext<'a> {
    access: &'a AccessContext,
    idempotency_id: &'a str,
    audit: RequestAudit,
    cancellation: CancellationToken,
}

/// One mutation to journal: every touched path with its precondition.
struct Mutation {
    tool: &'static str,
    primary_path: String,
    writes: Vec<PlannedWrite>,
    expected_revision: Option<u64>,
}

/// The terminal answer of one routed mutation.
enum Committed {
    /// The effect committed with a proven post-state.
    Applied {
        transaction_id: String,
        phase: String,
        conclusion: String,
        workspace: Value,
    },
    /// The caller's own earlier result for the same identity.
    Replayed(Value),
}

impl Committed {
    /// Merge the journal facts into a tool-specific result object.
    fn into_value(self, mut result: Map<String, Value>) -> Value {
        match self {
            Self::Applied {
                transaction_id,
                phase,
                conclusion,
                workspace,
            } => {
                result.insert("transaction_id".to_owned(), Value::String(transaction_id));
                result.insert("phase".to_owned(), Value::String(phase));
                result.insert("conclusion".to_owned(), Value::String(conclusion));
                result.insert("dry_run".to_owned(), Value::Bool(false));
                result.insert("replayed".to_owned(), Value::Bool(false));
                result.insert("workspace".to_owned(), workspace);
                Value::Object(result)
            }
            Self::Replayed(value) => value,
        }
    }
}

/// The shared mutation envelope: idempotency, revision guard and dry run.
struct Envelope {
    idempotency_id: String,
    expected_revision: Option<u64>,
    dry_run: bool,
}

impl Envelope {
    fn read(
        tool: &str,
        arguments: &Map<String, Value>,
        idempotency_id: Option<&str>,
    ) -> Result<Self, OutcomeError> {
        let idempotency_id = idempotency_id.ok_or_else(|| {
            outcome_error(
                ErrorKind::InvalidInput,
                format!("{tool} requires `idempotencyId`, so a retried call can never apply twice"),
            )
        })?;
        Ok(Self {
            idempotency_id: idempotency_id.to_owned(),
            expected_revision: optional_u64(arguments, "expectedRevision")?,
            dry_run: optional_bool(arguments, "dryRun")?.unwrap_or(false),
        })
    }

    /// Refuse a dry run whose caller-named revision is already superseded;
    /// a real mutation re-checks inside the effect lane.
    fn check_revision(&self, snapshot: &WorkspaceSnapshot) -> Result<(), OutcomeError> {
        match self.expected_revision {
            Some(expected) if expected != snapshot.revision() => {
                Err(stale_revision(expected, snapshot.revision()))
            }
            _ => Ok(()),
        }
    }
}

fn stale_revision(expected: u64, current: u64) -> OutcomeError {
    outcome_error_with(
        ErrorKind::Conflict,
        format!(
            "the workspace is at revision {current}, not the expected revision {expected}; \
             re-read and retry"
        ),
        [
            (
                "expected_revision",
                GodotValue::Integer(i64::try_from(expected).unwrap_or(i64::MAX)),
            ),
            (
                "current_revision",
                GodotValue::Integer(i64::try_from(current).unwrap_or(i64::MAX)),
            ),
        ],
    )
}

fn required_str<'a>(arguments: &'a Map<String, Value>, key: &str) -> Result<&'a str, OutcomeError> {
    arguments.get(key).and_then(Value::as_str).ok_or_else(|| {
        outcome_error(
            ErrorKind::InvalidInput,
            format!("`{key}` is required and must be a string"),
        )
    })
}

fn optional_str<'a>(
    arguments: &'a Map<String, Value>,
    key: &str,
) -> Result<Option<&'a str>, OutcomeError> {
    match arguments.get(key) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(outcome_error(
            ErrorKind::InvalidInput,
            format!("`{key}` must be a string"),
        )),
    }
}

fn optional_u64(arguments: &Map<String, Value>, key: &str) -> Result<Option<u64>, OutcomeError> {
    match arguments.get(key) {
        None => Ok(None),
        Some(value) => crate::arguments::non_negative_whole_u64(value)
            .map(Some)
            .ok_or_else(|| {
                outcome_error(
                    ErrorKind::InvalidInput,
                    format!("`{key}` must be a non-negative whole number"),
                )
            }),
    }
}

fn optional_bool(arguments: &Map<String, Value>, key: &str) -> Result<Option<bool>, OutcomeError> {
    match arguments.get(key) {
        None => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(outcome_error(
            ErrorKind::InvalidInput,
            format!("`{key}` must be a boolean"),
        )),
    }
}

/// Lowercase 64-hex SHA-256, or invalid input.
fn sha256_argument(arguments: &Map<String, Value>, key: &str) -> Result<String, OutcomeError> {
    let value = required_str(arguments, key)?;
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        Ok(value.to_owned())
    } else {
        Err(outcome_error(
            ErrorKind::InvalidInput,
            format!("`{key}` must be a lowercase 64-character hex SHA-256"),
        ))
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    crate::journal::hex_sha256(bytes)
}

/// Refuse a precondition that no longer holds before anything is journaled,
/// so a stale call learns the current hash.
fn precondition_mismatch(path: &str, expected: &str, current: Option<&[u8]>) -> OutcomeError {
    let observed = current.map(sha256_hex);
    outcome_error_with(
        ErrorKind::Conflict,
        match &observed {
            Some(observed) => format!(
                "{path} holds sha256 {observed}, not the expected {expected}; re-read it and retry"
            ),
            None => format!("{path} does not exist"),
        },
        [
            ("path", GodotValue::String(path.to_owned())),
            (
                "current_sha256",
                observed.map_or(GodotValue::Null, GodotValue::String),
            ),
        ],
    )
}

fn already_exists(path: &str, current: &[u8]) -> OutcomeError {
    outcome_error_with(
        ErrorKind::Conflict,
        format!("{path} already exists; patch it with its current sha256 instead"),
        [
            ("path", GodotValue::String(path.to_owned())),
            ("current_sha256", GodotValue::String(sha256_hex(current))),
        ],
    )
}

/// Text content as a UTF-8 string, or invalid input naming the field.
fn utf8_text(bytes: &[u8], path: &str) -> Result<String, OutcomeError> {
    String::from_utf8(bytes.to_vec()).map_err(|_| {
        outcome_error(
            ErrorKind::InvalidInput,
            format!("{path} is not UTF-8 text; binary data is changed through asset.put"),
        )
    })
}

/// The schema property every mutation shares.
fn envelope_properties() -> Map<String, Value> {
    let Value::Object(properties) = json!({
        "expectedRevision": {
            "type": "integer",
            "minimum": 0,
            "description": "Refuse unless the workspace is still at this revision."
        },
        "dryRun": {
            "type": "boolean",
            "description": "Validate and return the diff without writing anything."
        }
    }) else {
        unreachable!("a JSON object literal")
    };
    properties
}

/// A strict object schema whose properties are `properties` plus the envelope
/// when `mutation` is true.
fn object_schema(properties: Value, required: &[&str], mutation: bool) -> Value {
    let mut properties = properties.as_object().cloned().unwrap_or_default();
    let mut required: Vec<Value> = required
        .iter()
        .map(|name| Value::String((*name).to_owned()))
        .collect();
    if mutation {
        properties.extend(envelope_properties());
        required.push(Value::String("idempotencyId".to_owned()));
    }
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": properties,
        "required": required,
    })
}

fn path_property(description: &str) -> Value {
    json!({
        "type": "string",
        "minLength": 1,
        "maxLength": 1024,
        "description": description,
    })
}

fn sha256_property(description: &str) -> Value {
    json!({
        "type": "string",
        "pattern": "^[0-9a-f]{64}$",
        "description": description,
    })
}

/// Register a handler for every authoring tool the selected profile lists.
pub(crate) fn register_authoring_tools(
    kernel: &mut ServiceKernel,
    runtime: Arc<AuthoringRuntime>,
) -> Result<BTreeSet<String>, OutcomeError> {
    let manifest = canonical_manifest(kernel.profile());
    let mut registered = BTreeSet::new();
    for &operation in AuthoringOperation::ALL {
        let name = operation.tool_name();
        if !manifest.tools.contains(name) {
            continue;
        }
        kernel.register_tool_handler(
            operation.key(),
            Arc::new(AuthoringHandler {
                operation,
                runtime: Arc::clone(&runtime),
            }),
        )?;
        registered.insert(name.to_owned());
    }
    Ok(registered)
}

struct AuthoringHandler {
    operation: AuthoringOperation,
    runtime: Arc<AuthoringRuntime>,
}

#[async_trait]
impl KernelToolHandler for AuthoringHandler {
    async fn call(&self, call: KernelToolCall) -> Result<Value, OutcomeError> {
        if call.cancellation.is_cancelled() {
            return Err(outcome_error(
                ErrorKind::Cancelled,
                "the request was cancelled before it started",
            ));
        }
        let runtime = self.runtime.as_ref();
        let arguments = &call.arguments;
        let tool = self.operation.tool_name();
        if matches!(
            self.operation,
            AuthoringOperation::FilesList
                | AuthoringOperation::FileGet
                | AuthoringOperation::SettingsGet
                | AuthoringOperation::AssetStatus
                | AuthoringOperation::AddonStatus
        ) {
            return match self.operation {
                AuthoringOperation::FilesList => files::list(runtime, arguments),
                AuthoringOperation::FileGet => files::get(runtime, arguments),
                AuthoringOperation::SettingsGet => settings::get(runtime),
                AuthoringOperation::AddonStatus => addon::status(runtime),
                _ => assets::status(runtime, arguments),
            };
        }
        let envelope = Envelope::read(tool, arguments, call.idempotency_id.as_deref())?;
        let context = CallContext {
            access: &call.access,
            idempotency_id: &envelope.idempotency_id,
            audit: call.audit.clone(),
            cancellation: call.cancellation.clone(),
        };
        let canonical = Value::Object(arguments.clone());
        // An exact retry answers from the journal before anything is re-read or
        // re-validated, so its answer is the original one.
        if !envelope.dry_run
            && let Some(recorded) = runtime.recorded(tool, &canonical, &context)?
        {
            return Ok(recorded);
        }
        match self.operation {
            AuthoringOperation::FileCreate => {
                files::create(runtime, arguments, &envelope, &canonical, &context).await
            }
            AuthoringOperation::FilePatch => {
                files::patch(runtime, arguments, &envelope, &canonical, &context).await
            }
            AuthoringOperation::FileMove => {
                files::relocate(runtime, arguments, &envelope, &canonical, &context).await
            }
            AuthoringOperation::FileDelete => {
                files::delete(runtime, arguments, &envelope, &canonical, &context).await
            }
            AuthoringOperation::ScriptCreate => {
                scripts::create(runtime, arguments, &envelope, &canonical, &context).await
            }
            AuthoringOperation::ScriptPatch => {
                scripts::patch(runtime, arguments, &envelope, &canonical, &context).await
            }
            AuthoringOperation::SettingsPatch => {
                settings::patch(runtime, arguments, &envelope, &canonical, &context).await
            }
            AuthoringOperation::AssetPut => {
                assets::put(runtime, arguments, &envelope, &canonical, &context).await
            }
            AuthoringOperation::AssetDelete => {
                assets::delete(runtime, arguments, &envelope, &canonical, &context).await
            }
            AuthoringOperation::AddonInstall => {
                addon::install(runtime, arguments, &envelope, &canonical, &context).await
            }
            AuthoringOperation::AddonUpdate => {
                addon::update(runtime, arguments, &envelope, &canonical, &context).await
            }
            AuthoringOperation::AddonRemove => {
                addon::remove(runtime, arguments, &envelope, &canonical, &context).await
            }
            AuthoringOperation::FilesList
            | AuthoringOperation::FileGet
            | AuthoringOperation::SettingsGet
            | AuthoringOperation::AssetStatus
            | AuthoringOperation::AddonStatus => unreachable!("reads are answered above"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_authoring_tool_is_a_catalog_tool_with_a_strict_schema() {
        for &operation in AuthoringOperation::ALL {
            let descriptor = operation.descriptor();
            let name = descriptor.name;
            let schema = input_schema(operation);
            assert_eq!(schema["additionalProperties"], json!(false), "{name}");
            let required = schema["required"].as_array().expect("required list");
            assert_eq!(
                required.contains(&json!("idempotencyId")),
                descriptor.is_mutation(),
                "{name}: exactly the mutations require an idempotency id"
            );
        }
    }
}
