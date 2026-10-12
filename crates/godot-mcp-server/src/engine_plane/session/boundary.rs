//! The runtime effect boundary: journaled commands to a managed game session.
//!
//! Every command names its `session` in its payload. Synthetic input goes to
//! the session's runtime probe as one bridge mutation and settles from the
//! probe's three-state answer, exactly as a live-editor mutation does. A stop
//! is the session manager's own effect. A capture's effect is the artifact:
//! the probe captures and the server reads the PNG bytes back in bounded
//! chunks, checks each frame against the digest the probe reported, assembles
//! a recording's frames into one animated PNG and registers the result in the
//! artifact store; nothing about a capture is committed until that
//! registration is.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use godot_mcp_bridge::protocol::envelope::OperationClass;
use godot_mcp_bridge::protocol::mutation::MutationStatus;
use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::contracts::{
    ArtifactContract, ArtifactId, ArtifactKind, ArtifactMetadata, ArtifactOwner,
    ArtifactResourceUri, OutcomeError, PrincipalId, ProjectIdentity, RangeSemantics, SessionId,
};
use serde_json::{Map, Value, json};

use super::{SessionManager, apng};
use crate::artifacts::{ArtifactStore, sha256_hex};
use crate::editor::boundary::{bridge_outcome, receipt};
use crate::editor::bridge::{BridgeAnswer, BridgeFailure, EditorBridge};
use crate::effect::boundary::{
    BoundaryUnavailable, EditorEffectOutcome, EditorMutation, EffectTicket, RuntimeEffectBoundary,
    RuntimeOperation,
};
use crate::journal::{EffectBoundaryKind, TypedProof};

/// The probe's chunk size, and the most one chunk read asks for.
const CHUNK_BYTES: u64 = 1024 * 1024;
/// How long one capture or chunk read may take, beyond a recording's length.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(30);

/// The runtime boundary `serve` installs.
pub(crate) struct SessionRuntimeBoundary {
    sessions: Arc<SessionManager>,
    artifacts: Arc<ArtifactStore>,
    project: ProjectIdentity,
    retention_ms: u64,
    max_range_bytes: u64,
}

impl SessionRuntimeBoundary {
    pub(crate) fn new(
        sessions: Arc<SessionManager>,
        artifacts: Arc<ArtifactStore>,
        project: ProjectIdentity,
        retention_ms: u64,
        max_range_bytes: u64,
    ) -> Self {
        Self {
            sessions,
            artifacts,
            project,
            retention_ms,
            max_range_bytes,
        }
    }
}

fn unbound() -> TypedProof {
    TypedProof::EditorRuntime {
        bridge_reachable: false,
        receipt_transaction_id: None,
        receipt_action_id: None,
        live_state_probe: None,
    }
}

/// Nothing happened, and why.
fn not_started(error: &OutcomeError) -> EditorEffectOutcome {
    EditorEffectOutcome {
        status: MutationStatus::NotStarted,
        payload: json!({}),
        error: Some((error.kind(), error.message().to_owned())),
        proof: unbound(),
    }
}

fn failed(kind: ErrorKind, message: impl Into<String>) -> EditorEffectOutcome {
    EditorEffectOutcome {
        status: MutationStatus::NotStarted,
        payload: json!({}),
        error: Some((kind, message.into())),
        proof: unbound(),
    }
}

impl RuntimeEffectBoundary for SessionRuntimeBoundary {
    fn available(&self) -> bool {
        true
    }

    fn connected(&self) -> bool {
        self.sessions.knows_a_session_of(super::SessionKind::Game)
    }

    fn running(&self) -> bool {
        self.sessions.runs_a_session_of(super::SessionKind::Game)
    }

    fn invoke(
        &self,
        _ticket: &EffectTicket,
        _operation: &RuntimeOperation,
    ) -> Result<TypedProof, BoundaryUnavailable> {
        // Every runtime effect is a typed command carried out by `apply`.
        Err(BoundaryUnavailable {
            boundary: "runtime",
        })
    }

