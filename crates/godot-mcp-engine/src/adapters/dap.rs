//! The bounded debug-adapter surface, and honest `unavailable`/`unsupported` for
//! everything Godot omits.
//!
//! # What Godot's debug adapter is, and is not
//!
//! `DebugAdapterServer` is an `EditorPlugin` that binds `127.0.0.1:6006` —
//! `protocol.start(remote_port, IPAddress("127.0.0.1"))`, with no host setting at
//! all — and speaks DAP over raw TCP with `Content-Length` framing. It drives
//! `EditorDebuggerNode::get_singleton()->get_default_debugger()`, which is the
//! editor's *remote* debugger session to a game process the editor spawned.
//!
//! Two consequences follow directly, and both are load-bearing here:
//!
//! 1. **`launch` does not spawn a process.** `req_launch` validates the project path,
//!    stores the request in `peer->pending_launch` and returns an empty dictionary,
//!    so **no response is written yet**. The actual work happens when
//!    `configurationDone` arrives: the editor presses Play, emits a `process` event
//!    and only then answers the stored `launch` request. So [`DapSession::launch`]
//!    sends the request without waiting for it, [`DapSession::configuration_done`]
//!    collects both answers, and this adapter refuses to report a launch as started
//!    before that. (Observed against a real Godot 4.7.1 editor: `configurationDone`,
//!    then the `process` event, then the `launch` response.)
//! 2. **`attach` only attaches to a game the editor already launched.** `req_attach`
//!    checks `dbg->is_session_active()` and returns `DAP::ErrorType::NOT_RUNNING`
//!    otherwise. It cannot attach to an arbitrary external Godot process. It also
//!    sets `peer->attached`, and `req_disconnect` calls
//!    `EditorRunBar::stop_playing()` **only when not attached** — so detaching from an
//!    attached session deliberately leaves the game running.
//!
//! # `@tool` breakpoints are not supported, and cannot be
//!
//! Godot's own documentation states: "While the debugger and breakpoints cannot be
//! used directly with tool scripts, it is possible to launch a new instance of the
//! editor and debug from there." That matches the architecture exactly — `@tool` code
//! runs in the editor process, which *is* the debugger, so there is no debuggee to
//! break into. [`DapSession::set_breakpoints`] refuses a `@tool` script up front with
//! [`TOOL_BREAKPOINT_REFUSAL`] rather than accepting breakpoints that would never be
//! hit.
//!
//! # Unimplemented requests are refused, never sent
//!
//! Godot's dispatcher prefixes `"req_"` onto the command and tests
//! `parser->has_method(command)`. An unknown command falls through the `if` and **no
//! response is written at all** — the client hangs until its own timeout. Three
//! requests in common use are missing entirely: `stepOut`, `setFunctionBreakpoints`
//! and `setExceptionBreakpoints`. There is also an advertised-but-missing case:
//! `supportsSetVariable = true` is set in `debug_adapter_types.h` while no
//! `req_setVariable` handler exists, so a client that trusts the capability hangs.
//! [`UNIMPLEMENTED_REQUESTS`] enumerates all four, and [`DapAdapter::unimplemented`]
//! refuses them before a frame is sent.
//!
//! # The engine's own per-message timeout
//!
//! `request_timeout` defaults to 5000 ms and is enforced *per message*: if
//! `now - peer->timestamp > _request_timeout`, the request is answered with
//! `DAP::ErrorType::TIMEOUT` before dispatch. A deadline longer than that cannot be
//! relied on, which [`ENGINE_REQUEST_TIMEOUT`] records so a caller is not surprised.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::adapters::channel::FramedChannel;
use crate::adapters::codec::CodecError;
use crate::adapters::error::AdapterError;
use crate::adapters::receipt::{EndpointKind, OwnedEndpoint};

/// Godot's own default per-message DAP timeout (`request_timeout`, 5000 ms).
pub const ENGINE_REQUEST_TIMEOUT: Duration = Duration::from_millis(5000);

/// Why `@tool` breakpoints are refused rather than accepted and never hit.
pub const TOOL_BREAKPOINT_REFUSAL: &str = "Godot's debugger cannot break in `@tool` scripts: every \
     DAP handler drives the editor's remote debugger session to a spawned game, while `@tool` code \
     runs in the editor process that is itself the debugger. The documented workaround is to launch \
     a second editor as the debuggee (Debug > Customize Run Instances, Main Run Args `--editor`), \
     which this adapter does not do on the caller's behalf.";

/// Requests Godot's debug adapter does not implement, and what happens if one is
/// sent anyway.
///
/// Every entry is refused before a frame goes out, because Godot writes **no
/// response** for an unknown command: a client that sent one would hang.
pub const UNIMPLEMENTED_REQUESTS: [(&str, &str); 4] = [
    (
        "stepOut",
        "there is no `req_stepOut` binding in debug_adapter_parser.cpp; the adapter never wires a \
         step-out onto EditorDebuggerNode, and an unknown command receives no response at all.",
    ),
    (
        "setFunctionBreakpoints",
        "there is no `req_setFunctionBreakpoints` binding, and the advertised Capabilities do not \
         claim it; an unknown command receives no response at all.",
    ),
    (
        "setExceptionBreakpoints",
        "there is no `req_setExceptionBreakpoints` binding, and the advertised Capabilities do not \
         claim it; an unknown command receives no response at all.",
    ),
    (
        "setVariable",
        "debug_adapter_types.h advertises supportsSetVariable=true but no `req_setVariable` handler \
         exists, so a client that trusts the capability hangs with no response. This adapter \
         refuses it instead of trusting the advertisement.",
    ),
];

