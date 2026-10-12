//! The managed-session tools: `project.run.*`, `runtime.*`, `input.*`, the
//! runtime captures, and the editor sessions with their receipt-bound
//! `debug.*` commands.
//!
//! Starting a session and every command to one are journaled through the
//! outcome router like any other effect: idempotent replay, the write block,
//! effect-lane admission and a policy permit re-checked at the boundary. A
//! start is a process effect linked to its supervised launch; a command to a
//! game is a runtime effect and a command to an editor's language server or
//! debug adapter an `lsp_dap` effect, neither touching a project file. Reads
//! ask the session directly and change nothing in it.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use godot_mcp_bridge::protocol::envelope::OperationClass;
use godot_mcp_bridge::runtime as probe_files;
use godot_mcp_domain::ErrorKind;
pub(crate) use godot_mcp_domain::catalog::SessionOperation;
use godot_mcp_domain::contracts::OutcomeError;
use godot_mcp_domain::manifests::canonical_manifest;
use godot_mcp_engine::supervision::journal::{
    OperationFamily as EngineOperationFamily, ProjectIdentityClaim, TransactionEnvelope, hex_sha256,
};
use godot_mcp_engine::supervision::protocol::{LaunchRequest, OutputCaps};
use serde_json::{Map, Value, json};

mod export;

use super::debug::{DebugManager, SESSION_STOP, file_uri};
use super::{SessionKind, SessionManager, SessionState, new_session_id};
use crate::editor::bridge::{BridgeAnswer, BridgeFailure};
use crate::effect::ProcessEffectIntent;
use crate::effect::boundary::{CancelSignal, EditorMutation, SessionStartRequest};
use crate::effect::gateway::EditorEffectIntent;
use crate::engine_plane::EngineRuntime;
use crate::engine_plane::runtime::{engine_transport, journal_transport};
use crate::idempotency::replay::replay_outcome;
use crate::journal::OperationFamily;
use crate::kernel::{KernelToolCall, KernelToolHandler, ServiceKernel};
use crate::router::{
    EnginePresence, PlaneObservation, ProtocolFidelity, RouteEffect, RouteReport, RouteRequest,
    RuntimePresence, WorkspaceFreshness,
};

/// How long a game may take to start and connect its probe.
const READINESS: Duration = Duration::from_secs(60);
/// How long an editor may take to start (a first start imports the project)
/// and listen on both of its ports.
const EDITOR_READINESS: Duration = Duration::from_secs(180);
/// The engine flags an editor session's launch needs.
const EDITOR_FLAGS: [&str; 3] = ["--editor", "--lsp-port", "--dap-port"];
/// How long one language-server or debug-adapter command may take, beyond a
/// `stopped` wait.
const DEBUG_TIMEOUT: Duration = Duration::from_secs(20);
/// The most breakpoint lines one command sets in one script.
const MAX_BREAKPOINTS: usize = 256;
/// The default and the longest time one export may run.
const DEFAULT_EXPORT_SECONDS: u64 = 1800;
const MAX_EXPORT_SECONDS: u64 = 3600;
/// The engine flags an export needs.
const EXPORT_FLAGS: [&str; 3] = ["--headless", "--export-release", "--export-debug"];
/// The default and the longest lifetime of a session.
const DEFAULT_LIFETIME_SECONDS: u64 = 3600;
const MAX_LIFETIME_SECONDS: u64 = 14_400;
/// How long a probe read or a single input command may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
/// The longest input sequence, in total waiting time.
const MAX_SEQUENCE_MS: u64 = 10_000;
/// Display variables a windowed game inherits, and nothing else.
const DISPLAY_VARIABLES: &[&str] = &[
    "DISPLAY",
    "XAUTHORITY",
    "WAYLAND_DISPLAY",
    "XDG_RUNTIME_DIR",
];

/// How a host lets a game open a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisplayHost {
    /// Linux and the BSDs: an X11 or Wayland display named in the environment.
    Unix,
    /// macOS: the window server of a logged-in GUI session.
    MacOs,
    /// Windows: the interactive desktop of a logged-on session.
    Windows,
}

impl DisplayHost {
    const fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(windows) {
            Self::Windows
        } else {
            Self::Unix
        }
    }
}

/// Whether `host` offers a game a window. `variable` reads the server's
/// environment, and `desktop_session` asks macOS whether this is a logged-in
/// GUI session, or Windows whether this process's window station is the
/// visible one of a logged-on session.
fn display_offered(
    host: DisplayHost,
    variable: &dyn Fn(&str) -> Option<String>,
    desktop_session: impl FnOnce() -> bool,
) -> bool {
    let set = |name: &str| variable(name).is_some_and(|value| !value.is_empty());
    match host {
        DisplayHost::Unix => DISPLAY_VARIABLES[..3].iter().any(|name| set(name)),
        DisplayHost::MacOs | DisplayHost::Windows => desktop_session(),
    }
}

/// Whether this server's host offers a game a window right now.
fn display_available() -> bool {
    display_offered(
        DisplayHost::current(),
        &|name| std::env::var(name).ok(),
        desktop_session,
    )
}

/// macOS: `launchctl managername` names the session this process runs in,
/// `Aqua` for a logged-in GUI session and `Background` for a daemon's.
#[cfg(target_os = "macos")]
fn desktop_session() -> bool {
    static GUI_SESSION: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *GUI_SESSION.get_or_init(|| {
        std::process::Command::new("/bin/launchctl")
            .arg("managername")
            .output()
            .is_ok_and(|output| {
                output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "Aqua"
            })
    })
}

/// Windows: a service's or a container's window station has no visible
/// display surfaces; the interactive one of a logged-on session does.
#[cfg(windows)]
fn desktop_session() -> bool {
    godot_mcp_engine::supervision::procfs::window_station_is_visible().unwrap_or(false)
}

/// Elsewhere the display is named in the environment instead.
#[cfg(not(any(target_os = "macos", windows)))]
const fn desktop_session() -> bool {
    false
}

/// Why a windowed session cannot start on this host.
const fn no_display_reason() -> &'static str {
    match DisplayHost::current() {
        DisplayHost::Unix => {
            "a windowed session needs a display: start the server with DISPLAY or \
             WAYLAND_DISPLAY set, or start the session headless"
        }
        DisplayHost::MacOs => {
            "a windowed session needs a logged-in GUI session, and this server runs in a \
             background one; start it from a user's desktop session, or start the session \
             headless"
        }
        DisplayHost::Windows => {
            "a windowed session needs an interactive desktop, and this server's window station \
             has no visible display (a service or container session); start it from a \
             logged-on desktop, or start the session headless"
        }
    }
}

fn outcome_error(kind: ErrorKind, message: impl Into<String>) -> OutcomeError {
    OutcomeError::new(kind, message, false, BTreeMap::new())
}

fn invalid(message: impl Into<String>) -> OutcomeError {
    outcome_error(ErrorKind::InvalidInput, message)
}

/// What each managed-session operation asks of its session, beside the
/// catalog descriptor its operation serves.
trait SessionCall: Copy {
    /// The probe method a read or a command reaches.
    fn probe_method(self) -> &'static str;
    fn input_schema(self) -> Value;
}