    fn apply(
        &self,
        ticket: &EffectTicket,
        command: &EditorMutation,
    ) -> Result<EditorEffectOutcome, BoundaryUnavailable> {
        if ticket.boundary() != EffectBoundaryKind::Runtime || ticket.transaction_id().is_empty() {
            return Err(BoundaryUnavailable {
                boundary: "runtime",
            });
        }
        let mut payload: Map<String, Value> = match serde_json::from_str(&command.payload) {
            Ok(Value::Object(object)) => object,
            _ => {
                return Ok(failed(
                    ErrorKind::InvalidInput,
                    "a runtime command is an object",
                ));
            }
        };
        let session = match payload.remove("session") {
            Some(Value::String(session)) => session,
            _ => {
                return Ok(failed(
                    ErrorKind::InvalidInput,
                    "a runtime command names its session",
                ));
            }
        };
        let owner = payload.remove("owner");
        match command.action.as_str() {
            "runtime.session.stop" => Ok(self.stop(ticket, command, &session)),
            "runtime.capture.screenshot" | "runtime.capture.recording" => Ok(self.capture(
                ticket,
                command,
                &session,
                &Value::Object(payload),
                owner.as_ref(),
            )),
            action => {
                let bridge = match self.sessions.running_game(&session) {
                    Ok(bridge) => bridge,
                    Err(error) => return Ok(not_started(&error)),
                };
                let answer = bridge.call_cancellable(
                    action,
                    OperationClass::Mutation,
                    &Value::Object(payload),
                    &command.idempotency_id,
                    command.timeout,
                    &command.cancellation,
                );
                bridge_outcome(ticket, action, answer, "runtime")
            }
        }
    }
}

impl SessionRuntimeBoundary {
    fn stop(
        &self,
        ticket: &EffectTicket,
        command: &EditorMutation,
        session: &str,
    ) -> EditorEffectOutcome {
        match self.sessions.stop(session, &command.idempotency_id) {
            Ok(value) => EditorEffectOutcome {
                status: MutationStatus::Committed,
                proof: receipt(
                    ticket,
                    &command.action,
                    &json!({ "digest": value.get("state").and_then(Value::as_str).unwrap_or("stopped") }),
                ),
                payload: value,
                error: None,
            },
            Err(error) if error.kind() == ErrorKind::Uncertain => EditorEffectOutcome {
                status: MutationStatus::Unresolved,
                payload: json!({}),
                error: Some((error.kind(), error.message().to_owned())),
                proof: unbound(),
            },
            Err(error) => not_started(&error),
        }
    }

    fn capture(
        &self,
        ticket: &EffectTicket,
        command: &EditorMutation,
        session: &str,
        payload: &Value,
        owner: Option<&Value>,
    ) -> EditorEffectOutcome {
        let bridge = match self.sessions.running_game(session) {
            Ok(bridge) => bridge,
            Err(error) => return not_started(&error),
        };
        let recording = command.action == "runtime.capture.recording";
        let timeout = command.timeout.max(CAPTURE_TIMEOUT);
        // A recording runs for its whole duration on the probe: a client that
        // cancels it withdraws it there, and the probe keeps no frame of it.
        let described = match ask_cancellable(
            &bridge,
            &command.action,
            payload,
            &command.idempotency_id,
            timeout,
            &command.cancellation,
        ) {
            Ok(described) => described,
            Err(error) => return not_started(&error),
        };
        let frames = match fetch_frames(&bridge, &described, &command.idempotency_id) {
            Ok(frames) => frames,
            Err(error) => return not_started(&error),
        };
        let fps = described.get("fps").and_then(Value::as_u64).unwrap_or(10);
        let (bytes, kind, media_type) = if recording {
            match apng::assemble(&frames, u16::try_from(fps).unwrap_or(30)) {
                Ok(bytes) => (bytes, ArtifactKind::Recording, "image/apng"),
                Err(error) => return failed(ErrorKind::Internal, error),
            }
        } else {
            match frames.into_iter().next() {
                Some(bytes) => (bytes, ArtifactKind::Screenshot, "image/png"),
                None => return failed(ErrorKind::Internal, "the screenshot had no frame"),
            }
        };
        let contract = match self.contract(ticket, kind, media_type, &bytes, owner) {
            Ok(contract) => contract,
            Err(error) => return not_started(&error),
        };
        let artifact = artifact_value(&contract, &described, recording);
        if let Err(error) = self.artifacts.register(contract, bytes) {
            return not_started(&error);
        }
        let mut result = Map::new();
        result.insert("session".to_owned(), Value::String(session.to_owned()));
        result.insert("artifact".to_owned(), artifact);
        for key in ["revision", "frame"] {
            if let Some(value) = described.get(key) {
                result.insert(key.to_owned(), value.clone());
            }
        }
        let digest = result["artifact"]["sha256"].clone();
        EditorEffectOutcome {
            status: MutationStatus::Committed,
            proof: receipt(ticket, &command.action, &json!({ "digest": digest })),
            payload: Value::Object(result),
            error: None,
        }
    }

