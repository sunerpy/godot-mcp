use std::time::Duration;

use godot_mcp_domain::contracts::OutcomeError;
use godot_mcp_engine::canonical::request::CanonicalRequest;
use godot_mcp_engine::canonical::stage::{self, EngineContext};
use godot_mcp_engine::supervision::journal::{
    OperationFamily as EngineOperationFamily, ProjectIdentityClaim, TransactionEnvelope, hex_sha256,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::outcome_error;
use super::runtime::{EngineRuntime, engine_transport, journal_transport};
use crate::audit::RequestAudit;
use crate::effect::{FileEffectIntent, PlannedWrite, WritePrecondition};
use crate::idempotency::replay::replay_outcome;
use crate::journal::{FileOperation, OperationFamily};
use crate::kernel::AccessContext;
use crate::router::{RouteEffect, RouteReport, RouteRequest};

impl EngineRuntime {
    pub(super) async fn execute_canonical(
        &self,
        tool: &str,
        request: CanonicalRequest,
        access: &AccessContext,
        idempotency_id: Option<&str>,
        audit: RequestAudit,
        cancellation: CancellationToken,
    ) -> Result<Value, OutcomeError> {
        let kind = request
            .validate()
            .map_err(|error| outcome_error(error.error_kind(), error.to_string()))?;
        let request_value = request.canonical_value();
        let request_bytes = request.canonical_bytes();
        if let Some(idempotency_id) = idempotency_id {
            let replay_request = RouteRequest {
                tool: tool.to_owned(),
                idempotency_id: idempotency_id.to_owned(),
                transaction_id: None,
                path: request.target.clone(),
                bytes: Vec::new(),
                arguments: request_value.clone(),
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
                return Ok(result);
            }
        }
        // The helper derives the staged bytes from the target as it is now. A
        // change between this read and the transaction opening must refuse the
        // write rather than be overwritten by bytes derived from older content.
        let preimage = self
            .root()?
            .read(std::path::Path::new(&request.target))
            .ok()
            .map(|bytes| hex_sha256(&bytes));
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
                deadline: Duration::from_secs(60),
            };
            let scratch_root = context.project_root().to_path_buf();
            let staged = stage::stage(&context, self.root()?, &request, kind, &launch_envelope)
                .map_err(|error| outcome_error(error.error_kind(), error.to_string()))?;
            (staged, scratch_root)
        };

        if request.dry_run {
            let cleanup = stage::discard_scratch(&scratch_root, &staged)
                .map_err(|error| outcome_error(error.error_kind(), error.to_string()));
            let preview = json!({
                "dry_run": true,
                "target": request.target,
                "staged_sha256": hex_sha256(&staged.bytes),
                "uid": staged.uid,
                "argv": staged.argv,
                "launch_transaction_id": staged.launch_transaction_id,
            });
            cleanup?;
            return Ok(preview);
        }

        let transaction_id = self
            .router
            .next_transaction_id(tool, access.observed_at_unix_ms());
        let effect_id = idempotency_id
            .map(str::to_owned)
            .unwrap_or_else(|| format!("canonical-{transaction_id}"));
        let intent = FileEffectIntent {
            transaction_id: transaction_id.clone(),
            tool_name: tool.to_owned(),
            operation_family: OperationFamily::ProjectMutation,
            idempotency_id: effect_id,
            transport_mode: journal_transport(access.transport_mode()),
            principal: Some(access.mode_principal().to_journal_string()),
            session: Some(access.connection_session().to_owned()),
            arguments: request_value.clone(),
            writes: vec![PlannedWrite {
                path: request.target.clone(),
                operation: FileOperation::Update,
                bytes: Some(staged.bytes.clone()),
                precondition: preimage
                    .map_or(WritePrecondition::Observed, WritePrecondition::Sha256),
            }],
            required_process_transaction: Some(staged.launch_transaction_id.clone()),
            opened_at_unix_ms: access.observed_at_unix_ms(),
            expected_revision: None,
        };
        let route_request = RouteRequest {
            tool: tool.to_owned(),
            idempotency_id: intent.idempotency_id.clone(),
            transaction_id: Some(transaction_id.clone()),
            path: request.target.clone(),
            bytes: staged.bytes.clone(),
            arguments: request_value,
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
        let cleanup = stage::discard_scratch(&scratch_root, &staged)
            .map_err(|error| outcome_error(error.error_kind(), error.to_string()));
        let report = dispatched?;
        cleanup?;
        if let Some(result) = recorded_replay_result(&report)? {
            return Ok(result);
        }
        let outcome = report.effect_outcome().ok_or_else(|| {
            outcome_error(
                godot_mcp_domain::ErrorKind::Internal,
                "file route returned no effect outcome",
            )
        })?;
        Ok(json!({
            "transaction_id": outcome.transaction_id,
            "phase": outcome.phase.as_str(),
            "conclusion": outcome.conclusion.as_str(),
            "reason": outcome.reason,
            "target": request.target,
            "uid": staged.uid,
            "argv": staged.argv,
            "workspace": report.workspace(),
        }))
    }
}

