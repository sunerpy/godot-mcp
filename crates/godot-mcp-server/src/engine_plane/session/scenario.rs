//! Game speed, image comparison and test scenarios: the runtime effects of
//! `runtime.time_scale.set`, `runtime.image.compare` and `test.scenario.run`.
//!
//! A time-scale change is a write to `Engine.time_scale` through the probe's
//! own `runtime.properties.set`, read back in the game like every other write;
//! a read-back that differs from the request is uncertain. An image
//! comparison uploads the reference PNG to the probe in bounded chunks and has
//! the probe capture the root viewport after its next drawn frame and compare
//! the two with Godot's own `Image` API; the server reads the capture, and a
//! difference image when pixels differ, back in chunks and keeps them as
//! artifacts, as a screenshot is kept.
//!
//! A scenario runs its steps one after another against one session, each
//! through the probe method of the tool it mirrors, under one deadline that
//! withdraws whatever runs when it passes, and answers a report that is also
//! kept as a JSON artifact. Its property assertions are decided by the probe
//! from the live value with the comparisons `runtime.wait` makes, so they
//! follow the engine's own arithmetic rather than a value rounded on its way
//! to the server; a text assertion reads a Control's `text` property, the
//! text `runtime.ui.get` reports, and never reads the screen.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use godot_mcp_bridge::protocol::envelope::OperationClass;
use godot_mcp_bridge::protocol::mutation::MutationStatus;
use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::contracts::{ArtifactKind, OutcomeError};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::artifacts::sha256_hex;
use crate::editor::bridge::{BridgeAnswer, BridgeFailure, EditorBridge};
use crate::effect::boundary::CancelSignal;

/// The most steps one scenario runs.
pub(crate) const MAX_STEPS: usize = 64;
/// The most screenshots and image comparisons one scenario makes.
pub(crate) const MAX_IMAGE_STEPS: usize = 16;
/// The longest a scenario may run, and how long it may run by default.
pub(crate) const MAX_DURATION_MS: u64 = 600_000;
pub(crate) const DEFAULT_DURATION_MS: u64 = 60_000;
/// The largest reference image, and the most reference bytes one call reads.
pub(crate) const MAX_REFERENCE_BYTES: u64 = 16 * 1024 * 1024;
pub(crate) const MAX_REFERENCE_TOTAL: u64 = 64 * 1024 * 1024;
/// The fastest time scale; 0 stops game time.
pub(crate) const MAX_TIME_SCALE: f64 = 100.0;
/// The most instances one stress window adds, its longest window, its widest
/// sampling interval and their defaults.
pub(crate) const MAX_STRESS_INSTANCES: u64 = 1_000;
pub(crate) const MAX_STRESS_MS: u64 = 10_000;
pub(crate) const DEFAULT_STRESS_MS: u64 = 1_000;
pub(crate) const MAX_STRESS_INTERVAL_MS: u64 = 1_000;
pub(crate) const DEFAULT_STRESS_INTERVAL_MS: u64 = 100;

/// How much longer than its own duration one probe request may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
/// What a capture, a comparison and an instancing need beyond that: the
/// probe decodes, compares and encodes images on the game's main thread.
const CAPTURE_NEED: Duration = Duration::from_secs(15);
const COMPARE_NEED: Duration = Duration::from_secs(45);
const STRESS_NEED: Duration = Duration::from_secs(30);
/// How long the probe has to answer a request withdrawn at the deadline.
const WITHDRAWAL_GRACE: Duration = Duration::from_secs(10);
/// The most bytes of one reference chunk: the probe's own chunk size.
const UPLOAD_CHUNK: usize = 1024 * 1024;
/// The most bytes one of a step's `expected`, `actual` and `detail` reports.
const MAX_FIELD_BYTES: usize = 64 * 1024;
/// How often the deadline watch looks at the caller's signal.
const WATCH_POLL: Duration = Duration::from_millis(20);

/// What a time-scale change answers about itself: it is a live change.
pub(crate) const LIVE_ONLY: &str = "This changes the running game only: nothing is written to the \
                                    project or saved, and Engine.time_scale keeps this value until \
                                    it is changed again or the session ends.";

/// What a scenario asks of its session's probe: one request at a time, each
/// withdrawn when its signal fires.
pub(crate) trait Probe {
    /// Send one request and wait for its answer.
    fn request(
        &self,
        method: &str,
        class: OperationClass,
        payload: &Value,
        timeout: Duration,
        cancel: &CancelSignal,
    ) -> Result<BridgeAnswer, BridgeFailure>;

    /// What the probe said about itself when it connected.
    fn description(&self) -> Option<Value>;
}

/// Where the artifacts of one effect are kept.
pub(crate) trait Keeper {
    /// Keep `bytes` as an artifact of the effect, told apart from its other
    /// artifacts by `salt`, and answer its descriptor: `uri`, `media_type`,
    /// `byte_length`, `sha256` and `expires_at_unix_ms`.
    fn keep(
        &self,
        salt: &str,
        kind: ArtifactKind,
        media_type: &str,
        bytes: Vec<u8>,
    ) -> Result<Value, OutcomeError>;
}

/// A managed game session's probe, reached through its bridge.
pub(crate) struct BridgeProbe {
    bridge: Arc<EditorBridge>,
    idempotency_id: String,
}

impl BridgeProbe {
    pub(crate) fn new(bridge: Arc<EditorBridge>, idempotency_id: &str) -> Self {
        Self {
            bridge,
            idempotency_id: idempotency_id.to_owned(),
        }
    }
}

impl Probe for BridgeProbe {
    fn request(
        &self,
        method: &str,
        class: OperationClass,
        payload: &Value,
        timeout: Duration,
        cancel: &CancelSignal,
    ) -> Result<BridgeAnswer, BridgeFailure> {
        self.bridge.call_cancellable(
            method,
            class,
            payload,
            &self.idempotency_id,
            timeout,
            cancel,
        )
    }

    fn description(&self) -> Option<Value> {
        self.bridge.status().description
    }
}

fn outcome_error(kind: ErrorKind, message: impl Into<String>) -> OutcomeError {
    OutcomeError::new(kind, message, false, BTreeMap::new())
}

/// How one probe request ended.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Reply {
    /// A result, with the mutation status it carried.
    Done {
        status: MutationStatus,
        payload: Value,
    },
    /// A refusal: the probe's classification, and the status it carried.
    Refused {
        kind: ErrorKind,
        message: String,
        status: MutationStatus,
    },
    /// Nothing was sent: no probe is connected, or it lacks the method.
    Unsent { kind: ErrorKind, message: String },
    /// Sent and not answered: whether it ran is not decidable.
    Lost { message: String },
}

fn classify(answer: Result<BridgeAnswer, BridgeFailure>) -> Reply {
    match answer {
        Ok(BridgeAnswer::Result { status, payload }) => Reply::Done { status, payload },
        Ok(BridgeAnswer::Error {
            kind,
            message,
            status,
        }) => Reply::Refused {
            kind,
            message,
            status: status.unwrap_or(MutationStatus::NotStarted),
        },
        Err(failure) if failure.dispatched() => Reply::Lost {
            message: failure.probe_message(),
        },
        Err(failure @ BridgeFailure::Unsupported(_)) => Reply::Unsent {
            kind: ErrorKind::Unsupported,
            message: failure.probe_message(),
        },
        Err(failure) => Reply::Unsent {
            kind: ErrorKind::Unavailable,
            message: failure.probe_message(),
        },
    }
}

/// A request that did not end in a result, as an error: a lost answer and a
/// refusal the probe itself left unresolved are uncertain.
fn reply_error(reply: Reply) -> OutcomeError {
    match reply {
        Reply::Done { .. } => outcome_error(ErrorKind::Internal, "the probe answered a result"),
        Reply::Refused {
            message,
            status: MutationStatus::Unresolved,
            ..
        }
        | Reply::Lost { message } => outcome_error(ErrorKind::Uncertain, message),
        Reply::Refused { kind, message, .. } | Reply::Unsent { kind, message } => {
            outcome_error(kind, message)
        }
    }
}

/// The requests of one effect: each is withdrawn when `signal` fires, and
/// none is given longer than the effect's deadline allows.
pub(crate) struct Calls<'a> {
    probe: &'a dyn Probe,
    signal: CancelSignal,
    deadline: Option<Instant>,
}

impl<'a> Calls<'a> {
    pub(crate) fn new(
        probe: &'a dyn Probe,
        signal: CancelSignal,
        deadline: Option<Instant>,
    ) -> Self {
        Self {
            probe,
            signal,
            deadline,
        }
    }

    /// How long a request that runs for `need` on the probe may take.
    fn timeout(&self, need: Duration) -> Duration {
        let wanted = need + PROBE_TIMEOUT;
        match self.deadline {
            Some(deadline) => {
                wanted.min(deadline.saturating_duration_since(Instant::now()) + WITHDRAWAL_GRACE)
            }
            None => wanted,
        }
    }