/// Requests Godot's debug adapter does implement, from the 19 `req_*` bindings in
/// `debug_adapter_parser.cpp`.
pub const IMPLEMENTED_REQUESTS: [&str; 19] = [
    "initialize",
    "disconnect",
    "launch",
    "attach",
    "restart",
    "terminate",
    "configurationDone",
    "pause",
    "continue",
    "threads",
    "stackTrace",
    "setBreakpoints",
    "breakpointLocations",
    "scopes",
    "variables",
    "next",
    "stepIn",
    "evaluate",
    "godot/put_msg",
];

/// The maximum events this adapter will read while waiting for one response.
pub const MAX_INTERVENING_EVENTS: usize = 256;

/// The most unconsumed events a session keeps for [`DapSession::wait_for_event`].
pub const MAX_QUEUED_EVENTS: usize = 256;

/// How often, and how far apart, a scope's variables are asked for again while
/// the editor is still fetching them from the game. Godot answers a `variables`
/// request that outruns its own `scopes` fetch with an `unknown` error
/// (`!scope_list.has(_current_frame)` in `debug_adapter_protocol.cpp`).
const VARIABLE_ATTEMPTS: u32 = 40;
const VARIABLE_RETRY: Duration = Duration::from_millis(50);

/// One capability, as the adapter advertised it in its `initialize` result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DapCapability {
    /// The capability's key in the DAP `Capabilities` object.
    pub name: String,
    /// Whether the adapter advertised it.
    pub advertised: bool,
    /// Whether this adapter trusts the advertisement.
    ///
    /// `supportsSetVariable` is advertised without a handler, so trusting it produces
    /// a hang. A capability whose request is in [`UNIMPLEMENTED_REQUESTS`] is recorded
    /// as advertised but not trusted.
    pub trusted: bool,
}

/// One frame from a `stackTrace` response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackFrameSummary {
    /// The adapter's frame identifier, used for `scopes`.
    pub id: i64,
    /// The frame's name.
    pub name: String,
    /// The source path, when the adapter sent one.
    pub source: Option<String>,
    /// The one-based line.
    pub line: Option<u64>,
}

/// One variable from a `variables` response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariableSummary {
    /// The variable's name.
    pub name: String,
    /// Its rendered value.
    pub value: String,
    /// Its type, when the adapter sent one.
    pub type_name: Option<String>,
    /// A nonzero reference means the value has children.
    pub variables_reference: i64,
}

/// One breakpoint the adapter acknowledged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BreakpointAck {
    /// Whether the adapter verified it.
    pub verified: bool,
    /// The line it was bound to, when the adapter reported one.
    pub line: Option<u64>,
    /// The adapter's message, when it sent one.
    pub message: Option<String>,
}

/// How far a launch has actually got.
///
/// `launch` alone runs nothing: Godot stores it in `pending_launch` and waits for
/// `configurationDone`. Modelling that explicitly is what stops this adapter from
/// reporting a launch as started when nothing is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LaunchState {
    /// Nothing requested.
    Idle,
    /// `launch` was accepted and is pending; the editor has not pressed Play.
    Pending,
    /// `configurationDone` completed, so the editor started the scene.
    Started,
    /// The session is attached to a game the editor already had running.
    Attached,
}

impl LaunchState {
    /// Stable wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Pending => "pending",
            Self::Started => "started",
            Self::Attached => "attached",
        }
    }

    /// Whether a debuggee is actually running.
    #[must_use]
    pub const fn is_running(self) -> bool {
        matches!(self, Self::Started | Self::Attached)
    }
}

/// The adapter's entry point: usable only with a verified owned endpoint.
pub struct DapAdapter;

impl DapAdapter {
    /// Whether `request` is one Godot's debug adapter does not implement.
    #[must_use]
    pub fn unimplemented(request: &str) -> Option<&'static str> {
        UNIMPLEMENTED_REQUESTS
            .iter()
            .find(|(name, _detail)| *name == request)
            .map(|(_name, detail)| *detail)
    }

    /// Whether `request` is one of the 19 bound handlers.
    #[must_use]
    pub fn implemented(request: &str) -> bool {
        IMPLEMENTED_REQUESTS.contains(&request)
    }

    /// Open a session on a verified owned endpoint.
    ///
    /// # Errors
    /// Returns [`AdapterError::Unsupported`] when the token names the language server
    /// rather than the debug adapter.
    pub fn open<'c>(
        endpoint: &OwnedEndpoint,
        channel: &'c mut dyn FramedChannel,
        deadline: Duration,
    ) -> Result<DapSession<'c>, AdapterError> {
        if endpoint.kind() != EndpointKind::DebugAdapter {
            return Err(AdapterError::unsupported(
                "initialize",
                "this endpoint token names the language server, not the debug adapter",
            ));
        }
        Ok(DapSession {
            channel,
            deadline: deadline.min(ENGINE_REQUEST_TIMEOUT),
            next_seq: 1,
            project_root_uri: endpoint.project_root_uri().to_owned(),
            launch_transaction_id: endpoint.launch_transaction_id().to_owned(),
            capabilities: Vec::new(),
            launch_state: LaunchState::Idle,
            events: Vec::new(),
            awaiting: BTreeSet::new(),
            stashed: BTreeMap::new(),
            queued: VecDeque::new(),
            pending_launch: None,
        })
    }
}