fn recorded_replay_result(report: &RouteReport) -> Result<Option<Value>, OutcomeError> {
    if !report.replayed() {
        return Ok(None);
    }
    let status = report.recorded_status().ok_or_else(|| {
        outcome_error(
            godot_mcp_domain::ErrorKind::Internal,
            "replayed file route returned no recorded status",
        )
    })?;
    let transaction_id = report.transaction_id().ok_or_else(|| {
        outcome_error(
            godot_mcp_domain::ErrorKind::Internal,
            "replayed file route returned no transaction identifier",
        )
    })?;
    let verdict = report.recorded_verdict().ok_or_else(|| {
        outcome_error(
            godot_mcp_domain::ErrorKind::Unavailable,
            "replayed file route returned no typed terminal outcome",
        )
    })?;
    replay_outcome(transaction_id, status, report.recorded_result(), verdict).map(Some)
}

impl EngineRuntime {
    /// Create or edit one text scene or resource through the engine.
    ///
    /// The precondition is checked twice: before any process starts, against
    /// the bytes on disk now, and again when the transaction opens inside the
    /// effect lane, so a change made while the engine ran refuses rather than
    /// being overwritten by bytes derived from the older content.
    pub(super) async fn execute_authored(
        &self,
        author: super::authored::AuthorRequest,
        arguments: Value,
        access: &AccessContext,
        idempotency_id: Option<&str>,
        audit: RequestAudit,
        cancellation: CancellationToken,
    ) -> Result<Value, OutcomeError> {
        use godot_mcp_domain::ErrorKind;
        use godot_mcp_engine::canonical::request::CanonicalOperation;

        let tool = author.kind.tool_name();
        let idempotency_id = idempotency_id.ok_or_else(|| {
            outcome_error(
                ErrorKind::InvalidInput,
                format!("{tool} requires `idempotencyId`, so a retried call can never apply twice"),
            )
        })?;
        let mut request = CanonicalRequest {
            operation: if author.kind.creates() {
                CanonicalOperation::CreateResource
            } else {
                CanonicalOperation::ApplyBatch
            },
            root_type: author.root_type.clone(),
            root_name: author.root_name.clone(),
            resource_class: author.resource_class.clone(),
            batch: Some(author.batch.clone()),
            dry_run: author.dry_run,
            ..CanonicalRequest::create(&author.target)
        };
        let kind = request
            .validate()
            .map_err(|error| outcome_error(error.error_kind(), error.to_string()))?;

        if !author.dry_run {
            let replay_request = RouteRequest {
                tool: tool.to_owned(),
                idempotency_id: idempotency_id.to_owned(),
                transaction_id: None,
                path: author.target.clone(),
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

        let before = self.root()?.read(std::path::Path::new(&author.target)).ok();
        let (operation, precondition) = match (&before, &author.expected_sha256) {
            (None, None) => (FileOperation::Create, WritePrecondition::Absent),
            (Some(existing), None) => {
                return Err(conflict(
                    &author.target,
                    format!(
                        "{} already exists (sha256 {}); edit it with a batch instead",
                        author.target,
                        hex_sha256(existing)
                    ),
                    Some(existing),
                ));
            }
            (Some(existing), Some(expected)) if &hex_sha256(existing) == expected => (
                FileOperation::Update,
                WritePrecondition::Sha256(expected.clone()),
            ),
            (current, Some(expected)) => {
                return Err(conflict(
                    &author.target,
                    match current {
                        Some(existing) => format!(
                            "{} holds sha256 {}, not the expected {expected}; re-read it and retry",
                            author.target,
                            hex_sha256(existing)
                        ),
                        None => format!("{} does not exist", author.target),
                    },
                    current.as_deref(),
                ));
            }
        };
        if let Some(expected) = author.expected_revision {
            // The snapshot applies pending watch events, or rescans when no
            // watcher runs, so an external change supersedes the revision.
            let snapshot = self.workspace.snapshot();
            if let crate::workspace::WorkspaceCoherence::Stale { reason } = snapshot.coherence() {
                return Err(outcome_error(
                    ErrorKind::Conflict,
                    format!(
                        "the workspace is stale ({reason}), so expected revision {expected} \
                         cannot be proven; retry once it is coherent again"
                    ),
                ));
            }
            let current = snapshot.revision();
            if current != expected {
                return Err(outcome_error(
                    ErrorKind::Conflict,
                    format!(
                        "the workspace is at revision {current}, not the expected revision \
                         {expected}; re-read and retry"
                    ),
                ));
            }
        }
        // Node handles name nodes of the bytes the precondition just matched; a
        // handle of any other bytes refuses here, before an engine runs.
        let mut bound_handles = std::collections::BTreeMap::new();
        if !author.handles.is_empty() {
            bound_handles = super::authored::bind_handles(
                &author.target,
                before.as_deref().unwrap_or_default(),
                self.held.generation,
                &author.handles,
            )?;
            request.handles = Some(
                serde_json::to_string(&bound_handles)
                    .map_err(|error| outcome_error(ErrorKind::Internal, error.to_string()))?,
            );
            request
                .validate()
                .map_err(|error| outcome_error(error.error_kind(), error.to_string()))?;
        }

        // A scene open in the connected editor is changed there: the editor's
        // buffer is the scene the user sees, and the headless helper would
        // write the file underneath it. A dry run stays headless.
        if author.kind == super::authored::AuthorKind::SceneBatch
            && !author.dry_run
            && let Some(bridge) = &self.editor
            && self.open_in_editor(bridge, &author.target).await?
        {
            return self
                .execute_in_editor(
                    &author,
                    &arguments,
                    access,
                    idempotency_id,
                    audit,
                    cancellation,
                    before.as_deref().unwrap_or_default(),
                    &bound_handles,
                )
                .await;
        }

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
                deadline: Duration::from_secs(60),
            };
            let scratch_root = context.project_root().to_path_buf();
            let staged = stage::stage(&context, self.root()?, &request, kind, &launch_envelope)
                .map_err(|error| outcome_error(error.error_kind(), error.to_string()))?;
            (staged, scratch_root)
        };

        let before_text = before
            .as_deref()
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned());
        let after_text = String::from_utf8_lossy(&staged.bytes).into_owned();
        let mut result = serde_json::Map::new();
        result.insert("path".to_owned(), Value::String(author.target.clone()));
        result.insert(
            "operation".to_owned(),
            Value::String(if before.is_some() { "update" } else { "create" }.to_owned()),
        );
        result.insert(
            "before_sha256".to_owned(),
            before
                .as_deref()
                .map_or(Value::Null, |bytes| Value::String(hex_sha256(bytes))),
        );
        result.insert(
            "after_sha256".to_owned(),
            Value::String(hex_sha256(&staged.bytes)),
        );
        result.insert(
            "uid".to_owned(),
            staged.uid.clone().map_or(Value::Null, Value::String),
        );
        result.insert(
            "diff".to_owned(),
            crate::authoring::unified_diff_value(
                before_text.as_ref().map(|_| author.target.as_str()),
                Some(&author.target),
                before_text.as_deref().unwrap_or_default(),
                &after_text,
            ),
        );
        result.insert(
            "tree".to_owned(),
            super::authored::semantic_tree(&author.target, &after_text, self.held.generation),
        );
        result.insert("backend".to_owned(), Value::String("headless".to_owned()));