    fn ask(&self, method: &str, class: OperationClass, payload: &Value, need: Duration) -> Reply {
        classify(
            self.probe
                .request(method, class, payload, self.timeout(need), &self.signal),
        )
    }

    fn withdrawn(&self) -> bool {
        self.signal.is_cancelled()
    }

    /// Read every frame of the capture `described` back in bounded chunks.
    fn frames(&self, described: &Value) -> Result<Vec<Vec<u8>>, OutcomeError> {
        crate::capture::fetch_frames(described, |chunk| {
            if self.withdrawn() {
                return Err(outcome_error(
                    ErrorKind::Cancelled,
                    "the capture was withdrawn before it was read back",
                ));
            }
            match self.ask(
                "runtime.capture.chunk",
                OperationClass::Read,
                chunk,
                CAPTURE_NEED,
            ) {
                Reply::Done { payload, .. } => Ok(payload),
                other => Err(reply_error(other)),
            }
        })
    }
}

// --- the time scale --------------------------------------------------------------

/// How a time-scale write ended.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TimeScale {
    /// The scale reads back as asked: the answer.
    Set(Value),
    /// The write was made and reads back otherwise, or its answer does not
    /// say how it reads back: the answer, and why it is uncertain.
    Unsure { answer: Value, message: String },
    /// The request ended without a result.
    Other(Reply),
}

/// Write `value` to `Engine.time_scale` with the probe's own property write
/// and read it back, as `runtime.properties.set` does.
pub(crate) fn set_time_scale(calls: &Calls<'_>, value: f64) -> TimeScale {
    let request = json!({
        "singleton": "Engine",
        "properties": { "time_scale": { "type": "float", "value": value } }
    });
    let payload = match calls.ask(
        "runtime.properties.set",
        OperationClass::Mutation,
        &request,
        Duration::ZERO,
    ) {
        Reply::Done { payload, .. } => payload,
        other => return TimeScale::Other(other),
    };
    let written = payload
        .get("results")
        .and_then(Value::as_array)
        .and_then(|results| {
            results
                .iter()
                .find(|result| result.get("property").and_then(Value::as_str) == Some("time_scale"))
        });
    let number = |field: &str| {
        written
            .and_then(|result| result.get(field))
            .and_then(|tagged| tagged.get("value"))
            .and_then(Value::as_f64)
    };
    let (before, after) = (number("before"), number("after"));
    // The probe compares the value it reads back with the request in the
    // engine, at full precision; the numbers here are what it reported.
    let matched = written
        .and_then(|result| result.get("matched"))
        .and_then(Value::as_bool);
    let mut answer = Map::new();
    answer.insert("before".to_owned(), json!(before));
    answer.insert("requested".to_owned(), json!(value));
    answer.insert("after".to_owned(), json!(after));
    answer.insert("matched".to_owned(), json!(matched == Some(true)));
    answer.insert("persisted".to_owned(), json!(false));
    answer.insert("note".to_owned(), json!(LIVE_ONLY));
    for key in ["revision", "frame"] {
        if let Some(observed) = payload.get(key) {
            answer.insert(key.to_owned(), observed.clone());
        }
    }
    let answer = Value::Object(answer);
    match matched {
        Some(true) => TimeScale::Set(answer),
        Some(false) => TimeScale::Unsure {
            message: format!(
                "Engine.time_scale reads back as {} after {value} was asked for (it was {} \
                 before): whether the game runs at the requested speed is uncertain",
                after.map_or_else(|| "an unknown value".to_owned(), |after| after.to_string()),
                before.map_or_else(|| "unknown".to_owned(), |before| before.to_string()),
            ),
            answer,
        },
        None => TimeScale::Unsure {
            message: "the probe committed the write and its answer does not say how \
                      Engine.time_scale reads back afterwards"
                .to_owned(),
            answer,
        },
    }
}

// --- image comparison ------------------------------------------------------------

/// What one image comparison answered.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Compared {
    /// The probe's verdict and measurements.
    pub(crate) result: Map<String, Value>,
    /// The capture's artifact.
    pub(crate) capture: Value,
    /// The difference image's artifact, when pixels differ and one was asked
    /// for and kept, and why one that was made was not kept.
    pub(crate) difference: Option<Value>,
    pub(crate) difference_error: Option<String>,
}

impl Compared {
    /// Whether the two images are of one size and no pixel differs.
    pub(crate) fn matched(&self) -> bool {
        self.result.get("matched") == Some(&Value::Bool(true))
    }
}

/// The fields of the probe's comparison answer the tool reports.
const COMPARISON_FIELDS: &[&str] = &[
    "matched",
    "reason",
    "same_size",
    "capture_size",
    "reference",
    "tolerance",
    "total_pixels",
    "differing_pixels",
    "ratio",
    "max_channel_difference",
    "bounding_box",
    "revision",
    "frame",
];

/// Upload `reference` as upload `name` in bounded chunks.
fn upload(calls: &Calls<'_>, name: &str, reference: &[u8]) -> Result<(), OutcomeError> {
    let sha256 = sha256_hex(reference);
    let mut offset = 0_usize;
    let mut complete = false;
    for chunk in reference.chunks(UPLOAD_CHUNK) {
        if calls.withdrawn() {
            return Err(outcome_error(
                ErrorKind::Cancelled,
                "the comparison was withdrawn while its reference image was uploaded",
            ));
        }
        let request = json!({
            "upload": name,
            "offset": offset,
            "total": reference.len(),
            "sha256": sha256,
            "data_base64": crate::capture::base64_encode(chunk),
        });
        let answered = match calls.ask(
            "runtime.image.reference",
            OperationClass::Read,
            &request,
            Duration::ZERO,
        ) {
            Reply::Done { payload, .. } => payload,
            other => return Err(reply_error(other)),
        };
        offset += chunk.len();
        if answered.get("received").and_then(Value::as_u64) != Some(offset as u64) {
            return Err(outcome_error(
                ErrorKind::Internal,
                "the probe did not take the reference image as it was sent",
            ));
        }
        complete = answered.get("complete") == Some(&Value::Bool(true));
    }
    if complete {
        Ok(())
    } else {
        Err(outcome_error(
            ErrorKind::Internal,
            "the probe did not complete the reference image upload",
        ))
    }
}

/// Compare the root viewport after the probe's next drawn frame with
/// `reference`, keep the capture (and a difference image when pixels differ
/// and `difference` asks for one) and answer what the probe found. `name`
/// names the comparison's upload and sets its artifacts apart.
pub(crate) fn compare_image(
    calls: &Calls<'_>,
    keeper: &dyn Keeper,
    name: &str,
    reference: &[u8],
    tolerance: u8,
    difference: bool,
) -> Result<Compared, OutcomeError> {
    upload(calls, name, reference)?;
    if calls.withdrawn() {
        return Err(outcome_error(
            ErrorKind::Cancelled,
            "the comparison was withdrawn before the capture",
        ));
    }
    let described = match calls.ask(
        "runtime.image.compare",
        OperationClass::Read,
        &json!({ "upload": name, "tolerance": tolerance, "difference": difference }),
        COMPARE_NEED,
    ) {
        Reply::Done { payload, .. } => payload,
        other => return Err(reply_error(other)),
    };
    let mut frames = calls.frames(&described)?.into_iter();
    let capture_png = frames
        .next()
        .ok_or_else(|| outcome_error(ErrorKind::Internal, "the comparison kept no capture"))?;
    let size = described
        .get("capture_size")
        .cloned()
        .unwrap_or(Value::Null);
    let mut capture = keeper.keep(
        &format!("{name}/capture"),
        ArtifactKind::Screenshot,
        "image/png",
        capture_png,
    )?;
    capture["role"] = json!("capture");
    capture["width"] = size.get("width").cloned().unwrap_or(Value::Null);
    capture["height"] = size.get("height").cloned().unwrap_or(Value::Null);
    let mut kept_difference = None;
    let mut difference_error = None;
    if let Some(png) = frames.next() {
        match keeper.keep(
            &format!("{name}/difference"),
            ArtifactKind::Screenshot,
            "image/png",
            png,
        ) {
            Ok(mut artifact) => {
                artifact["role"] = json!("difference");
                artifact["width"] = capture["width"].clone();
                artifact["height"] = capture["height"].clone();
                kept_difference = Some(artifact);
            }
            Err(error) => difference_error = Some(error.message().to_owned()),
        }
    }
    let mut result = Map::new();
    for key in COMPARISON_FIELDS {
        if let Some(value) = described.get(*key) {
            result.insert((*key).to_owned(), value.clone());
        }
    }
    Ok(Compared {
        result,
        capture,
        difference: kept_difference,
        difference_error,
    })
}

// --- scenarios ---------------------------------------------------------------------