impl SessionCall for SessionOperation {
    fn probe_method(self) -> &'static str {
        match self {
            Self::TreeGet => "runtime.tree",
            Self::PropertiesGet => "runtime.properties",
            Self::SignalsGet => "runtime.signals",
            Self::UiGet => "runtime.ui",
            Self::ActionSend | Self::KeySend | Self::MouseSend => "runtime.input.send",
            Self::SequenceSend => "runtime.input.sequence",
            Self::ScreenshotCapture => "runtime.capture.screenshot",
            Self::RecordingCapture => "runtime.capture.recording",
            Self::RunStop => "runtime.session.stop",
            Self::EditorStop => SESSION_STOP,
            Self::ExportCancel => "project.export.cancel",
            Self::RunStart
            | Self::RunStatus
            | Self::RunLogs
            | Self::EditorStart
            | Self::DebugGet
            | Self::DebugCommand
            | Self::ExportPresetsList
            | Self::ExportStart
            | Self::ExportStatus => "",
        }
    }

    fn input_schema(self) -> Value {
        let session = json!({
            "type": "string",
            "minLength": 36,
            "maxLength": 39,
            "description": "The session ID project.run.start returned."
        });
        let editor = json!({
            "type": "string",
            "minLength": 36,
            "maxLength": 39,
            "description": "The editor session ID editor.session.start returned."
        });
        let lifetime = json!({
            "type": "integer",
            "minimum": 1,
            "maximum": MAX_LIFETIME_SECONDS,
            "description": "How long the session may run before it is terminated \
                            (default 3600)."
        });
        let export = json!({
            "type": "string",
            "minLength": 36,
            "maxLength": 39,
            "description": "The export ID project.export returned."
        });
        let node = json!({
            "type": "string",
            "minLength": 1,
            "maxLength": 1024,
            "description": "A node of the running game: an absolute path (/root/...) or a path \
                            relative to the current scene (`.` is the current scene)."
        });
        let state = json!({
            "type": "string",
            "enum": ["press", "release", "tap"],
            "description": "press holds it (until released, holdMs passes, the session stops or \
                            the probe loses the server), release lets go, tap presses and \
                            releases after holdMs. Default tap."
        });
        let hold = json!({
            "type": "integer",
            "minimum": 1,
            "maximum": 30000,
            "description": "How long a press or tap is held, in ms (default 500); a press is \
                            released at the latest then."
        });
        let position = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": { "x": { "type": "number" }, "y": { "type": "number" } },
            "required": ["x", "y"],
            "description": "Viewport coordinates in pixels."
        });
        let modifiers = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "shift": { "type": "boolean" },
                "ctrl": { "type": "boolean" },
                "alt": { "type": "boolean" },
                "meta": { "type": "boolean" }
            },
            "description": "Modifier keys held with the key."
        });
        let button = json!({
            "type": "string",
            "enum": ["left", "right", "middle", "wheel_up", "wheel_down"],
            "description": "The mouse button (default left)."
        });
        let (properties, required): (Value, &[&str]) = match self {
            Self::RunStart => (
                json!({
                    "scene": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 1024,
                        "description": "A project-relative or res:// scene to run instead of the \
                                        main scene."
                    },
                    "display": {
                        "type": "string",
                        "enum": ["auto", "window", "headless"],
                        "description": "window renders into the display the server was started \
                                        with (captures need it); headless renders nothing; auto \
                                        (default) is window when a display is set."
                    },
                    "resolution": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "width": { "type": "integer", "minimum": 64, "maximum": 7680 },
                            "height": { "type": "integer", "minimum": 64, "maximum": 4320 }
                        },
                        "required": ["width", "height"],
                        "description": "The window size; the project's own size when omitted."
                    },
                    "lifetimeSeconds": lifetime
                }),
                &[],
            ),
            Self::EditorStart => (json!({ "lifetimeSeconds": lifetime }), &[]),
            Self::ExportPresetsList => (json!({}), &[]),
            Self::ExportStart => (
                json!({
                    "preset": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 256,
                        "description": "The name of a preset in the project's export_presets.cfg."
                    },
                    "mode": {
                        "type": "string",
                        "enum": ["release", "debug"],
                        "description": "Export through the release (default) or the debug \
                                        template."
                    },
                    "smoke": {
                        "type": "boolean",
                        "description": "Run the exported build once, headless, when this host \
                                        can run it (default false)."
                    },
                    "lifetimeSeconds": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_EXPORT_SECONDS,
                        "description": "How long the export may run before it is terminated \
                                        (default 1800)."
                    }
                }),
                &["preset"],
            ),
            Self::ExportStatus => (json!({ "export": export }), &[]),
            Self::ExportCancel => (json!({ "export": export }), &["export"]),
            Self::EditorStop => (json!({ "session": editor }), &["session"]),
            Self::DebugGet => (json!({ "session": editor }), &[]),
            Self::DebugCommand => (
                json!({
                    "session": editor,
                    "command": debug_command_schema()
                }),
                &["session", "command"],
            ),
            Self::RunStatus => (json!({ "session": session }), &[]),
            Self::RunLogs => (
                json!({
                    "session": session,
                    "stream": {
                        "type": "string",
                        "enum": ["stdout", "stderr"],
                        "description": "Which captured stream (default stdout)."
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "The byte offset to read from (default 0); the previous \
                                        page's next_offset continues it."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": super::MAX_LOG_PAGE,
                        "description": "The most bytes to return (default 16384)."
                    }
                }),
                &["session"],
            ),
            Self::RunStop | Self::ScreenshotCapture => {
                (json!({ "session": session }), &["session"])
            }
            Self::TreeGet => (
                json!({
                    "session": session,
                    "root": node,
                    "depth": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 64,
                        "description": "How many levels below `root` to include (default 8)."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 2000,
                        "description": "The most nodes to return (default 500)."
                    }
                }),
                &["session"],
            ),
            Self::PropertiesGet => (
                json!({
                    "session": session,
                    "node": node,
                    "properties": {
                        "type": "array",
                        "maxItems": 256,
                        "items": { "type": "string", "minLength": 1, "maxLength": 256 },
                        "description": "Property names; the stored and editor-visible ones when \
                                        omitted."
                    }
                }),
                &["session", "node"],
            ),
            Self::SignalsGet => (
                json!({
                    "session": session,
                    "node": node,
                    "watch": {
                        "type": "boolean",
                        "description": "Also count every later emission of the node's signals; \
                                        later reads report the counts."
                    }
                }),
                &["session", "node"],
            ),
            Self::UiGet => (
                json!({
                    "session": session,
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 1000,
                        "description": "The most controls to return (default 500)."
                    },
                    "includeHidden": {
                        "type": "boolean",
                        "description": "Also list controls that are not visible."
                    }
                }),
                &["session"],
            ),
            Self::ActionSend => (
                json!({
                    "session": session,
                    "action": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 256,
                        "description": "An input action of the game's input map, e.g. ui_accept."
                    },
                    "state": state,
                    "strength": {
                        "type": "number",
                        "minimum": 0,
                        "maximum": 1,
                        "description": "How strongly the action is pressed (default 1)."
                    },
                    "holdMs": hold
                }),
                &["session", "action"],
            ),
            Self::KeySend => (
                json!({
                    "session": session,
                    "key": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 32,
                        "description": "A Godot key name: A, Space, Enter, Left, F1, ..."
                    },
                    "state": state,
                    "modifiers": modifiers,
                    "holdMs": hold
                }),
                &["session", "key"],
            ),
            Self::MouseSend => (
                json!({
                    "session": session,
                    "event": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "move": {
                                "type": "object",
                                "additionalProperties": false,
                                "properties": { "position": position },
                                "required": ["position"],
                                "description": "Move the pointer to `position`."
                            },
                            "button": {
                                "type": "object",
                                "additionalProperties": false,
                                "properties": {
                                    "button": button,
                                    "position": position,
                                    "state": state,
                                    "holdMs": hold
                                },
                                "description": "Press, release or tap a `button` (default \
                                                left) at `position`."
                            }
                        },
                        "minProperties": 1,
                        "maxProperties": 1,
                        "description": "One mouse event, keyed by its kind: `move` or `button`."
                    }
                }),
                &["session", "event"],
            ),
            Self::SequenceSend => (
                json!({
                    "session": session,
                    "steps": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 64,
                        "items": crate::schema::one_of(
                            STEP_KINDS
                                .iter()
                                .map(|(kind, fields, required)| {
                                    let fields: Vec<(&str, Value)> = fields
                                        .iter()
                                        .map(|field| {
                                            let schema = match *field {
                                                "action" => json!({ "type": "string", "minLength": 1, "maxLength": 256 }),
                                                "key" => json!({ "type": "string", "minLength": 1, "maxLength": 32 }),
                                                "button" => button.clone(),
                                                "position" => position.clone(),
                                                "state" => state.clone(),
                                                "strength" => json!({ "type": "number", "minimum": 0, "maximum": 1 }),
                                                "modifiers" => modifiers.clone(),
                                                "holdMs" => hold.clone(),
                                                _ => json!({ "type": "integer", "minimum": 0, "maximum": MAX_SEQUENCE_MS }),
                                            };
                                            (*field, schema)
                                        })
                                        .collect();
                                    crate::schema::variant("kind", kind, &fields, required)
                                })
                                .collect()
                        ),
                        "description": "Input steps applied in order; `wait` pauses for `ms`. At \
                                        most 10 s of waiting. Everything the sequence pressed is \
                                        released when it ends or is cancelled."
                    }
                }),
                &["session", "steps"],
            ),
            Self::RecordingCapture => (
                json!({
                    "session": session,
                    "durationMs": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 10000,
                        "description": "How long to record, in ms (default 1000)."
                    },
                    "fps": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 30,
                        "description": "Frames per second (default 10)."
                    },
                    "maxWidth": {
                        "type": "integer",
                        "minimum": 16,
                        "maximum": 4096,
                        "description": "Scale frames down to at most this width."
                    }
                }),
                &["session"],
            ),
        };
        let mut required: Vec<&str> = required.to_vec();
        if self.descriptor().is_mutation() {
            // The handler refuses a mutation without one before anything runs.
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

/// The input schema of one managed-session tool.
pub(crate) fn input_schema(operation: SessionOperation) -> Map<String, Value> {
    operation
        .input_schema()
        .as_object()
        .cloned()
        .expect("session schemas are objects")
}

/// Register every managed-session tool the kernel's profile lists.
pub(crate) fn register_session_tools(
    kernel: &mut ServiceKernel,
    engine: Arc<EngineRuntime>,
    debug: Arc<DebugManager>,
) -> Result<(), OutcomeError> {
    let manifest = canonical_manifest(kernel.profile());
    for &operation in SessionOperation::ALL {
        if manifest.tools.contains(operation.tool_name()) {
            kernel.register_tool_handler(
                operation.key(),
                Arc::new(SessionHandler {
                    operation,
                    engine: Arc::clone(&engine),
                    debug: Arc::clone(&debug),
                }),
            )?;
        }
    }
    Ok(())
}

struct SessionHandler {
    operation: SessionOperation,
    engine: Arc<EngineRuntime>,
    debug: Arc<DebugManager>,
}

fn string<'a>(
    arguments: &'a Map<String, Value>,
    key: &str,
) -> Result<Option<&'a str>, OutcomeError> {
    match arguments.get(key) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(invalid(format!("`{key}` must be a string"))),
    }
}

