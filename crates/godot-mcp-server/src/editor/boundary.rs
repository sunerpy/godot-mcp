//! The editor effect boundary and the live-editor view, both backed by the
//! bridge.
//!
//! The boundary turns one journaled [`EditorMutation`] into one bridge
//! request and the plugin's three-state answer into an `editor_runtime`
//! proof. A receipt is bound to the transaction exactly when the plugin
//! answered this request's correlation id: that answer is the receipt, its
//! action names what the editor recorded and its semantic digest is the live
//! state probe. A request that never reached the editor is
//! [`BoundaryUnavailable`]; one that reached it without an answer is
//! `unresolved`, with an unreachable bridge in the proof.
//!
//! A screenshot's effect is its artifact, as a managed game's is: the plugin
//! captures, the boundary reads the PNG back in bounded chunks, checks it and
//! registers it, owned by the caller, and the capture commits only once it is
//! registered. A resource preview is read back the same way, as one artifact
//! for the preview and one for its thumbnail.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use godot_mcp_bridge::protocol::envelope::OperationClass;
use godot_mcp_bridge::protocol::mutation::MutationStatus;
use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::contracts::{ArtifactKind, OutcomeError};
use serde_json::{Map, Value, json};

use super::bridge::{BridgeAnswer, BridgeFailure, EditorBridge, LateAnswer, now_unix_ms};
use crate::capture::CaptureSink;
use crate::effect::boundary::{
    BoundaryUnavailable, EditorEffectBoundary, EditorEffectOutcome, EditorMutation, EffectTicket,
};
use crate::effect::gateway::LateEditorAnswer;
use crate::journal::{EditorProcess, TypedProof};
use crate::router::{HeldPath, LiveEditorView};

/// How long the live-editor view waits for the editor to say what it holds.
const VIEW_TIMEOUT: Duration = Duration::from_secs(5);
/// The bridge method that captures what the editor shows.
pub(crate) const CAPTURE_VIEWPORT: &str = "editor.capture.viewport";
/// The bridge method that asks the editor for a resource's preview.
pub(crate) const PREVIEW_RESOURCE: &str = "editor.preview.resource";
/// The bridge method that reads a capture back.
const CAPTURE_CHUNK: &str = "editor.capture.chunk";
/// How long one chunk read may take.
const CHUNK_TIMEOUT: Duration = Duration::from_secs(30);

/// The bridge, seen as the editor effect boundary and the live-editor view.
pub(crate) struct BridgeEditorBoundary {
    bridge: Arc<EditorBridge>,
    captures: Option<CaptureSink>,
}

impl BridgeEditorBoundary {
    pub(crate) const fn new(bridge: Arc<EditorBridge>) -> Self {
        Self {
            bridge,
            captures: None,
        }
    }

    /// This boundary, keeping the editor's captures in `sink`.
    pub(crate) fn with_captures(mut self, sink: CaptureSink) -> Self {
        self.captures = Some(sink);
        self
    }
}

/// Nothing happened, and why.
fn not_started(kind: ErrorKind, message: impl Into<String>) -> EditorEffectOutcome {
    EditorEffectOutcome {
        status: MutationStatus::NotStarted,
        payload: json!({}),
        error: Some((kind, message.into())),
        proof: unbound_proof(),
    }
}

fn unbound_proof() -> TypedProof {
    TypedProof::EditorRuntime {
        bridge_reachable: false,
        receipt_transaction_id: None,
        receipt_action_id: None,
        live_state_probe: None,
        editor_process: None,
    }
}

pub(crate) fn receipt(ticket: &EffectTicket, action: &str, payload: &Value) -> TypedProof {
    TypedProof::EditorRuntime {
        bridge_reachable: true,
        receipt_transaction_id: Some(ticket.transaction_id().to_owned()),
        receipt_action_id: Some(
            payload
                .get("action")
                .and_then(Value::as_str)
                .filter(|recorded| !recorded.is_empty())
                .unwrap_or(action)
                .to_owned(),
        ),
        live_state_probe: Some(
            payload
                .get("digest")
                .and_then(Value::as_str)
                .map_or_else(|| "answered".to_owned(), str::to_owned),
        ),
        editor_process: None,
    }
}

impl EditorEffectBoundary for BridgeEditorBoundary {
    fn available(&self) -> bool {
        true
    }

