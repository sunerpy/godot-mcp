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
//! registration is. An input recording the probe hands back when it stops is
//! answered in full and also kept as a JSON artifact; a failed registration
//! leaves the answer without one and says why, since the probe keeps nothing
//! of a recording it handed back.
//!
//! A time-scale change, an image comparison and a test scenario are composed
//! here from several probe requests ([`super::scenario`]): the comparison's
//! capture and difference image are kept like a screenshot, and a scenario's
//! report is answered in full and also kept as a JSON artifact.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use godot_mcp_bridge::protocol::envelope::OperationClass;
use godot_mcp_bridge::protocol::mutation::MutationStatus;
use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::contracts::{ArtifactContract, ArtifactKind, OutcomeError, ProjectIdentity};
use serde_json::{Map, Value, json};

use super::scenario::{self, Reply, TimeScale};
use super::{SessionManager, apng};
use crate::artifacts::ArtifactStore;
use crate::capture::CaptureSink;
use crate::editor::boundary::{bridge_outcome, receipt};
use crate::editor::bridge::{BridgeAnswer, BridgeFailure, EditorBridge};
use crate::effect::boundary::{
    BoundaryUnavailable, EditorEffectOutcome, EditorMutation, EffectTicket, RuntimeEffectBoundary,
    RuntimeOperation,
};
use crate::journal::{EffectBoundaryKind, TypedProof};

/// How long one capture or chunk read may take, beyond a recording's length.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(30);

/// The media type of an input recording kept as an artifact.
const INPUT_RECORDING_MEDIA_TYPE: &str = "application/json";