fn required_string<'a>(
    arguments: &'a Map<String, Value>,
    key: &str,
) -> Result<&'a str, OutcomeError> {
    string(arguments, key)?.ok_or_else(|| invalid(format!("`{key}` is required")))
}

fn integer(
    arguments: &Map<String, Value>,
    key: &str,
    low: u64,
    high: u64,
) -> Result<Option<u64>, OutcomeError> {
    match arguments.get(key) {
        None => Ok(None),
        Some(value) => value
            .as_u64()
            .filter(|value| (low..=high).contains(value))
            .map(Some)
            .ok_or_else(|| invalid(format!("`{key}` must be an integer from {low} to {high}"))),
    }
}

fn boolean(arguments: &Map<String, Value>, key: &str) -> Result<Option<bool>, OutcomeError> {
    match arguments.get(key) {
        None => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(invalid(format!("`{key}` must be a boolean"))),
    }
}

fn res_scene(raw: &str) -> Result<String, OutcomeError> {
    let relative = raw.strip_prefix("res://").unwrap_or(raw);
    if relative.is_empty()
        || relative.starts_with('/')
        || relative.contains('\\')
        || relative
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || !(relative.ends_with(".tscn") || relative.ends_with(".scn"))
    {
        return Err(invalid(format!(
            "`{raw}` must name a project scene (.tscn or .scn)"
        )));
    }
    Ok(format!("res://{relative}"))
}

/// One input step in the probe's spelling, validated.
/// Every input step kind, every field it declares and the fields it requires.
/// The schema and [`sequence_step`] both read it.
const STEP_KINDS: &[(&str, &[&str], &[&str])] = &[
    (
        "action",
        &["action", "state", "strength", "holdMs"],
        &["action"],
    ),
    ("key", &["key", "state", "modifiers", "holdMs"], &["key"]),
    (
        "mouse_button",
        &["button", "position", "state", "holdMs"],
        &[],
    ),
    ("mouse_motion", &["position"], &["position"]),
    ("wait", &["ms"], &["ms"]),
];

/// One step of an input sequence, spelled for the probe.
fn sequence_step(step: &Map<String, Value>) -> Result<Value, OutcomeError> {
    let kind = required_string(step, "kind")?;
    let Some((_, fields, _)) = STEP_KINDS.iter().find(|(name, _, _)| *name == kind) else {
        return Err(invalid(format!("{kind} is not an input step kind")));
    };
    if let Some(unknown) = step
        .keys()
        .find(|key| *key != "kind" && !fields.contains(&key.as_str()))
    {
        return Err(invalid(format!(
            "a {kind} step does not accept `{unknown}`"
        )));
    }
    probe_step(step, kind)
}

fn probe_step(step: &Map<String, Value>, kind: &str) -> Result<Value, OutcomeError> {
    let mut out = Map::new();
    out.insert("kind".to_owned(), Value::String(kind.to_owned()));
    let state = string(step, "state")?.unwrap_or("tap");
    if !matches!(state, "press" | "release" | "tap") {
        return Err(invalid("`state` must be press, release or tap"));
    }
    if kind != "wait" && kind != "mouse_motion" {
        out.insert("state".to_owned(), Value::String(state.to_owned()));
    }
    if let Some(hold) = integer(step, "holdMs", 1, 30_000)? {
        out.insert("hold_ms".to_owned(), json!(hold));
    }
    match kind {
        "action" => {
            let action = required_string(step, "action")?;
            if action.is_empty() || action.len() > 256 {
                return Err(invalid("`action` must name an input action"));
            }
            out.insert("action".to_owned(), Value::String(action.to_owned()));
            if let Some(strength) = step.get("strength") {
                let strength = strength
                    .as_f64()
                    .filter(|strength| (0.0..=1.0).contains(strength))
                    .ok_or_else(|| invalid("`strength` must be a number from 0 to 1"))?;
                out.insert("strength".to_owned(), json!(strength));
            }
        }
        "key" => {
            let key = required_string(step, "key")?;
            if key.is_empty() || key.len() > 32 {
                return Err(invalid("`key` must name a key"));
            }
            out.insert("key".to_owned(), Value::String(key.to_owned()));
            if let Some(modifiers) = step.get("modifiers") {
                let modifiers = modifiers
                    .as_object()
                    .filter(|modifiers| {
                        modifiers.iter().all(|(name, value)| {
                            matches!(name.as_str(), "shift" | "ctrl" | "alt" | "meta")
                                && value.is_boolean()
                        })
                    })
                    .ok_or_else(|| {
                        invalid("`modifiers` maps shift, ctrl, alt and meta to booleans")
                    })?;
                out.insert("modifiers".to_owned(), Value::Object(modifiers.clone()));
            }
        }
        "mouse_button" | "mouse_motion" => {
            if let Some(position) = step.get("position") {
                let valid = position.as_object().is_some_and(|position| {
                    position.len() == 2
                        && position.get("x").is_some_and(Value::is_number)
                        && position.get("y").is_some_and(Value::is_number)
                });
                if !valid {
                    return Err(invalid("`position` is {x, y} in viewport pixels"));
                }
                out.insert("position".to_owned(), position.clone());
            } else if kind == "mouse_motion" {
                return Err(invalid("a mouse move needs `position`"));
            }
            if kind == "mouse_button" {
                let button = string(step, "button")?.unwrap_or("left");
                if !matches!(
                    button,
                    "left" | "right" | "middle" | "wheel_up" | "wheel_down"
                ) {
                    return Err(invalid(
                        "`button` must be left, right, middle, wheel_up or wheel_down",
                    ));
                }
                out.insert("button".to_owned(), Value::String(button.to_owned()));
            }
        }
        "wait" => {
            let ms = integer(step, "ms", 0, MAX_SEQUENCE_MS)?
                .ok_or_else(|| invalid("a wait step needs `ms`"))?;
            out.insert("ms".to_owned(), json!(ms));
        }
        other => return Err(invalid(format!("{other} is not an input step kind"))),
    }
    Ok(Value::Object(out))
}

#[async_trait]
impl KernelToolHandler for SessionHandler {
    async fn call(&self, call: KernelToolCall) -> Result<Value, OutcomeError> {
        if call.cancellation.is_cancelled() {
            return Err(outcome_error(
                ErrorKind::Cancelled,
                "the request was cancelled before it started",
            ));
        }
        // A mutation needs an idempotency id before anything runs; its schema
        // requires one for the same reason.
        if self.operation.descriptor().is_mutation() {
            self.idempotency(&call)?;
        }
        let arguments = &call.arguments;
        match self.operation {
            SessionOperation::RunStatus => self.status(arguments).await,
            SessionOperation::RunLogs => {
                let session = required_string(arguments, "session")?;
                let stream = string(arguments, "stream")?.unwrap_or("stdout");
                let offset = integer(arguments, "offset", 0, u64::MAX)?.unwrap_or(0);
                let limit =
                    integer(arguments, "limit", 1, super::MAX_LOG_PAGE)?.unwrap_or(16 * 1024);
                self.sessions().logs(session, stream, offset, limit)
            }
            SessionOperation::TreeGet
            | SessionOperation::PropertiesGet
            | SessionOperation::SignalsGet
            | SessionOperation::UiGet => self.read(arguments).await,
            SessionOperation::RunStart | SessionOperation::EditorStart => self.start(&call).await,
            SessionOperation::DebugGet => {
                let session = string(arguments, "session")?.map(str::to_owned);
                let debug = Arc::clone(&self.debug);
                tokio::task::spawn_blocking(move || debug.describe(session.as_deref()))
                    .await
                    .map_err(|error| outcome_error(ErrorKind::Internal, error.to_string()))?
            }
            SessionOperation::DebugCommand => self.debug_command(&call).await,
            SessionOperation::ExportPresetsList => self.export_presets().await,
            SessionOperation::ExportStart => self.export_start(&call).await,
            SessionOperation::ExportStatus => self.export_status(&call.access, arguments),
            _ => self.command(&call).await,
        }
    }
}

impl SessionHandler {
    fn sessions(&self) -> &SessionManager {
        self.debug.sessions()
    }