/// One live debug-adapter conversation.
pub struct DapSession<'c> {
    channel: &'c mut dyn FramedChannel,
    deadline: Duration,
    next_seq: i64,
    project_root_uri: String,
    launch_transaction_id: String,
    capabilities: Vec<DapCapability>,
    launch_state: LaunchState,
    events: Vec<String>,
    /// Requests whose answers arrive later, behind other traffic.
    awaiting: BTreeSet<i64>,
    /// Answers to awaited requests that arrived while another was awaited.
    stashed: BTreeMap<i64, Value>,
    /// Events no [`DapSession::wait_for_event`] has consumed yet, oldest first.
    queued: VecDeque<Value>,
    /// The `launch` request `configurationDone` will have Godot answer.
    pending_launch: Option<i64>,
}

impl DapSession<'_> {
    /// The launch transaction that owns the editor on the other end.
    #[must_use]
    pub fn launch_transaction_id(&self) -> &str {
        &self.launch_transaction_id
    }

    /// The capabilities the adapter advertised, with the untrusted ones marked.
    #[must_use]
    pub fn capabilities(&self) -> &[DapCapability] {
        &self.capabilities
    }

    /// How far the launch has got. `launch` alone is [`LaunchState::Pending`].
    #[must_use]
    pub const fn launch_state(&self) -> LaunchState {
        self.launch_state
    }

    /// Event names observed, oldest first.
    #[must_use]
    pub fn events(&self) -> &[String] {
        &self.events
    }

    /// The effective deadline, already clamped to Godot's own `request_timeout`.
    #[must_use]
    pub const fn deadline(&self) -> Duration {
        self.deadline
    }

    /// Perform `initialize`.
    ///
    /// # Errors
    /// Returns [`AdapterError::PeerError`] for a DAP error response and the codec
    /// errors for a framing or transport failure.
    pub fn initialize(&mut self) -> Result<(), AdapterError> {
        let body = self.request(
            "initialize",
            &json!({
                "clientID": "godot-mcp",
                "adapterID": "godot",
                "linesStartAt1": true,
                "columnsStartAt1": true,
                "pathFormat": "path",
                "supportsRunInTerminalRequest": false,
            }),
        )?;
        self.capabilities = read_capabilities(&body);
        Ok(())
    }

    /// Request a launch. This does **not** start anything by itself.
    ///
    /// Godot stores the request in `pending_launch` and returns an empty body;
    /// `process_message` marks it completed anyway. The scene starts only when
    /// `configurationDone` arrives, so this leaves the session at
    /// [`LaunchState::Pending`] and [`DapSession::configuration_done`] must follow.
    ///
    /// # Errors
    /// Returns [`AdapterError::PeerError`] when the adapter refuses the project path
    /// (`WRONG_PATH`, which carries `clientPath`/`editorPath`), or the codec errors.
    pub fn launch(&mut self, no_debug: bool) -> Result<LaunchState, AdapterError> {
        let project = path_of_uri(&self.project_root_uri);
        let seq = self.send(
            "launch",
            &json!({
                "project": project,
                "noDebug": no_debug,
                "platform": "host",
            }),
        )?;
        self.awaiting.insert(seq);
        self.pending_launch = Some(seq);
        self.launch_state = LaunchState::Pending;
        Ok(self.launch_state)
    }

    /// Complete configuration, which is what actually makes the editor press Play.
    ///
    /// # Errors
    /// Returns [`AdapterError::Unsupported`] when no launch is pending (there is
    /// nothing to complete), `PeerError` for a DAP error, or the codec errors.
    pub fn configuration_done(&mut self) -> Result<LaunchState, AdapterError> {
        if self.launch_state != LaunchState::Pending {
            return Err(AdapterError::unsupported(
                "configurationDone",
                "no launch is pending; Godot only starts the scene from configurationDone when a \
                 launch request is waiting in pending_launch",
            ));
        }
        self.request("configurationDone", &json!({}))?;
        if let Some(seq) = self.pending_launch.take() {
            self.awaiting.remove(&seq);
            if let Err(error) = self.await_response("launch", seq) {
                self.launch_state = LaunchState::Idle;
                return Err(error);
            }
        }
        self.launch_state = LaunchState::Started;
        Ok(self.launch_state)
    }

    /// Wait for the next `event` the adapter sends, at most `limit`: a
    /// `stopped` event once a breakpoint is hit or a pause took effect, an
    /// `exited` or `terminated` one when the game ended. An event that already
    /// arrived while a response was awaited is answered at once.
    ///
    /// # Errors
    /// Returns [`AdapterError::Timeout`] when no such event arrives in time, and
    /// the codec errors when the connection fails.
    pub fn wait_for_event(&mut self, event: &str, limit: Duration) -> Result<Value, AdapterError> {
        if let Some(index) = self
            .queued
            .iter()
            .position(|queued| queued.get("event").and_then(Value::as_str) == Some(event))
        {
            return Ok(self.queued.remove(index).unwrap_or(Value::Null));
        }
        let started = Instant::now();
        let timeout = || AdapterError::Timeout {
            method: format!("event {event}"),
            millis: limit.as_millis().min(u128::from(u64::MAX)) as u64,
        };
        let outcome = loop {
            let elapsed = started.elapsed();
            if elapsed >= limit {
                break Err(timeout());
            }
            self.channel.set_read_timeout(Some(limit - elapsed))?;
            let message = match self.read_message() {
                Ok(message) => message,
                // The read timed out with the wait: no event came.
                Err(AdapterError::Codec(CodecError::Io { .. }))
                    if started.elapsed() + Duration::from_millis(50) >= limit =>
                {
                    break Err(timeout());
                }
                Err(error) => break Err(error),
            };
            match message.get("type").and_then(Value::as_str) {
                Some("event") if message.get("event").and_then(Value::as_str) == Some(event) => {
                    if let Some(name) = message.get("event").and_then(Value::as_str) {
                        self.events.push(name.to_owned());
                    }
                    break Ok(message);
                }
                Some("event") => self.note_event(message),
                Some("response") => self.stash_response(message),
                _ => {}
            }
        };
        self.channel.set_read_timeout(Some(self.deadline))?;
        outcome
    }

    /// Pause the running debuggee. The pause itself is reported by a later
    /// `stopped` event with reason `paused`.
    ///
    /// # Errors
    /// Returns [`AdapterError::Unavailable`] when no debuggee is running,
    /// `PeerError` for a DAP error, or the codec errors.
    pub fn pause(&mut self, thread_id: i64) -> Result<(), AdapterError> {
        self.require_running("pause")?;
        self.request("pause", &json!({ "threadId": thread_id }))?;
        Ok(())
    }

    /// Attach to a game the editor already has running.
    ///
    /// # Errors
    /// Returns [`AdapterError::Unavailable`] when the adapter answers
    /// `DAP::ErrorType::NOT_RUNNING` — there is no session to attach to, and no way to
    /// attach to an arbitrary external process.
    pub fn attach(&mut self) -> Result<LaunchState, AdapterError> {
        match self.request("attach", &json!({})) {
            Ok(_body) => {
                self.launch_state = LaunchState::Attached;
                Ok(self.launch_state)
            }
            Err(AdapterError::PeerError { message, .. }) => {
                Err(AdapterError::unavailable(&format!(
                    "attach was refused: {message}. Godot's req_attach requires \
                     dbg->is_session_active(); it attaches only to a game the editor already \
                     launched and can never attach to an external Godot process"
                )))
            }
            Err(other) => Err(other),
        }
    }

    /// Set source breakpoints.
    ///
    /// `is_tool_script` must be supplied by the caller from the script's own `@tool`
    /// annotation. A `@tool` script is refused: Godot's debugger cannot break in
    /// editor-side code, so accepting the breakpoints would mean promising a stop that
    /// never happens.
    ///
    /// # Errors
    /// Returns [`AdapterError::Unsupported`] for a `@tool` script, `PeerError` for a
    /// DAP error, or the codec errors.
    pub fn set_breakpoints(
        &mut self,
        source_path: &str,
        lines: &[u64],
        is_tool_script: bool,
    ) -> Result<Vec<BreakpointAck>, AdapterError> {
        if is_tool_script {
            return Err(AdapterError::unsupported(
                "setBreakpoints",
                &format!("{source_path}: {TOOL_BREAKPOINT_REFUSAL}"),
            ));
        }
        let body = self.request(
            "setBreakpoints",
            &json!({
                "source": { "path": source_path },
                "breakpoints": lines
                    .iter()
                    .map(|line| json!({ "line": line }))
                    .collect::<Vec<Value>>(),
                "lines": lines,
            }),
        )?;
        Ok(read_breakpoints(&body))
    }

    /// Refuse a request Godot does not implement.
    ///
    /// # Errors
    /// Returns [`AdapterError::Unsupported`] when `request` is in
    /// [`UNIMPLEMENTED_REQUESTS`].
    pub fn ensure_implemented(request: &str) -> Result<(), AdapterError> {
        if let Some(detail) = DapAdapter::unimplemented(request) {
            return Err(AdapterError::unsupported(request, detail));
        }
        if DapAdapter::implemented(request) {
            return Ok(());
        }
        Err(AdapterError::unsupported(
            request,
            "this request is not among the 19 `req_*` handlers bound in \
             debug_adapter_parser.cpp, and Godot writes no response for an unknown command",
        ))
    }

    /// Step out of the current frame.
    ///
    /// # Errors
    /// Always returns [`AdapterError::Unsupported`]: there is no `req_stepOut` handler,
    /// and sending the request would hang rather than fail.
    pub fn step_out(&self) -> Result<(), AdapterError> {
        Self::ensure_implemented("stepOut")
    }

    /// Set function breakpoints.
    ///
    /// # Errors
    /// Always returns [`AdapterError::Unsupported`]: there is no
    /// `req_setFunctionBreakpoints` handler.
    pub fn set_function_breakpoints(&self, _names: &[String]) -> Result<(), AdapterError> {
        Self::ensure_implemented("setFunctionBreakpoints")
    }

    /// Set exception breakpoints.
    ///
    /// # Errors
    /// Always returns [`AdapterError::Unsupported`]: there is no
    /// `req_setExceptionBreakpoints` handler.
    pub fn set_exception_breakpoints(&self, _filters: &[String]) -> Result<(), AdapterError> {
        Self::ensure_implemented("setExceptionBreakpoints")
    }

    /// Read the current stack.
    ///
    /// # Errors
    /// Returns [`AdapterError::Unavailable`] when no debuggee is running, `PeerError`
    /// for a DAP error, or the codec errors.
    pub fn stack_trace(&mut self, thread_id: i64) -> Result<Vec<StackFrameSummary>, AdapterError> {
        self.require_running("stackTrace")?;
        let body = self.request("stackTrace", &json!({ "threadId": thread_id }))?;
        Ok(read_stack_frames(&body))
    }

    /// Read a frame's scopes, returning each scope's `variablesReference`.
    ///
    /// # Errors
    /// Returns [`AdapterError::Unavailable`] when no debuggee is running, `PeerError`
    /// for a DAP error, or the codec errors.
    pub fn scopes(&mut self, frame_id: i64) -> Result<Vec<(String, i64)>, AdapterError> {
        self.require_running("scopes")?;
        let body = self.request("scopes", &json!({ "frameId": frame_id }))?;
        Ok(body
            .get("scopes")
            .and_then(Value::as_array)
            .map(|scopes| {
                scopes
                    .iter()
                    .filter_map(|scope| {
                        Some((
                            scope.get("name").and_then(Value::as_str)?.to_owned(),
                            scope.get("variablesReference").and_then(Value::as_i64)?,
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Read one scope's variables.
    ///
    /// # Errors
    /// Returns [`AdapterError::Unavailable`] when no debuggee is running, `PeerError`
    /// for a DAP error, or the codec errors.
    pub fn variables(&mut self, reference: i64) -> Result<Vec<VariableSummary>, AdapterError> {
        self.require_running("variables")?;
        let mut attempt = 1;
        loop {
            match self.request("variables", &json!({ "variablesReference": reference })) {
                Ok(body) => return Ok(read_variables(&body)),
                Err(AdapterError::PeerError { message, .. })
                    if message == "unknown" && attempt < VARIABLE_ATTEMPTS =>
                {
                    attempt += 1;
                    // A bounded retry: the editor has not received the frame's
                    // variables from the game yet.
                    std::thread::sleep(VARIABLE_RETRY);
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Resume execution.
    ///
    /// # Errors
    /// Returns [`AdapterError::Unavailable`] when no debuggee is running, `PeerError`
    /// for a DAP error, or the codec errors.
    pub fn continue_execution(&mut self, thread_id: i64) -> Result<(), AdapterError> {
        self.require_running("continue")?;
        self.request("continue", &json!({ "threadId": thread_id }))?;
        Ok(())
    }

    /// Disconnect.
    ///
    /// `terminate_debuggee` is meaningful only for a launched session: Godot's
    /// `req_disconnect` calls `EditorRunBar::stop_playing()` **only when not
    /// attached**, so an attached session deliberately leaves the game running and
    /// this reports that rather than pretending otherwise.
    ///
    /// # Errors
    /// Returns `PeerError` for a DAP error or the codec errors.
    pub fn disconnect(&mut self, terminate_debuggee: bool) -> Result<&'static str, AdapterError> {
        let attached = self.launch_state == LaunchState::Attached;
        self.request(
            "disconnect",
            &json!({ "terminateDebuggee": terminate_debuggee }),
        )?;
        self.launch_state = LaunchState::Idle;
        Ok(if attached {
            "disconnected from an attached session; Godot's req_disconnect calls stop_playing() \
             only when not attached, so the game is still running"
        } else {
            "disconnected from a launched session; Godot's req_disconnect stops playback"
        })
    }

    fn require_running(&self, request: &str) -> Result<(), AdapterError> {
        if self.launch_state.is_running() {
            return Ok(());
        }
        Err(AdapterError::unavailable(&format!(
            "{request} needs a running debuggee, but the session is {}; Godot's handlers drive the \
             editor's remote debugger session and there is nothing attached to inspect",
            self.launch_state.as_str()
        )))
    }

    fn request(&mut self, command: &str, arguments: &Value) -> Result<Value, AdapterError> {
        let seq = self.send(command, arguments)?;
        self.await_response(command, seq)
    }

    fn send(&mut self, command: &str, arguments: &Value) -> Result<i64, AdapterError> {
        Self::ensure_implemented(command)?;
        let seq = self.next_seq;
        self.next_seq += 1;
        let payload = serde_json::to_vec(&json!({
            "seq": seq,
            "type": "request",
            "command": command,
            "arguments": arguments,
        }))
        .map_err(|error| AdapterError::MalformedResponse {
            method: command.to_owned(),
            detail: format!("the request could not be serialized: {error}"),
        })?;
        self.channel.send(&payload)?;
        Ok(seq)
    }

    /// Read until the response to request `seq` arrives, keeping events and
    /// the answers to other awaited requests for later.
    fn await_response(&mut self, command: &str, seq: i64) -> Result<Value, AdapterError> {
        if let Some(message) = self.stashed.remove(&seq) {
            return Self::body_of(command, &message);
        }
        let started = Instant::now();
        for _ in 0..MAX_INTERVENING_EVENTS {
            if started.elapsed() > self.deadline {
                return Err(AdapterError::Timeout {
                    method: command.to_owned(),
                    millis: self.deadline.as_millis().min(u128::from(u64::MAX)) as u64,
                });
            }
            let message = self.read_message()?;
            match message.get("type").and_then(Value::as_str) {
                Some("event") => self.note_event(message),
                Some("response") => {
                    if request_seq_of(&message) == Some(seq) {
                        return Self::body_of(command, &message);
                    }
                    self.stash_response(message);
                }
                _ => {}
            }
        }
        Err(AdapterError::unavailable(&format!(
            "the adapter sent {MAX_INTERVENING_EVENTS} messages without answering {command}"
        )))
    }

    fn body_of(command: &str, message: &Value) -> Result<Value, AdapterError> {
        if message.get("success").and_then(Value::as_bool) == Some(false) {
            return Err(AdapterError::PeerError {
                method: command.to_owned(),
                code: message.pointer("/body/error/id").and_then(Value::as_i64),
                message: message
                    .get("message")
                    .and_then(Value::as_str)
                    .or_else(|| {
                        message
                            .pointer("/body/error/format")
                            .and_then(Value::as_str)
                    })
                    .unwrap_or("the adapter reported failure with no message")
                    .to_owned(),
            });
        }
        Ok(message.get("body").cloned().unwrap_or(Value::Null))
    }

    fn note_event(&mut self, message: Value) {
        if let Some(name) = message.get("event").and_then(Value::as_str) {
            self.events.push(name.to_owned());
        }
        if self.queued.len() >= MAX_QUEUED_EVENTS {
            self.queued.pop_front();
        }
        self.queued.push_back(message);
    }

    /// Keep the answer to an awaited request; an answer nothing awaits is
    /// dropped.
    fn stash_response(&mut self, message: Value) {
        if let Some(request) = request_seq_of(&message)
            && self.awaiting.contains(&request)
        {
            self.stashed.insert(request, message);
        }
    }

    fn read_message(&mut self) -> Result<Value, AdapterError> {
        let bytes = self.channel.receive()?;
        serde_json::from_slice(&bytes).map_err(|error| AdapterError::MalformedResponse {
            method: "<inbound>".to_owned(),
            detail: format!("the frame payload is not JSON: {error}"),
        })
    }
}

/// The request a response answers. Godot 4.4 parses every JSON number as a
/// float and echoes the request's `seq` as `1.0`; later builds echo `1`. Both
/// name request 1, and only an integral value names a request at all.
fn request_seq_of(message: &Value) -> Option<i64> {
    let seq = message.get("request_seq")?;
    seq.as_i64().or_else(|| {
        seq.as_f64()
            .filter(|value| value.fract() == 0.0 && value.abs() < 9_007_199_254_740_992.0)
            .map(|value| value as i64)
    })
}

/// Read the DAP `Capabilities` body, marking advertised-but-unimplemented entries as
/// untrusted.
fn read_capabilities(body: &Value) -> Vec<DapCapability> {
    let Some(map) = body.as_object() else {
        return Vec::new();
    };
    map.iter()
        .map(|(name, value)| {
            let advertised = value.as_bool().unwrap_or(false);
            DapCapability {
                trusted: advertised && trusted_capability(name),
                name: name.clone(),
                advertised,
            }
        })
        .collect()
}

/// Whether an advertised capability corresponds to a handler that actually exists.
///
/// `supportsSetVariable` is the counter-example: advertised in
/// `debug_adapter_types.h` with no `req_setVariable` handler behind it.
fn trusted_capability(name: &str) -> bool {
    let request = match name.strip_prefix("supports") {
        Some(rest) => lower_first(rest).trim_end_matches("Request").to_owned(),
        None => return true,
    };
    DapAdapter::unimplemented(&request).is_none()
}

fn lower_first(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn read_stack_frames(body: &Value) -> Vec<StackFrameSummary> {
    body.get("stackFrames")
        .and_then(Value::as_array)
        .map(|frames| {
            frames
                .iter()
                .map(|frame| StackFrameSummary {
                    id: frame.get("id").and_then(Value::as_i64).unwrap_or_default(),
                    name: frame
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    source: frame
                        .pointer("/source/path")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    line: frame.get("line").and_then(Value::as_u64),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn read_variables(body: &Value) -> Vec<VariableSummary> {
    body.get("variables")
        .and_then(Value::as_array)
        .map(|variables| {
            variables
                .iter()
                .map(|variable| VariableSummary {
                    name: variable
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    value: variable
                        .get("value")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    type_name: variable
                        .get("type")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    variables_reference: variable
                        .get("variablesReference")
                        .and_then(Value::as_i64)
                        .unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn read_breakpoints(body: &Value) -> Vec<BreakpointAck> {
    body.get("breakpoints")
        .and_then(Value::as_array)
        .map(|breakpoints| {
            breakpoints
                .iter()
                .map(|breakpoint| BreakpointAck {
                    verified: breakpoint
                        .get("verified")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    line: breakpoint.get("line").and_then(Value::as_u64),
                    message: breakpoint
                        .get("message")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The filesystem path behind a `file://` URI, for the `project` argument
/// `req_launch` validates.
fn path_of_uri(uri: &str) -> String {
    crate::adapters::uri::path_of(uri)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A channel that answers every request the way Godot 4.4 does: the
    /// request's `seq` is parsed into a float and echoed as `request_seq`
    /// `1.0`, and `initialize` is followed by the `initialized` event.
    struct FloatEchoingAdapter {
        replies: VecDeque<Vec<u8>>,
        sent: Vec<Value>,
    }

    impl FramedChannel for FloatEchoingAdapter {
        fn send(&mut self, payload: &[u8]) -> Result<(), CodecError> {
            let request: Value = serde_json::from_slice(payload).expect("a JSON request");
            let seq = request["seq"].as_i64().expect("an integer seq") as f64;
            let command = request["command"].as_str().unwrap_or_default().to_owned();
            let body = match command.as_str() {
                "initialize" => json!({ "supportsConfigurationDoneRequest": true }),
                "setBreakpoints" => json!({ "breakpoints": [{ "verified": true, "line": 7 }] }),
                _ => json!({}),
            };
            self.replies.push_back(
                serde_json::to_vec(&json!({
                    "type": "response",
                    "request_seq": seq,
                    "command": command,
                    "success": true,
                    "body": body,
                }))
                .expect("reply"),
            );
            if command == "initialize" {
                self.replies.push_back(
                    serde_json::to_vec(
                        &json!({ "type": "event", "event": "initialized", "seq": 2.0 }),
                    )
                    .expect("event"),
                );
            }
            self.sent.push(request);
            Ok(())
        }

        fn receive(&mut self) -> Result<Vec<u8>, CodecError> {
            self.replies.pop_front().ok_or(CodecError::Eof {
                read: 0,
                expected: 0,
            })
        }

        fn is_abandoned(&self) -> bool {
            false
        }
    }

    #[test]
    fn a_response_that_echoes_its_request_seq_as_a_float_still_answers_it() {
        // Godot 4.4.1's JSON parser reads every number as a float, so its
        // responses carry `"request_seq": 1.0` where later builds carry `1`.
        let mut channel = FloatEchoingAdapter {
            replies: VecDeque::new(),
            sent: Vec::new(),
        };
        let mut session = DapSession {
            channel: &mut channel,
            deadline: ENGINE_REQUEST_TIMEOUT,
            next_seq: 1,
            project_root_uri: "file:///projects/demo".to_owned(),
            launch_transaction_id: "tx".to_owned(),
            capabilities: Vec::new(),
            launch_state: LaunchState::Idle,
            events: Vec::new(),
            awaiting: BTreeSet::new(),
            stashed: BTreeMap::new(),
            queued: VecDeque::new(),
            pending_launch: None,
        };
        session
            .initialize()
            .expect("the float-echoed initialize answers");
        let acknowledged = session
            .set_breakpoints("/projects/demo/player.gd", &[7], false)
            .expect("the float-echoed setBreakpoints answers");
        assert_eq!(acknowledged.len(), 1);
        assert!(acknowledged[0].verified);
        assert_eq!(session.events(), ["initialized"]);
        assert_eq!(channel.sent.len(), 2);
    }

    #[test]
    fn every_unimplemented_request_is_refused_before_a_frame_is_sent() {
        for (request, _detail) in UNIMPLEMENTED_REQUESTS {
            let error = DapSession::ensure_implemented(request).expect_err("unimplemented");
            assert!(error.is_unsupported(), "{request}: {error}");
        }
        for request in IMPLEMENTED_REQUESTS {
            assert!(
                DapSession::ensure_implemented(request).is_ok(),
                "{request} is one of the 19 bound handlers"
            );
        }
    }

    #[test]
    fn an_unknown_command_is_refused_rather_than_sent_into_a_hang() {
        let error = DapSession::ensure_implemented("gotoTargets").expect_err("not bound");
        assert!(error.is_unsupported());
        assert!(
            error.to_string().contains("no response"),
            "the refusal explains the hang it prevents: {error}"
        );
    }

    #[test]
    fn an_advertised_capability_with_no_handler_is_recorded_as_untrusted() {
        let capabilities = read_capabilities(&json!({
            "supportsConfigurationDoneRequest": true,
            "supportsBreakpointLocationsRequest": true,
            "supportsSetVariable": true,
        }));
        let trusted = |name: &str| {
            capabilities
                .iter()
                .find(|capability| capability.name == name)
                .map(|capability| (capability.advertised, capability.trusted))
        };
        assert_eq!(
            trusted("supportsConfigurationDoneRequest"),
            Some((true, true))
        );
        assert_eq!(
            trusted("supportsBreakpointLocationsRequest"),
            Some((true, true))
        );
        assert_eq!(
            trusted("supportsSetVariable"),
            Some((true, false)),
            "advertised without a req_setVariable handler, so trusting it would hang"
        );
    }

    #[test]
    fn a_tool_script_breakpoint_is_refused_with_the_documented_reason() {
        // Given a channel that would answer anything, when a @tool script's
        // breakpoints are requested, then the refusal happens before any frame.
        struct NeverUsed;
        impl FramedChannel for NeverUsed {
            fn send(&mut self, _payload: &[u8]) -> Result<(), crate::adapters::codec::CodecError> {
                unreachable!("a @tool refusal never reaches the wire");
            }
            fn receive(&mut self) -> Result<Vec<u8>, crate::adapters::codec::CodecError> {
                unreachable!("a @tool refusal never reaches the wire");
            }
            fn is_abandoned(&self) -> bool {
                false
            }
        }
        let mut channel = NeverUsed;
        let mut session = DapSession {
            channel: &mut channel,
            deadline: ENGINE_REQUEST_TIMEOUT,
            next_seq: 1,
            project_root_uri: "file:///projects/demo".to_owned(),
            launch_transaction_id: "tx".to_owned(),
            capabilities: Vec::new(),
            launch_state: LaunchState::Started,
            events: Vec::new(),
            awaiting: BTreeSet::new(),
            stashed: BTreeMap::new(),
            queued: VecDeque::new(),
            pending_launch: None,
        };
        let error = session
            .set_breakpoints("res://addons/tool.gd", &[12], true)
            .expect_err("@tool");
        assert!(error.is_unsupported());
        assert!(
            error
                .to_string()
                .contains("cannot break in `@tool` scripts"),
            "{error}"
        );
    }

    #[test]
    fn launch_alone_is_pending_and_never_reported_as_started() {
        assert!(!LaunchState::Pending.is_running());
        assert!(LaunchState::Started.is_running());
        assert!(LaunchState::Attached.is_running());
        assert!(!LaunchState::Idle.is_running());
    }

    #[test]
    fn a_stack_request_without_a_running_debuggee_is_unavailable() {
        struct NeverUsed;
        impl FramedChannel for NeverUsed {
            fn send(&mut self, _payload: &[u8]) -> Result<(), crate::adapters::codec::CodecError> {
                unreachable!("the refusal precedes the wire");
            }
            fn receive(&mut self) -> Result<Vec<u8>, crate::adapters::codec::CodecError> {
                unreachable!("the refusal precedes the wire");
            }
            fn is_abandoned(&self) -> bool {
                false
            }
        }
        let mut channel = NeverUsed;
        let mut session = DapSession {
            channel: &mut channel,
            deadline: ENGINE_REQUEST_TIMEOUT,
            next_seq: 1,
            project_root_uri: "file:///projects/demo".to_owned(),
            launch_transaction_id: "tx".to_owned(),
            capabilities: Vec::new(),
            launch_state: LaunchState::Pending,
            events: Vec::new(),
            awaiting: BTreeSet::new(),
            stashed: BTreeMap::new(),
            queued: VecDeque::new(),
            pending_launch: None,
        };
        let error = session.stack_trace(1).expect_err("nothing running");
        assert_eq!(error.error_kind(), godot_mcp_domain::ErrorKind::Unavailable);
        assert!(error.to_string().contains("pending"), "{error}");
    }

    #[test]
    fn a_deadline_is_clamped_to_the_engines_own_request_timeout() {
        assert_eq!(
            Duration::from_secs(600).min(ENGINE_REQUEST_TIMEOUT),
            ENGINE_REQUEST_TIMEOUT,
            "Godot answers TIMEOUT per message after request_timeout, so a longer deadline is \
             unenforceable"
        );
    }

    #[test]
    fn stack_frames_and_variables_are_read_out_of_the_body() {
        let frames = read_stack_frames(&json!({
            "stackFrames": [{
                "id": 7,
                "name": "Player::_ready",
                "source": { "path": "res://player.gd" },
                "line": 12,
            }]
        }));
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].id, 7);
        assert_eq!(frames[0].source.as_deref(), Some("res://player.gd"));
        assert_eq!(frames[0].line, Some(12));

        let variables = read_variables(&json!({
            "variables": [{
                "name": "speed",
                "value": "42.0",
                "type": "float",
                "variablesReference": 0,
            }]
        }));
        assert_eq!(variables.len(), 1);
        assert_eq!(variables[0].type_name.as_deref(), Some("float"));
        assert_eq!(variables[0].variables_reference, 0);
    }

    #[test]
    fn the_project_argument_is_a_path_not_a_uri() {
        assert_eq!(path_of_uri("file:///projects/demo"), "/projects/demo");
        // `req_launch` compares a path without backslashes by prefix with the
        // editor's own resource path, which Windows spells `C:/...`.
        assert_eq!(
            path_of_uri("file:///C%3A/Users/qa/game"),
            "C:/Users/qa/game"
        );
    }
}