/// A scenario as the tool checked it, each step in the probe's spelling.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Plan {
    pub(crate) stop_on_failure: bool,
    pub(crate) max_duration_ms: u64,
    pub(crate) steps: Vec<Planned>,
}

/// One step of a plan: its unique id and what it does.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Planned {
    pub(crate) id: String,
    pub(crate) action: Action,
}

/// What one step does, through the probe method of the tool it mirrors.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Action {
    /// `runtime.input.sequence` with these probe steps, waiting `wait_ms`
    /// in all, as `input.sequence.send`.
    Input { steps: Vec<Value>, wait_ms: u64 },
    /// `runtime.wait` with this request, as `runtime.wait`.
    Wait {
        request: Map<String, Value>,
        timeout_ms: u64,
    },
    /// `runtime.properties.set` with this request, as
    /// `runtime.properties.set`.
    SetProperty { request: Map<String, Value> },
    /// `runtime.ui.press` with this request, as `runtime.ui.press`.
    Press {
        request: Map<String, Value>,
        hold_ms: u64,
    },
    /// `Engine.time_scale`, as `runtime.time_scale.set`.
    TimeScale { time_scale: f64 },
    /// `runtime.scenario.check` of a property; `expected` is what the
    /// report says was expected.
    AssertProperty {
        request: Map<String, Value>,
        expected: Value,
    },
    /// `runtime.scenario.check` of a Control's text.
    AssertUiText {
        request: Map<String, Value>,
        expected: Value,
    },
    /// `runtime.capture.screenshot`, as `runtime.screenshot.capture`.
    Screenshot,
    /// An image comparison, as `runtime.image.compare`.
    CompareImage {
        reference: Reference,
        tolerance: u8,
        difference: bool,
    },
    /// `runtime.scenario.stress` with this request.
    Stress {
        request: Map<String, Value>,
        count: u64,
        duration_ms: u64,
    },
}

impl Action {
    /// The step kind, as the caller spelled it.
    pub(crate) const fn kind(&self) -> &'static str {
        match self {
            Self::Input { .. } => "input",
            Self::Wait { .. } => "wait",
            Self::SetProperty { .. } => "set_property",
            Self::Press { .. } => "press",
            Self::TimeScale { .. } => "time_scale",
            Self::AssertProperty { .. } => "assert_property",
            Self::AssertUiText { .. } => "assert_ui_text",
            Self::Screenshot => "screenshot",
            Self::CompareImage { .. } => "compare_image",
            Self::Stress { .. } => "stress",
        }
    }

    /// The probe methods the step sends.
    pub(crate) const fn methods(&self) -> &'static [&'static str] {
        match self {
            Self::Input { .. } => &["runtime.input.sequence"],
            Self::Wait { .. } => &["runtime.wait"],
            Self::SetProperty { .. } | Self::TimeScale { .. } => &["runtime.properties.set"],
            Self::Press { .. } => &["runtime.ui.press"],
            Self::AssertProperty { .. } | Self::AssertUiText { .. } => &["runtime.scenario.check"],
            Self::Screenshot => &["runtime.capture.screenshot", "runtime.capture.chunk"],
            Self::CompareImage { .. } => &[
                "runtime.image.reference",
                "runtime.image.compare",
                "runtime.capture.chunk",
            ],
            Self::Stress { .. } => &["runtime.scenario.stress"],
        }
    }
}

/// What an image comparison step compares with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Reference {
    /// A project PNG, read by the tool before anything ran.
    Path { path: String },
    /// The screenshot an earlier step took.
    Step { step: String },
}

/// How one step ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    /// It ran, and what it checks held.
    Passed,
    /// It ran, and what it checks did not hold.
    Failed,
    /// It could not run.
    Error,
    /// It did not run: an earlier step stopped the scenario.
    Skipped,
    /// Its outcome cannot be decided; the scenario stops.
    Uncertain,
}

impl Status {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Error => "error",
            Self::Skipped => "skipped",
            Self::Uncertain => "uncertain",
        }
    }
}

/// What one step answers.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Outcome {
    pub(crate) status: Status,
    expected: Option<Value>,
    actual: Option<Value>,
    detail: Option<Value>,
    error: Option<(ErrorKind, String)>,
    reason: Option<String>,
    artifacts: Vec<Value>,
}

impl Outcome {
    fn of(status: Status) -> Self {
        Self {
            status,
            expected: None,
            actual: None,
            detail: None,
            error: None,
            reason: None,
            artifacts: Vec::new(),
        }
    }

    fn passed(detail: Value) -> Self {
        Self::of(Status::Passed).detail(detail)
    }

    fn failed(expected: Value, actual: Value, reason: impl Into<String>) -> Self {
        Self {
            expected: Some(expected),
            actual: Some(actual),
            reason: Some(reason.into()),
            ..Self::of(Status::Failed)
        }
    }

    fn error(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            error: Some((kind, message.into())),
            ..Self::of(Status::Error)
        }
    }

    fn uncertain(reason: impl Into<String>) -> Self {
        Self {
            reason: Some(reason.into()),
            ..Self::of(Status::Uncertain)
        }
    }

    fn skipped(reason: &str) -> Self {
        Self {
            reason: Some(reason.to_owned()),
            ..Self::of(Status::Skipped)
        }
    }

    fn detail(mut self, detail: Value) -> Self {
        self.detail = Some(detail);
        self
    }

    fn expecting(mut self, expected: Value, actual: Value) -> Self {
        self.expected = Some(expected);
        self.actual = Some(actual);
        self
    }

    fn artifact(mut self, artifact: Value) -> Self {
        self.artifacts.push(artifact);
        self
    }

    /// A request that ended without a result: what was withdrawn or lost is
    /// uncertain, anything else could not run.
    fn of_error(error: &OutcomeError) -> Self {
        match error.kind() {
            ErrorKind::Uncertain | ErrorKind::Cancelled => Self::uncertain(error.message()),
            kind => Self::error(kind, error.message()),
        }
    }

    fn of_reply(reply: Reply) -> Self {
        Self::of_error(&reply_error(reply))
    }
}

/// Why a scenario stopped before its last step, or at it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stop {
    /// A step failed or could not run, and the scenario stops on failure.
    Failure,
    /// A step's outcome could not be decided.
    Uncertain,
    /// The scenario reached its longest duration.
    Deadline,
    /// The caller cancelled, or the session was told to stop.
    Cancelled,
}

impl Stop {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Failure => "failure",
            Self::Uncertain => "uncertain",
            Self::Deadline => "deadline",
            Self::Cancelled => "cancelled",
        }
    }

    const fn skip_reason(self) -> &'static str {
        match self {
            Self::Failure => {
                "an earlier step failed or could not run, and the scenario stops on failure"
            }
            Self::Uncertain => "an earlier step's outcome could not be decided",
            Self::Deadline => "the scenario reached its maxDurationMs",
            Self::Cancelled => "the scenario was cancelled",
        }
    }
}

/// The overall verdict of a scenario whose steps ended so: passed only when
/// every step passed; otherwise failed when an assertion did not hold, error
/// when none failed and a step could not run, and uncertain when neither
/// happened and a step's outcome could not be decided or a step did not run.
pub(crate) fn verdict(statuses: &[Status]) -> &'static str {
    if !statuses.is_empty() && statuses.iter().all(|status| *status == Status::Passed) {
        "passed"
    } else if statuses.contains(&Status::Failed) {
        "failed"
    } else if statuses.contains(&Status::Error) {
        "error"
    } else {
        "uncertain"
    }
}

/// A scenario's own withdrawal: it fires when the caller's signal does or the
/// scenario's deadline passes, and says which came first.
struct Watch {
    token: tokio_util::sync::CancellationToken,
    shared: Arc<(Mutex<Watched>, Condvar)>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[derive(Debug, Default)]
struct Watched {
    finished: bool,
    fired: Option<Stop>,
}

impl Watch {
    fn start(caller: CancelSignal, deadline: Instant) -> Result<Self, OutcomeError> {
        let token = tokio_util::sync::CancellationToken::new();
        let shared = Arc::new((Mutex::new(Watched::default()), Condvar::new()));
        let (fires, state) = (token.clone(), Arc::clone(&shared));
        let thread = std::thread::Builder::new()
            .name("scenario-deadline".to_owned())
            .spawn(move || {
                let (lock, changed) = &*state;
                let mut watched = lock.lock().unwrap_or_else(PoisonError::into_inner);
                loop {
                    if watched.finished {
                        return;
                    }
                    let fired = if caller.is_cancelled() {
                        Some(Stop::Cancelled)
                    } else if Instant::now() >= deadline {
                        Some(Stop::Deadline)
                    } else {
                        None
                    };
                    if let Some(stop) = fired {
                        watched.fired = Some(stop);
                        fires.cancel();
                        return;
                    }
                    let wait = deadline
                        .saturating_duration_since(Instant::now())
                        .min(WATCH_POLL);
                    watched = changed
                        .wait_timeout(watched, wait)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0;
                }
            })
            .map_err(|error| {
                outcome_error(
                    ErrorKind::Internal,
                    format!("the scenario's deadline could not be watched: {error}"),
                )
            })?;
        Ok(Self {
            token,
            shared,
            thread: Some(thread),
        })
    }