    /// What a command to `session` finds: the session's own state decides
    /// whether a running game, or a running editor hosting its language
    /// server and debug adapter, is present.
    fn observation(&self, session: &str) -> PlaneObservation {
        let active = self
            .sessions()
            .session(session)
            .ok()
            .filter(|session| !session.state().is_terminal())
            .map(|session| session.kind());
        let (engine, runtime) = match active {
            Some(SessionKind::Game) => (EnginePresence::HeadlessOnly, RuntimePresence::GameRunning),
            Some(SessionKind::Editor) => {
                (EnginePresence::EditorConnected, RuntimePresence::NoInstance)
            }
            Some(SessionKind::Export) | None => {
                (EnginePresence::HeadlessOnly, RuntimePresence::NoInstance)
            }
        };
        PlaneObservation::new(
            self.engine.router.plane_availability(),
            engine,
            runtime,
            ProtocolFidelity::Full,
            if self.engine.workspace.current().coherence().is_coherent() {
                WorkspaceFreshness::Fresh
            } else {
                WorkspaceFreshness::StaleIndex
            },
        )
    }

    async fn status(&self, arguments: &Map<String, Value>) -> Result<Value, OutcomeError> {
        let Some(id) = string(arguments, "session")? else {
            return Ok(json!({ "sessions": self.sessions().list() }));
        };
        let session = self.sessions().session(id)?;
        let mut value = session.public();
        if let (Ok(bridge), Value::Object(object)) = (self.sessions().running_game(id), &mut value)
        {
            let held = tokio::task::spawn_blocking(move || {
                bridge.call(
                    "runtime.input.state",
                    OperationClass::Read,
                    &json!({}),
                    "run-status",
                    PROBE_TIMEOUT,
                )
            })
            .await
            .map_err(|error| outcome_error(ErrorKind::Internal, error.to_string()))?;
            if let Ok(BridgeAnswer::Result { payload, .. }) = held {
                object.insert(
                    "held_input".to_owned(),
                    payload.get("held").cloned().unwrap_or(Value::Null),
                );
            }
        }
        Ok(value)
    }

    async fn read(&self, arguments: &Map<String, Value>) -> Result<Value, OutcomeError> {
        let id = required_string(arguments, "session")?;
        let bridge = self.sessions().running_game(id)?;
        let mut payload = Map::new();
        match self.operation {
            SessionOperation::TreeGet => {
                if let Some(root) = string(arguments, "root")? {
                    payload.insert("root".to_owned(), Value::String(root.to_owned()));
                }
                if let Some(depth) = integer(arguments, "depth", 0, 64)? {
                    payload.insert("depth".to_owned(), json!(depth));
                }
                if let Some(limit) = integer(arguments, "limit", 1, 2000)? {
                    payload.insert("limit".to_owned(), json!(limit));
                }
            }
            SessionOperation::PropertiesGet => {
                payload.insert(
                    "node".to_owned(),
                    Value::String(required_string(arguments, "node")?.to_owned()),
                );
                if let Some(names) = arguments.get("properties") {
                    let names = names
                        .as_array()
                        .filter(|names| names.len() <= 256 && names.iter().all(Value::is_string))
                        .ok_or_else(|| invalid("`properties` is an array of at most 256 names"))?;
                    payload.insert("properties".to_owned(), Value::Array(names.clone()));
                }
            }
            SessionOperation::SignalsGet => {
                payload.insert(
                    "node".to_owned(),
                    Value::String(required_string(arguments, "node")?.to_owned()),
                );
                if let Some(watch) = boolean(arguments, "watch")? {
                    payload.insert("watch".to_owned(), Value::Bool(watch));
                }
            }
            _ => {
                if let Some(limit) = integer(arguments, "limit", 1, 1000)? {
                    payload.insert("limit".to_owned(), json!(limit));
                }
                if let Some(hidden) = boolean(arguments, "includeHidden")? {
                    payload.insert("include_hidden".to_owned(), Value::Bool(hidden));
                }
            }
        }
        let method = self.operation.probe_method();
        let answer = tokio::task::spawn_blocking(move || {
            bridge.call(
                method,
                OperationClass::Read,
                &Value::Object(payload),
                "runtime-read",
                PROBE_TIMEOUT,
            )
        })
        .await
        .map_err(|error| outcome_error(ErrorKind::Internal, error.to_string()))?;
        let mut value = match answer {
            Ok(BridgeAnswer::Result { payload, .. }) => payload,
            Ok(BridgeAnswer::Error { kind, message, .. }) => {
                return Err(outcome_error(kind, message));
            }
            Err(failure @ BridgeFailure::Unsupported(_)) => {
                return Err(outcome_error(
                    ErrorKind::Unsupported,
                    failure.probe_message(),
                ));
            }
            Err(failure) => {
                return Err(outcome_error(
                    ErrorKind::Unavailable,
                    failure.probe_message(),
                ));
            }
        };
        if let Value::Object(object) = &mut value {
            object.insert("session".to_owned(), Value::String(id.to_owned()));
        }
        Ok(value)
    }

    fn idempotency(&self, call: &KernelToolCall) -> Result<String, OutcomeError> {
        call.idempotency_id.clone().ok_or_else(|| {
            invalid(format!(
                "{} requires `idempotencyId`, so a retried call can never apply twice",
                self.operation.tool_name()
            ))
        })
    }

    async fn start(&self, call: &KernelToolCall) -> Result<Value, OutcomeError> {
        let tool = self.operation.tool_name();
        let idempotency_id = self.idempotency(call)?;
        let arguments = &call.arguments;
        let access = &call.access;
        let engine = self.engine.as_ref();
        let kind = if self.operation == SessionOperation::EditorStart {
            SessionKind::Editor
        } else {
            SessionKind::Game
        };
        let scene = string(arguments, "scene")?.map(res_scene).transpose()?;
        let display_requested = match kind {
            // An editor session never shows the editor itself; a game it runs
            // under the debugger renders into the server's display, when it
            // has one.
            SessionKind::Editor | SessionKind::Export => "headless",
            SessionKind::Game => string(arguments, "display")?.unwrap_or("auto"),
        };
        let has_display = display_available();
        let display = match display_requested {
            "auto" if has_display => "window",
            "auto" | "headless" => "headless",
            "window" if has_display => "window",
            "window" => {
                return Err(outcome_error(ErrorKind::Unavailable, no_display_reason()));
            }
            _ => return Err(invalid("`display` must be auto, window or headless")),
        };
        let resolution = match arguments.get("resolution") {
            None => None,
            Some(Value::Object(size)) => {
                let width = size
                    .get("width")
                    .and_then(Value::as_u64)
                    .filter(|width| (64..=7680).contains(width));
                let height = size
                    .get("height")
                    .and_then(Value::as_u64)
                    .filter(|height| (64..=4320).contains(height));
                match (width, height, size.len()) {
                    (Some(width), Some(height), 2) => Some((width, height)),
                    _ => return Err(invalid("`resolution` is {width 64..7680, height 64..4320}")),
                }
            }
            Some(_) => return Err(invalid("`resolution` is {width, height}")),
        };
        let lifetime = integer(arguments, "lifetimeSeconds", 1, MAX_LIFETIME_SECONDS)?
            .unwrap_or(DEFAULT_LIFETIME_SECONDS);

        let resolved = engine.resolve_engine().await?;
        let session_id = new_session_id(kind);
        let paths = self.sessions().paths(&session_id);
        let mut argv: Vec<String> = Vec::new();
        let mut environment = resolved.binding.environment.clone();
        let mut ports = (None, None);
        if kind == SessionKind::Editor {
            if let Some(flag) = EDITOR_FLAGS
                .iter()
                .find(|flag| !resolved.binding.capabilities.contains(**flag))
            {
                return Err(outcome_error(
                    ErrorKind::Unsupported,
                    format!(
                        "this Godot build does not advertise {flag}, which an editor session \
                         needs; use an editor build of Godot 4.4 or later"
                    ),
                ));
            }
            let (lsp, dap) = self.sessions().allocate_editor_ports()?;
            ports = (Some(lsp), Some(dap));
            argv.extend([
                "--headless".to_owned(),
                "--editor".to_owned(),
                "--path".to_owned(),
                self.sessions().project_root().display().to_string(),
                "--lsp-port".to_owned(),
                lsp.to_string(),
                "--dap-port".to_owned(),
                dap.to_string(),
            ]);
            if has_display {
                environment
                    .allow
                    .extend(DISPLAY_VARIABLES.iter().map(|name| (*name).to_owned()));
            }
        } else {
            if display == "headless" {
                argv.push("--headless".to_owned());
            } else {
                if let Some((width, height)) = resolution {
                    argv.push("--resolution".to_owned());
                    argv.push(format!("{width}x{height}"));
                }
                argv.push("--position".to_owned());
                argv.push("0,0".to_owned());
            }
            argv.push("--path".to_owned());
            argv.push(self.sessions().project_root().display().to_string());
            argv.push("--script".to_owned());
            argv.push(paths.launcher.display().to_string());
            environment.extra.insert(
                probe_files::RENDEZVOUS_ENV.to_owned(),
                paths.rendezvous.display().to_string(),
            );
            if let Some(scene) = &scene {
                environment
                    .extra
                    .insert(probe_files::SCENE_ENV.to_owned(), scene.clone());
            }
            if display == "window" {
                environment
                    .allow
                    .extend(DISPLAY_VARIABLES.iter().map(|name| (*name).to_owned()));
            }
        }

        let observed = access.observed_at_unix_ms();
        let outer_id = engine.router.next_transaction_id(tool, observed);
        let inner_id = engine
            .router
            .next_transaction_id("supervised-launch", observed);
        let principal = access.mode_principal().to_journal_string();
        let connection = Some(access.connection_session().to_owned());
        let canonical = Value::Object(arguments.clone());
        let canonical_bytes = serde_json::to_vec(&canonical)
            .map_err(|error| outcome_error(ErrorKind::Internal, error.to_string()))?;
        let intent = ProcessEffectIntent {
            transaction_id: outer_id.clone(),
            tool_name: tool.to_owned(),
            operation_family: OperationFamily::RuntimeExecution,
            idempotency_id: idempotency_id.clone(),
            transport_mode: journal_transport(access.transport_mode()),
            principal: Some(principal.clone()),
            session: connection.clone(),
            arguments: canonical.clone(),
            required_process_transaction: inner_id.clone(),
            opened_at_unix_ms: observed,
        };
        let request = SessionStartRequest {
            session_id,
            kind,
            launch: LaunchRequest {
                envelope: TransactionEnvelope {
                    transaction_id: inner_id,
                    project: ProjectIdentityClaim {
                        object_identity: engine.held.object_identity.clone(),
                        root_object: engine.held.root_object.clone(),
                        file_object: engine.held.file_object.clone(),
                        generation: engine.held.generation,
                    },
                    transport_mode: engine_transport(access.transport_mode()),
                    principal: Some(principal),
                    session: connection,
                    operation_family: EngineOperationFamily::RuntimeExecution,
                    tool_name: tool.to_owned(),
                    idempotency_id: format!("launch-{idempotency_id}"),
                    canonical_request_hash: hex_sha256(&canonical_bytes),
                    canonical_request_bytes: canonical_bytes.len() as u64,
                    opened_at_unix_ms: observed,
                },
                trust: resolved.binding.trust,
                program: resolved.binding.program,
                arguments: argv,
                binary_sha256: resolved.binding.binary_sha256,
                supervisor_binary: crate::engine_plane::process_supervisor_binary(),
                project_root_display: self.sessions().project_root().to_path_buf(),
                environment,
                caps: OutputCaps {
                    stdout_bytes: 1_048_576,
                    stderr_bytes: 1_048_576,
                },
                deadline: Duration::from_secs(lifetime),
                work_dir: self.sessions().work_dir().to_path_buf(),
                evidence_path: None,
            },
            scene,
            display: display.to_owned(),
            lifetime: Duration::from_secs(lifetime),
            readiness: match kind {
                SessionKind::Editor => EDITOR_READINESS,
                SessionKind::Game | SessionKind::Export => READINESS,
            },
            godot_version: None,
            lsp_port: ports.0,
            dap_port: ports.1,
            export: None,
        };
        let route = RouteRequest {
            tool: tool.to_owned(),
            idempotency_id,
            transaction_id: Some(outer_id),
            path: String::new(),
            bytes: Vec::new(),
            arguments: canonical,
            observation: engine.route_observation(),
            cancellation: call.cancellation.clone(),
            queued_at_unix_ms: observed,
            identity: Some(EngineRuntime::route_identity(access)),
            effect: Some(RouteEffect::SessionStart {
                intent,
                request: Box::new(request),
            }),
            audit: Some(call.audit.clone()),
        };
        let report = engine
            .router
            .route(&route)
            .await
            .map_err(|refusal| refusal.to_outcome())?;
        self.start_answer(&report, "session")
    }