    fn connected(&self) -> bool {
        self.bridge.status().connected
    }

    fn mutate(
        &self,
        ticket: &EffectTicket,
        mutation: &EditorMutation,
    ) -> Result<TypedProof, BoundaryUnavailable> {
        self.apply(ticket, mutation).map(|outcome| outcome.proof)
    }

    fn apply(
        &self,
        ticket: &EffectTicket,
        mutation: &EditorMutation,
    ) -> Result<EditorEffectOutcome, BoundaryUnavailable> {
        if mutation.action == CAPTURE_VIEWPORT || mutation.action == PREVIEW_RESOURCE {
            return self.capture(ticket, mutation);
        }
        let payload: Value = serde_json::from_str(&mutation.payload).unwrap_or_else(|_| json!({}));
        let (answer, editor_process) = self.bridge.call_with_process(
            &mutation.action,
            OperationClass::Mutation,
            &payload,
            &mutation.idempotency_id,
            mutation.timeout,
        );
        bridge_outcome(ticket, &mutation.action, answer, "editor", editor_process)
    }
}

impl BridgeEditorBoundary {
    /// Capture what the editor shows and keep it as the caller's artifact.
    /// The capture changes nothing in the editor, so every failure before the
    /// artifact is registered is `not_started`.
    fn capture(
        &self,
        ticket: &EffectTicket,
        mutation: &EditorMutation,
    ) -> Result<EditorEffectOutcome, BoundaryUnavailable> {
        let Some(sink) = &self.captures else {
            return Ok(not_started(
                ErrorKind::Unavailable,
                "this server keeps no editor captures",
            ));
        };
        let mut payload: Map<String, Value> = match serde_json::from_str(&mutation.payload) {
            Ok(Value::Object(object)) => object,
            _ => return Ok(not_started(ErrorKind::Internal, "a capture is an object")),
        };
        // The owner is the server's to record; the editor never sees it.
        let Some(owner) = payload.remove("owner") else {
            return Ok(not_started(
                ErrorKind::Internal,
                "a capture names the caller who owns it",
            ));
        };
        let described = match self.bridge.call(
            &mutation.action,
            OperationClass::Read,
            &Value::Object(payload),
            &mutation.idempotency_id,
            mutation.timeout,
        ) {
            Ok(BridgeAnswer::Result { payload, .. }) => payload,
            Ok(BridgeAnswer::Error { kind, message, .. }) => return Ok(not_started(kind, message)),
            Err(failure) if failure.dispatched() => {
                return Ok(not_started(ErrorKind::Unavailable, failure.message()));
            }
            Err(_) => return Err(BoundaryUnavailable { boundary: "editor" }),
        };
        if mutation.action == PREVIEW_RESOURCE {
            return Ok(self.keep_preview(sink, ticket, mutation, &described, &owner));
        }
        let frames = crate::capture::fetch_frames(&described, |chunk| {
            read(&self.bridge, CAPTURE_CHUNK, chunk, &mutation.idempotency_id)
        });
        let bytes = match frames.map(|frames| frames.into_iter().next()) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return Ok(not_started(ErrorKind::Internal, "the capture had no frame")),
            Err(error) => return Ok(not_started(error.kind(), error.message())),
        };
        let contract = match crate::capture::contract(
            sink,
            ticket,
            ArtifactKind::Screenshot,
            "image/png",
            &bytes,
            Some(&owner),
            now_unix_ms(),
        ) {
            Ok(contract) => contract,
            Err(error) => return Ok(not_started(error.kind(), error.message())),
        };
        let artifact = json!({
            "uri": contract.resource_uri().as_str(),
            "media_type": contract.metadata().media_type,
            "byte_length": contract.metadata().byte_length,
            "sha256": contract.metadata().sha256,
            "width": described.get("width"),
            "height": described.get("height"),
            "expires_at_unix_ms": contract.expires_at_unix_ms(),
        });
        if let Err(error) = sink.store.register(contract, bytes) {
            return Ok(not_started(error.kind(), error.message()));
        }
        let digest = artifact["sha256"].clone();
        Ok(EditorEffectOutcome {
            status: MutationStatus::Committed,
            proof: receipt(ticket, &mutation.action, &json!({ "digest": digest })),
            payload: json!({
                "artifact": artifact,
                "view": described.get("view").cloned().unwrap_or(Value::Null),
            }),
            error: None,
        })
    }
}