        if author.dry_run {
            let cleanup = stage::discard_scratch(&scratch_root, &staged)
                .map_err(|error| outcome_error(error.error_kind(), error.to_string()));
            cleanup?;
            result.insert("dry_run".to_owned(), Value::Bool(true));
            result.insert("workspace".to_owned(), self.workspace.snapshot().to_value());
            return Ok(Value::Object(result));
        }

        let transaction_id = self
            .router
            .next_transaction_id(tool, access.observed_at_unix_ms());
        let intent = FileEffectIntent {
            transaction_id: transaction_id.clone(),
            tool_name: tool.to_owned(),
            operation_family: OperationFamily::ProjectMutation,
            idempotency_id: idempotency_id.to_owned(),
            transport_mode: journal_transport(access.transport_mode()),
            principal: Some(access.mode_principal().to_journal_string()),
            session: Some(access.connection_session().to_owned()),
            arguments: arguments.clone(),
            writes: vec![PlannedWrite {
                path: author.target.clone(),
                operation,
                bytes: Some(staged.bytes.clone()),
                precondition,
            }],
            required_process_transaction: Some(staged.launch_transaction_id.clone()),
            opened_at_unix_ms: access.observed_at_unix_ms(),
            expected_revision: author.expected_revision,
        };
        let route_request = RouteRequest {
            tool: tool.to_owned(),
            idempotency_id: idempotency_id.to_owned(),
            transaction_id: Some(transaction_id),
            path: author.target.clone(),
            bytes: staged.bytes.clone(),
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
        let cleanup = stage::discard_scratch(&scratch_root, &staged)
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
                    "the change did not land and the file still holds its prior state: {}",
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

impl EngineRuntime {
    /// Whether the connected editor holds `target` open. No editor connected
    /// is `false`; an editor that does not answer refuses the write.
    async fn open_in_editor(
        &self,
        bridge: &std::sync::Arc<crate::editor::bridge::EditorBridge>,
        target: &str,
    ) -> Result<bool, OutcomeError> {
        use crate::router::LiveEditorView as _;
        let boundary =
            crate::editor::boundary::BridgeEditorBoundary::new(std::sync::Arc::clone(bridge));
        let held = tokio::task::spawn_blocking(move || boundary.held_paths())
            .await
            .map_err(|error| {
                outcome_error(godot_mcp_domain::ErrorKind::Internal, error.to_string())
            })?
            .map_err(|reason| {
                outcome_error(
                    godot_mcp_domain::ErrorKind::Unavailable,
                    format!("the connected editor did not say whether {target} is open ({reason})"),
                )
            })?;
        Ok(held
            .unwrap_or_default()
            .iter()
            .any(|open| open.path == target && target.ends_with(".tscn")))
    }

    /// Apply a scene batch in the connected editor as one undoable action and
    /// answer with the same result a headless batch gives.
    #[allow(clippy::too_many_arguments)]
    async fn execute_in_editor(
        &self,
        author: &super::authored::AuthorRequest,
        arguments: &Value,
        access: &AccessContext,
        idempotency_id: &str,
        audit: RequestAudit,
        cancellation: CancellationToken,
        before: &[u8],
        handles: &std::collections::BTreeMap<String, String>,
    ) -> Result<Value, OutcomeError> {
        use crate::effect::boundary::EditorMutation;
        use crate::effect::gateway::EditorEffectIntent;
        use godot_mcp_domain::ErrorKind;

        let tool = author.kind.tool_name();
        let expected = hex_sha256(before);
        let operations: Value = serde_json::from_str(&author.batch)
            .map_err(|error| outcome_error(ErrorKind::Internal, error.to_string()))?;
        let transaction_id = self
            .router
            .next_transaction_id(tool, access.observed_at_unix_ms());
        let payload = json!({
            "path": format!("res://{}", author.target),
            "operations": operations,
            "handles": handles,
            "expected_sha256": expected,
            "label": format!("godot-mcp: {tool}"),
            "transaction_id": transaction_id,
        });
        let intent = EditorEffectIntent {
            transaction_id: transaction_id.clone(),
            tool_name: tool.to_owned(),
            operation_family: OperationFamily::ProjectMutation,
            idempotency_id: idempotency_id.to_owned(),
            transport_mode: journal_transport(access.transport_mode()),
            principal: Some(access.mode_principal().to_journal_string()),
            session: Some(access.connection_session().to_owned()),
            arguments: arguments.clone(),
            files: vec![(
                author.target.clone(),
                WritePrecondition::Sha256(expected.clone()),
            )],
            opened_at_unix_ms: access.observed_at_unix_ms(),
        };
        let route_request = RouteRequest {
            tool: tool.to_owned(),
            idempotency_id: idempotency_id.to_owned(),
            transaction_id: Some(transaction_id),
            path: author.target.clone(),
            bytes: Vec::new(),
            arguments: arguments.clone(),
            observation: self.route_observation(),
            cancellation,
            queued_at_unix_ms: access.observed_at_unix_ms(),
            identity: Some(Self::route_identity(access)),
            effect: Some(RouteEffect::Editor {
                intent,
                mutation: Box::new(EditorMutation {
                    action: "editor.scene.batch".to_owned(),
                    payload: payload.to_string(),
                    idempotency_id: idempotency_id.to_owned(),
                    timeout: std::time::Duration::from_secs(60),
                    cancellation: crate::effect::boundary::CancelSignal::default(),
                }),
            }),
            audit: Some(audit),
        };
        let report = self
            .router
            .route(&route_request)
            .await
            .map_err(|refusal| refusal.to_outcome())?;
        let answer = crate::editor::tools::editor_answer(&report)?;
        if answer.get("replayed") == Some(&Value::Bool(true)) {
            return Ok(answer);
        }
        // The file the editor saved is the published result.
        let after = self
            .root()?
            .read(std::path::Path::new(&author.target))
            .map_err(|error| outcome_error(error.error_kind(), error.to_string()))?;
        let before_text = String::from_utf8_lossy(before).into_owned();
        let after_text = String::from_utf8_lossy(&after).into_owned();
        let tree =
            super::authored::semantic_tree(&author.target, &after_text, self.held.generation);
        let mut result = serde_json::Map::new();
        result.insert("path".to_owned(), Value::String(author.target.clone()));
        result.insert("operation".to_owned(), Value::String("update".to_owned()));
        result.insert("before_sha256".to_owned(), Value::String(expected));
        result.insert("after_sha256".to_owned(), Value::String(hex_sha256(&after)));
        result.insert(
            "uid".to_owned(),
            tree.get("uid").cloned().unwrap_or(Value::Null),
        );
        result.insert(
            "diff".to_owned(),
            crate::authoring::unified_diff_value(
                Some(&author.target),
                Some(&author.target),
                &before_text,
                &after_text,
            ),
        );
        result.insert("tree".to_owned(), tree);
        result.insert("backend".to_owned(), Value::String("editor".to_owned()));
        for key in ["transaction_id", "workspace", "mutation_status"] {
            if let Some(value) = answer.get(key) {
                result.insert(key.to_owned(), value.clone());
            }
        }
        result.insert(
            "editor".to_owned(),
            json!({
                "action": answer.get("action"),
                "history_version": answer.get("version"),
                "digest": answer.get("digest"),
                "saved_sha256": answer.get("sha256"),
            }),
        );
        result.insert(
            "phase".to_owned(),
            Value::String("committed_fsynced".to_owned()),
        );
        result.insert(
            "conclusion".to_owned(),
            Value::String("proven_post_state".to_owned()),
        );
        result.insert("dry_run".to_owned(), Value::Bool(false));
        result.insert("replayed".to_owned(), Value::Bool(false));
        Ok(Value::Object(result))
    }
}

/// The caller's own recorded result, marked as a replay.
fn marked_replay(mut value: Value) -> Value {
    if let Value::Object(object) = &mut value {
        object.insert("replayed".to_owned(), Value::Bool(true));
    }
    value
}

/// A precondition refusal naming the current hash, when there is one.
fn conflict(path: &str, message: String, current: Option<&[u8]>) -> OutcomeError {
    let mut details = std::collections::BTreeMap::new();
    details.insert(
        "path".to_owned(),
        godot_mcp_domain::contracts::GodotValue::String(path.to_owned()),
    );
    details.insert(
        "current_sha256".to_owned(),
        current.map_or(godot_mcp_domain::contracts::GodotValue::Null, |bytes| {
            godot_mcp_domain::contracts::GodotValue::String(hex_sha256(bytes))
        }),
    );
    OutcomeError::new(
        godot_mcp_domain::ErrorKind::Conflict,
        message,
        false,
        details,
    )
}