    /// The answer to a start, naming the session under `key`.
    fn start_answer(&self, report: &RouteReport, key: &str) -> Result<Value, OutcomeError> {
        let transaction_id = report.transaction_id().unwrap_or_default().to_owned();
        if report.replayed() {
            // The replay answers with the session the recorded start created.
            if let Some(session) = self.sessions().started_by(&transaction_id) {
                let mut answer = json!({
                    "transaction_id": transaction_id,
                    "mutation_status": "committed",
                    "replayed": true,
                });
                answer[key] = session.public();
                return Ok(answer);
            }
            let status = report.recorded_status().ok_or_else(|| {
                outcome_error(ErrorKind::Internal, "a replay carried no recorded status")
            })?;
            let verdict = report.recorded_verdict().ok_or_else(|| {
                outcome_error(
                    ErrorKind::Unavailable,
                    "a replay carried no terminal outcome",
                )
            })?;
            return replay_outcome(&transaction_id, status, report.recorded_result(), verdict);
        }
        let outcome = report.session_outcome().ok_or_else(|| {
            outcome_error(ErrorKind::Internal, "the session start returned no outcome")
        })?;
        let mut answer = json!({
            "transaction_id": transaction_id,
            "mutation_status": "committed",
            "execution": {
                "boundary_kind": outcome.boundary_kind,
                "boundary_strength": outcome.boundary_strength,
            },
        });
        answer[key] = outcome.session.clone();
        Ok(answer)
    }

    async fn command(&self, call: &KernelToolCall) -> Result<Value, OutcomeError> {
        let tool = self.operation.tool_name();
        let idempotency_id = self.idempotency(call)?;
        let arguments = &call.arguments;
        // An export is named by its own key; every other session by `session`.
        let session = if self.operation == SessionOperation::ExportCancel {
            required_string(arguments, "export")?.to_owned()
        } else {
            required_string(arguments, "session")?.to_owned()
        };
        // A command is refused up front, unjournaled, when its session cannot
        // take one; stopping a session that already ended answers its record.
        if matches!(
            self.operation,
            SessionOperation::RunStop
                | SessionOperation::EditorStop
                | SessionOperation::ExportCancel
        ) {
            let known = self.sessions().session(&session)?;
            if !known.belongs_to(call.access.principal().as_str()) {
                return Err(export::not_this_callers(&session));
            }
            let expected = match self.operation {
                SessionOperation::EditorStop => SessionKind::Editor,
                SessionOperation::ExportCancel => SessionKind::Export,
                _ => SessionKind::Game,
            };
            if known.kind() != expected {
                let other_tool = match known.kind() {
                    SessionKind::Game => SessionOperation::RunStop.tool_name(),
                    SessionKind::Editor => SessionOperation::EditorStop.tool_name(),
                    SessionKind::Export => SessionOperation::ExportCancel.tool_name(),
                };
                return Err(invalid(format!(
                    "{session} is a {} session; stop it with {other_tool}",
                    known.kind().as_str()
                )));
            }
            if known.state().is_terminal() {
                // Nothing is left to stop, so nothing is journaled.
                let mut value = known.public();
                if let Value::Object(object) = &mut value {
                    object.insert("already_ended".to_owned(), Value::Bool(true));
                }
                return Ok(value);
            }
        } else {
            self.sessions().running_game(&session)?;
        }
        let mut payload = Map::new();
        payload.insert("session".to_owned(), Value::String(session.clone()));
        let mut timeout = PROBE_TIMEOUT;
        match self.operation {
            SessionOperation::ActionSend => {
                payload.extend(as_object(probe_step(
                    &arguments_without_session(arguments),
                    "action",
                )?));
            }
            SessionOperation::KeySend => {
                payload.extend(as_object(probe_step(
                    &arguments_without_session(arguments),
                    "key",
                )?));
            }
            SessionOperation::MouseSend => {
                payload.extend(as_object(mouse_step(arguments)?));
            }
            SessionOperation::SequenceSend => {
                let steps = arguments
                    .get("steps")
                    .and_then(Value::as_array)
                    .filter(|steps| !steps.is_empty() && steps.len() <= 64)
                    .ok_or_else(|| invalid("`steps` is an array of 1 to 64 steps"))?;
                let mut probe_steps = Vec::with_capacity(steps.len());
                let mut waited = 0_u64;
                for step in steps {
                    let step = step
                        .as_object()
                        .ok_or_else(|| invalid("every step is an object"))?;
                    let converted = sequence_step(step)?;
                    waited += converted.get("ms").and_then(Value::as_u64).unwrap_or(0);
                    probe_steps.push(converted);
                }
                if waited > MAX_SEQUENCE_MS {
                    return Err(invalid("a sequence waits at most 10000 ms in total"));
                }
                payload.insert("steps".to_owned(), Value::Array(probe_steps));
                timeout += Duration::from_millis(waited);
            }
            SessionOperation::RecordingCapture => {
                let duration = integer(arguments, "durationMs", 1, 10_000)?.unwrap_or(1000);
                payload.insert("duration_ms".to_owned(), json!(duration));
                payload.insert(
                    "fps".to_owned(),
                    json!(integer(arguments, "fps", 1, 30)?.unwrap_or(10)),
                );
                if let Some(width) = integer(arguments, "maxWidth", 16, 4096)? {
                    payload.insert("max_width".to_owned(), json!(width));
                }
                timeout += Duration::from_millis(duration);
            }
            _ => {}
        }
        if matches!(
            self.operation,
            SessionOperation::ScreenshotCapture | SessionOperation::RecordingCapture
        ) {
            payload.insert(
                "owner".to_owned(),
                json!({
                    "principal": call.access.principal().as_str(),
                    "session": call.access.session().map(|session| session.as_str().to_owned()),
                }),
            );
        }
        let engine = self.engine.as_ref();
        let access = &call.access;
        let observed = access.observed_at_unix_ms();
        let transaction_id = engine.router.next_transaction_id(tool, observed);
        let canonical = Value::Object(arguments.clone());
        let intent = EditorEffectIntent {
            transaction_id: transaction_id.clone(),
            tool_name: tool.to_owned(),
            operation_family: if self.operation == SessionOperation::ExportCancel {
                OperationFamily::DestructiveExport
            } else {
                OperationFamily::RuntimeExecution
            },
            idempotency_id: idempotency_id.clone(),
            transport_mode: journal_transport(access.transport_mode()),
            principal: Some(access.mode_principal().to_journal_string()),
            session: Some(access.connection_session().to_owned()),
            arguments: canonical.clone(),
            files: Vec::new(),
            opened_at_unix_ms: observed,
        };
        // Synthetic input in flight is withdrawn from the probe when the
        // caller cancels, so a cancelled sequence releases what it pressed at
        // once rather than when it would have ended.
        let command = Box::new(EditorMutation {
            action: self.operation.probe_method().to_owned(),
            payload: Value::Object(payload).to_string(),
            idempotency_id,
            timeout,
            cancellation: CancelSignal::of(&call.cancellation),
        });
        let effect = match self.operation {
            SessionOperation::EditorStop => RouteEffect::Debug { intent, command },
            SessionOperation::ExportCancel => RouteEffect::SessionStop { intent, command },
            _ => RouteEffect::Runtime { intent, command },
        };
        self.route_command(call, transaction_id, canonical, effect, session)
            .await
    }