/// The media type of a scenario report kept as an artifact.
const REPORT_MEDIA_TYPE: &str = "application/json";

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
            "runtime.input.record.stop" => self.input_recording(
                ticket,
                command,
                &session,
                &Value::Object(payload),
                owner.as_ref(),
            ),
            "runtime.time_scale.set" => Ok(self.time_scale(ticket, command, &session, &payload)),
            "runtime.image.compare" => {
                Ok(self.image_compare(ticket, command, &session, &payload, owner.as_ref()))
            }
            "test.scenario.run" => {
                Ok(self.scenario(ticket, command, &session, &payload, owner.as_ref()))
            }
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

    /// Stop the session's input recording and keep what it answers as a JSON
    /// artifact beside the answer.
    fn input_recording(
        &self,
        ticket: &EffectTicket,
        command: &EditorMutation,
        session: &str,
        payload: &Value,
        owner: Option<&Value>,
    ) -> Result<EditorEffectOutcome, BoundaryUnavailable> {
        let bridge = match self.sessions.running_game(session) {
            Ok(bridge) => bridge,
            Err(error) => return Ok(not_started(&error)),
        };
        let answer = bridge.call_cancellable(
            &command.action,
            OperationClass::Mutation,
            payload,
            &command.idempotency_id,
            command.timeout,
            &command.cancellation,
        );
        let mut outcome = bridge_outcome(ticket, &command.action, answer, "runtime")?;
        if outcome.status != MutationStatus::Committed || outcome.error.is_some() {
            return Ok(outcome);
        }
        let Value::Object(result) = &mut outcome.payload else {
            return Ok(outcome);
        };
        // The recording itself, as input.replay takes its events; the frame and
        // revision the probe answered at say nothing about it.
        let mut document = result.clone();
        document.remove("revision");
        document.remove("frame");
        let registered = serde_json::to_vec(&Value::Object(document))
            .map_err(|error| {
                OutcomeError::new(
                    ErrorKind::Internal,
                    error.to_string(),
                    false,
                    BTreeMap::new(),
                )
            })
            .and_then(|bytes| {
                let contract = self.contract(
                    ticket,
                    ArtifactKind::Recording,
                    INPUT_RECORDING_MEDIA_TYPE,
                    &bytes,
                    owner,
                )?;
                let artifact = json!({
                    "uri": contract.resource_uri().as_str(),
                    "media_type": contract.metadata().media_type,
                    "byte_length": contract.metadata().byte_length,
                    "sha256": contract.metadata().sha256,
                    "expires_at_unix_ms": contract.expires_at_unix_ms(),
                });
                self.artifacts.register(contract, bytes)?;
                Ok(artifact)
            });
        match registered {
            Ok(artifact) => {
                outcome.proof = receipt(
                    ticket,
                    &command.action,
                    &json!({ "digest": artifact["sha256"] }),
                );
                result.insert("artifact".to_owned(), artifact);
            }
            Err(error) => {
                result.insert("artifact".to_owned(), Value::Null);
                result.insert(
                    "artifact_error".to_owned(),
                    Value::String(error.message().to_owned()),
                );
            }
        }
        Ok(outcome)
    }

    fn sink(&self) -> CaptureSink {
        CaptureSink {
            store: Arc::clone(&self.artifacts),
            project: self.project.clone(),
            retention_ms: self.retention_ms,
            max_range_bytes: self.max_range_bytes,
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
        crate::capture::contract(
            &self.sink(),
            ticket,
            kind,
            media_type,
            bytes,
            owner,
            super::now_unix_ms(),
        )
    }

    /// Write `Engine.time_scale` and read it back: a read-back that differs
    /// from the request is committed and uncertain.
    fn time_scale(
        &self,
        ticket: &EffectTicket,
        command: &EditorMutation,
        session: &str,
        payload: &Map<String, Value>,
    ) -> EditorEffectOutcome {
        let bridge = match self.sessions.running_game(session) {
            Ok(bridge) => bridge,
            Err(error) => return not_started(&error),
        };
        let Some(value) = payload
            .get("time_scale")
            .and_then(Value::as_f64)
            .filter(|value| (0.0..=scenario::MAX_TIME_SCALE).contains(value))
        else {
            return failed(
                ErrorKind::InvalidInput,
                "a time-scale change names a scale from 0 to 100",
            );
        };
        let probe = scenario::BridgeProbe::new(bridge, &command.idempotency_id);
        let calls = scenario::Calls::new(&probe, command.cancellation.clone(), None);
        match scenario::set_time_scale(&calls, value) {
            TimeScale::Set(answer) => EditorEffectOutcome {
                status: MutationStatus::Committed,
                proof: receipt(ticket, &command.action, &answer),
                payload: answer,
                error: None,
            },
            TimeScale::Unsure { answer, message } => EditorEffectOutcome {
                status: MutationStatus::Committed,
                proof: receipt(ticket, &command.action, &answer),
                payload: answer,
                error: Some((ErrorKind::Uncertain, message)),
            },
            TimeScale::Other(reply) => mutation_outcome(ticket, &command.action, reply),
        }
    }

    /// Compare the root viewport with the reference the tool read, and keep
    /// the capture and the difference image. Like a capture, nothing is
    /// committed until the capture's artifact is registered.
    fn image_compare(
        &self,
        ticket: &EffectTicket,
        command: &EditorMutation,
        session: &str,
        payload: &Map<String, Value>,
        owner: Option<&Value>,
    ) -> EditorEffectOutcome {
        let bridge = match self.sessions.running_game(session) {
            Ok(bridge) => bridge,
            Err(error) => return not_started(&error),
        };
        let reference = payload.get("reference");
        let path = reference
            .and_then(|reference| reference.get("path"))
            .and_then(Value::as_str);
        let bytes = reference
            .and_then(|reference| reference.get("data_base64"))
            .and_then(Value::as_str)
            .and_then(crate::capture::base64_decode);
        let (Some(path), Some(bytes)) = (path, bytes) else {
            return failed(
                ErrorKind::InvalidInput,
                "an image comparison carries the reference image it compares with",
            );
        };
        let tolerance = payload
            .get("tolerance")
            .and_then(Value::as_u64)
            .and_then(|tolerance| u8::try_from(tolerance).ok())
            .unwrap_or(0);
        let difference = payload
            .get("difference")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let probe = scenario::BridgeProbe::new(bridge, &command.idempotency_id);
        let calls = scenario::Calls::new(&probe, command.cancellation.clone(), None);
        let keeper = EffectKeeper {
            boundary: self,
            ticket,
            owner,
        };
        let compared = match scenario::compare_image(
            &calls, &keeper, "compare", &bytes, tolerance, difference,
        ) {
            Ok(compared) => compared,
            // A comparison changes nothing in the game: one the probe
            // did not answer kept nothing, so nothing was committed.
            Err(error) if error.kind() == ErrorKind::Uncertain => {
                return failed(ErrorKind::Unavailable, error.message());
            }
            Err(error) => return not_started(&error),
        };
        let digest = compared.capture["sha256"].clone();
        let mut result = compared.result;
        if let Some(Value::Object(described)) = result.get_mut("reference") {
            described.insert("path".to_owned(), json!(path));
        }
        result.insert("session".to_owned(), json!(session));
        result.insert("capture".to_owned(), compared.capture);
        result.insert(
            "difference".to_owned(),
            compared.difference.unwrap_or(Value::Null),
        );
        if let Some(error) = compared.difference_error {
            result.insert("difference_error".to_owned(), json!(error));
        }
        EditorEffectOutcome {
            status: MutationStatus::Committed,
            proof: receipt(ticket, &command.action, &json!({ "digest": digest })),
            payload: Value::Object(result),
            error: None,
        }
    }

    /// Run a scenario and keep its report as a JSON artifact beside the
    /// answer. A scenario that ran is committed whatever its verdict: the
    /// report says what each step did.
    fn scenario(
        &self,
        ticket: &EffectTicket,
        command: &EditorMutation,
        session: &str,
        payload: &Map<String, Value>,
        owner: Option<&Value>,
    ) -> EditorEffectOutcome {
        let bridge = match self.sessions.running_game(session) {
            Ok(bridge) => bridge,
            Err(error) => return not_started(&error),
        };
        let Some(Ok(plan)) = payload
            .get("plan")
            .cloned()
            .map(serde_json::from_value::<scenario::Plan>)
        else {
            return failed(
                ErrorKind::InvalidInput,
                "a scenario carries its checked plan",
            );
        };
        let mut references = BTreeMap::new();
        if let Some(listed) = payload.get("references").and_then(Value::as_object) {
            for (path, data) in listed {
                let Some(bytes) = data.as_str().and_then(crate::capture::base64_decode) else {
                    return failed(
                        ErrorKind::InvalidInput,
                        format!("the reference image {path} is not carried as base64"),
                    );
                };
                references.insert(path.clone(), bytes);
            }
        }
        let probe = scenario::BridgeProbe::new(bridge, &command.idempotency_id);
        let keeper = EffectKeeper {
            boundary: self,
            ticket,
            owner,
        };
        let mut report = match scenario::run_scenario(
            &probe,
            &keeper,
            &plan,
            &references,
            &command.cancellation,
        ) {
            Ok(report) => report,
            Err(error) => return not_started(&error),
        };
        report["transaction_id"] = json!(ticket.transaction_id());
        report["session"] = json!(session);
        let kept = serde_json::to_vec(&report)
            .map_err(|error| {
                OutcomeError::new(
                    ErrorKind::Internal,
                    error.to_string(),
                    false,
                    BTreeMap::new(),
                )
            })
            .and_then(|bytes| {
                scenario::Keeper::keep(
                    &keeper,
                    "report",
                    ArtifactKind::Report,
                    REPORT_MEDIA_TYPE,
                    bytes,
                )
            });
        let proof = match kept {
            Ok(artifact) => {
                let proof = receipt(
                    ticket,
                    &command.action,
                    &json!({ "digest": artifact["sha256"] }),
                );
                report["artifact"] = artifact;
                proof
            }
            Err(error) => {
                report["artifact"] = Value::Null;
                report["artifact_error"] = json!(error.message());
                receipt(ticket, &command.action, &json!({}))
            }
        };
        EditorEffectOutcome {
            status: MutationStatus::Committed,
            proof,
            payload: report,
            error: None,
        }
    }
}