    fn contract(
        &self,
        ticket: &EffectTicket,
        kind: ArtifactKind,
        media_type: &str,
        bytes: &[u8],
        owner: Option<&Value>,
    ) -> Result<ArtifactContract, OutcomeError> {
        let sha256 = sha256_hex(bytes);
        let token = sha256_hex(format!("{}\n{sha256}", ticket.transaction_id()).as_bytes());
        let id = ArtifactId::parse(format!("artifact_{token}")).map_err(|error| {
            OutcomeError::new(
                ErrorKind::Internal,
                error.to_string(),
                false,
                BTreeMap::new(),
            )
        })?;
        let length = bytes.len() as u64;
        let created = super::now_unix_ms();
        let principal = owner
            .and_then(|owner| owner.get("principal"))
            .and_then(Value::as_str)
            .and_then(|principal| PrincipalId::parse(principal).ok());
        let session = owner
            .and_then(|owner| owner.get("session"))
            .and_then(Value::as_str)
            .and_then(|session| SessionId::parse(session).ok());
        let owner = (principal.is_some() || session.is_some())
            .then_some(ArtifactOwner { principal, session });
        let violation = |error: godot_mcp_domain::contracts::ContractViolation| {
            OutcomeError::new(
                ErrorKind::Internal,
                error.to_string(),
                false,
                BTreeMap::new(),
            )
        };
        ArtifactContract::new(
            id.clone(),
            ArtifactResourceUri::for_artifact(&id),
            self.project.clone(),
            owner,
            ArtifactMetadata {
                kind,
                media_type: media_type.to_owned(),
                byte_length: length,
                sha256,
                created_at_unix_ms: created,
            },
            created.saturating_add(self.retention_ms.max(1)),
            RangeSemantics::new(
                length.max(1),
                length.clamp(1, self.max_range_bytes.max(1)),
                true,
            )
            .map_err(violation)?,
        )
        .map_err(violation)
    }
}

fn artifact_value(contract: &ArtifactContract, described: &Value, recording: bool) -> Value {
    json!({
        "uri": contract.resource_uri().as_str(),
        "media_type": contract.metadata().media_type,
        "byte_length": contract.metadata().byte_length,
        "sha256": contract.metadata().sha256,
        "width": described.get("width"),
        "height": described.get("height"),
        "frames": if recording {
            described.get("frames").and_then(Value::as_array).map_or(0, Vec::len)
        } else {
            1
        },
        "fps": if recording { described.get("fps").cloned().unwrap_or(Value::Null) } else { Value::Null },
        "frame_sha256": described
            .get("frames")
            .and_then(Value::as_array)
            .map(|frames| frames.iter().filter_map(|frame| frame.get("sha256").cloned()).collect::<Vec<_>>()),
        "expires_at_unix_ms": contract.expires_at_unix_ms(),
    })
}

/// One read the probe answers, as a payload or the classified failure.
fn ask(
    bridge: &EditorBridge,
    method: &str,
    payload: &Value,
    idempotency_id: &str,
    timeout: Duration,
) -> Result<Value, OutcomeError> {
    answer_of(bridge.call(
        method,
        OperationClass::Read,
        payload,
        idempotency_id,
        timeout,
    ))
}

/// [`ask`], withdrawn from the probe as soon as `cancellation` fires.
fn ask_cancellable(
    bridge: &EditorBridge,
    method: &str,
    payload: &Value,
    idempotency_id: &str,
    timeout: Duration,
    cancellation: &crate::effect::boundary::CancelSignal,
) -> Result<Value, OutcomeError> {
    answer_of(bridge.call_cancellable(
        method,
        OperationClass::Read,
        payload,
        idempotency_id,
        timeout,
        cancellation,
    ))
}