    /// Route one journaled command to `session` and answer it.
    async fn route_command(
        &self,
        call: &KernelToolCall,
        transaction_id: String,
        canonical: Value,
        effect: RouteEffect,
        session: String,
    ) -> Result<Value, OutcomeError> {
        let engine = self.engine.as_ref();
        let route = RouteRequest {
            tool: self.operation.tool_name().to_owned(),
            idempotency_id: self.idempotency(call)?,
            transaction_id: Some(transaction_id),
            path: String::new(),
            bytes: Vec::new(),
            arguments: canonical,
            observation: self.observation(&session),
            cancellation: call.cancellation.clone(),
            queued_at_unix_ms: call.access.observed_at_unix_ms(),
            identity: Some(EngineRuntime::route_identity(&call.access)),
            effect: Some(effect),
            audit: Some(call.audit.clone()),
        };
        let report = engine
            .router
            .route(&route)
            .await
            .map_err(|refusal| refusal.to_outcome())?;
        let mut answer = crate::editor::tools::editor_answer(&report)?;
        if let Value::Object(object) = &mut answer {
            object
                .entry("session".to_owned())
                .or_insert(Value::String(session));
        }
        Ok(answer)
    }

    /// One project GDScript file's text, read through the held lease.
    fn project_text(&self, relative: &str) -> Result<String, OutcomeError> {
        let root = self.engine.root()?;
        let bytes = root
            .read(std::path::Path::new(relative))
            .map_err(|error| match error {
                godot_mcp_domain::safe_root::SafeRootError::StalePreimage { .. } => outcome_error(
                    ErrorKind::NotFound,
                    format!("res://{relative} does not exist"),
                ),
                error => outcome_error(
                    error.error_kind(),
                    format!("res://{relative} could not be read as project source"),
                ),
            })?;
        String::from_utf8(bytes).map_err(|_| invalid(format!("res://{relative} is not UTF-8 text")))
    }

    async fn debug_command(&self, call: &KernelToolCall) -> Result<Value, OutcomeError> {
        let tool = self.operation.tool_name();
        let idempotency_id = self.idempotency(call)?;
        let arguments = &call.arguments;
        let session = required_string(arguments, "session")?.to_owned();
        let (command, fields) = debug_command_of(arguments)?;
        // A command is refused up front, unjournaled, when its session cannot
        // take one.
        let known = self.sessions().session(&session)?;
        if known.kind() != SessionKind::Editor {
            return Err(invalid(format!(
                "{session} is a game session; debugging needs an editor session from \
                 editor.session.start"
            )));
        }
        if known.state() != SessionState::Running {
            return Err(outcome_error(
                ErrorKind::Unavailable,
                format!("{session} is {}, not running", known.state().as_str()),
            ));
        }
        let mut payload = Map::new();
        payload.insert("session".to_owned(), Value::String(session.clone()));
        let mut timeout = DEBUG_TIMEOUT;
        match command {
            "diagnostics" | "symbols" | "breakpoints" => {
                let relative = gdscript_path(required_string(fields, "path")?)?;
                let text = self.project_text(&relative)?;
                let absolute = self.sessions().project_root().join(&relative);
                payload.insert("path".to_owned(), json!(format!("res://{relative}")));
                payload.insert("uri".to_owned(), json!(file_uri(&absolute)));
                payload.insert("absolute".to_owned(), json!(absolute.display().to_string()));
                if command == "diagnostics" {
                    payload.insert("text".to_owned(), Value::String(text));
                } else if command == "breakpoints" {
                    let lines = breakpoint_lines(fields)?;
                    payload.insert("lines".to_owned(), Value::Array(lines.clone()));
                    payload.insert("tool".to_owned(), Value::Bool(is_tool_script(&text)));
                }
            }
            "stopped" => {
                let wait = integer(fields, "timeoutMs", 1, super::debug::MAX_STOP_WAIT_MS)?
                    .unwrap_or(10_000);
                payload.insert("timeout_ms".to_owned(), json!(wait));
                timeout += Duration::from_millis(wait);
            }
            "stack" | "continue" | "pause" => {
                if let Some(thread) = integer(fields, "thread", 1, 2_147_483_647)? {
                    payload.insert("thread".to_owned(), json!(thread));
                }
            }
            "scopes" => {
                let frame = integer(fields, "frame", 0, 2_147_483_647)?.unwrap_or(0);
                payload.insert("frame".to_owned(), json!(frame));
            }
            "variables" => {
                let reference = integer(fields, "reference", 1, 2_147_483_647)?
                    .ok_or_else(|| invalid("variables needs `reference` from scopes"))?;
                payload.insert("reference".to_owned(), json!(reference));
            }
            _ => {}
        }
        let engine = self.engine.as_ref();
        let access = &call.access;
        let observed = access.observed_at_unix_ms();
        let transaction_id = engine.router.next_transaction_id(tool, observed);
        let canonical = Value::Object(arguments.clone());
        let intent = EditorEffectIntent {
            transaction_id: transaction_id.clone(),
            tool_name: tool.to_owned(),
            operation_family: OperationFamily::RuntimeExecution,
            idempotency_id: idempotency_id.clone(),
            transport_mode: journal_transport(access.transport_mode()),
            principal: Some(access.mode_principal().to_journal_string()),
            session: Some(access.connection_session().to_owned()),
            arguments: canonical.clone(),
            files: Vec::new(),
            opened_at_unix_ms: observed,
        };
        let effect = RouteEffect::Debug {
            intent,
            command: Box::new(EditorMutation {
                action: command.to_owned(),
                payload: Value::Object(payload).to_string(),
                idempotency_id,
                timeout,
                cancellation: CancelSignal::default(),
            }),
        };
        self.route_command(call, transaction_id, canonical, effect, session)
            .await
    }
}

/// One `input.mouse.send` request as a probe step.
fn mouse_step(arguments: &Map<String, Value>) -> Result<Value, OutcomeError> {
    let keyed = arguments
        .get("event")
        .and_then(Value::as_object)
        .filter(|keyed| keyed.len() == 1)
        .ok_or_else(|| invalid("`event` must be an object with one member: `move` or `button`"))?;
    let (kind, fields) = keyed.iter().next().expect("one member");
    let kind = match kind.as_str() {
        "move" => "mouse_motion",
        "button" => "mouse_button",
        other => {
            return Err(invalid(format!(
                "`{other}` is not a mouse event; use move or button"
            )));
        }
    };
    let fields = fields
        .as_object()
        .ok_or_else(|| invalid("a mouse event takes an object of its fields"))?;
    // The key already names the kind, and the step's `kind` is set from it, so
    // a `kind` among the fields would be silently replaced.
    if fields.contains_key("kind") {
        return Err(invalid(
            "a mouse event does not accept `kind`; its key names the kind",
        ));
    }
    // The event is the sequence step of that kind, with the same fields.
    let mut step = fields.clone();
    step.insert("kind".to_owned(), Value::String(kind.to_owned()));
    sequence_step(&step)
}

/// Every debug command, the fields that apply to it, and the ones it needs.
/// The schema and [`debug_command_of`] both read it.
const DEBUG_COMMAND_FIELDS: &[(&[&str], &[&str], &[&str])] = &[
    (&["diagnostics", "symbols"], &["path"], &["path"]),
    (&["breakpoints"], &["path", "lines"], &["path", "lines"]),
    (&["stopped"], &["timeoutMs"], &[]),
    (&["stack", "continue", "pause"], &["thread"], &[]),
    (&["scopes"], &["frame"], &[]),
    (&["variables"], &["reference"], &["reference"]),
    (&["launch", "stop"], &[], &[]),
];