    fn signal(&self) -> CancelSignal {
        CancelSignal::of(&self.token)
    }

    fn fired(&self) -> Option<Stop> {
        self.shared
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .fired
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        {
            let (lock, changed) = &*self.shared;
            lock.lock().unwrap_or_else(PoisonError::into_inner).finished = true;
            changed.notify_all();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn elapsed_ms(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// `value`, or what stands for it when it is larger than one report field
/// may be.
fn bounded(value: Option<Value>) -> Value {
    let Some(value) = value else {
        return Value::Null;
    };
    let text = value.to_string();
    if text.len() <= MAX_FIELD_BYTES {
        return value;
    }
    json!({
        "truncated": true,
        "byte_length": text.len(),
        "sha256": sha256_hex(text.as_bytes()),
    })
}

/// The named fields of `payload`, in that order.
fn pick(payload: &Value, keys: &[&str]) -> Value {
    let mut out = Map::new();
    for key in keys {
        if let Some(value) = payload.get(*key) {
            out.insert((*key).to_owned(), value.clone());
        }
    }
    Value::Object(out)
}

fn record(
    index: usize,
    planned: &Planned,
    started_ms: u64,
    elapsed_ms: u64,
    outcome: Outcome,
) -> Value {
    json!({
        "index": index,
        "id": planned.id,
        "kind": planned.action.kind(),
        "status": outcome.status.as_str(),
        "started_ms": started_ms,
        "elapsed_ms": elapsed_ms,
        "expected": bounded(outcome.expected),
        "actual": bounded(outcome.actual),
        "detail": bounded(outcome.detail),
        "error": outcome
            .error
            .map(|(kind, message)| json!({ "kind": kind.code(), "message": message })),
        "reason": outcome.reason,
        "artifacts": outcome.artifacts,
    })
}

/// What runs the steps of one scenario.
struct Runner<'a> {
    calls: Calls<'a>,
    keeper: &'a dyn Keeper,
    references: &'a BTreeMap<String, Vec<u8>>,
    /// The ids of the screenshot steps a later comparison names, and the
    /// screenshots they took.
    referenced: BTreeSet<String>,
    screenshots: BTreeMap<String, Vec<u8>>,
    /// Whether the session renders, as its probe described it.
    renders: Option<bool>,
}

/// Run `plan` against the session `probe` serves and answer its report: one
/// record per step, in order, and the verdict. `references` holds the bytes of
/// every project PNG a comparison names, by path.
pub(crate) fn run_scenario(
    probe: &dyn Probe,
    keeper: &dyn Keeper,
    plan: &Plan,
    references: &BTreeMap<String, Vec<u8>>,
    caller: &CancelSignal,
) -> Result<Value, OutcomeError> {
    let started = Instant::now();
    let deadline = started + Duration::from_millis(plan.max_duration_ms);
    let watch = Watch::start(caller.clone(), deadline)?;
    let description = probe.description().unwrap_or(Value::Null);
    let mut runner = Runner {
        calls: Calls::new(probe, watch.signal(), Some(deadline)),
        keeper,
        references,
        referenced: plan
            .steps
            .iter()
            .filter_map(|planned| match &planned.action {
                Action::CompareImage {
                    reference: Reference::Step { step },
                    ..
                } => Some(step.clone()),
                _ => None,
            })
            .collect(),
        screenshots: BTreeMap::new(),
        renders: description.get("renders").and_then(Value::as_bool),
    };
    // Whether the run is withdrawn now: what the watch fired for, or what it
    // is about to, read at once rather than when its thread next looks.
    let interrupted = || {
        watch.fired().or_else(|| {
            if caller.is_cancelled() {
                Some(Stop::Cancelled)
            } else if Instant::now() >= deadline {
                Some(Stop::Deadline)
            } else {
                None
            }
        })
    };
    let mut records = Vec::with_capacity(plan.steps.len());
    let mut statuses = Vec::with_capacity(plan.steps.len());
    let mut stop: Option<Stop> = None;
    for (index, planned) in plan.steps.iter().enumerate() {
        if stop.is_none() {
            stop = interrupted();
        }
        let at = elapsed_ms(started);
        let outcome = match stop {
            Some(stopped) => Outcome::skipped(stopped.skip_reason()),
            None => {
                let outcome = runner.step(planned);
                stop = match (outcome.status, interrupted()) {
                    (Status::Passed, _) => None,
                    // Whatever did not pass while the deadline or a
                    // cancellation withdrew it ended for that reason.
                    (_, Some(fired)) => Some(fired),
                    (Status::Uncertain, None) => Some(Stop::Uncertain),
                    (Status::Failed | Status::Error, None) if plan.stop_on_failure => {
                        Some(Stop::Failure)
                    }
                    _ => None,
                };
                outcome
            }
        };
        statuses.push(outcome.status);
        records.push(record(
            index,
            planned,
            at,
            elapsed_ms(started).saturating_sub(at),
            outcome,
        ));
    }
    drop(watch);
    let count = |status: Status| statuses.iter().filter(|seen| **seen == status).count();
    Ok(json!({
        "verdict": verdict(&statuses),
        "stopped_by": stop.map(Stop::as_str),
        "stop_on_failure": plan.stop_on_failure,
        "max_duration_ms": plan.max_duration_ms,
        "elapsed_ms": elapsed_ms(started),
        "counts": {
            "total": statuses.len(),
            "passed": count(Status::Passed),
            "failed": count(Status::Failed),
            "error": count(Status::Error),
            "skipped": count(Status::Skipped),
            "uncertain": count(Status::Uncertain),
        },
        "godot_version": description.get("godot_version").cloned().unwrap_or(Value::Null),
        "steps": records,
    }))
}

impl Runner<'_> {
    fn step(&mut self, planned: &Planned) -> Outcome {
        match &planned.action {
            Action::Input { steps, wait_ms } => self.input(steps, *wait_ms),
            Action::Wait {
                request,
                timeout_ms,
            } => self.wait(request, *timeout_ms),
            Action::SetProperty { request } => self.set_property(request),
            Action::Press { request, hold_ms } => self.press(request, *hold_ms),
            Action::TimeScale { time_scale } => self.time_scale(*time_scale),
            Action::AssertProperty { request, expected } => self.check(request, expected, true),
            Action::AssertUiText { request, expected } => self.check(request, expected, false),
            Action::Screenshot => self.screenshot(&planned.id),
            Action::CompareImage {
                reference,
                tolerance,
                difference,
            } => self.compare(&planned.id, reference, *tolerance, *difference),
            Action::Stress {
                request,
                count,
                duration_ms,
            } => self.stress(request, *count, *duration_ms),
        }
    }

    fn input(&self, steps: &[Value], wait_ms: u64) -> Outcome {
        let answered = match self.calls.ask(
            "runtime.input.sequence",
            OperationClass::Mutation,
            &json!({ "steps": steps }),
            Duration::from_millis(wait_ms),
        ) {
            Reply::Done { payload, .. } => payload,
            other => return Outcome::of_reply(other),
        };
        let detail = pick(&answered, &["steps_run", "cancelled", "held"]);
        let run = answered.get("steps_run").and_then(Value::as_u64);
        if answered.get("cancelled") == Some(&Value::Bool(true)) {
            Outcome::uncertain(format!(
                "the sequence was withdrawn after {} of {} steps; what it pressed was released",
                run.unwrap_or(0),
                steps.len()
            ))
            .detail(detail)
        } else if run == Some(steps.len() as u64) {
            Outcome::passed(detail)
        } else {
            Outcome::uncertain("the probe did not say that every input step was sent")
                .detail(detail)
        }
    }

    fn wait(&self, request: &Map<String, Value>, timeout_ms: u64) -> Outcome {
        let answered = match self.calls.ask(
            "runtime.wait",
            OperationClass::Read,
            &Value::Object(request.clone()),
            Duration::from_millis(timeout_ms),
        ) {
            Reply::Done { payload, .. } => payload,
            other => return Outcome::of_reply(other),
        };
        let detail = pick(
            &answered,
            &[
                "condition",
                "outcome",
                "satisfied",
                "elapsed_ms",
                "frames_waited",
                "timeout_ms",
                "observed",
            ],
        );
        match answered.get("outcome").and_then(Value::as_str) {
            Some("satisfied") => Outcome::passed(detail),
            Some("timeout") => Outcome::uncertain(format!(
                "the condition did not hold within {timeout_ms} ms: the wait ended without a \
                 decision"
            ))
            .detail(detail),
            Some(other) => Outcome::uncertain(format!(
                "the wait ended ({other}) before its condition held"
            ))
            .detail(detail),
            None => Outcome::uncertain("the probe did not say how the wait ended").detail(detail),
        }
    }

    fn set_property(&self, request: &Map<String, Value>) -> Outcome {
        let answered = match self.calls.ask(
            "runtime.properties.set",
            OperationClass::Mutation,
            &Value::Object(request.clone()),
            Duration::ZERO,
        ) {
            Reply::Done { payload, .. } => payload,
            other => return Outcome::of_reply(other),
        };
        let detail = pick(&answered, &["target", "class", "results", "all_matched"]);
        if answered.get("all_matched") == Some(&Value::Bool(true)) {
            return Outcome::passed(detail);
        }
        let mut expected = Map::new();
        let mut actual = Map::new();
        for result in answered
            .get("results")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(property) = result.get("property").and_then(Value::as_str) {
                expected.insert(
                    property.to_owned(),
                    result.get("requested").cloned().unwrap_or(Value::Null),
                );
                actual.insert(
                    property.to_owned(),
                    result.get("after").cloned().unwrap_or(Value::Null),
                );
            }
        }
        Outcome::failed(
            Value::Object(expected),
            Value::Object(actual),
            "a property holds another value after the write: its setter changed or refused the \
             value",
        )
        .detail(detail)
    }

    fn press(&self, request: &Map<String, Value>, hold_ms: u64) -> Outcome {
        let answered = match self.calls.ask(
            "runtime.ui.press",
            OperationClass::Mutation,
            &Value::Object(request.clone()),
            Duration::from_millis(hold_ms),
        ) {
            Reply::Done { payload, .. } => payload,
            other => return Outcome::of_reply(other),
        };
        let detail = pick(
            &answered,
            &[
                "node",
                "text",
                "position",
                "outcome",
                "pressed_signal",
                "emissions",
                "button_pressed",
                "present",
            ],
        );
        match answered.get("outcome").and_then(Value::as_str) {
            Some("clicked") if answered.get("pressed_signal") == Some(&Value::Bool(true)) => {
                Outcome::passed(detail)
            }
            Some("clicked") => Outcome::failed(
                json!({ "pressed_signal": true }),
                json!({
                    "pressed_signal": false,
                    "emissions": answered.get("emissions").cloned().unwrap_or(Value::Null),
                }),
                "the button was clicked and its pressed signal did not fire",
            )
            .detail(detail),
            Some(other) => Outcome::uncertain(format!(
                "the press ended ({other}) before the button was released"
            ))
            .detail(detail),
            None => Outcome::uncertain("the probe did not say how the press ended").detail(detail),
        }
    }

    fn time_scale(&self, value: f64) -> Outcome {
        match set_time_scale(&self.calls, value) {
            TimeScale::Set(answer) => Outcome::passed(answer),
            TimeScale::Unsure { answer, message } => Outcome::uncertain(message).detail(answer),
            TimeScale::Other(reply) => Outcome::of_reply(reply),
        }
    }

    /// An assertion: the probe decides whether it holds.
    fn check(&self, request: &Map<String, Value>, expected: &Value, property: bool) -> Outcome {
        let answered = match self.calls.ask(
            "runtime.scenario.check",
            OperationClass::Read,
            &Value::Object(request.clone()),
            Duration::ZERO,
        ) {
            Reply::Done { payload, .. } => payload,
            other => return Outcome::of_reply(other),
        };
        let actual = if property {
            pick(&answered, &["value", "special", "type", "unrepresentable"])
        } else {
            pick(&answered, &["text", "text_length", "truncated", "visible"])
        };
        let detail = pick(&answered, &["node", "singleton", "class", "property"]);
        match answered.get("holds") {
            Some(Value::Bool(true)) => Outcome::passed(detail).expecting(expected.clone(), actual),
            Some(Value::Bool(false)) => Outcome::failed(
                expected.clone(),
                actual,
                if property {
                    "the property does not compare with the expected value as asked"
                } else {
                    "the Control's text is not the expected text"
                },
            )
            .detail(detail),
            _ => Outcome::error(
                ErrorKind::InvalidInput,
                answered
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("the probe did not decide the assertion"),
            )
            .expecting(expected.clone(), actual),
        }
    }

    fn unrendered(&self) -> Option<Outcome> {
        (self.renders == Some(false)).then(|| {
            Outcome::error(
                ErrorKind::Unavailable,
                "this session renders nothing: it runs headless; start it with display window \
                 for image steps",
            )
        })
    }

    fn screenshot(&mut self, id: &str) -> Outcome {
        if let Some(refused) = self.unrendered() {
            return refused;
        }
        let described = match self.calls.ask(
            "runtime.capture.screenshot",
            OperationClass::Read,
            &json!({}),
            CAPTURE_NEED,
        ) {
            Reply::Done { payload, .. } => payload,
            other => return Outcome::of_reply(other),
        };
        let png = match self.calls.frames(&described) {
            Ok(frames) => match frames.into_iter().next() {
                Some(png) => png,
                None => return Outcome::error(ErrorKind::Internal, "the screenshot had no frame"),
            },
            Err(error) => return Outcome::of_error(&error),
        };
        if self.referenced.contains(id) {
            self.screenshots.insert(id.to_owned(), png.clone());
        }
        let mut artifact = match self.keeper.keep(
            &format!("{id}/screenshot"),
            ArtifactKind::Screenshot,
            "image/png",
            png,
        ) {
            Ok(artifact) => artifact,
            Err(error) => return Outcome::error(error.kind(), error.message()),
        };
        artifact["role"] = json!("screenshot");
        artifact["step"] = json!(id);
        artifact["width"] = described.get("width").cloned().unwrap_or(Value::Null);
        artifact["height"] = described.get("height").cloned().unwrap_or(Value::Null);
        Outcome::passed(pick(&described, &["width", "height", "revision", "frame"]))
            .artifact(artifact)
    }

    fn compare(&self, id: &str, reference: &Reference, tolerance: u8, difference: bool) -> Outcome {
        if let Some(refused) = self.unrendered() {
            return refused;
        }
        let (bytes, named) = match reference {
            Reference::Path { path } => (self.references.get(path), json!({ "path": path })),
            Reference::Step { step } => (self.screenshots.get(step), json!({ "step": step })),
        };
        let Some(bytes) = bytes else {
            return Outcome::error(
                ErrorKind::InvalidInput,
                match reference {
                    Reference::Step { step } => {
                        format!("step {step} kept no screenshot to compare with")
                    }
                    Reference::Path { path } => format!("{path} was not read before the run"),
                },
            );
        };
        let compared =
            match compare_image(&self.calls, self.keeper, id, bytes, tolerance, difference) {
                Ok(compared) => compared,
                Err(error) => return Outcome::of_error(&error),
            };
        let mut expected = named;
        expected["sha256"] = json!(sha256_hex(bytes));
        expected["tolerance"] = json!(tolerance);
        let actual = pick(
            &Value::Object(compared.result.clone()),
            &[
                "same_size",
                "capture_size",
                "reference",
                "total_pixels",
                "differing_pixels",
                "ratio",
                "max_channel_difference",
                "bounding_box",
            ],
        );
        let mut detail = Value::Object(compared.result.clone());
        if let Some(error) = &compared.difference_error {
            detail["difference_error"] = json!(error);
        }
        let mut outcome = if compared.matched() {
            Outcome::passed(detail).expecting(expected, actual)
        } else {
            Outcome::failed(
                expected,
                actual,
                compared
                    .result
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("the capture does not match the reference"),
            )
            .detail(detail)
        };
        let mut capture = compared.capture;
        capture["step"] = json!(id);
        outcome = outcome.artifact(capture);
        if let Some(mut kept) = compared.difference {
            kept["step"] = json!(id);
            outcome = outcome.artifact(kept);
        }
        outcome
    }

    fn stress(&self, request: &Map<String, Value>, count: u64, duration_ms: u64) -> Outcome {
        let answered = match self.calls.ask(
            "runtime.scenario.stress",
            OperationClass::Mutation,
            &Value::Object(request.clone()),
            Duration::from_millis(duration_ms) + STRESS_NEED,
        ) {
            Reply::Done { payload, .. } => payload,
            other => return Outcome::of_reply(other),
        };
        let detail = pick(
            &answered,
            &[
                "scene",
                "parent",
                "requested",
                "created",
                "present_during",
                "remaining",
                "freed",
                "outcome",
                "duration_ms",
                "interval_ms",
                "samples",
                "monitors",
                "counts",
            ],
        );
        let remaining = answered.get("remaining").and_then(Value::as_u64);
        let created = answered.get("created").and_then(Value::as_u64);
        match answered.get("outcome").and_then(Value::as_str) {
            Some("completed") if remaining == Some(0) && created == Some(count) => {
                Outcome::passed(detail)
            }
            Some("completed") if created == Some(count) => Outcome::failed(
                json!({ "remaining": 0 }),
                json!({ "remaining": remaining, "freed": answered.get("freed").cloned().unwrap_or(Value::Null) }),
                format!(
                    "{} of {count} instances were still there two frames after they were freed",
                    remaining.map_or_else(|| "some".to_owned(), |left| left.to_string())
                ),
            )
            .detail(detail),
            Some("completed") => Outcome::error(
                ErrorKind::Internal,
                "the probe did not add every instance it was asked to",
            )
            .detail(detail),
            Some(other) => Outcome::uncertain(format!(
                "the stress window ended ({other}) before it completed; its instances were freed"
            ))
            .detail(detail),
            None => Outcome::uncertain("the probe did not say how the stress window ended")
                .detail(detail),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    /// One scripted answer of the test probe.
    enum Scripted {
        /// Answer at once.
        Now(Result<BridgeAnswer, BridgeFailure>),
        /// Answer this payload once the request is withdrawn, as a probe ends
        /// a wait at its next frame.
        OnWithdrawal(Value),
    }

    /// A probe that answers each request with the next scripted answer for
    /// its method, and records every request.
    struct TestProbe {
        answers: Mutex<VecDeque<(&'static str, Scripted)>>,
        sent: Mutex<Vec<(String, Value)>>,
        description: Value,
    }

    impl TestProbe {
        fn new(answers: Vec<(&'static str, Scripted)>) -> Self {
            Self {
                answers: Mutex::new(answers.into()),
                sent: Mutex::new(Vec::new()),
                description: json!({ "renders": true, "godot_version": "4.7.1.stable" }),
            }
        }

        fn headless(mut self) -> Self {
            self.description["renders"] = json!(false);
            self
        }

        fn sent(&self) -> Vec<(String, Value)> {
            self.sent.lock().expect("sent").clone()
        }
    }

    impl Probe for TestProbe {
        fn request(
            &self,
            method: &str,
            _class: OperationClass,
            payload: &Value,
            _timeout: Duration,
            cancel: &CancelSignal,
        ) -> Result<BridgeAnswer, BridgeFailure> {
            self.sent
                .lock()
                .expect("sent")
                .push((method.to_owned(), payload.clone()));
            let (expected, answer) = self
                .answers
                .lock()
                .expect("answers")
                .pop_front()
                .unwrap_or_else(|| panic!("no answer is scripted for {method}"));
            assert_eq!(expected, method, "the scenario sent {method} out of order");
            match answer {
                Scripted::Now(answer) => answer,
                Scripted::OnWithdrawal(payload) => {
                    let until = Instant::now() + Duration::from_secs(10);
                    while !cancel.is_cancelled() {
                        assert!(Instant::now() < until, "the request was never withdrawn");
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Ok(BridgeAnswer::Result {
                        status: MutationStatus::NotStarted,
                        payload,
                    })
                }
            }
        }

        fn description(&self) -> Option<Value> {
            Some(self.description.clone())
        }
    }

    /// Keeps every artifact in memory; refuses the salts it is told to.
    #[derive(Default)]
    struct TestKeeper {
        kept: Mutex<Vec<(String, Vec<u8>)>>,
        refuse: Vec<&'static str>,
    }

    impl Keeper for TestKeeper {
        fn keep(
            &self,
            salt: &str,
            _kind: ArtifactKind,
            media_type: &str,
            bytes: Vec<u8>,
        ) -> Result<Value, OutcomeError> {
            if self.refuse.contains(&salt) {
                return Err(outcome_error(ErrorKind::Unavailable, "the store is full"));
            }
            let descriptor = json!({
                "uri": format!("godot-mcp://artifacts/test-{salt}"),
                "media_type": media_type,
                "byte_length": bytes.len(),
                "sha256": sha256_hex(&bytes),
            });
            self.kept
                .lock()
                .expect("kept")
                .push((salt.to_owned(), bytes));
            Ok(descriptor)
        }
    }

    fn result(payload: Value) -> Scripted {
        Scripted::Now(Ok(BridgeAnswer::Result {
            status: MutationStatus::NotStarted,
            payload,
        }))
    }

    fn committed(payload: Value) -> Scripted {
        Scripted::Now(Ok(BridgeAnswer::Result {
            status: MutationStatus::Committed,
            payload,
        }))
    }

    fn refused(kind: ErrorKind, message: &str) -> Scripted {
        Scripted::Now(Ok(BridgeAnswer::Error {
            kind,
            message: message.to_owned(),
            status: Some(MutationStatus::NotStarted),
        }))
    }

    fn step(id: &str, action: Action) -> Planned {
        Planned {
            id: id.to_owned(),
            action,
        }
    }

    fn plan(steps: Vec<Planned>) -> Plan {
        Plan {
            stop_on_failure: true,
            max_duration_ms: 60_000,
            steps,
        }
    }

    fn check_property(op: &str) -> Action {
        Action::AssertProperty {
            request: json!({ "kind": "property", "node": "Player", "property": "position", "op": op, "value": { "type": "vector2", "value": { "x": 1.0, "y": 2.0 } }, "tolerance": 0.5 })
                .as_object()
                .cloned()
                .expect("object"),
            expected: json!({ "op": "approx", "value": { "type": "vector2", "value": { "x": 1.0, "y": 2.0 } }, "tolerance": 0.5 }),
        }
    }

    fn checked(holds: Value, value: Value, special: Value) -> Scripted {
        result(json!({
            "node": "/root/Main/Player",
            "class": "Node2D",
            "property": "position",
            "op": "approx",
            "holds": holds,
            "reason": null,
            "value": value,
            "special": special,
            "type": "Vector2",
            "unrepresentable": null,
            "revision": 1,
            "frame": 9,
        }))
    }

    fn run(probe: &TestProbe, keeper: &TestKeeper, plan: &Plan) -> Value {
        run_scenario(
            probe,
            keeper,
            plan,
            &BTreeMap::new(),
            &CancelSignal::default(),
        )
        .expect("the scenario runs")
    }

    fn statuses(report: &Value) -> Vec<String> {
        report["steps"]
            .as_array()
            .expect("steps")
            .iter()
            .map(|step| step["status"].as_str().expect("status").to_owned())
            .collect()
    }

    #[test]
    fn the_verdict_is_passed_only_when_every_step_passed() {
        use Status::{Error, Failed, Passed, Skipped, Uncertain};
        assert_eq!(verdict(&[Passed, Passed]), "passed");
        assert_eq!(verdict(&[]), "uncertain", "no step is no pass");
        assert_eq!(verdict(&[Passed, Failed, Skipped]), "failed");
        assert_eq!(verdict(&[Passed, Error, Skipped]), "error");
        assert_eq!(verdict(&[Passed, Uncertain, Skipped]), "uncertain");
        assert_eq!(verdict(&[Passed, Skipped]), "uncertain");
        // A proven failure is the verdict even beside steps that could not
        // run or be decided.
        assert_eq!(verdict(&[Error, Failed, Uncertain]), "failed");
        assert_eq!(verdict(&[Error, Uncertain]), "error");
    }

    #[test]
    fn an_assertion_that_holds_passes_and_the_report_names_what_was_compared() {
        let probe = TestProbe::new(vec![(
            "runtime.scenario.check",
            checked(
                json!(true),
                json!({ "type": "vector2", "value": { "x": 1.2, "y": 2.0 } }),
                Value::Null,
            ),
        )]);
        let report = run(
            &probe,
            &TestKeeper::default(),
            &plan(vec![step("near", check_property("approx"))]),
        );
        assert_eq!(report["verdict"], json!("passed"), "{report}");
        let first = &report["steps"][0];
        assert_eq!(first["id"], json!("near"));
        assert_eq!(first["kind"], json!("assert_property"));
        assert_eq!(first["expected"]["op"], json!("approx"));
        assert_eq!(first["actual"]["value"]["value"]["x"], json!(1.2));
        assert_eq!(
            probe.sent()[0].1["op"],
            json!("approx"),
            "the probe decides it"
        );
        assert_eq!(report["counts"]["passed"], json!(1));
        assert_eq!(report["stopped_by"], Value::Null);
    }

    /// A NaN never compares equal or near, and the report says the live value
    /// was not finite rather than inventing a number for it.
    #[test]
    fn a_value_that_is_not_finite_fails_and_is_named() {
        let probe = TestProbe::new(vec![(
            "runtime.scenario.check",
            checked(json!(false), Value::Null, json!("non_finite")),
        )]);
        let report = run(
            &probe,
            &TestKeeper::default(),
            &plan(vec![step("nan", check_property("approx"))]),
        );
        let first = &report["steps"][0];
        assert_eq!(first["status"], json!("failed"), "{report}");
        assert_eq!(first["actual"]["special"], json!("non_finite"));
        assert_eq!(first["actual"]["value"], Value::Null);
        assert_eq!(report["verdict"], json!("failed"));
    }

    #[test]
    fn a_comparison_that_does_not_apply_is_an_error() {
        let probe = TestProbe::new(vec![(
            "runtime.scenario.check",
            result(
                json!({ "holds": null, "reason": "the live value is Vector2; it never compares with a String", "value": null, "special": null, "type": "Vector2", "unrepresentable": null }),
            ),
        )]);
        let report = run(
            &probe,
            &TestKeeper::default(),
            &plan(vec![step("typed", check_property("=="))]),
        );
        let first = &report["steps"][0];
        assert_eq!(first["status"], json!("error"), "{report}");
        assert_eq!(first["error"]["kind"], json!("invalid_input"));
        assert!(
            first["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("never compares")),
            "{first}"
        );
    }

    #[test]
    fn a_failure_skips_the_steps_after_it_unless_told_otherwise() {
        let answers = || {
            vec![
                (
                    "runtime.scenario.check",
                    checked(
                        json!(false),
                        json!({ "type": "vector2", "value": { "x": 9.0, "y": 9.0 } }),
                        Value::Null,
                    ),
                ),
                (
                    "runtime.scenario.check",
                    checked(
                        json!(true),
                        json!({ "type": "vector2", "value": { "x": 1.0, "y": 2.0 } }),
                        Value::Null,
                    ),
                ),
            ]
        };
        let steps = || {
            vec![
                step("first", check_property("approx")),
                step("second", check_property("approx")),
            ]
        };
        let probe = TestProbe::new(answers());
        let report = run(&probe, &TestKeeper::default(), &plan(steps()));
        assert_eq!(statuses(&report), ["failed", "skipped"], "{report}");
        assert_eq!(report["stopped_by"], json!("failure"));
        assert_eq!(probe.sent().len(), 1, "the skipped step sent nothing");

        let probe = TestProbe::new(answers());
        let mut going_on = plan(steps());
        going_on.stop_on_failure = false;
        let report = run(&probe, &TestKeeper::default(), &going_on);
        assert_eq!(statuses(&report), ["failed", "passed"], "{report}");
        assert_eq!(report["verdict"], json!("failed"));
        assert_eq!(report["stopped_by"], Value::Null);
    }

    #[test]
    fn an_uncertain_step_stops_the_scenario_whatever_it_was_told() {
        let probe = TestProbe::new(vec![(
            "runtime.input.sequence",
            Scripted::Now(Err(BridgeFailure::Lost)),
        )]);
        let mut going_on = plan(vec![
            step(
                "press",
                Action::Input {
                    steps: vec![json!({ "kind": "action", "action": "jump", "state": "tap" })],
                    wait_ms: 0,
                },
            ),
            step("then", check_property("approx")),
        ]);
        going_on.stop_on_failure = false;
        let report = run(&probe, &TestKeeper::default(), &going_on);
        assert_eq!(statuses(&report), ["uncertain", "skipped"], "{report}");
        assert_eq!(report["verdict"], json!("uncertain"));
        assert_eq!(report["stopped_by"], json!("uncertain"));
        assert!(
            report["steps"][0]["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("lost"))
        );
    }

    #[test]
    fn a_wait_that_ends_without_its_condition_is_uncertain() {
        let probe = TestProbe::new(vec![(
            "runtime.wait",
            result(
                json!({ "condition": "node_exists", "satisfied": false, "outcome": "timeout", "elapsed_ms": 100, "frames_waited": 6, "timeout_ms": 100, "observed": { "present": false } }),
            ),
        )]);
        let report = run(
            &probe,
            &TestKeeper::default(),
            &plan(vec![step(
                "appear",
                Action::Wait {
                    request: json!({ "condition": { "node_exists": { "node": "Boss" } }, "timeout_ms": 100 })
                        .as_object()
                        .cloned()
                        .expect("object"),
                    timeout_ms: 100,
                },
            )]),
        );
        assert_eq!(statuses(&report), ["uncertain"], "{report}");
        assert_eq!(report["steps"][0]["detail"]["outcome"], json!("timeout"));
    }

    /// The deadline withdraws the step that runs when it passes; that step is
    /// uncertain, the rest are skipped and the report says why.
    #[test]
    fn the_deadline_withdraws_the_running_step_and_skips_the_rest() {
        let probe = TestProbe::new(vec![(
            "runtime.wait",
            Scripted::OnWithdrawal(
                json!({ "condition": "node_exists", "satisfied": false, "outcome": "cancelled", "elapsed_ms": 60, "frames_waited": 4, "timeout_ms": 30000, "observed": null }),
            ),
        )]);
        let mut short = plan(vec![
            step(
                "long",
                Action::Wait {
                    request: json!({ "condition": { "node_exists": { "node": "Never" } }, "timeout_ms": 30000 })
                        .as_object()
                        .cloned()
                        .expect("object"),
                    timeout_ms: 30_000,
                },
            ),
            step("after", check_property("approx")),
        ]);
        short.max_duration_ms = 50;
        let report = run(&probe, &TestKeeper::default(), &short);
        assert_eq!(statuses(&report), ["uncertain", "skipped"], "{report}");
        assert_eq!(report["stopped_by"], json!("deadline"));
        assert_eq!(report["verdict"], json!("uncertain"));
        assert!(
            report["steps"][1]["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("maxDurationMs"))
        );
    }

    #[test]
    fn a_cancelled_caller_skips_what_has_not_run() {
        let token = tokio_util::sync::CancellationToken::new();
        token.cancel();
        let probe = TestProbe::new(Vec::new());
        let report = run_scenario(
            &probe,
            &TestKeeper::default(),
            &plan(vec![step("never", check_property("approx"))]),
            &BTreeMap::new(),
            &CancelSignal::of(&token),
        )
        .expect("the scenario answers");
        assert_eq!(statuses(&report), ["skipped"], "{report}");
        assert_eq!(report["stopped_by"], json!("cancelled"));
        assert_eq!(report["verdict"], json!("uncertain"));
        assert!(probe.sent().is_empty());
    }

    #[test]
    fn a_refused_step_is_an_error_with_the_probes_kind() {
        let probe = TestProbe::new(vec![(
            "runtime.ui.press",
            refused(
                ErrorKind::NotFound,
                "no visible button shows the text Start",
            ),
        )]);
        let report = run(
            &probe,
            &TestKeeper::default(),
            &plan(vec![step(
                "start",
                Action::Press {
                    request: json!({ "text": "Start", "hold_ms": 100 })
                        .as_object()
                        .cloned()
                        .expect("object"),
                    hold_ms: 100,
                },
            )]),
        );
        assert_eq!(statuses(&report), ["error"], "{report}");
        assert_eq!(report["steps"][0]["error"]["kind"], json!("not_found"));
        assert_eq!(report["verdict"], json!("error"));
    }

    fn time_scale_answer(after: f64, matched: bool) -> Scripted {
        committed(json!({
            "target": "Engine",
            "class": "Engine",
            "results": [{
                "property": "time_scale",
                "before": { "type": "float", "value": 1.0 },
                "requested": { "type": "float", "value": 2.0 },
                "after": { "type": "float", "value": after },
                "matched": matched
            }],
            "all_matched": matched,
            "revision": 1,
            "frame": 3
        }))
    }

    #[test]
    fn a_time_scale_that_reads_back_otherwise_is_uncertain() {
        let probe = TestProbe::new(vec![(
            "runtime.properties.set",
            time_scale_answer(2.0, true),
        )]);
        let calls = Calls::new(&probe, CancelSignal::default(), None);
        let TimeScale::Set(answer) = set_time_scale(&calls, 2.0) else {
            panic!("the scale reads back as asked");
        };
        assert_eq!(answer["before"], json!(1.0));
        assert_eq!(answer["after"], json!(2.0));
        assert_eq!(answer["persisted"], json!(false));
        assert_eq!(
            probe.sent()[0].1,
            json!({ "singleton": "Engine", "properties": { "time_scale": { "type": "float", "value": 2.0 } } })
        );

        let probe = TestProbe::new(vec![(
            "runtime.properties.set",
            time_scale_answer(1.0, false),
        )]);
        let calls = Calls::new(&probe, CancelSignal::default(), None);
        let TimeScale::Unsure { answer, message } = set_time_scale(&calls, 2.0) else {
            panic!("a scale that reads back otherwise is uncertain");
        };
        assert_eq!(answer["after"], json!(1.0));
        assert!(message.contains("uncertain"), "{message}");

        // An answer that does not describe the write proves nothing.
        let probe = TestProbe::new(vec![(
            "runtime.properties.set",
            committed(json!({ "results": [] })),
        )]);
        let calls = Calls::new(&probe, CancelSignal::default(), None);
        assert!(matches!(
            set_time_scale(&calls, 2.0),
            TimeScale::Unsure { .. }
        ));

        // In a scenario the same is an uncertain step.
        let probe = TestProbe::new(vec![(
            "runtime.properties.set",
            time_scale_answer(1.0, false),
        )]);
        let report = run(
            &probe,
            &TestKeeper::default(),
            &plan(vec![step("fast", Action::TimeScale { time_scale: 2.0 })]),
        );
        assert_eq!(statuses(&report), ["uncertain"], "{report}");
    }

    fn png(byte: u8) -> Vec<u8> {
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        bytes.push(byte);
        bytes
    }

    fn described(capture: &[u8], difference: Option<&[u8]>) -> Value {
        let mut frames =
            vec![json!({ "byte_length": capture.len(), "sha256": sha256_hex(capture) })];
        if let Some(difference) = difference {
            frames
                .push(json!({ "byte_length": difference.len(), "sha256": sha256_hex(difference) }));
        }
        json!({ "capture_id": "capture-1", "width": 4, "height": 2, "frames": frames, "fps": null })
    }

    fn chunk(bytes: &[u8]) -> Scripted {
        result(json!({ "data_base64": crate::capture::base64_encode(bytes) }))
    }

    #[test]
    fn a_screenshot_is_kept_and_an_earlier_one_is_a_reference() {
        let shot = png(1);
        let capture = png(2);
        let difference = png(3);
        let mut comparison = described(&capture, Some(&difference));
        let measured = json!({
            "matched": false,
            "reason": "8 of 8 pixels differ by more than 0 in a channel",
            "same_size": true,
            "capture_size": { "width": 4, "height": 2 },
            "reference": { "width": 4, "height": 2, "byte_length": shot.len(), "sha256": sha256_hex(&shot) },
            "tolerance": 0,
            "total_pixels": 8,
            "differing_pixels": 8,
            "ratio": 1.0,
            "max_channel_difference": 255,
            "bounding_box": { "x": 0, "y": 0, "width": 4, "height": 2 },
            "difference_frame": 1
        });
        for (key, value) in measured.as_object().expect("object") {
            comparison[key] = value.clone();
        }
        let probe = TestProbe::new(vec![
            ("runtime.capture.screenshot", result(described(&shot, None))),
            ("runtime.capture.chunk", chunk(&shot)),
            (
                "runtime.image.reference",
                result(
                    json!({ "upload": "same", "received": shot.len(), "total": shot.len(), "complete": true }),
                ),
            ),
            ("runtime.image.compare", result(comparison)),
            ("runtime.capture.chunk", chunk(&capture)),
            ("runtime.capture.chunk", chunk(&difference)),
        ]);
        let keeper = TestKeeper::default();
        let report = run(
            &probe,
            &keeper,
            &plan(vec![
                step("before", Action::Screenshot),
                step(
                    "same",
                    Action::CompareImage {
                        reference: Reference::Step {
                            step: "before".to_owned(),
                        },
                        tolerance: 0,
                        difference: true,
                    },
                ),
            ]),
        );
        assert_eq!(statuses(&report), ["passed", "failed"], "{report}");
        let uploaded = &probe.sent()[2].1;
        assert_eq!(
            uploaded["sha256"],
            json!(sha256_hex(&shot)),
            "the earlier screenshot is the reference"
        );
        assert_eq!(uploaded["offset"], json!(0));
        let kept: Vec<String> = keeper
            .kept
            .lock()
            .expect("kept")
            .iter()
            .map(|(salt, _)| salt.clone())
            .collect();
        assert_eq!(
            kept,
            ["before/screenshot", "same/capture", "same/difference"]
        );
        let second = &report["steps"][1];
        assert_eq!(second["actual"]["differing_pixels"], json!(8));
        assert_eq!(second["expected"]["step"], json!("before"));
        assert_eq!(second["artifacts"].as_array().map(Vec::len), Some(2));
        assert_eq!(second["artifacts"][1]["role"], json!("difference"));
        assert_eq!(report["steps"][0]["artifacts"][0]["step"], json!("before"));
    }

    #[test]
    fn a_headless_session_answers_unavailable_for_image_steps() {
        let probe = TestProbe::new(Vec::new()).headless();
        let mut going_on = plan(vec![
            step("shot", Action::Screenshot),
            step(
                "compare",
                Action::CompareImage {
                    reference: Reference::Path {
                        path: "res://ref.png".to_owned(),
                    },
                    tolerance: 0,
                    difference: true,
                },
            ),
        ]);
        going_on.stop_on_failure = false;
        let report = run_scenario(
            &probe,
            &TestKeeper::default(),
            &going_on,
            &BTreeMap::from([("res://ref.png".to_owned(), png(1))]),
            &CancelSignal::default(),
        )
        .expect("the scenario runs");
        assert_eq!(statuses(&report), ["error", "error"], "{report}");
        for record in report["steps"].as_array().expect("steps") {
            assert_eq!(record["error"]["kind"], json!("unavailable"));
        }
        assert!(
            probe.sent().is_empty(),
            "nothing was asked of a probe that renders nothing"
        );
    }

    #[test]
    fn an_artifact_the_store_cannot_keep_is_an_error_of_its_step() {
        let shot = png(1);
        let probe = TestProbe::new(vec![
            ("runtime.capture.screenshot", result(described(&shot, None))),
            ("runtime.capture.chunk", chunk(&shot)),
        ]);
        let keeper = TestKeeper {
            refuse: vec!["full/screenshot"],
            ..TestKeeper::default()
        };
        let report = run(
            &probe,
            &keeper,
            &plan(vec![step("full", Action::Screenshot)]),
        );
        assert_eq!(statuses(&report), ["error"], "{report}");
        assert_eq!(report["steps"][0]["error"]["kind"], json!("unavailable"));
    }

    #[test]
    fn a_stress_window_passes_when_every_instance_is_freed() {
        let answer = |remaining: u64| {
            committed(json!({
                "scene": "res://bullet.tscn", "parent": "/root/Main", "requested": 50, "created": 50,
                "present_during": 50, "remaining": remaining, "freed": 50 - remaining, "outcome": "completed",
                "duration_ms": 200, "interval_ms": 50, "samples": 4,
                "monitors": { "TIME_FPS": { "count": 4, "min": 58.0, "max": 60.0, "mean": 59.5, "last": 60.0 } },
                "counts": { "before": { "OBJECT_NODE_COUNT": 10.0 }, "during": { "OBJECT_NODE_COUNT": 60.0 }, "after": { "OBJECT_NODE_COUNT": 10.0 } }
            }))
        };
        let stress = || {
            step(
                "load",
                Action::Stress {
                    request: json!({ "scene": "res://bullet.tscn", "count": 50, "parent": ".", "duration_ms": 200, "interval_ms": 50 })
                        .as_object()
                        .cloned()
                        .expect("object"),
                    count: 50,
                    duration_ms: 200,
                },
            )
        };
        let probe = TestProbe::new(vec![("runtime.scenario.stress", answer(0))]);
        let report = run(&probe, &TestKeeper::default(), &plan(vec![stress()]));
        assert_eq!(statuses(&report), ["passed"], "{report}");
        assert_eq!(
            report["steps"][0]["detail"]["monitors"]["TIME_FPS"]["min"],
            json!(58.0)
        );

        let probe = TestProbe::new(vec![("runtime.scenario.stress", answer(3))]);
        let report = run(&probe, &TestKeeper::default(), &plan(vec![stress()]));
        assert_eq!(statuses(&report), ["failed"], "{report}");
        assert_eq!(report["steps"][0]["actual"]["remaining"], json!(3));
    }

    #[test]
    fn every_report_field_is_bounded() {
        let large = json!({ "text": "x".repeat(MAX_FIELD_BYTES) });
        let bounded = bounded(Some(large.clone()));
        assert_eq!(bounded["truncated"], json!(true));
        assert_eq!(
            bounded["sha256"],
            json!(sha256_hex(large.to_string().as_bytes()))
        );
        assert_eq!(
            super::bounded(Some(json!({ "small": 1 }))),
            json!({ "small": 1 })
        );
        assert_eq!(super::bounded(None), Value::Null);
    }

    #[test]
    fn a_plan_reads_back_as_it_was_written() {
        let written = plan(vec![
            step("a", Action::Screenshot),
            step(
                "b",
                Action::CompareImage {
                    reference: Reference::Step {
                        step: "a".to_owned(),
                    },
                    tolerance: 4,
                    difference: false,
                },
            ),
            step("c", Action::TimeScale { time_scale: 0.5 }),
        ]);
        let text = serde_json::to_string(&written).expect("serialize");
        let read: Plan = serde_json::from_str(&text).expect("deserialize");
        assert_eq!(read, written);
        assert_eq!(read.steps[1].action.kind(), "compare_image");
    }
}
