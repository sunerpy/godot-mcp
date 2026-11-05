//! `project.batch.apply`: one batch of typed operations for each of several
//! existing text scenes and resources, landing together or not at all.
//!
//! Every file's batch has the shape `scene.batch.apply` or
//! `resource.batch.apply` takes and is validated by the same code. One
//! supervised helper run applies every batch and stages each result, so one
//! linked launch proof covers every staged file, and the staged files are
//! published as one journaled file transaction: each bound to its caller's
//! SHA-256, every recorded pre-state re-read before the first write, and the
//! files already written put back when a later write fails. A file open in the
//! connected editor is refused rather than changed underneath the editor's
//! buffer; `scene.batch.apply` changes such a scene in the editor.
//!
//! Each file is edited as it is on disk: no file sees what the batch does to
//! another, so a scene instancing another file of the same batch keeps
//! referring to that file by its path, and sees its new content once both land.

use std::collections::BTreeSet;
use std::time::Duration;

use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::contracts::OutcomeError;
use godot_mcp_engine::canonical::request::{BatchTarget, BatchesRequest, MAX_BATCH_TARGETS};
use godot_mcp_engine::canonical::stage::{self, EngineContext};
use godot_mcp_engine::supervision::journal::{
    OperationFamily as EngineOperationFamily, ProjectIdentityClaim, TransactionEnvelope, hex_sha256,
};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use super::authored::{self, AuthorKind, AuthorRequest};
use super::canonical::{conflict, marked_replay, recorded_replay_result};
use super::outcome_error;
use super::request::EngineOperation;
use super::runtime::{EngineRuntime, engine_transport, journal_transport};
use crate::audit::RequestAudit;
use crate::effect::{FileEffectIntent, PlannedWrite, WritePrecondition};
use crate::journal::{FileOperation, OperationFamily};
use crate::kernel::AccessContext;
use crate::router::{RouteEffect, RouteRequest};

/// Most operations every file's batch together may carry.
const MAX_TOTAL_OPERATIONS: usize = 512;
/// Largest serialized batches, every file's together.
const MAX_TOTAL_BATCH_BYTES: usize = 4 * 1024 * 1024;
/// How long the one helper run that stages every file may take.
const HELPER_DEADLINE: Duration = Duration::from_secs(120);
/// The fields one file of the batch takes.
const FILE_FIELDS: [&str; 4] = ["kind", "path", "expectedSha256", "operations"];

/// One file of a validated multi-file batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BatchFile {
    /// The scene or resource batch, validated as its own tool validates it.
    pub(super) author: AuthorRequest,
}

impl BatchFile {
    const fn kind_name(&self) -> &'static str {
        match self.author.kind {
            AuthorKind::SceneBatch | AuthorKind::SceneCreate => "scene",
            AuthorKind::ResourceBatch | AuthorKind::ResourceCreate => "resource",
        }
    }
}

/// One validated `project.batch.apply` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BatchesArguments {
    pub(super) files: Vec<BatchFile>,
    pub(super) expected_revision: Option<u64>,
    pub(super) dry_run: bool,
}

fn invalid(message: impl Into<String>) -> OutcomeError {
    outcome_error(ErrorKind::InvalidInput, message)
}

fn tool_name() -> &'static str {
    EngineOperation::ProjectBatch.tool_name()
}

/// The schema of one file of the batch: a scene or a resource.
fn file_schema(scene: bool) -> Value {
    let (kind, extension) = if scene {
        ("scene", ".tscn")
    } else {
        ("resource", ".tres")
    };
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "kind": {
                "const": kind,
                "description": if scene {
                    "A text scene, edited with the operations scene.batch.apply takes."
                } else {
                    "A text resource, edited with the operations resource.batch.apply takes."
                }
            },
            "path": {
                "type": "string",
                "minLength": 1,
                "maxLength": 1024,
                "description": format!("Project-relative {extension} path, optionally res://-prefixed.")
            },
            "expectedSha256": {
                "type": "string",
                "pattern": "^[0-9a-f]{64}$",
                "description": "SHA-256 of the file's current bytes; a stale value refuses every file."
            },
            "operations": authored::operations_schema(scene),
        },
        "required": ["kind", "path", "expectedSha256", "operations"],
    })
}