/// Keeps the artifacts of one runtime effect in the store, bound to its
/// transaction and owned by the caller the tool recorded.
struct EffectKeeper<'a> {
    boundary: &'a SessionRuntimeBoundary,
    ticket: &'a EffectTicket,
    owner: Option<&'a Value>,
}

impl scenario::Keeper for EffectKeeper<'_> {
    fn keep(
        &self,
        salt: &str,
        kind: ArtifactKind,
        media_type: &str,
        bytes: Vec<u8>,
    ) -> Result<Value, OutcomeError> {
        let contract = crate::capture::contract_salted(
            &self.boundary.sink(),
            self.ticket,
            salt,
            kind,
            media_type,
            &bytes,
            self.owner,
            super::now_unix_ms(),
        )?;
        let artifact = json!({
            "uri": contract.resource_uri().as_str(),
            "media_type": contract.metadata().media_type,
            "byte_length": contract.metadata().byte_length,
            "sha256": contract.metadata().sha256,
            "expires_at_unix_ms": contract.expires_at_unix_ms(),
        });
        self.boundary.artifacts.register(contract, bytes)?;
        Ok(artifact)
    }
}

/// How a mutation the probe did not answer with a result ends, as
/// [`bridge_outcome`] decides it for one answer.
fn mutation_outcome(ticket: &EffectTicket, action: &str, reply: Reply) -> EditorEffectOutcome {
    match reply {
        Reply::Done { status, payload } => EditorEffectOutcome {
            status,
            proof: if status == MutationStatus::Committed {
                receipt(ticket, action, &payload)
            } else {
                unbound()
            },
            payload,
            error: None,
        },
        Reply::Refused {
            kind,
            message,
            status,
        } => EditorEffectOutcome {
            status,
            payload: json!({}),
            error: Some((kind, message)),
            proof: if status == MutationStatus::Committed {
                receipt(ticket, action, &json!({}))
            } else {
                unbound()
            },
        },
        Reply::Unsent { kind, message } => failed(kind, message),
        Reply::Lost { message } => EditorEffectOutcome {
            status: MutationStatus::Unresolved,
            payload: json!({}),
            error: Some((ErrorKind::Uncertain, message)),
            proof: unbound(),
        },
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

/// Read every frame of a capture back from the probe in bounded chunks.
fn fetch_frames(
    bridge: &EditorBridge,
    described: &Value,
    idempotency_id: &str,
) -> Result<Vec<Vec<u8>>, OutcomeError> {
    crate::capture::fetch_frames(described, |chunk| {
        ask(
            bridge,
            "runtime.capture.chunk",
            chunk,
            idempotency_id,
            CAPTURE_TIMEOUT,
        )
    })
}