impl BridgeEditorBoundary {
    /// Read back the preview `described` and keep each of its images, the
    /// preview and, when the editor made one, its thumbnail, as the caller's
    /// artifact. Nothing is kept unless every image arrived as described.
    fn keep_preview(
        &self,
        sink: &CaptureSink,
        ticket: &EffectTicket,
        mutation: &EditorMutation,
        described: &Value,
        owner: &Value,
    ) -> EditorEffectOutcome {
        let roles: Vec<&str> = described
            .get("images")
            .and_then(Value::as_array)
            .map(|images| {
                images
                    .iter()
                    .filter_map(|image| image.get("role").and_then(Value::as_str))
                    .collect()
            })
            .unwrap_or_default();
        let frame_count = described
            .get("frames")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        if !(roles == ["preview"] || roles == ["preview", "thumbnail"])
            || roles.len() != frame_count
        {
            return not_started(
                ErrorKind::Internal,
                "the editor described a preview it did not complete",
            );
        }
        let frames = match crate::capture::fetch_frames(described, |chunk| {
            read(&self.bridge, CAPTURE_CHUNK, chunk, &mutation.idempotency_id)
        }) {
            Ok(frames) => frames,
            Err(error) => return not_started(error.kind(), error.message()),
        };
        let mut kept: Vec<(Value, Vec<u8>)> = Vec::with_capacity(frames.len());
        let mut artifacts = Map::new();
        for (index, bytes) in frames.into_iter().enumerate() {
            let image = &described["images"][index];
            // Two identical images are one artifact.
            let artifact = if let Some((artifact, _)) = kept.iter().find(|(_, kept)| *kept == bytes)
            {
                let mut artifact = artifact.clone();
                artifact["width"] = image.get("width").cloned().unwrap_or(Value::Null);
                artifact["height"] = image.get("height").cloned().unwrap_or(Value::Null);
                artifact
            } else {
                let contract = match crate::capture::contract(
                    sink,
                    ticket,
                    ArtifactKind::Screenshot,
                    "image/png",
                    &bytes,
                    Some(owner),
                    now_unix_ms(),
                ) {
                    Ok(contract) => contract,
                    Err(error) => return not_started(error.kind(), error.message()),
                };
                let artifact = json!({
                    "uri": contract.resource_uri().as_str(),
                    "media_type": contract.metadata().media_type,
                    "byte_length": contract.metadata().byte_length,
                    "sha256": contract.metadata().sha256,
                    "width": image.get("width"),
                    "height": image.get("height"),
                    "expires_at_unix_ms": contract.expires_at_unix_ms(),
                });
                if let Err(error) = sink.store.register(contract, bytes.clone()) {
                    return not_started(error.kind(), error.message());
                }
                kept.push((artifact.clone(), bytes));
                artifact
            };
            artifacts.insert(roles[index].to_owned(), artifact);
        }
        let digest = artifacts
            .get("preview")
            .and_then(|preview| preview.get("sha256"))
            .cloned()
            .unwrap_or(Value::Null);
        EditorEffectOutcome {
            status: MutationStatus::Committed,
            proof: receipt(ticket, &mutation.action, &json!({ "digest": digest })),
            payload: json!({
                "path": described.get("path").cloned().unwrap_or(Value::Null),
                "preview": artifacts.get("preview").cloned().unwrap_or(Value::Null),
                "thumbnail": artifacts.get("thumbnail").cloned().unwrap_or(Value::Null),
            }),
            error: None,
        }
    }
}

/// One read the editor answers, as a payload or the classified failure.
fn read(
    bridge: &EditorBridge,
    method: &str,
    payload: &Value,
    idempotency_id: &str,
) -> Result<Value, OutcomeError> {
    match bridge.call(
        method,
        OperationClass::Read,
        payload,
        idempotency_id,
        CHUNK_TIMEOUT,
    ) {
        Ok(BridgeAnswer::Result { payload, .. }) => Ok(payload),
        Ok(BridgeAnswer::Error { kind, message, .. }) => {
            Err(OutcomeError::new(kind, message, false, BTreeMap::new()))
        }
        Err(failure) => Err(OutcomeError::new(
            ErrorKind::Unavailable,
            failure.message(),
            false,
            BTreeMap::new(),
        )),
    }
}