/// The strict input schema of `project.batch.apply`.
pub(super) fn input_schema() -> Map<String, Value> {
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "files": {
                "type": "array",
                "minItems": 1,
                "maxItems": MAX_BATCH_TARGETS,
                "description": format!(
                    "The scenes and resources to edit, each once, in the order the engine \
                     applies them: at most {MAX_TOTAL_OPERATIONS} operations together. Every \
                     file changes, or none does."
                ),
                "items": { "oneOf": [file_schema(true), file_schema(false)] }
            },
            "expectedRevision": {
                "type": "integer",
                "minimum": 0,
                "description": "Refuse unless the workspace is still at this revision."
            },
            "dryRun": {
                "type": "boolean",
                "description": "Run the engine and return every file's diff and tree without publishing."
            }
        },
        "required": ["files", "idempotencyId"],
        "$defs": crate::schema::value_definitions(),
    });
    schema
        .as_object()
        .cloned()
        .expect("the batch tool schema is a JSON object")
}

/// Validate a `project.batch.apply` request before any engine runs.
///
/// # Errors
/// Returns [`ErrorKind::InvalidInput`] for an unknown or null field, no file
/// or too many, a file named twice, a file whose batch its own tool would
/// refuse, or batches above the totals.
pub(super) fn parse(arguments: Map<String, Value>) -> Result<BatchesArguments, OutcomeError> {
    let tool = tool_name();
    if let Some(key) = arguments
        .iter()
        .find_map(|(key, value)| value.is_null().then_some(key))
    {
        return Err(invalid(format!(
            "invalid engine request: `{key}` must not be null"
        )));
    }
    let accepted = ["files", "expectedRevision", "dryRun"];
    if let Some(unknown) = arguments
        .keys()
        .find(|key| !accepted.contains(&key.as_str()))
    {
        return Err(invalid(format!(
            "invalid engine request: unknown field `{unknown}`, expected one of {}",
            accepted.join(", ")
        )));
    }
    let expected_revision = match arguments.get("expectedRevision") {
        None => None,
        Some(value) => Some(
            crate::arguments::non_negative_whole_u64(value).ok_or_else(|| {
                invalid(format!(
                    "{tool}: `expectedRevision` must be a non-negative integer"
                ))
            })?,
        ),
    };
    let dry_run = match arguments.get("dryRun") {
        None => false,
        Some(Value::Bool(value)) => *value,
        Some(_) => return Err(invalid(format!("{tool}: `dryRun` must be a boolean"))),
    };
    let listed = match arguments.get("files") {
        Some(Value::Array(listed)) if !listed.is_empty() && listed.len() <= MAX_BATCH_TARGETS => {
            listed
        }
        Some(Value::Array(listed)) => {
            return Err(invalid(format!(
                "{tool}: `files` lists between 1 and {MAX_BATCH_TARGETS} files, not {}",
                listed.len()
            )));
        }
        Some(_) => return Err(invalid(format!("{tool}: `files` must be an array"))),
        None => return Err(invalid(format!("{tool}: `files` is required"))),
    };

    let mut files = Vec::with_capacity(listed.len());
    let mut seen = BTreeSet::new();
    let (mut operations, mut bytes) = (0_usize, 0_usize);
    for (index, entry) in listed.iter().enumerate() {
        let context = format!("{tool}: files[{index}]");
        let Value::Object(entry) = entry else {
            return Err(invalid(format!("{context} must be an object")));
        };
        if let Some(unknown) = entry
            .keys()
            .find(|key| !FILE_FIELDS.contains(&key.as_str()))
        {
            return Err(invalid(format!(
                "{context}: unknown field `{unknown}`, expected one of {}",
                FILE_FIELDS.join(", ")
            )));
        }
        let kind = match entry.get("kind").and_then(Value::as_str) {
            Some("scene") => AuthorKind::SceneBatch,
            Some("resource") => AuthorKind::ResourceBatch,
            _ => {
                return Err(invalid(format!(
                    "{context}: `kind` must be \"scene\" or \"resource\""
                )));
            }
        };
        let mut single = entry.clone();
        single.remove("kind");
        let author = authored::parse_as(kind, single, &context)?;
        if !seen.insert(author.target.to_lowercase()) {
            return Err(invalid(format!(
                "{context}: {} is listed twice; a batch names each file once (letter case \
                 aside, since Windows and macOS file systems ignore it)",
                author.target
            )));
        }
        operations += serde_json::from_str::<Vec<Value>>(&author.batch)
            .map(|parsed| parsed.len())
            .unwrap_or_default();
        bytes += author.batch.len();
        files.push(BatchFile { author });
    }
    if operations > MAX_TOTAL_OPERATIONS {
        return Err(invalid(format!(
            "{tool}: the files carry {operations} operations; at most {MAX_TOTAL_OPERATIONS} \
             together"
        )));
    }
    if bytes > MAX_TOTAL_BATCH_BYTES {
        return Err(invalid(format!(
            "{tool}: the batches exceed {MAX_TOTAL_BATCH_BYTES} bytes together"
        )));
    }
    Ok(BatchesArguments {
        files,
        expected_revision,
        dry_run,
    })
}