/// The `command` of `debug.command`: an object with exactly one member, keyed
/// by the command's name, whose closed shape declares exactly that command's
/// fields of [`DEBUG_COMMAND_FIELDS`], the ones it needs required.
fn debug_command_schema() -> Value {
    let field = |name: &str| match name {
        "path" => json!({
            "type": "string",
            "minLength": 1,
            "maxLength": 1024,
            "pattern": crate::schema::gdscript_path_pattern(),
            "description": "A project GDScript file, project-relative or res://."
        }),
        "lines" => json!({
            "type": "array",
            "maxItems": MAX_BREAKPOINTS,
            "items": { "type": "integer", "minimum": 1, "maximum": 1_000_000 },
            "description": "One-based breakpoint lines; an empty list clears the file's \
                            breakpoints."
        }),
        "timeoutMs" => json!({
            "type": "integer",
            "minimum": 1,
            "maximum": super::debug::MAX_STOP_WAIT_MS,
            "description": "How long to wait, in ms (default 10000)."
        }),
        "thread" => json!({
            "type": "integer",
            "minimum": 1,
            "maximum": 2_147_483_647,
            "description": "The debugged thread (default 1, the main thread)."
        }),
        "frame" => json!({
            "type": "integer",
            "minimum": 0,
            "maximum": 2_147_483_647,
            "description": "A frame id stack returned (default 0)."
        }),
        "reference" => json!({
            "type": "integer",
            "minimum": 1,
            "maximum": 2_147_483_647,
            "description": "A variables reference scopes or variables returned."
        }),
        other => unreachable!("no debug command takes `{other}`"),
    };
    let mut commands = Map::new();
    for (names, applies, needs) in DEBUG_COMMAND_FIELDS {
        let fields: Map<String, Value> = applies
            .iter()
            .map(|name| ((*name).to_owned(), field(name)))
            .collect();
        for name in *names {
            commands.insert(
                (*name).to_owned(),
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": fields,
                    "required": needs,
                }),
            );
        }
    }
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": commands,
        "minProperties": 1,
        "maxProperties": 1,
        "description": "One command, keyed by its name, with its own fields. Language server: \
                        diagnostics (the engine's own for `path`) or symbols (of `path`). Debug \
                        adapter: breakpoints (replace `path`'s with `lines`), launch (run the \
                        main scene under the debugger), stopped (wait up to `timeoutMs` for the \
                        next stop), stack, continue and pause (of `thread`), scopes (of \
                        `frame`), variables (of `reference`), or stop (end the debugged game)."
    })
}

/// The `command` of a `debug.command` request, once it is checked as far as
/// it can be without its session: its name, the fields it takes and needs, and
/// each field's value. Answers the name and the command's fields.
fn debug_command_of(
    arguments: &Map<String, Value>,
) -> Result<(&str, &Map<String, Value>), OutcomeError> {
    let keyed = arguments
        .get("command")
        .and_then(Value::as_object)
        .filter(|keyed| keyed.len() == 1)
        .ok_or_else(|| {
            invalid("`command` must be an object with one member, keyed by the command's name")
        })?;
    let (command, fields) = keyed.iter().next().expect("one member");
    let command = command.as_str();
    let Some((_, applies, needs)) = DEBUG_COMMAND_FIELDS
        .iter()
        .find(|(commands, _, _)| commands.contains(&command))
    else {
        return Err(invalid(format!("`{command}` is not a debug command")));
    };
    let fields = fields
        .as_object()
        .ok_or_else(|| invalid(format!("{command} takes an object of its fields")))?;
    if let Some(stray) = fields.keys().find(|key| !applies.contains(&key.as_str())) {
        return Err(invalid(format!("`{stray}` does not apply to {command}")));
    }
    if let Some(missing) = needs.iter().find(|field| !fields.contains_key(**field)) {
        return Err(invalid(format!("{command} needs `{missing}`")));
    }
    if applies.contains(&"path") {
        gdscript_path(required_string(fields, "path")?)?;
    }
    if applies.contains(&"lines") {
        breakpoint_lines(fields)?;
    }
    integer(fields, "timeoutMs", 1, super::debug::MAX_STOP_WAIT_MS)?;
    integer(fields, "thread", 1, 2_147_483_647)?;
    integer(fields, "frame", 0, 2_147_483_647)?;
    integer(fields, "reference", 1, 2_147_483_647)?;
    Ok((command, fields))
}

/// The one-based lines of a `breakpoints` command.
fn breakpoint_lines(arguments: &Map<String, Value>) -> Result<&Vec<Value>, OutcomeError> {
    arguments
        .get("lines")
        .and_then(Value::as_array)
        .filter(|lines| {
            lines.len() <= MAX_BREAKPOINTS
                && lines.iter().all(|line| {
                    line.as_u64()
                        .is_some_and(|line| (1..=1_000_000).contains(&line))
                })
        })
        .ok_or_else(|| {
            invalid(format!(
                "breakpoints needs `lines`: at most {MAX_BREAKPOINTS} one-based line numbers"
            ))
        })
}

/// The project-relative spelling of a project GDScript file named
/// project-relative or by `res://`.
fn gdscript_path(raw: &str) -> Result<String, OutcomeError> {
    let relative = raw.strip_prefix("res://").unwrap_or(raw);
    if relative.is_empty()
        || relative.starts_with('/')
        || relative.contains('\\')
        || relative
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || !relative.ends_with(".gd")
    {
        return Err(invalid(format!(
            "`{raw}` must name a project GDScript file"
        )));
    }
    Ok(relative.to_owned())
}

/// Whether a script is a `@tool` script: its first statement is the
/// annotation.
fn is_tool_script(text: &str) -> bool {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .is_some_and(|line| line.starts_with("@tool"))
}

fn arguments_without_session(arguments: &Map<String, Value>) -> Map<String, Value> {
    let mut step = arguments.clone();
    step.remove("session");
    step
}