/// The three-state outcome of one bridge mutation and the `editor_runtime`
/// proof the journal records for it, for any bridge peer: a result or error
/// answer carries its mutation status, a request that reached the peer and
/// was never answered is `unresolved`, and one that never reached it is
/// [`BoundaryUnavailable`].
///
/// `editor_process` is the editor process the request was sent to, as its
/// bridge session proved it. Only the proof of a request that reached the
/// peer and was never answered keeps it: once that process has ended, nothing
/// can still write for the request, which is what lets an operator settle the
/// transaction (`transaction repair --operation acknowledge-safe-state`).
pub(crate) fn bridge_outcome(
    ticket: &EffectTicket,
    action: &str,
    answer: Result<BridgeAnswer, BridgeFailure>,
    boundary: &'static str,
    editor_process: Option<EditorProcess>,
) -> Result<EditorEffectOutcome, BoundaryUnavailable> {
    match answer {
        Ok(BridgeAnswer::Result { status, payload }) => Ok(EditorEffectOutcome {
            status,
            proof: if status == MutationStatus::Committed {
                receipt(ticket, action, &payload)
            } else {
                unbound_proof()
            },
            payload,
            error: None,
        }),
        Ok(BridgeAnswer::Error {
            kind,
            message,
            status,
        }) => {
            let status = status.unwrap_or(MutationStatus::NotStarted);
            Ok(EditorEffectOutcome {
                status,
                payload: json!({}),
                error: Some((kind, message)),
                proof: if status == MutationStatus::Committed {
                    receipt(ticket, action, &json!({}))
                } else {
                    unbound_proof()
                },
            })
        }
        Err(failure) if failure.dispatched() => Ok(EditorEffectOutcome {
            status: MutationStatus::Unresolved,
            payload: json!({}),
            error: Some((
                ErrorKind::Uncertain,
                if boundary == "runtime" {
                    failure.probe_message()
                } else {
                    failure.message()
                },
            )),
            proof: TypedProof::EditorRuntime {
                bridge_reachable: false,
                receipt_transaction_id: None,
                receipt_action_id: None,
                live_state_probe: None,
                editor_process,
            },
        }),
        Err(_) => Err(BoundaryUnavailable { boundary }),
    }
}

/// The late answer the bridge bound to a transaction, read as the gateway
/// settles it: by the rules [`bridge_outcome`] reads an answer in time by.
pub(crate) fn late_editor_answer(late: LateAnswer) -> LateEditorAnswer {
    let (status, error, payload) = match late.answer {
        BridgeAnswer::Result { status, payload } => (status, None, payload),
        BridgeAnswer::Error {
            kind,
            message,
            status,
        } => (
            status.unwrap_or(MutationStatus::NotStarted),
            Some((kind, message)),
            json!({}),
        ),
    };
    let text = |key: &str| {
        payload
            .get(key)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    };
    LateEditorAnswer {
        transaction_id: late.transaction_id,
        idempotency_id: late.idempotency_id,
        method: late.method,
        sent_here: late.sent_here,
        status,
        error,
        action: text("action"),
        digest: text("digest"),
    }
}

impl LiveEditorView for BridgeEditorBoundary {
    fn held_paths(&self) -> Result<Option<Vec<HeldPath>>, String> {
        let state = match self.bridge.call(
            "editor.state",
            OperationClass::Read,
            &json!({}),
            "live-editor-view",
            VIEW_TIMEOUT,
        ) {
            Ok(BridgeAnswer::Result { payload, .. }) => payload,
            Ok(BridgeAnswer::Error { message, .. }) => return Err(message),
            // Between the editor that was asked to restart and the restarted
            // one, an editor process may still write its files, and the
            // restarted one is loading them: nothing is free to be written.
            Err(BridgeFailure::NotConnected) if self.bridge.restarting() => {
                return Err(
                    "the editor is being restarted with editor.restart, and the \
                            restarted editor has not connected yet"
                        .to_owned(),
                );
            }
            Err(BridgeFailure::NotConnected) => return Ok(None),
            Err(failure) => return Err(failure.message()),
        };
        Ok(Some(held_paths(&state)))
    }
}