impl EngineRuntime {
    /// Apply every file's batch through one helper run and publish the staged
    /// files as one file transaction.
    pub(super) async fn execute_batches(
        &self,
        batches: BatchesArguments,
        arguments: Value,
        access: &AccessContext,
        idempotency_id: Option<&str>,
        audit: RequestAudit,
        cancellation: CancellationToken,
    ) -> Result<Value, OutcomeError> {
        let tool = tool_name();
        let idempotency_id = idempotency_id.ok_or_else(|| {
            outcome_error(
                ErrorKind::InvalidInput,
                format!("{tool} requires `idempotencyId`, so a retried call can never apply twice"),
            )
        })?;
        let first = batches
            .files
            .first()
            .map(|file| file.author.target.clone())
            .ok_or_else(|| invalid(format!("{tool}: `files` is empty")))?;

        if !batches.dry_run {
            let replay_request = RouteRequest {
                tool: tool.to_owned(),
                idempotency_id: idempotency_id.to_owned(),
                transaction_id: None,
                path: first.clone(),
                bytes: Vec::new(),
                arguments: arguments.clone(),
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
                return Ok(marked_replay(result));
            }
        }

        // Every file must hold the bytes its caller saw, before any engine runs.
        let root = self.root()?;
        let mut before = Vec::with_capacity(batches.files.len());
        for file in &batches.files {
            let target = &file.author.target;
            let expected = file.author.expected_sha256.as_deref().unwrap_or_default();
            match root.read(std::path::Path::new(target)).ok() {
                Some(bytes) if hex_sha256(&bytes) == expected => before.push(bytes),
                Some(bytes) => {
                    return Err(conflict(
                        target,
                        format!(
                            "{target} holds sha256 {}, not the expected {expected}; re-read it \
                             and retry. No file was written",
                            hex_sha256(&bytes)
                        ),
                        Some(&bytes),
                    ));
                }
                None => {
                    return Err(conflict(
                        target,
                        format!("{target} does not exist. No file was written"),
                        None,
                    ));
                }
            }
        }
        if let Some(expected) = batches.expected_revision {
            self.require_revision(expected)?;
        }

        // A file the connected editor holds open is the editor's: changing it
        // underneath the editor's buffer would be lost or overwritten.
        if let Some(bridge) = &self.editor {
            use crate::router::LiveEditorView as _;
            let boundary =
                crate::editor::boundary::BridgeEditorBoundary::new(std::sync::Arc::clone(bridge));
            let held = tokio::task::spawn_blocking(move || boundary.held_paths())
                .await
                .map_err(|error| outcome_error(ErrorKind::Internal, error.to_string()))?
                .map_err(|reason| {
                    outcome_error(
                        ErrorKind::Unavailable,
                        format!(
                            "the connected editor did not say which files it holds open \
                             ({reason}); no file was written"
                        ),
                    )
                })?
                .unwrap_or_default();
            if let Some(open) = batches
                .files
                .iter()
                .find(|file| held.iter().any(|open| open.path == file.author.target))
            {
                let target = &open.author.target;
                return Err(outcome_error(
                    ErrorKind::Conflict,
                    format!(
                        "{target} is open in the connected editor, so {tool} would change it \
                         underneath the editor's buffer; edit it with scene.batch.apply, which \
                         the editor carries out, or close it in the editor. No file was written"
                    ),
                ));
            }
        }

        // Node handles name nodes of the bytes just matched; a handle of any
        // other bytes refuses here, before an engine runs.
        let mut targets = Vec::with_capacity(batches.files.len());
        for (file, bytes) in batches.files.iter().zip(&before) {
            let handles = if file.author.handles.is_empty() {
                None
            } else {
                let bound = authored::bind_handles(
                    &file.author.target,
                    bytes,
                    self.held.generation,
                    &file.author.handles,
                )?;
                Some(
                    serde_json::to_string(&bound)
                        .map_err(|error| outcome_error(ErrorKind::Internal, error.to_string()))?,
                )
            };
            targets.push(BatchTarget {
                target: file.author.target.clone(),
                batch: file.author.batch.clone(),
                handles,
            });
        }
        let request = BatchesRequest {
            targets,
            open_resources: Vec::new(),
            dry_run: batches.dry_run,
        };
        let kinds = request
            .validate()
            .map_err(|error| outcome_error(error.error_kind(), error.to_string()))?;

        let request_bytes = request.canonical_bytes();
        let resolved = self.resolve_engine().await?;
        let launch_envelope = TransactionEnvelope {
            transaction_id: self
                .router
                .next_transaction_id("canonical-helper", access.observed_at_unix_ms()),
            project: ProjectIdentityClaim {
                object_identity: self.held.object_identity.clone(),
                root_object: self.held.root_object.clone(),
                file_object: self.held.file_object.clone(),
                generation: self.held.generation,
            },
            transport_mode: engine_transport(access.transport_mode()),
            principal: Some(access.mode_principal().to_journal_string()),
            session: Some(access.connection_session().to_owned()),
            operation_family: EngineOperationFamily::ProjectMutation,
            tool_name: tool.to_owned(),
            idempotency_id: format!("canonical-launch-{}", request.staging_id()),
            canonical_request_hash: hex_sha256(&request_bytes),
            canonical_request_bytes: request_bytes.len() as u64,
            opened_at_unix_ms: access.observed_at_unix_ms(),
        };
        let (staged, scratch_root) = {
            let context = EngineContext {
                launch_journal: self.journal.as_ref(),
                provider: resolved.provider.as_ref(),
                binding: resolved.binding,
                deadline: HELPER_DEADLINE,
            };
            let scratch_root = context.project_root().to_path_buf();
            let staged =
                stage::stage_batches(&context, self.root()?, &request, &kinds, &launch_envelope)
                    .map_err(|error| outcome_error(error.error_kind(), error.to_string()))?;
            (staged, scratch_root)
        };

        let mut answered = Vec::with_capacity(staged.targets.len());
        for ((file, staged_file), bytes) in batches.files.iter().zip(&staged.targets).zip(&before) {
            let before_text = String::from_utf8_lossy(bytes).into_owned();
            let after_text = String::from_utf8_lossy(&staged_file.bytes).into_owned();
            let target = &file.author.target;
            answered.push(json!({
                "path": target,
                "kind": file.kind_name(),
                "before_sha256": hex_sha256(bytes),
                "after_sha256": hex_sha256(&staged_file.bytes),
                "changed": bytes != &staged_file.bytes,
                "uid": staged_file.uid,
                "diff": crate::authoring::unified_diff_value(
                    Some(target),
                    Some(target),
                    &before_text,
                    &after_text,
                ),
                "tree": authored::semantic_tree(target, &after_text, self.held.generation),
                "results": staged_file.results,
            }));
        }
        let mut result = Map::new();
        result.insert("files".to_owned(), Value::Array(answered));
        result.insert("backend".to_owned(), Value::String("headless".to_owned()));

        if batches.dry_run {
            stage::discard_batches_scratch(&scratch_root, &staged)
                .map_err(|error| outcome_error(error.error_kind(), error.to_string()))?;
            result.insert("dry_run".to_owned(), Value::Bool(true));
            result.insert("workspace".to_owned(), self.workspace.snapshot().to_value());
            return Ok(Value::Object(result));
        }

        let transaction_id = self
            .router
            .next_transaction_id(tool, access.observed_at_unix_ms());
        let writes: Vec<PlannedWrite> = batches
            .files
            .iter()
            .zip(&staged.targets)
            .map(|(file, staged_file)| PlannedWrite {
                path: file.author.target.clone(),
                operation: FileOperation::Update,
                bytes: Some(staged_file.bytes.clone()),
                precondition: WritePrecondition::Sha256(
                    file.author.expected_sha256.clone().unwrap_or_default(),
                ),
            })
            .collect();
        // What the audit digests as the published content: each file's path
        // and the hash of the bytes it receives.
        let content: Vec<Value> = writes
            .iter()
            .map(|write| {
                json!({
                    "path": write.path,
                    "sha256": write.bytes.as_deref().map(hex_sha256),
                })
            })
            .collect();
        let intent = FileEffectIntent {
            transaction_id: transaction_id.clone(),
            tool_name: tool.to_owned(),
            operation_family: OperationFamily::ProjectMutation,
            idempotency_id: idempotency_id.to_owned(),
            transport_mode: journal_transport(access.transport_mode()),
            principal: Some(access.mode_principal().to_journal_string()),
            session: Some(access.connection_session().to_owned()),
            arguments: arguments.clone(),
            writes,
            required_process_transaction: Some(staged.launch_transaction_id.clone()),
            opened_at_unix_ms: access.observed_at_unix_ms(),
            expected_revision: batches.expected_revision,
        };
        let route_request = RouteRequest {
            tool: tool.to_owned(),
            idempotency_id: idempotency_id.to_owned(),
            transaction_id: Some(transaction_id),
            path: first,
            bytes: serde_json::to_vec(&content).unwrap_or_default(),
            arguments,
            observation: self.route_observation(),
            cancellation,
            queued_at_unix_ms: access.observed_at_unix_ms(),
            identity: Some(Self::route_identity(access)),
            effect: Some(RouteEffect::File(intent)),
            audit: Some(audit),
        };
        let dispatched = self
            .router
            .route(&route_request)
            .await
            .map_err(|refusal| refusal.to_outcome());
        let cleanup = stage::discard_batches_scratch(&scratch_root, &staged)
            .map_err(|error| outcome_error(error.error_kind(), error.to_string()));
        let report = dispatched?;
        cleanup?;
        if let Some(replayed) = recorded_replay_result(&report)? {
            return Ok(marked_replay(replayed));
        }
        let outcome = report.effect_outcome().ok_or_else(|| {
            outcome_error(
                ErrorKind::Internal,
                "the file route returned no effect outcome",
            )
        })?;
        if outcome.conclusion != crate::journal::Conclusion::ProvenPostState {
            return Err(outcome_error(
                ErrorKind::Conflict,
                format!(
                    "the change did not land and every file still holds its prior state: {}",
                    outcome.reason
                ),
            ));
        }
        result.insert(
            "transaction_id".to_owned(),
            Value::String(outcome.transaction_id.clone()),
        );
        result.insert(
            "phase".to_owned(),
            Value::String(outcome.phase.as_str().to_owned()),
        );
        result.insert(
            "conclusion".to_owned(),
            Value::String(outcome.conclusion.as_str().to_owned()),
        );
        result.insert("dry_run".to_owned(), Value::Bool(false));
        result.insert("replayed".to_owned(), Value::Bool(false));
        result.insert(
            "workspace".to_owned(),
            report.workspace().cloned().unwrap_or(Value::Null),
        );
        Ok(Value::Object(result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(files: &[BatchFile], index: usize) -> &AuthorRequest {
        &files[index].author
    }

    const SHA: &str = "0000000000000000000000000000000000000000000000000000000000000000";

    fn arguments(value: Value) -> Map<String, Value> {
        value.as_object().cloned().expect("an object")
    }

    fn scene(path: &str) -> Value {
        json!({
            "kind": "scene",
            "path": path,
            "expectedSha256": SHA,
            "operations": [{"op": "add_node", "name": "Added", "type": "Node2D"}],
        })
    }

    fn resource(path: &str) -> Value {
        json!({
            "kind": "resource",
            "path": path,
            "expectedSha256": SHA,
            "operations": [{
                "op": "set_property",
                "property": "resource_name",
                "value": {"type": "string", "value": "renamed"}
            }],
        })
    }

    #[test]
    fn a_batch_validates_every_file_as_its_own_tool_would() {
        let parsed = parse(arguments(json!({
            "files": [scene("res://scenes/a.tscn"), resource("themes/b.tres")],
            "dryRun": true,
        })))
        .expect("a valid batch");
        assert!(parsed.dry_run);
        assert_eq!(file(&parsed.files, 0).target, "scenes/a.tscn");
        assert_eq!(file(&parsed.files, 0).kind, AuthorKind::SceneBatch);
        assert_eq!(file(&parsed.files, 1).kind, AuthorKind::ResourceBatch);
        assert_eq!(parsed.files[1].kind_name(), "resource");

        let refused = |value: Value| {
            parse(arguments(value))
                .expect_err("refused before any engine runs")
                .message()
                .to_owned()
        };
        assert!(refused(json!({"files": []})).contains("between 1 and 16"));
        let many: Vec<Value> = (0..17).map(|n| scene(&format!("s/{n}.tscn"))).collect();
        assert!(refused(json!({ "files": many })).contains("not 17"));
        assert!(
            refused(json!({"files": [scene("a.tscn"), scene("A.tscn")]})).contains("listed twice")
        );
        let wrong_kind = refused(json!({"files": [scene("a.tscn"), resource("b.tscn")]}));
        assert!(
            wrong_kind.contains("files[1]") && wrong_kind.contains(".tres"),
            "{wrong_kind}"
        );
        let mut unknown = scene("a.tscn");
        unknown["dryRun"] = json!(true);
        assert!(refused(json!({ "files": [unknown] })).contains("unknown field `dryRun`"));
        let mut kindless = scene("a.tscn");
        kindless["kind"] = json!("script");
        assert!(refused(json!({ "files": [kindless] })).contains("`kind`"));
        let mut empty = scene("a.tscn");
        empty["operations"] = json!([]);
        assert!(refused(json!({ "files": [empty] })).contains("files[0]"));
        assert!(refused(json!({"files": [scene("a.tscn")], "path": "a.tscn"})).contains("unknown"));
        assert!(refused(json!({"files": null})).contains("must not be null"));
    }

    #[test]
    fn the_operations_of_every_file_count_toward_one_total() {
        let mut big = scene("a.tscn");
        big["operations"] = Value::Array(
            (0..128)
                .map(|n| json!({"op": "add_node", "name": format!("N{n}"), "type": "Node"}))
                .collect(),
        );
        let error =
            parse(arguments(json!({ "files": files_of(&big, 5) }))).expect_err("640 operations");
        assert!(
            error.message().contains("at most 512"),
            "{}",
            error.message()
        );
        assert!(parse(arguments(json!({ "files": files_of(&big, 4) }))).is_ok());
    }

    fn files_of(template: &Value, count: usize) -> Vec<Value> {
        (0..count)
            .map(|n| {
                let mut file = template.clone();
                file["path"] = json!(format!("s/{n}.tscn"));
                file
            })
            .collect()
    }

    #[test]
    fn the_schema_and_the_parser_agree_on_the_fields() {
        let schema = Value::Object(input_schema());
        let properties: BTreeSet<&str> = schema["properties"]
            .as_object()
            .expect("properties")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            properties,
            BTreeSet::from(["files", "expectedRevision", "dryRun"])
        );
        for variant in schema["properties"]["files"]["items"]["oneOf"]
            .as_array()
            .expect("two file shapes")
        {
            let fields: BTreeSet<&str> = variant["properties"]
                .as_object()
                .expect("file properties")
                .keys()
                .map(String::as_str)
                .collect();
            assert_eq!(fields, BTreeSet::from(FILE_FIELDS));
            assert_eq!(variant["required"], json!(FILE_FIELDS));
        }
        assert_eq!(
            schema["properties"]["files"]["maxItems"],
            json!(MAX_BATCH_TARGETS)
        );
    }
}