fn as_object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(object) => object,
        _ => Map::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(value: Value) -> Map<String, Value> {
        value.as_object().cloned().expect("object")
    }

    #[test]
    fn a_display_is_what_each_desktop_offers_its_windows() {
        let none = |_: &str| None::<String>;
        let x11 = |name: &str| (name == "DISPLAY").then(|| ":0".to_owned());
        let empty_x11 = |name: &str| (name == "DISPLAY").then(String::new);
        let wayland = |name: &str| (name == "WAYLAND_DISPLAY").then(|| "wayland-0".to_owned());
        let console = |name: &str| (name == "SESSIONNAME").then(|| "Console".to_owned());

        assert!(display_offered(DisplayHost::Unix, &x11, || false));
        assert!(display_offered(DisplayHost::Unix, &wayland, || false));
        assert!(!display_offered(DisplayHost::Unix, &empty_x11, || false));
        assert!(!display_offered(DisplayHost::Unix, &none, || true));
        // macOS draws through the window server of a logged-in GUI session, which
        // no variable names; a background session has none.
        assert!(display_offered(DisplayHost::MacOs, &none, || true));
        assert!(!display_offered(DisplayHost::MacOs, &x11, || false));
        // Windows draws on the visible window station of a logged-on session,
        // whatever the environment says: a program Task Scheduler starts on
        // that desktop has no SESSIONNAME, and a service may inherit one.
        assert!(display_offered(DisplayHost::Windows, &none, || true));
        assert!(!display_offered(DisplayHost::Windows, &console, || false));
    }

    /// A sequence step declares exactly its kind's fields, as the schema does.
    #[test]
    fn a_sequence_step_refuses_a_field_its_kind_does_not_declare() {
        for step in [
            json!({"kind": "wait", "ms": 10, "colour": "red"}),
            json!({"kind": "wait", "ms": 10, "holdMs": 5}),
            json!({"kind": "action", "action": "jump", "ms": 5}),
            json!({"kind": "mouse_motion", "position": {"x": 1, "y": 2}, "button": "left"}),
        ] {
            assert!(sequence_step(&map(step.clone())).is_err(), "{step}");
        }
        assert!(sequence_step(&map(json!({"kind": "wait", "ms": 10}))).is_ok());
        assert!(
            sequence_step(&map(
                json!({"kind": "action", "action": "jump", "state": "press", "strength": 0.5, "holdMs": 100})
            ))
            .is_ok()
        );
    }

    #[test]
    fn input_steps_are_validated_and_spelled_for_the_probe() {
        assert_eq!(
            probe_step(&map(json!({"action": "jump", "holdMs": 200})), "action").expect("action"),
            json!({"kind": "action", "action": "jump", "state": "tap", "hold_ms": 200})
        );
        assert_eq!(
            probe_step(
                &map(json!({"key": "Space", "state": "press", "modifiers": {"shift": true}})),
                "key"
            )
            .expect("key"),
            json!({"kind": "key", "key": "Space", "state": "press", "modifiers": {"shift": true}})
        );
        assert_eq!(
            probe_step(&map(json!({"position": {"x": 3, "y": 4}})), "mouse_motion")
                .expect("motion"),
            json!({"kind": "mouse_motion", "position": {"x": 3, "y": 4}})
        );
        assert_eq!(
            probe_step(&map(json!({"ms": 250})), "wait").expect("wait"),
            json!({"kind": "wait", "ms": 250})
        );
        for (step, kind) in [
            (json!({"action": "jump", "state": "hold"}), "action"),
            (json!({"action": "jump", "strength": 2.0}), "action"),
            (json!({"key": "A", "modifiers": {"hyper": true}}), "key"),
            (json!({}), "mouse_motion"),
            (json!({"button": "side"}), "mouse_button"),
            (json!({"ms": 20000}), "wait"),
            (json!({}), "teleport"),
        ] {
            assert!(
                probe_step(&map(step.clone()), kind).is_err(),
                "{kind} {step}"
            );
        }
    }

    /// The advertised schema of a managed-session tool.
    fn advertised(tool: &str) -> Value {
        Value::Object(crate::kernel::tool_schema(
            tool,
            crate::core::CoreLimits::default(),
        ))
    }

    const SESSION: &str = "00000000-0000-0000-0000-000000000000";

    /// Check `cases` of one tool's arguments against its advertised schema and
    /// the handler's own check: each case is (arguments without the session
    /// and the idempotency id, the schema's verdict, the handler's verdict).
    fn verdicts(
        tool: &str,
        cases: &[(Value, bool, bool)],
        handler: impl Fn(&Map<String, Value>) -> bool,
    ) {
        let schema = advertised(tool);
        for (case, schema_verdict, handler_verdict) in cases {
            let mut arguments = map(case.clone());
            arguments.insert("session".to_owned(), json!(SESSION));
            arguments.insert("idempotencyId".to_owned(), json!("call-1"));
            let arguments = Value::Object(arguments);
            assert_eq!(
                crate::schema::conformance::conforms(&schema, &arguments),
                *schema_verdict,
                "the {tool} schema's verdict on {arguments}"
            );
            // The kernel takes the idempotency id out before a handler runs.
            let mut handed = map(arguments.clone());
            handed.remove("idempotencyId");
            assert_eq!(
                handler(&handed),
                *handler_verdict,
                "the {tool} handler's verdict on {arguments}"
            );
        }
    }

    /// The mouse schema and the mouse check give the same verdict on every
    /// case, and it is the intended one: one `event`, keyed by its kind: a
    /// `move` with exactly its position, or a `button` press that may name its
    /// button and position.
    #[test]
    fn the_mouse_schema_and_the_mouse_check_agree() {
        let at = json!({ "x": 1, "y": 2.5 });
        let event = |event: Value| json!({ "event": event });
        verdicts(
            "input.mouse.send",
            &[
                (event(json!({ "move": { "position": at } })), true, true),
                (event(json!({ "button": {} })), true, true),
                (
                    event(
                        json!({ "button": { "button": "right", "position": at, "state": "press", "holdMs": 100 } }),
                    ),
                    true,
                    true,
                ),
                (event(json!({ "move": {} })), false, false),
                (
                    event(json!({ "move": { "position": at, "button": "left" } })),
                    false,
                    false,
                ),
                (
                    event(json!({ "move": { "position": { "x": 1 } } })),
                    false,
                    false,
                ),
                (
                    event(json!({ "move": { "position": { "x": 1, "y": 2, "z": 3 } } })),
                    false,
                    false,
                ),
                (
                    event(json!({ "move": { "position": { "x": "1", "y": 2 } } })),
                    false,
                    false,
                ),
                (
                    event(json!({ "button": { "button": "thumb" } })),
                    false,
                    false,
                ),
                (
                    event(json!({ "button": { "state": "hold" } })),
                    false,
                    false,
                ),
                (event(json!({ "button": { "holdMs": 0 } })), false, false),
                (event(json!({ "drag": { "position": at } })), false, false),
                (
                    event(json!({ "move": { "position": at }, "button": {} })),
                    false,
                    false,
                ),
                (event(json!({})), false, false),
                (event(json!({ "move": at, "kind": "move" })), false, false),
                // The key names the kind; a `kind` inside the event is a field
                // neither form declares, whatever its value.
                (
                    event(json!({ "move": { "position": at, "kind": "mouse_motion" } })),
                    false,
                    false,
                ),
                (
                    event(json!({ "button": { "kind": "mouse_motion" } })),
                    false,
                    false,
                ),
                (event(json!("move")), false, false),
                (json!({}), false, false),
            ],
            |arguments| mouse_step(arguments).is_ok(),
        );
    }

    /// The debug command schema and the command check give the same verdict
    /// on every case, and it is the intended one: one `command`, keyed by its
    /// name, which takes exactly its own fields, the ones it needs included.
    #[test]
    fn the_debug_command_schema_and_the_command_check_agree() {
        let command = |name: &str, fields: Value| json!({ "command": { name: fields } });
        verdicts(
            "debug.command",
            &[
                (
                    command("diagnostics", json!({ "path": "res://a.gd" })),
                    true,
                    true,
                ),
                (
                    command("symbols", json!({ "path": "scripts/a.gd" })),
                    true,
                    true,
                ),
                (
                    command(
                        "breakpoints",
                        json!({ "path": "res://a.gd", "lines": [1, 20] }),
                    ),
                    true,
                    true,
                ),
                (
                    command("breakpoints", json!({ "path": "res://a.gd", "lines": [] })),
                    true,
                    true,
                ),
                (command("stopped", json!({})), true, true),
                (command("stopped", json!({ "timeoutMs": 500 })), true, true),
                (command("stack", json!({ "thread": 1 })), true, true),
                (command("continue", json!({})), true, true),
                (command("pause", json!({})), true, true),
                (command("scopes", json!({ "frame": 0 })), true, true),
                (command("scopes", json!({})), true, true),
                (command("variables", json!({ "reference": 3 })), true, true),
                (command("launch", json!({})), true, true),
                (command("stop", json!({})), true, true),
                (command("diagnostics", json!({})), false, false),
                (
                    command("breakpoints", json!({ "path": "res://a.gd" })),
                    false,
                    false,
                ),
                (
                    command("breakpoints", json!({ "lines": [1] })),
                    false,
                    false,
                ),
                (
                    command("stopped", json!({ "path": "res://a.gd" })),
                    false,
                    false,
                ),
                (command("variables", json!({})), false, false),
                (command("launch", json!({ "thread": 1 })), false, false),
                (
                    command("diagnostics", json!({ "path": "a.txt" })),
                    false,
                    false,
                ),
                (
                    command("diagnostics", json!({ "path": "../a.gd" })),
                    false,
                    false,
                ),
                (
                    command("diagnostics", json!({ "path": "a//b.gd" })),
                    false,
                    false,
                ),
                (
                    command("diagnostics", json!({ "path": "a\\b.gd" })),
                    false,
                    false,
                ),
                (
                    command("diagnostics", json!({ "path": "res://" })),
                    false,
                    false,
                ),
                (
                    command("breakpoints", json!({ "path": "res://a.gd", "lines": [0] })),
                    false,
                    false,
                ),
                (command("stopped", json!({ "timeoutMs": 0 })), false, false),
                (command("pause", json!({ "thread": 0 })), false, false),
                (command("scopes", json!({ "frame": -1 })), false, false),
                (command("explode", json!({})), false, false),
                (command("launch", json!(true)), false, false),
                (
                    json!({ "command": { "launch": {}, "stop": {} } }),
                    false,
                    false,
                ),
                (json!({ "command": {} }), false, false),
                (json!({ "command": "launch" }), false, false),
                (json!({}), false, false),
            ],
            |arguments| debug_command_of(arguments).is_ok(),
        );
    }

    /// A session tool's schema requires an idempotency id exactly when the
    /// tool is a mutation, which is when its handler refuses a call without
    /// one.
    #[test]
    fn exactly_the_session_mutations_require_an_idempotency_id() {
        for &operation in SessionOperation::ALL {
            let schema = advertised(operation.tool_name());
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

    /// The command table names exactly the commands the debug manager runs.
    #[test]
    fn the_debug_command_table_is_every_command() {
        use super::super::debug::{DEBUG_COMMANDS, LANGUAGE_COMMANDS};

        let mut tabled: Vec<&str> = DEBUG_COMMAND_FIELDS
            .iter()
            .flat_map(|(commands, _, _)| commands.iter().copied())
            .collect();
        tabled.sort_unstable();
        let mut known: Vec<&str> = LANGUAGE_COMMANDS
            .into_iter()
            .chain(DEBUG_COMMANDS)
            .collect();
        known.sort_unstable();
        assert_eq!(tabled, known);
    }

    #[test]
    fn every_operation_has_a_strict_schema() {
        for &operation in SessionOperation::ALL {
            let schema = input_schema(operation);
            assert_eq!(
                schema["additionalProperties"],
                json!(false),
                "{}",
                operation.tool_name()
            );
        }
        assert!(res_scene("levels/one.tscn").is_ok());
        assert!(res_scene("../escape.tscn").is_err());
        assert!(res_scene("script.gd").is_err());
    }
}