/// The paths an `editor.state` answer says the editor holds, project-relative:
/// every open scene, whose live tree a file write would silently diverge
/// from, every open script whose editor buffer differs from its file or
/// cannot be compared, and `project.godot`, whose settings a running editor
/// keeps in memory and writes back whenever it saves them. A script open with
/// no edits is reloaded by the editor and is not held.
pub(crate) fn held_paths(state: &Value) -> Vec<HeldPath> {
    let mut held = Vec::new();
    for (key, always) in [("open_scenes", true), ("open_scripts", false)] {
        for entry in state
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(path) = entry
                .get("path")
                .and_then(Value::as_str)
                .and_then(|path| path.strip_prefix("res://"))
            else {
                continue;
            };
            let unsaved = entry.get("unsaved").and_then(Value::as_bool);
            if always || unsaved != Some(false) {
                held.push(HeldPath {
                    path: path.to_owned(),
                    unsaved,
                });
            }
        }
    }
    held.push(HeldPath {
        path: PROJECT_SETTINGS.to_owned(),
        unsaved: None,
    });
    held
}

/// The project settings file a connected editor owns.
pub(crate) const PROJECT_SETTINGS: &str = "project.godot";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_scenes_and_edited_scripts_are_held() {
        let state = json!({
            "open_scenes": [
                { "path": "res://main.tscn", "unsaved": false },
                { "path": "res://levels/one.tscn", "unsaved": null }
            ],
            "open_scripts": [
                { "path": "res://player.gd", "unsaved": false },
                { "path": "res://enemy.gd", "unsaved": true },
                { "path": "res://hud.gd", "unsaved": null },
                { "path": "user://outside.gd", "unsaved": true }
            ]
        });
        let held: Vec<(String, Option<bool>)> = held_paths(&state)
            .into_iter()
            .map(|held| (held.path, held.unsaved))
            .collect();
        assert_eq!(
            held,
            vec![
                ("main.tscn".to_owned(), Some(false)),
                ("levels/one.tscn".to_owned(), None),
                ("enemy.gd".to_owned(), Some(true)),
                ("hud.gd".to_owned(), None),
                ("project.godot".to_owned(), None),
            ]
        );
    }

    /// A late answer is read exactly as one in time: a committed result's
    /// recorded action and digest are its receipt, and an error that names no
    /// status did not start the mutation.
    #[test]
    fn a_late_answer_reads_as_an_answer_in_time_would() {
        let late = |answer| LateAnswer {
            transaction_id: "00000000-0000-4000-8000-000000000601".to_owned(),
            idempotency_id: "idem-late".to_owned(),
            method: "editor.scene.save".to_owned(),
            sent_here: false,
            answer,
        };
        let committed = late_editor_answer(late(BridgeAnswer::Result {
            status: MutationStatus::Committed,
            payload: json!({ "action": "godot-mcp: editor.scene.save", "digest": "saved" }),
        }));
        assert_eq!(
            committed,
            LateEditorAnswer {
                transaction_id: "00000000-0000-4000-8000-000000000601".to_owned(),
                idempotency_id: "idem-late".to_owned(),
                method: "editor.scene.save".to_owned(),
                sent_here: false,
                status: MutationStatus::Committed,
                error: None,
                action: Some("godot-mcp: editor.scene.save".to_owned()),
                digest: Some("saved".to_owned()),
            }
        );
        let refused = late_editor_answer(late(BridgeAnswer::Error {
            kind: ErrorKind::Unavailable,
            message: "the connection was lost before the request started".to_owned(),
            status: None,
        }));
        assert_eq!(refused.status, MutationStatus::NotStarted);
        assert_eq!(
            refused.error,
            Some((
                ErrorKind::Unavailable,
                "the connection was lost before the request started".to_owned()
            ))
        );
        let failed = late_editor_answer(late(BridgeAnswer::Error {
            kind: ErrorKind::Internal,
            message: "the script raised an error".to_owned(),
            status: Some(MutationStatus::Committed),
        }));
        assert_eq!(failed.status, MutationStatus::Committed);
        assert_eq!((failed.action, failed.digest), (None, None));
    }

    /// A running editor writes its in-memory project settings back whenever it
    /// saves them, so a connected editor holds `project.godot` even with
    /// nothing open.
    #[test]
    fn a_connected_editor_always_holds_project_godot() {
        let held = held_paths(&json!({ "open_scenes": [], "open_scripts": [] }));
        assert_eq!(
            held,
            vec![HeldPath {
                path: "project.godot".to_owned(),
                unsaved: None,
            }]
        );
    }
}