fn answer_of(answer: Result<BridgeAnswer, BridgeFailure>) -> Result<Value, OutcomeError> {
    match answer {
        Ok(BridgeAnswer::Result { payload, .. }) => Ok(payload),
        Ok(BridgeAnswer::Error { kind, message, .. }) => {
            Err(OutcomeError::new(kind, message, false, BTreeMap::new()))
        }
        Err(failure @ BridgeFailure::Unsupported(_)) => Err(OutcomeError::new(
            ErrorKind::Unsupported,
            failure.probe_message(),
            false,
            BTreeMap::new(),
        )),
        Err(failure) => Err(OutcomeError::new(
            ErrorKind::Unavailable,
            failure.probe_message(),
            false,
            BTreeMap::new(),
        )),
    }
}

/// Read every frame of a capture back in bounded chunks and check each one
/// against the length and SHA-256 the probe reported for it.
fn fetch_frames(
    bridge: &EditorBridge,
    described: &Value,
    idempotency_id: &str,
) -> Result<Vec<Vec<u8>>, OutcomeError> {
    let malformed = || {
        OutcomeError::new(
            ErrorKind::Internal,
            "the probe described a capture it did not complete",
            false,
            BTreeMap::new(),
        )
    };
    let capture = described
        .get("capture_id")
        .and_then(Value::as_str)
        .ok_or_else(malformed)?;
    let frames = described
        .get("frames")
        .and_then(Value::as_array)
        .ok_or_else(malformed)?;
    let mut fetched = Vec::with_capacity(frames.len());
    for (index, frame) in frames.iter().enumerate() {
        let length = frame
            .get("byte_length")
            .and_then(Value::as_u64)
            .ok_or_else(malformed)?;
        let expected = frame
            .get("sha256")
            .and_then(Value::as_str)
            .ok_or_else(malformed)?;
        let mut bytes: Vec<u8> = Vec::with_capacity(usize::try_from(length).unwrap_or(0));
        while (bytes.len() as u64) < length {
            let chunk = ask(
                bridge,
                "runtime.capture.chunk",
                &json!({
                    "capture_id": capture,
                    "frame": index,
                    "offset": bytes.len(),
                    "length": CHUNK_BYTES,
                }),
                idempotency_id,
                CAPTURE_TIMEOUT,
            )?;
            let data = chunk
                .get("data_base64")
                .and_then(Value::as_str)
                .ok_or_else(malformed)?;
            let decoded = base64_decode(data).ok_or_else(malformed)?;
            if decoded.is_empty() {
                return Err(malformed());
            }
            bytes.extend_from_slice(&decoded);
        }
        if bytes.len() as u64 != length || sha256_hex(&bytes) != expected {
            return Err(OutcomeError::new(
                ErrorKind::Internal,
                format!("frame {index} did not arrive as the probe described it"),
                false,
                BTreeMap::new(),
            ));
        }
        fetched.push(bytes);
    }
    Ok(fetched)
}

/// Decode standard base64 (with padding), as Godot's `Marshalls` writes it.
fn base64_decode(text: &str) -> Option<Vec<u8>> {
    fn value(byte: u8) -> Option<u32> {
        Some(u32::from(match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        }))
    }
    let bytes: Vec<u8> = text
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for quad in bytes.chunks(4) {
        let padding = quad.iter().rev().take_while(|byte| **byte == b'=').count();
        if padding > 2 {
            return None;
        }
        let mut word = 0_u32;
        for (position, byte) in quad.iter().enumerate() {
            let digit = if position >= 4 - padding {
                0
            } else {
                value(*byte)?
            };
            word = (word << 6) | digit;
        }
        let decoded = [(word >> 16) as u8, (word >> 8) as u8, word as u8];
        out.extend_from_slice(&decoded[..3 - padding]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_decodes_what_godot_encodes() {
        assert_eq!(base64_decode("aGVsbG8=").as_deref(), Some(&b"hello"[..]));
        assert_eq!(base64_decode("aGk=").as_deref(), Some(&b"hi"[..]));
        assert_eq!(base64_decode("aGV5").as_deref(), Some(&b"hey"[..]));
        assert_eq!(base64_decode("").as_deref(), Some(&b""[..]));
        assert!(base64_decode("a$==").is_none());
        assert!(base64_decode("abc").is_none());
    }
}
