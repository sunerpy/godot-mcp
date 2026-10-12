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

use std::sync::Arc;
use std::time::Duration;

use godot_mcp_bridge::protocol::envelope::OperationClass;
use godot_mcp_bridge::protocol::mutation::MutationStatus;
use godot_mcp_domain::ErrorKind;
use serde_json::{Value, json};

use super::bridge::{BridgeAnswer, BridgeFailure, EditorBridge};
use crate::effect::boundary::{
    BoundaryUnavailable, EditorEffectBoundary, EditorEffectOutcome, EditorMutation, EffectTicket,
};
use crate::journal::TypedProof;
use crate::router::{HeldPath, LiveEditorView};

/// How long the live-editor view waits for the editor to say what it holds.
const VIEW_TIMEOUT: Duration = Duration::from_secs(5);

/// The bridge, seen as the editor effect boundary and the live-editor view.
pub(crate) struct BridgeEditorBoundary {
    bridge: Arc<EditorBridge>,
}

impl BridgeEditorBoundary {
    pub(crate) const fn new(bridge: Arc<EditorBridge>) -> Self {
        Self { bridge }
    }
}

fn unbound_proof() -> TypedProof {
    TypedProof::EditorRuntime {
        bridge_reachable: false,
        receipt_transaction_id: None,
        receipt_action_id: None,
        live_state_probe: None,
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
        let payload: Value = serde_json::from_str(&mutation.payload).unwrap_or_else(|_| json!({}));
        let answer = self.bridge.call(
            &mutation.action,
            OperationClass::Mutation,
            &payload,
            &mutation.idempotency_id,
            mutation.timeout,
        );
        bridge_outcome(ticket, &mutation.action, answer, "editor")
    }
}

/// The three-state outcome of one bridge mutation and the `editor_runtime`
/// proof the journal records for it, for any bridge peer: a result or error
/// answer carries its mutation status, a request that reached the peer and
/// was never answered is `unresolved`, and one that never reached it is
/// [`BoundaryUnavailable`].
pub(crate) fn bridge_outcome(
    ticket: &EffectTicket,
    action: &str,
    answer: Result<BridgeAnswer, BridgeFailure>,
    boundary: &'static str,
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
            proof: unbound_proof(),
        }),
        Err(_) => Err(BoundaryUnavailable { boundary }),
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
            Err(BridgeFailure::NotConnected) => return Ok(None),
            Err(failure) => return Err(failure.message()),
        };
        Ok(Some(held_paths(&state)))
    }
}

/// The paths an `editor.state` answer says the editor holds, project-relative:
/// every open scene, whose live tree a file write would silently diverge
/// from, and every open script whose editor buffer differs from its file or
/// cannot be compared. A script open with no edits is reloaded by the editor
/// and is not held.
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
    held
}

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
            ]
        );
    }
}
