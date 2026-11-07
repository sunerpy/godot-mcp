//! The live-editor tools.
//!
//! Reads (`editor.status.get`, `editor.scene.tree.get`, `editor.types.get`)
//! ask the connected plugin directly; they change nothing in the editor.
//! Mutations (`editor.scene.open`, `editor.selection.set`, `editor.undo`,
//! `editor.redo`, `editor.addon.reload`) are journaled `editor_runtime`
//! transactions routed through the outcome router: idempotent replay, the
//! write block, effect-lane admission, a policy permit re-checked at the editor
//! boundary, and a terminal record derived from the plugin's three-state
//! answer. An answer that never came back is reported as unresolved and never
//! sent again.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use godot_mcp_bridge::protocol::envelope::OperationClass;
use godot_mcp_bridge::protocol::mutation::MutationStatus;
use godot_mcp_domain::ErrorKind;
pub(crate) use godot_mcp_domain::catalog::EditorOperation;
use godot_mcp_domain::config::ProcessTrust;
use godot_mcp_domain::contracts::{GodotValue, OutcomeError};
use godot_mcp_domain::editor_preferences::{
    DEFAULT_PAGE, MAX_CHANGES, MAX_NAME_CHARS, MAX_NAMES, MAX_PAGE,
};
use godot_mcp_domain::editor_shortcuts;
use godot_mcp_domain::manifests::canonical_manifest;
use godot_mcp_domain::safe_root::SafeRoot;
use serde_json::{Map, Value, json};

use super::bridge::{BridgeAnswer, BridgeFailure, BridgeStatus, EditorBridge};
use crate::effect::WritePrecondition;
use crate::effect::boundary::EditorMutation;
use crate::effect::gateway::EditorEffectIntent;
use crate::engine_plane::session::SessionManager;
use crate::idempotency::TransportAuthMode;
use crate::idempotency::replay::replay_outcome;
use crate::journal::{OperationFamily, TransportMode};
use crate::kernel::{AccessContext, KernelToolCall, KernelToolHandler, ServiceKernel};
use crate::router::{
    EnginePresence, OutcomeRouter, PlaneObservation, ProtocolFidelity, RouteEffect, RouteReport,
    RouteRequest, RouterIdentity, RuntimePresence, WorkspaceFreshness,
};
use crate::workspace::WorkspaceCoordinator;

/// How long an editor read or mutation may take.
const EDITOR_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest node path a node read or a selection takes, in characters:
/// Unicode scalar values, as a schema's `maxLength` counts them.
const MAX_NODE_PATH_CHARS: usize = 1024;
/// Most nodes one selection names.
const MAX_SELECTION: usize = 256;
/// Most properties one node read names.
const MAX_PROPERTIES: usize = 256;
/// Most classes one search returns.
const MAX_CLASSES: u64 = 500;
/// Most log entries one read returns.
const MAX_LOG_ENTRIES: u64 = 500;
/// The main screens every supported editor build names alike.
const MAIN_SCREENS: [&str; 3] = ["2D", "3D", "Script"];
/// What a screenshot shows: the editor's whole window, or a main-screen viewport.
const CAPTURE_VIEWS: [&str; 3] = ["window", "2d", "3d"];
/// The widest screenshot, in pixels.
const MAX_CAPTURE_WIDTH: u64 = 4096;
/// Most assets one scan reimports.
const MAX_REIMPORTS: usize = 256;
/// How long a scan may take, in milliseconds: the least, the default and the most.
const SCAN_TIMEOUT_MS: (u64, u64, u64) = (1_000, 300_000, 600_000);
/// The highest line or column a script can be opened at.
const MAX_SCRIPT_POSITION: u64 = 1_000_000;
/// The kinds of entry the editor log holds.
const LOG_KINDS: [&str; 5] = ["message", "error", "warning", "script", "shader"];
/// Most operations one live edit applies.
const MAX_EDIT_OPERATIONS: usize = 128;
/// Longest undo-action label of a live edit, in characters.
const MAX_LABEL_CHARS: usize = 96;
/// The editor views a camera read selects.
const CAMERA_VIEWS: [&str; 2] = ["2d", "3d"];
/// The farthest a camera's position or target lies from the origin.
const MAX_COORDINATE: f64 = 1.0e9;
/// How near a camera's target may lie to its position.
const MIN_LOOK_DISTANCE: f64 = 1.0e-4;
/// Most dialogs one read lists, and how many it lists by default.
const MAX_DIALOGS: u64 = 64;
const DEFAULT_DIALOGS: u64 = 32;
/// A dialog's `id`: the editor's object ID of the dialog, never 0.
const DIALOG_ID_PATTERN: &str = "^[1-9][0-9]{0,19}$";
/// A dialog button's name: no control character in it.
const BUTTON_PATTERN: &str = r"^[^\x00-\x1F\x7F-\x9F]+$";
/// Longest dialog button name, in characters.
const MAX_BUTTON_CHARS: usize = 256;
/// Most monitors one performance read names.
const MAX_MONITORS: usize = 256;
/// How long a restarted editor may take to connect, in milliseconds: the
/// least, the default and the most.
const RESTART_TIMEOUT_MS: (u64, u64, u64) = (1_000, 120_000, 600_000);
/// How often a restart is checked for while it is awaited.
const RESTART_POLL: Duration = Duration::from_millis(250);
/// How long one check of a restart may wait for the editor that was asked.
const RESTART_CHECK: Duration = Duration::from_secs(2);
/// How long a preview may take, in milliseconds: the least, the default and
/// the most.
const PREVIEW_TIMEOUT_MS: (u64, u64, u64) = (100, 10_000, 60_000);
/// How much longer than the preview itself its request may take: the images
/// are read back after it.
const PREVIEW_MARGIN: Duration = Duration::from_secs(10);

fn outcome_error(kind: ErrorKind, message: impl Into<String>) -> OutcomeError {
    OutcomeError::new(kind, message, false, BTreeMap::new())
}

/// The refusal an editor tool answers while no editor is connected.
fn not_connected() -> OutcomeError {
    let mut details = BTreeMap::new();
    details.insert(
        crate::scheduler::PLANE_DETAIL_KEY.to_owned(),
        GodotValue::String("editor".to_owned()),
    );
    OutcomeError::new(
        ErrorKind::Unavailable,
        "no Godot editor running the godot_mcp plugin is connected; install it with \
         editor.addon.install, enable it, and open the project in the editor",
        true,
        details,
    )
}

/// The refusal for a bridge method the connected plugin does not implement:
/// it predates the method, so the refusal names its version and the update.
pub(crate) fn plugin_lacks(status: &BridgeStatus, method: &str) -> OutcomeError {
    outcome_error(
        ErrorKind::Unsupported,
        format!(
            "the connected godot_mcp plugin ({}) does not implement {method}; update it with \
             editor.addon.update, then editor.addon.reload, and retry. Nothing was changed.",
            status
                .plugin_version
                .as_deref()
                .unwrap_or("of an unknown version")
        ),
    )
}

/// Why the connected editor does not serve `method`: a plugin whose session
/// says `flag` is false runs on a Godot that lacks what the method needs,
/// which `lacking` says; any other plugin predates the method.
fn editor_lacks(status: &BridgeStatus, method: &str, flag: &str, lacking: &str) -> OutcomeError {
    let capable = status
        .description
        .as_ref()
        .and_then(|description| description.get(flag));
    if capable == Some(&Value::Bool(false)) {
        return outcome_error(
            ErrorKind::Unsupported,
            format!(
                "the connected editor runs Godot {}, which {lacking}",
                status
                    .godot_version
                    .as_deref()
                    .unwrap_or("of an unknown version")
            ),
        );
    }
    plugin_lacks(status, method)
}

/// Why the connected editor does not serve `editor.log.read`: a plugin that
/// says it cannot capture the log runs on a Godot without the `Logger` class;
/// any other predates log capture.
fn no_log_capture(status: &BridgeStatus) -> OutcomeError {
    editor_lacks(
        status,
        "editor.log.read",
        "log_capture",
        "cannot hand its log to a plugin: log capture needs the Logger class of Godot 4.5 or \
         later. Nothing was read.",
    )
}

/// Why the connected editor does not serve `editor.scene.close`: a plugin
/// that says it cannot close scenes runs on a Godot whose `EditorInterface`
/// has no `close_scene`; any other predates closing.
fn no_scene_close(status: &BridgeStatus) -> OutcomeError {
    editor_lacks(
        status,
        "editor.scene.close",
        "scene_close",
        "cannot close a scene from a plugin: closing needs EditorInterface.close_scene of \
         Godot 4.5 or later. Nothing was changed.",
    )
}

/// Why the connected editor does not serve the shortcut tools: a plugin that
/// says it cannot reach shortcuts runs on a Godot whose `EditorSettings` names
/// none to a plugin; any other predates them. `outcome` says what the refused
/// call left as it was.
fn no_shortcuts(status: &BridgeStatus, method: &str, outcome: &str) -> OutcomeError {
    editor_lacks(
        status,
        method,
        "shortcuts",
        &format!(
            "cannot hand its shortcuts to a plugin: reading and changing them needs \
             EditorSettings.get_shortcut_list of Godot 4.6 or later. {outcome}"
        ),
    )
}

/// Why the connected editor does not serve `editor.camera.set`: a plugin
/// that says it cannot move a camera runs on a Godot whose 3D editor does
/// not keep a camera a plugin moves; any other predates the method.
fn no_camera_set(status: &BridgeStatus) -> OutcomeError {
    editor_lacks(
        status,
        "editor.camera.set",
        "camera_set",
        "does not keep a camera a plugin moves: its 3D editor puts its orbit pivot at the \
         camera and draws the view 4.0 units behind the point asked for (measured on Godot \
         4.4.1); cameras a plugin moves hold from Godot 4.5.1 on. Nothing was changed.",
    )
}

/// Why the connected editor does not serve `editor.script.close`: a plugin
/// that says it cannot close a script tab runs on a Godot whose
/// `ScriptEditor` has no `close_file`; any other predates the method.
fn no_script_close(status: &BridgeStatus) -> OutcomeError {
    editor_lacks(
        status,
        "editor.script.close",
        "script_close",
        "cannot close a script tab from a plugin: closing needs ScriptEditor.close_file of \
         Godot 4.7 or later. Nothing was changed.",
    )
}

/// A setting name or prefix, as the schemas below spell the rule: no control
/// character in it, the characters `char::is_control` names.
const SETTING_NAME_PATTERN: &str =
    r"^[^\x00-\x1F\x7F-\x9F/][^\x00-\x1F\x7F-\x9F]*/[^\x00-\x1F\x7F-\x9F]*$";
const SETTING_PREFIX_PATTERN: &str = r"^[^\x00-\x1F\x7F-\x9F]*$";

/// A setting name: a `/` after its first character, no control character and
/// at most [`MAX_NAME_CHARS`] characters.
fn setting_name(value: &Value) -> Result<String, OutcomeError> {
    let refused = || {
        outcome_error(
            ErrorKind::InvalidInput,
            format!(
                "every setting name is at most {MAX_NAME_CHARS} characters with a `/` after its \
                 first, as editor.preferences.get lists them"
            ),
        )
    };
    let name = value.as_str().ok_or_else(refused)?;
    let mut characters = name.chars();
    let first = characters.next().ok_or_else(refused)?;
    if first == '/'
        || !characters.as_str().contains('/')
        || name.chars().any(char::is_control)
        || name.chars().count() > MAX_NAME_CHARS
    {
        return Err(refused());
    }
    Ok(name.to_owned())
}

/// What `editor.preferences.get` asks the editor: the named settings, or
/// every one, under `prefix`, `limit` of them from `offset`.
fn preferences_read(arguments: &Map<String, Value>) -> Result<Value, OutcomeError> {
    let settings = match arguments.get("settings") {
        None => Value::Null,
        Some(Value::Array(names)) if (1..=MAX_NAMES).contains(&names.len()) => Value::Array(
            names
                .iter()
                .map(|name| setting_name(name).map(Value::String))
                .collect::<Result<_, _>>()?,
        ),
        Some(_) => {
            return Err(outcome_error(
                ErrorKind::InvalidInput,
                format!("`settings` names 1 to {MAX_NAMES} settings"),
            ));
        }
    };
    let prefix = match arguments.get("prefix") {
        None => String::new(),
        Some(Value::String(prefix))
            if prefix.chars().count() <= MAX_NAME_CHARS
                && !prefix.chars().any(char::is_control) =>
        {
            prefix.clone()
        }
        Some(_) => {
            return Err(outcome_error(
                ErrorKind::InvalidInput,
                format!(
                    "`prefix` is at most {MAX_NAME_CHARS} characters without a control character"
                ),
            ));
        }
    };
    let offset = match arguments.get("offset") {
        None => 0,
        Some(value) => value
            .as_u64()
            .ok_or_else(|| outcome_error(ErrorKind::InvalidInput, "`offset` is 0 or more"))?,
    };
    let limit = match arguments.get("limit") {
        None => DEFAULT_PAGE,
        Some(value) => value
            .as_u64()
            .filter(|limit| (1..=MAX_PAGE).contains(limit))
            .ok_or_else(|| {
                outcome_error(
                    ErrorKind::InvalidInput,
                    format!("`limit` is from 1 to {MAX_PAGE}"),
                )
            })?,
    };
    Ok(json!({ "settings": settings, "prefix": prefix, "offset": offset, "limit": limit }))
}

/// A tagged value an editor setting can hold: a writable contract value that
/// refers to no resource and no object, at any depth.
fn plain_value(value: &Value, name: &str) -> Result<(), OutcomeError> {
    fn refers(value: &GodotValue) -> bool {
        match value {
            GodotValue::Resource(_) | GodotValue::Object(_) => true,
            GodotValue::Array(items) => items.iter().any(refers),
            GodotValue::Dictionary(entries) => entries.values().any(refers),
            _ => false,
        }
    }
    crate::engine_plane::authored::tagged_value(value, name)?;
    let parsed: GodotValue = serde_json::from_value(value.clone()).map_err(|error| {
        outcome_error(
            ErrorKind::InvalidInput,
            format!("{name}: not a tagged value: {error}"),
        )
    })?;
    if refers(&parsed) {
        return Err(outcome_error(
            ErrorKind::InvalidInput,
            format!("{name}: an editor setting holds a plain value, never a resource"),
        ));
    }
    Ok(())
}

/// What `editor.preferences.set` asks the editor: every setting it changes,
/// each with a plain tagged value. A setting refused by its name is refused
/// here, before the editor is asked; the editor refuses the rest it may not
/// change by their hints.
fn preferences_change(arguments: &Map<String, Value>) -> Result<Value, OutcomeError> {
    let settings = arguments
        .get("settings")
        .and_then(Value::as_object)
        .filter(|settings| (1..=MAX_CHANGES).contains(&settings.len()))
        .ok_or_else(|| {
            outcome_error(
                ErrorKind::InvalidInput,
                format!("`settings` maps 1 to {MAX_CHANGES} setting names to tagged values"),
            )
        })?;
    for (name, value) in settings {
        plain_value(value, name)?;
    }
    let refused: Vec<String> = settings
        .keys()
        .filter_map(|name| {
            godot_mcp_domain::editor_preferences::refusal(name).map(|why| format!("{name}: {why}"))
        })
        .collect();
    if !refused.is_empty() {
        return Err(outcome_error(
            ErrorKind::PermissionDenied,
            format!(
                "nothing was changed, because these settings cannot be changed from here: {}",
                refused.join("; ")
            ),
        ));
    }
    Ok(json!({ "settings": settings }))
}

/// A shortcut name or a key name, as the schemas below spell the rules: no
/// control character in either, and no `+` in a key name, which Godot reads as
/// a modifier before the key.
const SHORTCUT_NAME_PATTERN: &str = r"^[^\x00-\x1F\x7F-\x9F]+$";
const KEY_PATTERN: &str = r"^[^\x00-\x1F\x7F-\x9F+]+$";
/// The modifier flags a key event may hold.
const KEY_FLAGS: [&str; 5] = ["shift", "ctrl", "alt", "meta", "command_or_control"];
/// How a key event matches its key: by the key the layout maps, by its
/// position, or by the character the layout prints on it.
const KEY_MATCHES: [&str; 3] = ["keycode", "physical", "label"];
/// Longest event `text` accepted back from an answer, in characters.
const MAX_EVENT_TEXT_CHARS: usize = 256;

/// A shortcut name: 1 to [`editor_shortcuts::MAX_NAME_CHARS`] characters
/// without a control character.
fn shortcut_name(name: &str) -> Result<String, OutcomeError> {
    if name.is_empty()
        || name.chars().any(char::is_control)
        || name.chars().count() > editor_shortcuts::MAX_NAME_CHARS
    {
        return Err(outcome_error(
            ErrorKind::InvalidInput,
            format!(
                "every shortcut name is 1 to {} characters without a control character, as \
                 editor.shortcuts.get lists them",
                editor_shortcuts::MAX_NAME_CHARS
            ),
        ));
    }
    Ok(name.to_owned())
}

/// What `editor.shortcuts.get` asks the editor: the named shortcuts, or every
/// one, under `prefix`, only the changed ones with `changedOnly`, `limit` of
/// them from `offset`.
fn shortcuts_read(arguments: &Map<String, Value>) -> Result<Value, OutcomeError> {
    let shortcuts = match arguments.get("shortcuts") {
        None => Value::Null,
        Some(Value::Array(names)) if (1..=editor_shortcuts::MAX_NAMES).contains(&names.len()) => {
            Value::Array(
                names
                    .iter()
                    .map(|name| {
                        name.as_str()
                            .ok_or_else(|| {
                                outcome_error(
                                    ErrorKind::InvalidInput,
                                    "every `shortcuts` entry is a shortcut name",
                                )
                            })
                            .and_then(shortcut_name)
                            .map(Value::String)
                    })
                    .collect::<Result<_, _>>()?,
            )
        }
        Some(_) => {
            return Err(outcome_error(
                ErrorKind::InvalidInput,
                format!(
                    "`shortcuts` names 1 to {} shortcuts",
                    editor_shortcuts::MAX_NAMES
                ),
            ));
        }
    };
    let prefix = match arguments.get("prefix") {
        None => String::new(),
        Some(Value::String(prefix))
            if prefix.chars().count() <= editor_shortcuts::MAX_NAME_CHARS
                && !prefix.chars().any(char::is_control) =>
        {
            prefix.clone()
        }
        Some(_) => {
            return Err(outcome_error(
                ErrorKind::InvalidInput,
                format!(
                    "`prefix` is at most {} characters without a control character",
                    editor_shortcuts::MAX_NAME_CHARS
                ),
            ));
        }
    };
    let changed_only = match arguments.get("changedOnly") {
        None => false,
        Some(Value::Bool(flag)) => *flag,
        Some(_) => {
            return Err(outcome_error(
                ErrorKind::InvalidInput,
                "`changedOnly` is a boolean",
            ));
        }
    };
    let offset = match arguments.get("offset") {
        None => 0,
        Some(value) => value
            .as_u64()
            .ok_or_else(|| outcome_error(ErrorKind::InvalidInput, "`offset` is 0 or more"))?,
    };
    let limit = match arguments.get("limit") {
        None => editor_shortcuts::DEFAULT_PAGE,
        Some(value) => value
            .as_u64()
            .filter(|limit| (1..=editor_shortcuts::MAX_PAGE).contains(limit))
            .ok_or_else(|| {
                outcome_error(
                    ErrorKind::InvalidInput,
                    format!("`limit` is from 1 to {}", editor_shortcuts::MAX_PAGE),
                )
            })?,
    };
    Ok(json!({
        "shortcuts": shortcuts,
        "prefix": prefix,
        "changed_only": changed_only,
        "offset": offset,
        "limit": limit,
    }))
}

/// One key event a shortcut is given: a key name, how it is matched and
/// modifier flags, as editor.shortcuts.get answers them. The `text` an answer
/// adds may be given back, and is not read.
fn key_event(event: &Value, name: &str) -> Result<(), OutcomeError> {
    let refused = |why: &str| {
        outcome_error(
            ErrorKind::InvalidInput,
            format!("{name}: every key event {why}. Nothing was changed"),
        )
    };
    let event = event
        .as_object()
        .ok_or_else(|| refused("is an object with a `key`"))?;
    let key = event
        .get("key")
        .and_then(Value::as_str)
        .ok_or_else(|| refused("names its `key`"))?;
    if key.is_empty()
        || key.chars().count() > editor_shortcuts::MAX_KEY_CHARS
        || key
            .chars()
            .any(|character| character.is_control() || character == '+')
    {
        return Err(refused(&format!(
            "names its `key` in 1 to {} characters, as Godot spells it (`S`, `F9`, `Enter`), \
             with no `+` and no modifier in it",
            editor_shortcuts::MAX_KEY_CHARS
        )));
    }
    for (field, value) in event {
        match field.as_str() {
            "key" => {}
            "text" => {
                if value
                    .as_str()
                    .is_none_or(|text| text.chars().count() > MAX_EVENT_TEXT_CHARS)
                {
                    return Err(refused(&format!(
                        "gives `text`, when it does, as the editor's text of at most \
                         {MAX_EVENT_TEXT_CHARS} characters"
                    )));
                }
            }
            "match" => {
                if !value.as_str().is_some_and(|how| KEY_MATCHES.contains(&how)) {
                    return Err(refused(&format!(
                        "matches its key by one of {}",
                        KEY_MATCHES.join(", ")
                    )));
                }
            }
            flag if KEY_FLAGS.contains(&flag) => {
                if !value.is_boolean() {
                    return Err(refused(&format!("holds `{field}` as a boolean")));
                }
            }
            _ => {
                return Err(refused(&format!(
                    "holds only `key`, `match`, `text` and the flags {}; `{field}` is not one",
                    KEY_FLAGS.join(", ")
                )));
            }
        }
    }
    let set = |field: &str| event.get(field) == Some(&Value::Bool(true));
    if set("command_or_control") && (set("ctrl") || set("meta")) {
        return Err(refused(
            "with `command_or_control` leaves `ctrl` and `meta` to the platform",
        ));
    }
    Ok(())
}

/// What `editor.shortcuts.set` asks the editor: every shortcut it changes,
/// each to a list of key events or to `default`.
fn shortcuts_change(arguments: &Map<String, Value>) -> Result<Value, OutcomeError> {
    let shortcuts = arguments
        .get("shortcuts")
        .and_then(Value::as_object)
        .filter(|shortcuts| (1..=editor_shortcuts::MAX_CHANGES).contains(&shortcuts.len()))
        .ok_or_else(|| {
            outcome_error(
                ErrorKind::InvalidInput,
                format!(
                    "`shortcuts` maps 1 to {} shortcut names to key events or `default`",
                    editor_shortcuts::MAX_CHANGES
                ),
            )
        })?;
    for (name, events) in shortcuts {
        shortcut_name(name)?;
        match events {
            Value::String(word) if word == "default" => {}
            Value::Array(events) if events.len() <= editor_shortcuts::MAX_EVENTS => {
                for event in events {
                    key_event(event, name)?;
                }
            }
            _ => {
                return Err(outcome_error(
                    ErrorKind::InvalidInput,
                    format!(
                        "{name}: a shortcut is given at most {} key events, or `default` for \
                         its default events. Nothing was changed",
                        editor_shortcuts::MAX_EVENTS
                    ),
                ));
            }
        }
    }
    Ok(json!({ "shortcuts": shortcuts }))
}

/// A node path relative to the scene root, `.` when omitted.
fn node_argument(arguments: &Map<String, Value>) -> Result<String, OutcomeError> {
    let node = match arguments.get("node") {
        None => return Ok(".".to_owned()),
        Some(Value::String(node)) => node,
        Some(_) => {
            return Err(outcome_error(
                ErrorKind::InvalidInput,
                "`node` must be a string",
            ));
        }
    };
    if node.is_empty()
        || node.chars().count() > MAX_NODE_PATH_CHARS
        || node.starts_with('/')
        || node.split('/').any(|part| part.is_empty() || part == "..")
    {
        return Err(outcome_error(
            ErrorKind::InvalidInput,
            format!("`{node}` must be a node path relative to the scene root"),
        ));
    }
    Ok(node.clone())
}

/// What `editor.node.get` asks the editor: the scene, the node, and the
/// properties to read when only some are named.
fn node_read(arguments: &Map<String, Value>) -> Result<Value, OutcomeError> {
    let scene = optional_scene(arguments)?;
    let node = node_argument(arguments)?;
    let properties = match arguments.get("properties") {
        None => Value::Null,
        Some(Value::Array(names))
            if names.len() <= MAX_PROPERTIES
                && names.iter().all(|name| {
                    name.as_str()
                        .is_some_and(|name| !name.is_empty() && name.chars().count() <= 256)
                }) =>
        {
            Value::Array(names.clone())
        }
        Some(_) => {
            return Err(outcome_error(
                ErrorKind::InvalidInput,
                format!("`properties` names at most {MAX_PROPERTIES} properties"),
            ));
        }
    };
    Ok(json!({
        "scene": scene.unwrap_or_default(),
        "node": node,
        "properties": properties,
    }))
}

/// What `editor.selection.set` asks the editor: the scene and the nodes to
/// select.
fn selection_request(arguments: &Map<String, Value>) -> Result<Value, OutcomeError> {
    let scene = optional_scene(arguments)?;
    let nodes: Vec<String> = arguments
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| outcome_error(ErrorKind::InvalidInput, "`nodes` is required"))?
        .iter()
        .map(|node| {
            node.as_str()
                .filter(|node| !node.is_empty() && node.chars().count() <= MAX_NODE_PATH_CHARS)
                .map(str::to_owned)
                .ok_or_else(|| {
                    outcome_error(
                        ErrorKind::InvalidInput,
                        "every node is a non-empty node path",
                    )
                })
        })
        .collect::<Result<_, _>>()?;
    if nodes.len() > MAX_SELECTION {
        return Err(outcome_error(
            ErrorKind::InvalidInput,
            format!("at most {MAX_SELECTION} nodes may be selected"),
        ));
    }
    Ok(json!({ "scene": scene.unwrap_or_default(), "nodes": nodes }))
}

/// What `editor.scene.edit` asks the editor: the scene, the digest it must
/// still have, the operations, and the undo action's label, which names
/// `tool` when the call names none.
fn edit_request(arguments: &Map<String, Value>, tool: &str) -> Result<Value, OutcomeError> {
    let scene = optional_scene(arguments)?;
    let expected = arguments
        .get("expectedDigest")
        .and_then(Value::as_str)
        .filter(|digest| {
            digest.len() == 64
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
        .ok_or_else(|| {
            outcome_error(
                ErrorKind::InvalidInput,
                "`expectedDigest` is the 64-digit lowercase hex digest of the scene as the \
                 editor holds it",
            )
        })?;
    let operations = arguments
        .get("operations")
        .and_then(Value::as_array)
        .filter(|operations| !operations.is_empty() && operations.len() <= MAX_EDIT_OPERATIONS)
        .ok_or_else(|| {
            outcome_error(
                ErrorKind::InvalidInput,
                format!("`operations` holds between 1 and {MAX_EDIT_OPERATIONS} operations"),
            )
        })?;
    for (index, operation) in operations.iter().enumerate() {
        crate::engine_plane::authored::validate_live_operation(operation, index)?;
    }
    let label = match arguments.get("label") {
        None => tool.to_owned(),
        Some(Value::String(label))
            if !label.is_empty()
                && label.chars().count() <= MAX_LABEL_CHARS
                && !label.chars().any(char::is_control) =>
        {
            label.clone()
        }
        Some(_) => {
            return Err(outcome_error(
                ErrorKind::InvalidInput,
                format!("`label` is 1 to {MAX_LABEL_CHARS} characters without control characters"),
            ));
        }
    };
    Ok(json!({
        "scene": scene.unwrap_or_default(),
        "expected_digest": expected,
        "operations": operations,
        "label": format!("godot-mcp: {label}"),
    }))
}

/// A shortcut name, as the shortcut schemas take one.
fn shortcut_name_schema() -> Value {
    json!({
        "type": "string",
        "minLength": 1,
        "maxLength": editor_shortcuts::MAX_NAME_CHARS,
        "pattern": SHORTCUT_NAME_PATTERN
    })
}

/// One key event a shortcut is given, as editor.shortcuts.get answers one:
/// either shape of a closed object, the one whose `command_or_control` leaves
/// `ctrl` and `meta` to the platform, or the one without it.
fn key_event_schema() -> Value {
    let flag = |description: &str| json!({ "type": "boolean", "description": description });
    let shape = |command_or_control: Value, ctrl: Value, meta: Value| {
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["key"],
            "properties": {
                "key": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": editor_shortcuts::MAX_KEY_CHARS,
                    "pattern": KEY_PATTERN,
                    "description": "The key, as Godot names it: `S`, `F9`, `Enter`, `Space`, \
                                    `Kp Add`. Any case; no modifier in it."
                },
                "match": {
                    "type": "string",
                    "enum": KEY_MATCHES,
                    "description": "`keycode`, the default, matches the key the user's layout \
                                    maps; `physical` the key at this position on a US QWERTY \
                                    keyboard, whatever the layout prints on it; `label` the key \
                                    the layout prints this on."
                },
                "shift": flag("Shift held (default false)."),
                "alt": flag("Alt, or Option on macOS, held (default false)."),
                "text": {
                    "type": "string",
                    "maxLength": MAX_EVENT_TEXT_CHARS,
                    "description": "The editor's own text for the event, as an answer gives \
                                    it, so an answer's events can be given back as they are; \
                                    it is not read."
                },
                "ctrl": ctrl,
                "meta": meta,
                "command_or_control": command_or_control
            }
        })
    };
    json!({
        "anyOf": [
            shape(
                json!({ "const": false, "description": "Without Ctrl-or-Command (the default)." }),
                flag("Ctrl held (default false)."),
                flag("Meta, the Windows or Command key, held (default false)."),
            ),
            shape(
                json!({
                    "const": true,
                    "description": "Ctrl held, or Command on macOS; `ctrl` and `meta` are then \
                                    left to the platform."
                }),
                json!({ "const": false, "description": "Left to the platform." }),
                json!({ "const": false, "description": "Left to the platform." }),
            ),
        ]
    })
}

/// An integer argument `key` within `range`, or `default` when it is absent.
fn bounded(
    arguments: &Map<String, Value>,
    key: &str,
    range: std::ops::RangeInclusive<u64>,
    default: u64,
) -> Result<u64, OutcomeError> {
    match arguments.get(key) {
        None => Ok(default),
        Some(value) => value
            .as_u64()
            .filter(|number| range.contains(number))
            .ok_or_else(|| {
                outcome_error(
                    ErrorKind::InvalidInput,
                    format!("`{key}` is from {} to {}", range.start(), range.end()),
                )
            }),
    }
}

/// `editor.camera.get`'s arguments: the view read, `2d`, `3d` or both
/// (empty), and one 3D viewport, or -1 for every one.
fn camera_read(arguments: &Map<String, Value>) -> Result<Value, OutcomeError> {
    let view = match arguments.get("view") {
        None => None,
        Some(Value::String(view)) if CAMERA_VIEWS.contains(&view.as_str()) => Some(view.as_str()),
        Some(_) => {
            return Err(outcome_error(ErrorKind::InvalidInput, "`view` is 2d or 3d"));
        }
    };
    let viewport =
        match arguments.get("viewport") {
            None => None,
            Some(value) => Some(value.as_u64().filter(|index| *index <= 3).ok_or_else(|| {
                outcome_error(ErrorKind::InvalidInput, "`viewport` is from 0 to 3")
            })?),
        };
    if view == Some("2d") && viewport.is_some() {
        return Err(outcome_error(
            ErrorKind::InvalidInput,
            "`viewport` selects a 3D viewport, not part of the 2D view",
        ));
    }
    let view = view.unwrap_or(if viewport.is_some() { "3d" } else { "" });
    Ok(json!({
        "view": view,
        "viewport": viewport.map_or(-1, |index| i64::try_from(index).unwrap_or(-1)),
    }))
}

/// A point `{x, y, z}` in global coordinates, each within ±`MAX_COORDINATE`.
fn point(value: Option<&Value>, key: &str) -> Result<[f64; 3], OutcomeError> {
    let invalid = || {
        outcome_error(
            ErrorKind::InvalidInput,
            format!("`{key}` is {{x, y, z}}, each a number within ±{MAX_COORDINATE:e}"),
        )
    };
    let object = value.and_then(Value::as_object).ok_or_else(invalid)?;
    if object.len() != 3 {
        return Err(invalid());
    }
    let mut point = [0.0; 3];
    for (slot, axis) in point.iter_mut().zip(["x", "y", "z"]) {
        *slot = object
            .get(axis)
            .and_then(Value::as_f64)
            .filter(|coordinate| coordinate.is_finite() && coordinate.abs() <= MAX_COORDINATE)
            .ok_or_else(invalid)?;
    }
    Ok(point)
}

/// `editor.camera.set`'s arguments: the 3D viewport, where its camera is put
/// and the point it looks at.
fn camera_move(arguments: &Map<String, Value>) -> Result<Value, OutcomeError> {
    let viewport = bounded(arguments, "viewport", 0..=3, 0)?;
    let position = point(arguments.get("position"), "position")?;
    let target = point(arguments.get("target"), "target")?;
    let distance = position
        .iter()
        .zip(target)
        .map(|(from, to)| (to - from).powi(2))
        .sum::<f64>()
        .sqrt();
    if distance < MIN_LOOK_DISTANCE {
        return Err(outcome_error(
            ErrorKind::InvalidInput,
            "`target` must differ from `position`: a camera cannot look at the point it is at",
        ));
    }
    let spelled = |point: [f64; 3]| json!({ "x": point[0], "y": point[1], "z": point[2] });
    Ok(json!({
        "viewport": viewport,
        "position": spelled(position),
        "target": spelled(target),
    }))
}

/// `editor.script.close`'s argument: the `res://` path of a GDScript file.
fn script_tab(arguments: &Map<String, Value>) -> Result<Value, OutcomeError> {
    let path = res_path(
        arguments
            .get("path")
            .and_then(Value::as_str)
            .filter(|path| !path.is_empty() && path.len() <= 1024)
            .ok_or_else(|| outcome_error(ErrorKind::InvalidInput, "`path` is required"))?,
    )?;
    if !path.ends_with(".gd") {
        return Err(outcome_error(
            ErrorKind::InvalidInput,
            format!("`{path}` must name a .gd script"),
        ));
    }
    Ok(json!({ "path": path }))
}

/// `editor.dialogs.get`'s argument: how many dialogs to list at most.
fn dialogs_read(arguments: &Map<String, Value>) -> Result<Value, OutcomeError> {
    Ok(json!({ "limit": bounded(arguments, "limit", 1..=MAX_DIALOGS, DEFAULT_DIALOGS)? }))
}

/// `editor.dialog.respond`'s arguments: the dialog's `id` and the button's
/// name.
fn dialog_press(arguments: &Map<String, Value>) -> Result<Value, OutcomeError> {
    let dialog = arguments
        .get("dialog")
        .and_then(Value::as_str)
        .filter(|id| {
            !id.is_empty()
                && id.len() <= 20
                && !id.starts_with('0')
                && id.bytes().all(|byte| byte.is_ascii_digit())
        })
        .ok_or_else(|| {
            outcome_error(
                ErrorKind::InvalidInput,
                "`dialog` is the `id` editor.dialogs.get answered for a dialog",
            )
        })?;
    let button = arguments
        .get("button")
        .and_then(Value::as_str)
        .filter(|button| {
            !button.is_empty()
                && button.chars().count() <= MAX_BUTTON_CHARS
                && !button.chars().any(char::is_control)
        })
        .ok_or_else(|| {
            outcome_error(
                ErrorKind::InvalidInput,
                format!(
                    "`button` is `ok`, `cancel` or a button's action, 1 to {MAX_BUTTON_CHARS} \
                     characters without control characters"
                ),
            )
        })?;
    Ok(json!({ "dialog": dialog, "button": button }))
}

/// `editor.restart`'s arguments: whether to save first, and how long the
/// restarted editor may take to connect.
fn restart_request(arguments: &Map<String, Value>) -> Result<(bool, u64), OutcomeError> {
    let save = match arguments.get("save") {
        None => false,
        Some(Value::Bool(save)) => *save,
        Some(_) => {
            return Err(outcome_error(
                ErrorKind::InvalidInput,
                "`save` must be a boolean",
            ));
        }
    };
    let timeout = bounded(
        arguments,
        "timeoutMs",
        RESTART_TIMEOUT_MS.0..=RESTART_TIMEOUT_MS.2,
        RESTART_TIMEOUT_MS.1,
    )?;
    Ok((save, timeout))
}

/// `editor.performance.get`'s argument: the monitors to read, every one when
/// absent.
fn performance_read(arguments: &Map<String, Value>) -> Result<Value, OutcomeError> {
    let monitors = match arguments.get("monitors") {
        None => Value::Null,
        Some(Value::Array(names))
            if names.len() <= MAX_MONITORS
                && names.iter().all(|name| {
                    name.as_str()
                        .is_some_and(|name| !name.is_empty() && name.chars().count() <= 256)
                }) =>
        {
            Value::Array(names.clone())
        }
        Some(_) => {
            return Err(outcome_error(
                ErrorKind::InvalidInput,
                format!("`monitors` is an array of at most {MAX_MONITORS} monitor names"),
            ));
        }
    };
    Ok(json!({ "monitors": monitors }))
}

/// `editor.resource.preview`'s arguments: the resource's `res://` path and
/// how long the editor may take, in milliseconds.
fn preview_request(arguments: &Map<String, Value>) -> Result<(String, u64), OutcomeError> {
    let path = res_path(
        arguments
            .get("path")
            .and_then(Value::as_str)
            .filter(|path| !path.is_empty() && path.len() <= 1024)
            .ok_or_else(|| outcome_error(ErrorKind::InvalidInput, "`path` is required"))?,
    )?;
    let timeout = bounded(
        arguments,
        "timeoutMs",
        PREVIEW_TIMEOUT_MS.0..=PREVIEW_TIMEOUT_MS.2,
        PREVIEW_TIMEOUT_MS.1,
    )?;
    Ok((path, timeout))
}

/// The schema of a point in global coordinates.
fn point_schema(description: &str) -> Value {
    let coordinate = json!({
        "type": "number",
        "minimum": -MAX_COORDINATE,
        "maximum": MAX_COORDINATE
    });
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": { "x": coordinate, "y": coordinate, "z": coordinate },
        "required": ["x", "y", "z"],
        "description": description
    })
}

/// What each live-editor operation asks of the bridge, beside the catalog
/// descriptor its operation serves.
trait EditorCall: Copy {
    fn bridge_method(self) -> &'static str;
    fn is_read(self) -> bool;
    fn input_schema(self) -> Value;
}

impl EditorCall for EditorOperation {
    fn bridge_method(self) -> &'static str {
        match self {
            Self::StatusGet => "editor.state",
            Self::SceneTreeGet => "editor.scene.tree",
            Self::TypesGet => "editor.types.class",
            Self::NodeGet => "editor.node",
            Self::TypesSearch => "editor.types.search",
            Self::LogGet => "editor.log.read",
            Self::PreferencesGet => "editor.preferences.read",
            Self::ShortcutsGet => "editor.shortcuts.read",
            Self::SceneEdit => "editor.scene.edit",
            Self::SceneSave => "editor.scene.save",
            Self::SceneClose => "editor.scene.close",
            Self::MainScreenSet => "editor.main_screen.set",
            Self::ScreenshotCapture => crate::editor::boundary::CAPTURE_VIEWPORT,
            Self::FilesystemScan => "editor.filesystem.scan",
            Self::ScriptOpen => "editor.script.open",
            Self::PreferencesSet => "editor.preferences.set",
            Self::ShortcutsSet => "editor.shortcuts.set",
            Self::PlayStart => "editor.play.start",
            Self::PlayStop => "editor.play.stop",
            Self::SceneOpen => "editor.scene.open",
            Self::SelectionSet => "editor.selection.set",
            Self::Undo => "editor.history.undo",
            Self::Redo => "editor.history.redo",
            Self::AddonReload => "editor.addon.reload",
            Self::ScriptRun => super::bridge::SCRIPT_RUN_METHOD,
            Self::CameraGet => "editor.camera.read",
            Self::CameraSet => "editor.camera.set",
            Self::ScriptClose => "editor.script.close",
            Self::DialogsGet => "editor.dialogs.read",
            Self::DialogRespond => "editor.dialog.respond",
            Self::Restart => super::bridge::RESTART_METHOD,
            Self::PerformanceGet => "editor.performance.read",
            Self::ResourcePreview => crate::editor::boundary::PREVIEW_RESOURCE,
            Self::SceneRead => super::scene_read::BRIDGE_METHOD,
        }
    }

    fn is_read(self) -> bool {
        matches!(
            self,
            Self::StatusGet
                | Self::SceneTreeGet
                | Self::TypesGet
                | Self::NodeGet
                | Self::TypesSearch
                | Self::LogGet
                | Self::PreferencesGet
                | Self::ShortcutsGet
                | Self::CameraGet
                | Self::DialogsGet
                | Self::PerformanceGet
                | Self::SceneRead
        )
    }

    fn input_schema(self) -> Value {
        let scene = json!({
            "type": "string",
            "minLength": 1,
            "maxLength": 1024,
            "description": "An open text scene, project-relative or res://; the scene being \
                            edited when omitted."
        });
        let (properties, required): (Value, &[&str]) = match self {
            Self::StatusGet | Self::AddonReload | Self::PlayStop => (json!({}), &[]),
            Self::ScriptRun => (
                Value::Object(crate::live_script::properties(
                    crate::live_script::Place::Editor,
                )),
                &["source"],
            ),
            Self::PlayStart => (
                json!({
                    "scene": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 1024,
                        "description": "`main`, the default, runs the project's main scene; \
                                        `current` the scene being edited; anything else names \
                                        a text scene, project-relative or res://."
                    }
                }),
                &[],
            ),
            Self::SceneTreeGet | Self::SceneClose => (json!({ "scene": scene }), &[]),
            Self::SceneRead => (super::scene_read::properties(scene), &["calls"]),
            Self::PreferencesGet => (
                json!({
                    "settings": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": MAX_NAMES,
                        "items": {
                            "type": "string",
                            "maxLength": MAX_NAME_CHARS,
                            "pattern": SETTING_NAME_PATTERN
                        },
                        "description": "Only these settings, by name, in this order; every \
                                        setting when omitted. A name the editor does not have \
                                        is listed in `unknown`."
                    },
                    "prefix": {
                        "type": "string",
                        "maxLength": MAX_NAME_CHARS,
                        "pattern": SETTING_PREFIX_PATTERN,
                        "description": "Only settings whose names start with this, e.g. \
                                        `text_editor/`; every one when omitted or empty."
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Skip this many matching settings: the `next_offset` of \
                                        the previous page (default 0)."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_PAGE,
                        "description": "Most settings returned (default 100)."
                    }
                }),
                &[],
            ),
            Self::PreferencesSet => (
                json!({
                    "settings": {
                        "type": "object",
                        "minProperties": 1,
                        "maxProperties": MAX_CHANGES,
                        "additionalProperties": crate::schema::plain_value(),
                        "description": "Setting names, as editor.preferences.get lists them, \
                                        mapped to tagged values of the settings' types. A \
                                        setting that names a program, a path, a host or a \
                                        credential is refused, and with it the whole change."
                    }
                }),
                &["settings"],
            ),
            Self::ShortcutsGet => (
                json!({
                    "shortcuts": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": editor_shortcuts::MAX_NAMES,
                        "items": shortcut_name_schema(),
                        "description": "Only these shortcuts and built-in actions, by name, in \
                                        this order; every one when omitted. A name the editor \
                                        does not have is listed in `unknown`."
                    },
                    "prefix": {
                        "type": "string",
                        "maxLength": editor_shortcuts::MAX_NAME_CHARS,
                        "pattern": SETTING_PREFIX_PATTERN,
                        "description": "Only shortcuts whose names start with this, e.g. \
                                        `script_editor/` or `ui_`; every one when omitted or \
                                        empty."
                    },
                    "changedOnly": {
                        "type": "boolean",
                        "description": "Only shortcuts whose key events differ from their \
                                        defaults (default false)."
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Skip this many matching shortcuts: the `next_offset` \
                                        of the previous page (default 0)."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": editor_shortcuts::MAX_PAGE,
                        "description": "Most shortcuts returned (default 100)."
                    }
                }),
                &[],
            ),
            Self::ShortcutsSet => (
                json!({
                    "shortcuts": {
                        "type": "object",
                        "minProperties": 1,
                        "maxProperties": editor_shortcuts::MAX_CHANGES,
                        "propertyNames": shortcut_name_schema(),
                        "additionalProperties": {
                            "oneOf": [
                                {
                                    "type": "array",
                                    "maxItems": editor_shortcuts::MAX_EVENTS,
                                    "items": key_event_schema()
                                },
                                { "const": "default" }
                            ]
                        },
                        "description": "Shortcut and built-in action names, as \
                                        editor.shortcuts.get lists them, mapped to the key \
                                        events that trigger them, in order: an empty list for \
                                        none, or `default` for their default events."
                    }
                }),
                &["shortcuts"],
            ),
            Self::TypesGet => (
                json!({
                    "class": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 256,
                        "description": "Engine class name, e.g. CharacterBody2D."
                    }
                }),
                &["class"],
            ),
            Self::NodeGet => (
                json!({
                    "scene": scene,
                    "node": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": MAX_NODE_PATH_CHARS,
                        "pattern": format!("^{}$", crate::schema::relative_node_path("")),
                        "description": "Node path relative to the scene root (`.` is the root, the \
                                        default); a node inside an instanced scene may be read."
                    },
                    "properties": {
                        "type": "array",
                        "maxItems": MAX_PROPERTIES,
                        "items": { "type": "string", "minLength": 1, "maxLength": 256 },
                        "description": "Only these properties; every property when omitted."
                    }
                }),
                &[],
            ),
            Self::TypesSearch => (
                json!({
                    "query": {
                        "type": "string",
                        "maxLength": 128,
                        "description": "Text the class name contains, in any case; every class \
                                        when omitted or empty."
                    },
                    "base": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 256,
                        "description": "Only classes inheriting this engine or script class."
                    },
                    "instantiable": {
                        "type": "boolean",
                        "description": "Only classes that can (true) or cannot (false) be \
                                        instantiated."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_CLASSES,
                        "description": "Most classes returned (default 100)."
                    }
                }),
                &[],
            ),
            Self::LogGet => (
                json!({
                    "after": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Return the entries after this sequence number, the \
                                        `next` of the previous read (default 0, every entry kept)."
                    },
                    "epoch": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 64,
                        "description": "The `epoch` of the previous read. When the plugin has \
                                        restarted since, the read starts from the oldest entry \
                                        kept and says `reset`."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_LOG_ENTRIES,
                        "description": "Most entries returned (default 100)."
                    },
                    "kinds": {
                        "type": "array",
                        "minItems": 1,
                        "uniqueItems": true,
                        "items": { "type": "string", "enum": LOG_KINDS },
                        "description": "Only entries of these kinds; every kind when omitted."
                    }
                }),
                &[],
            ),
            Self::FilesystemScan => (
                json!({
                    "reimport": {
                        "type": "array",
                        "maxItems": MAX_REIMPORTS,
                        "items": { "type": "string", "minLength": 1, "maxLength": 1024 },
                        "description": "Assets to reimport after the rescan, project-relative \
                                        or res://."
                    },
                    "timeoutMs": {
                        "type": "integer",
                        "minimum": SCAN_TIMEOUT_MS.0,
                        "maximum": SCAN_TIMEOUT_MS.2,
                        "description": "How long the scan, imports and reloads may take, in \
                                        milliseconds (default 300000)."
                    }
                }),
                &[],
            ),
            Self::ScriptOpen => (
                json!({
                    "path": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 1024,
                        "description": "Project-relative or res:// path of a .gd script."
                    },
                    "line": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_SCRIPT_POSITION,
                        "description": "Line to put the caret on, from 1 (default 1)."
                    },
                    "column": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_SCRIPT_POSITION,
                        "description": "Column to put the caret at, from 1 (default 1)."
                    }
                }),
                &["path"],
            ),
            Self::MainScreenSet => (
                json!({
                    "screen": {
                        "type": "string",
                        "enum": MAIN_SCREENS,
                        "description": "The main screen to show."
                    }
                }),
                &["screen"],
            ),
            Self::ScreenshotCapture => (
                json!({
                    "view": {
                        "type": "string",
                        "enum": CAPTURE_VIEWS,
                        "description": "The editor's whole window (the default), or the 2D or a \
                                        3D viewport, which must be on the main screen shown."
                    },
                    "viewport": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 3,
                        "description": "Which of the 3D main screen's four viewports (default 0)."
                    },
                    "maxWidth": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_CAPTURE_WIDTH,
                        "description": "Scale a wider capture down to this many pixels."
                    }
                }),
                &[],
            ),
            Self::SceneOpen => (
                json!({
                    "path": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 1024,
                        "description": "Project-relative or res:// path of a text scene."
                    },
                    "reload": {
                        "type": "boolean",
                        "description": "Reload an already open scene from its file; refused \
                                        while the editor holds changes the file does not."
                    }
                }),
                &["path"],
            ),
            Self::SelectionSet => (
                json!({
                    "scene": scene,
                    "nodes": {
                        "type": "array",
                        "maxItems": MAX_SELECTION,
                        "items": { "type": "string", "minLength": 1, "maxLength": MAX_NODE_PATH_CHARS },
                        "description": "Node paths relative to the scene root (`.` is the root); \
                                        empty clears the selection."
                    }
                }),
                &["nodes"],
            ),
            Self::Undo | Self::Redo => (
                json!({
                    "scene": scene,
                    "save": {
                        "type": "boolean",
                        "description": "Also save the scene afterwards (default false)."
                    }
                }),
                &[],
            ),
            Self::SceneEdit => (
                json!({
                    "scene": scene,
                    "expectedDigest": {
                        "type": "string",
                        "pattern": "^[0-9a-f]{64}$",
                        "description": "The semantic digest editor.scene.tree.get or \
                                        editor.node.get returned for the scene as the editor \
                                        holds it; any other digest refuses the edit."
                    },
                    "operations": crate::engine_plane::authored::live_operations_schema(),
                    "label": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": MAX_LABEL_CHARS,
                        "pattern": crate::schema::NO_CONTROL_PATTERN,
                        "description": "Names the undo action, after `godot-mcp: `."
                    }
                }),
                &["expectedDigest", "operations"],
            ),
            Self::SceneSave => (
                json!({
                    "scene": scene,
                    "all": {
                        "type": "boolean",
                        "description": "Save every open scene; not with `scene`."
                    }
                }),
                &[],
            ),
            Self::CameraGet => (
                json!({
                    "view": {
                        "type": "string",
                        "enum": CAMERA_VIEWS,
                        "description": "Only the 2D editor's view, or only the 3D viewports; \
                                        both when omitted."
                    },
                    "viewport": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 3,
                        "description": "Only this one of the 3D main screen's four viewports; \
                                        not with `view` 2d."
                    }
                }),
                &[],
            ),
            Self::CameraSet => (
                json!({
                    "viewport": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 3,
                        "description": "Which of the 3D main screen's four viewports (default 0)."
                    },
                    "position": point_schema("Where the camera is put, in global coordinates."),
                    "target": point_schema(
                        "The point the camera looks at, in global coordinates; it must differ \
                         from `position`. The top of the view faces +Y, or -Z when the camera \
                         looks straight up or down."
                    )
                }),
                &["position", "target"],
            ),
            Self::ScriptClose => (
                json!({
                    "path": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 1024,
                        "description": "Project-relative or res:// path of the .gd script whose \
                                        tab is closed."
                    }
                }),
                &["path"],
            ),
            Self::DialogsGet => (
                json!({
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_DIALOGS,
                        "description": "Most dialogs listed (default 32)."
                    }
                }),
                &[],
            ),
            Self::DialogRespond => (
                json!({
                    "dialog": {
                        "type": "string",
                        "pattern": DIALOG_ID_PATTERN,
                        "description": "The `id` editor.dialogs.get answered for the dialog."
                    },
                    "button": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": MAX_BUTTON_CHARS,
                        "pattern": BUTTON_PATTERN,
                        "description": "`ok`, `cancel`, or the action of another button, as \
                                        editor.dialogs.get names the dialog's buttons."
                    }
                }),
                &["dialog", "button"],
            ),
            Self::Restart => (
                json!({
                    "save": {
                        "type": "boolean",
                        "description": "Save every open scene, and from Godot 4.7.1 every open \
                                        script, before restarting (default false); without it \
                                        an unsaved scene or script refuses the restart."
                    },
                    "timeoutMs": {
                        "type": "integer",
                        "minimum": RESTART_TIMEOUT_MS.0,
                        "maximum": RESTART_TIMEOUT_MS.2,
                        "description": "How long the restarted editor may take to connect, in \
                                        milliseconds (default 120000)."
                    }
                }),
                &[],
            ),
            Self::PerformanceGet => (
                json!({
                    "monitors": {
                        "type": "array",
                        "maxItems": MAX_MONITORS,
                        "items": { "type": "string", "minLength": 1, "maxLength": 256 },
                        "description": "Only these monitors, by the names an earlier read \
                                        answered; every monitor when omitted."
                    }
                }),
                &[],
            ),
            Self::ResourcePreview => (
                json!({
                    "path": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 1024,
                        "description": "Project-relative or res:// path of the resource to \
                                        preview."
                    },
                    "timeoutMs": {
                        "type": "integer",
                        "minimum": PREVIEW_TIMEOUT_MS.0,
                        "maximum": PREVIEW_TIMEOUT_MS.2,
                        "description": "How long the editor may take to make the preview, in \
                                        milliseconds (default 10000)."
                    }
                }),
                &["path"],
            ),
        };
        let mut required: Vec<&str> = required.to_vec();
        if !self.is_read() {
            // The handler refuses a change without one before the bridge is asked.
            required.push("idempotencyId");
        }
        let mut schema = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": properties,
            "required": required,
        });
        match self {
            Self::SceneEdit => crate::schema::define_shared(
                &mut schema,
                &crate::engine_plane::authored::live_shared_fields(),
            ),
            Self::PreferencesSet | Self::SceneRead => crate::schema::define_values(&mut schema),
            _ => {}
        }
        schema
    }
}

/// The input schema of one live-editor tool.
pub(crate) fn input_schema(operation: EditorOperation) -> Map<String, Value> {
    operation
        .input_schema()
        .as_object()
        .cloned()
        .expect("editor schemas are objects")
}

/// What one live-editor mutation may write: the project files the editor may
/// save, each with the precondition it must hold, and whether it may also
/// write files nobody can list in advance.
#[derive(Debug, Default)]
pub(crate) struct EditorScope {
    files: Vec<(String, WritePrecondition)>,
    project_wide: bool,
    /// How long the editor may take, when not the default.
    timeout: Option<Duration>,
}

impl EditorScope {
    /// A change to editor state only.
    fn state() -> Self {
        Self::default()
    }

    /// A change that saves `files`.
    fn files(files: Vec<(String, WritePrecondition)>) -> Self {
        Self {
            files,
            ..Self::default()
        }
    }

    /// A change that saves `files` and may write any other project file.
    fn project_wide(files: Vec<(String, WritePrecondition)>) -> Self {
        Self {
            files,
            project_wide: true,
            ..Self::default()
        }
    }

    /// This change, given `timeout` to finish.
    const fn within(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }
}

/// The runtime behind every live-editor tool.
pub(crate) struct EditorToolRuntime {
    pub(crate) bridge: Arc<EditorBridge>,
    pub(crate) router: Arc<OutcomeRouter>,
    pub(crate) workspace: Arc<WorkspaceCoordinator>,
    pub(crate) root: SafeRoot,
    /// Whether the operator trusts the project's code: the editor runs a game
    /// outside every boundary this server holds.
    pub(crate) trust: ProcessTrust,
    /// Whether `editor.script.run` runs caller-written GDScript (ADR 0012).
    pub(crate) allow_scripts: bool,
    /// The managed sessions of a profile that runs the project: an editor
    /// one of them runs is never restarted (ADR 0013).
    pub(crate) sessions: Option<Arc<SessionManager>>,
}

/// Register every live-editor tool the kernel's profile lists.
pub(crate) fn register_editor_tools(
    kernel: &mut ServiceKernel,
    runtime: Arc<EditorToolRuntime>,
) -> Result<(), OutcomeError> {
    let manifest = canonical_manifest(kernel.profile());
    for &operation in EditorOperation::ALL {
        if manifest.tools.contains(operation.tool_name()) {
            kernel.register_tool_handler(
                operation.key(),
                Arc::new(EditorHandler {
                    operation,
                    runtime: Arc::clone(&runtime),
                }),
            )?;
        }
    }
    Ok(())
}

struct EditorHandler {
    operation: EditorOperation,
    runtime: Arc<EditorToolRuntime>,
}

fn res_path(raw: &str) -> Result<String, OutcomeError> {
    let relative = raw.strip_prefix("res://").unwrap_or(raw);
    if relative.is_empty()
        || relative.starts_with('/')
        || relative.contains('\\')
        || relative
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(outcome_error(
            ErrorKind::InvalidInput,
            format!("`{raw}` must be a project-relative path"),
        ));
    }
    Ok(format!("res://{relative}"))
}

fn scene_path(raw: &str) -> Result<String, OutcomeError> {
    let path = res_path(raw)?;
    if !path.ends_with(".tscn") {
        return Err(outcome_error(
            ErrorKind::InvalidInput,
            format!("`{raw}` must name a .tscn text scene"),
        ));
    }
    Ok(path)
}

/// What `editor.play.start` runs: `main`, `current`, or a scene's `res://`
/// path.
fn play_scene(arguments: &Map<String, Value>) -> Result<String, OutcomeError> {
    match arguments.get("scene") {
        None => Ok("main".to_owned()),
        Some(Value::String(choice)) if choice == "main" || choice == "current" => {
            Ok(choice.clone())
        }
        Some(Value::String(raw)) => scene_path(raw),
        Some(_) => Err(outcome_error(
            ErrorKind::InvalidInput,
            "`scene` must be `main`, `current` or a text scene",
        )),
    }
}

/// Why a game is never run in the editor for an untrusted project.
const UNTRUSTED_PLAY: &str = "editor.play.start runs the project's code as the editor's own \
     child process, outside every boundary this server holds, and process.trust is \
     untrusted; run the game in a managed session with project.run.start instead";

fn optional_scene(arguments: &Map<String, Value>) -> Result<Option<String>, OutcomeError> {
    match arguments.get("scene") {
        None => Ok(None),
        Some(Value::String(raw)) => scene_path(raw).map(Some),
        Some(_) => Err(outcome_error(
            ErrorKind::InvalidInput,
            "`scene` must be a string",
        )),
    }
}

impl EditorToolRuntime {
    async fn ask(&self, method: &str, payload: Value) -> Result<Value, OutcomeError> {
        let bridge = Arc::clone(&self.bridge);
        let method = method.to_owned();
        let answer = tokio::task::spawn_blocking(move || {
            bridge.call(
                &method,
                OperationClass::Read,
                &payload,
                "editor-read",
                EDITOR_TIMEOUT,
            )
        })
        .await
        .map_err(|error| outcome_error(ErrorKind::Internal, error.to_string()))?;
        match answer {
            Ok(BridgeAnswer::Result { payload, .. }) => Ok(payload),
            Ok(BridgeAnswer::Error { kind, message, .. }) => Err(outcome_error(kind, message)),
            Err(BridgeFailure::NotConnected) => Err(not_connected()),
            Err(BridgeFailure::Unsupported(method)) => {
                Err(plugin_lacks(&self.bridge.status(), &method))
            }
            Err(failure) => Err(outcome_error(ErrorKind::Unavailable, failure.message())),
        }
    }

    fn observation(&self) -> PlaneObservation {
        PlaneObservation::new(
            self.router.plane_availability(),
            EnginePresence::EditorConnected,
            RuntimePresence::NoInstance,
            ProtocolFidelity::Full,
            if self.workspace.current().coherence().is_coherent() {
                WorkspaceFreshness::Fresh
            } else {
                WorkspaceFreshness::StaleIndex
            },
        )
    }

    /// Journal and send one live-editor mutation through the outcome router.
    #[allow(clippy::too_many_arguments)]
    async fn mutate(
        &self,
        tool: &str,
        method: &str,
        payload: Value,
        scope: EditorScope,
        arguments: &Value,
        call: &KernelToolCall,
        idempotency_id: &str,
    ) -> Result<Value, OutcomeError> {
        let access = &call.access;
        let transaction_id = self
            .router
            .next_transaction_id(tool, access.observed_at_unix_ms());
        let mut payload = payload;
        if let Value::Object(object) = &mut payload {
            object.insert(
                "transaction_id".to_owned(),
                Value::String(transaction_id.clone()),
            );
        }
        // A game the editor runs is the runtime family; every other change
        // the editor makes is an editor mutation.
        let operation_family = godot_mcp_domain::catalog::descriptor(tool)
            .and_then(|descriptor| descriptor.family)
            .map_or(
                OperationFamily::EditorMutation,
                crate::policy::matrix::operation_family,
            );
        let intent = EditorEffectIntent {
            transaction_id: transaction_id.clone(),
            tool_name: tool.to_owned(),
            operation_family,
            idempotency_id: idempotency_id.to_owned(),
            transport_mode: journal_transport(access.transport_mode()),
            principal: Some(access.mode_principal().to_journal_string()),
            session: Some(access.connection_session().to_owned()),
            arguments: arguments.clone(),
            files: scope.files,
            project_wide: scope.project_wide,
            observe_all: scope.project_wide,
            expected_revision: None,
            opened_at_unix_ms: access.observed_at_unix_ms(),
        };
        let request = RouteRequest {
            tool: tool.to_owned(),
            idempotency_id: idempotency_id.to_owned(),
            transaction_id: Some(transaction_id),
            path: String::new(),
            bytes: Vec::new(),
            arguments: arguments.clone(),
            observation: self.observation(),
            cancellation: call.cancellation.clone(),
            queued_at_unix_ms: access.observed_at_unix_ms(),
            identity: Some(route_identity(access)),
            effect: Some(RouteEffect::Editor {
                intent,
                mutation: Box::new(EditorMutation {
                    action: method.to_owned(),
                    payload: payload.to_string(),
                    idempotency_id: idempotency_id.to_owned(),
                    timeout: scope.timeout.unwrap_or(EDITOR_TIMEOUT),
                    cancellation: crate::effect::boundary::CancelSignal::default(),
                }),
            }),
            audit: Some(call.audit.clone()),
        };
        let report = self
            .router
            .route(&request)
            .await
            .map_err(|refusal| refusal.to_outcome())?;
        editor_answer(&report)
    }

    /// The edited scene's path, asked from the editor.
    async fn edited_scene(&self) -> Result<String, OutcomeError> {
        let state = self.ask("editor.state", json!({})).await?;
        state
            .get("edited_scene")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| outcome_error(ErrorKind::NotFound, "no scene is being edited"))
    }

    /// Every open scene that has a file, as the editor lists them.
    async fn open_scene_files(&self) -> Result<Vec<String>, OutcomeError> {
        let state = self.ask("editor.state", json!({})).await?;
        Ok(state
            .get("open_scenes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.get("path").and_then(Value::as_str))
            .filter(|path| path.starts_with("res://"))
            .map(str::to_owned)
            .collect())
    }

    /// The `res://` scenes a save writes: `scene`, every open scene for
    /// `all`, or the edited scene. Each must be open in the editor.
    async fn scenes_to_save(
        &self,
        scene: Option<String>,
        all: bool,
    ) -> Result<Vec<String>, OutcomeError> {
        let state = self.ask("editor.state", json!({})).await?;
        let open: Vec<String> = state
            .get("open_scenes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.get("path").and_then(Value::as_str))
            .map(str::to_owned)
            .collect();
        if all {
            if open.is_empty() {
                return Err(outcome_error(ErrorKind::NotFound, "no scene is open"));
            }
            return Ok(open);
        }
        let path = match scene {
            Some(scene) => scene,
            None => state
                .get("edited_scene")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| outcome_error(ErrorKind::NotFound, "no scene is being edited"))?,
        };
        if !open.contains(&path) {
            return Err(outcome_error(
                ErrorKind::NotFound,
                format!("{path} is not open in the editor"),
            ));
        }
        Ok(vec![path])
    }

    fn disk_precondition(&self, res_path: &str) -> Result<WritePrecondition, OutcomeError> {
        let relative = res_path.strip_prefix("res://").unwrap_or(res_path);
        match self.root.read(std::path::Path::new(relative)) {
            Ok(bytes) => Ok(WritePrecondition::Sha256(
                godot_mcp_engine::supervision::journal::hex_sha256(&bytes),
            )),
            Err(_) => Ok(WritePrecondition::Absent),
        }
    }

    /// One read the editor answers within `timeout`, or `None`.
    async fn ask_within(&self, method: &str, timeout: Duration) -> Option<Value> {
        let bridge = Arc::clone(&self.bridge);
        let method = method.to_owned();
        let answer = tokio::task::spawn_blocking(move || {
            bridge.call(
                &method,
                OperationClass::Read,
                &json!({}),
                "editor-restart-check",
                timeout,
            )
        })
        .await
        .ok()?;
        match answer {
            Ok(BridgeAnswer::Result { payload, .. }) => Some(payload),
            _ => None,
        }
    }

    /// `editor.restart` (ADR 0013): refuse an editor this server runs as a
    /// managed session, journal the restart, and answer success only once
    /// the add-on of a different editor process connects.
    async fn restart(
        &self,
        tool: &str,
        arguments: &Map<String, Value>,
        canonical: &Value,
        call: &KernelToolCall,
        idempotency_id: &str,
    ) -> Result<Value, OutcomeError> {
        let (save, timeout_ms) = restart_request(arguments)?;
        let status = self.bridge.status();
        let previous_pid = status
            .description
            .as_ref()
            .and_then(|description| description.get("pid"))
            .and_then(Value::as_u64)
            .and_then(|pid| u32::try_from(pid).ok())
            .ok_or_else(|| {
                outcome_error(
                    ErrorKind::Unavailable,
                    "the connected editor did not say which process it runs in, so whether \
                     this server runs it as a managed session cannot be told; nothing was \
                     restarted",
                )
            })?;
        if let Some(session) = self
            .sessions
            .as_ref()
            .and_then(|sessions| sessions.running_process(previous_pid))
        {
            return Err(outcome_error(
                ErrorKind::Conflict,
                format!(
                    "the connected editor (process {previous_pid}) is the managed session \
                     {session}, which editor.session.start runs inside this server's process \
                     boundary; a restarted editor would be a new process outside it. Stop the \
                     session with editor.session.stop and start a new one instead. Nothing was \
                     restarted."
                ),
            ));
        }
        let state = self.ask("editor.state", json!({})).await?;
        let declined_before = state["restart"]["declined"].as_u64().unwrap_or(0);
        let mut scenes = Vec::new();
        let mut files = Vec::new();
        if save {
            // Saving writes every open scene and script, with whatever
            // external resources they changed.
            let listed = |key: &str| -> Vec<String> {
                state
                    .get(key)
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|entry| entry.get("path").and_then(Value::as_str))
                    .filter(|path| path.starts_with("res://"))
                    .map(str::to_owned)
                    .collect()
            };
            for path in listed("open_scenes") {
                let precondition = self.disk_precondition(&path)?;
                scenes.push(json!({
                    "path": path,
                    "sha256": match &precondition {
                        WritePrecondition::Sha256(sha256) => Value::String(sha256.clone()),
                        _ => Value::Null,
                    },
                }));
                files.push((path.trim_start_matches("res://").to_owned(), precondition));
            }
            for path in listed("open_scripts") {
                let precondition = self.disk_precondition(&path)?;
                files.push((path.trim_start_matches("res://").to_owned(), precondition));
            }
        }
        let scope = if save {
            EditorScope::project_wide(files)
        } else {
            EditorScope::state()
        };
        // From here until the restarted editor connects, nothing else is sent
        // to the editor that is about to exit.
        let guard = self.bridge.begin_restart();
        let opened_before = self.bridge.status().sessions_opened;
        let answer = self
            .mutate(
                tool,
                super::bridge::RESTART_METHOD,
                json!({ "save": save, "scenes": scenes }),
                scope,
                canonical,
                call,
                idempotency_id,
            )
            .await?;
        if answer.get("replayed") == Some(&Value::Bool(true)) {
            return Ok(answer);
        }
        let timeout = Duration::from_millis(timeout_ms);
        let started = std::time::Instant::now();
        let outcome = loop {
            let now = self.bridge.status();
            let pid = now
                .description
                .as_ref()
                .and_then(|description| description.get("pid"))
                .and_then(Value::as_u64);
            if now.connected
                && now.sessions_opened > opened_before
                && pid.is_some_and(|pid| pid != u64::from(previous_pid))
            {
                break Awaited::Restarted(pid.unwrap_or_default());
            }
            if now.connected && pid == Some(u64::from(previous_pid)) {
                // The editor that was asked is still there: did it ask a
                // question, which the add-on cancelled?
                if let Some(state) = self.ask_within("editor.state", RESTART_CHECK).await
                    && state["restart"]["declined"].as_u64().unwrap_or(0) > declined_before
                {
                    break Awaited::Declined(declined(&state["restart"]["question"], &answer));
                }
            }
            let waited = started.elapsed();
            if waited >= timeout {
                let still = now.connected && pid == Some(u64::from(previous_pid));
                break Awaited::Unproven(format!(
                    "the editor (process {previous_pid}) was asked to restart, but no restarted \
                     editor connected within {timeout_ms} ms; {}",
                    if still {
                        "the editor that was asked is still connected and did not restart"
                    } else {
                        "the editor that was asked disconnected. An editor that cannot start, \
                         for example without a display, never connects"
                    }
                ));
            }
            if call.cancellation.is_cancelled() {
                break Awaited::Unproven(format!(
                    "the wait for the restarted editor was cancelled after {} ms; the editor \
                     (process {previous_pid}) was asked to restart",
                    waited.as_millis()
                ));
            }
            tokio::time::sleep(RESTART_POLL).await;
        };
        drop(guard);
        let waited_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        match outcome {
            Awaited::Restarted(pid) => {
                let mut answer = answer;
                if let Value::Object(object) = &mut answer {
                    object.remove("requested");
                    object.insert("restarted".to_owned(), Value::Bool(true));
                    object.insert("previous_pid".to_owned(), json!(previous_pid));
                    object.insert("pid".to_owned(), json!(pid));
                    object.insert("waited_ms".to_owned(), json!(waited_ms));
                }
                Ok(answer)
            }
            Awaited::Declined(message) => Err(restart_error(ErrorKind::Conflict, message, &answer)),
            Awaited::Unproven(message) => {
                Err(restart_error(ErrorKind::Uncertain, message, &answer))
            }
        }
    }
}

/// The error of a restart the editor was asked for and that is not proven:
/// `message`, with the transaction it is journaled in.
fn restart_error(kind: ErrorKind, message: String, answer: &Value) -> OutcomeError {
    let mut details = BTreeMap::new();
    details.insert(
        "transaction_id".to_owned(),
        GodotValue::String(
            answer["transaction_id"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        ),
    );
    details.insert(
        "mutation_status".to_owned(),
        GodotValue::String("committed".to_owned()),
    );
    OutcomeError::new(kind, message, false, details)
}

/// How the wait for a restarted editor ended.
enum Awaited {
    /// The add-on of this editor process connected.
    Restarted(u64),
    /// The editor that was asked declined: it asked a question, which the
    /// add-on cancelled.
    Declined(String),
    /// Nothing proves the editor restarted, and why.
    Unproven(String),
}

/// The refusal of a restart the editor declined: the question it asked,
/// which the add-on cancelled, and what the restart saved before it asked
/// the editor (`answer`, the add-on's answer to the request).
fn declined(question: &Value, answer: &Value) -> String {
    let said = |key: &str| {
        question
            .get(key)
            .and_then(Value::as_str)
            .map(|text| text.chars().take(512).collect::<String>())
            .unwrap_or_default()
    };
    let (title, text) = (said("title"), said("text"));
    let first = saved_first(answer);
    if title.is_empty() && text.is_empty() {
        return format!(
            "the editor was asked to restart and neither restarted nor asked anything; nothing \
             was restarted{first}"
        );
    }
    format!(
        "the editor asked a question before restarting (\"{title}: {text}\"), and godot-mcp \
         cancelled it, so the question saved and discarded nothing and the editor was not \
         restarted{first}. Save or undo what it names, then retry; `save: true` saves the open \
         scenes first, and the script tabs from Godot 4.7.1 on"
    )
}

/// What a saving restart wrote before it asked the editor to restart, as
/// the tail of a sentence, or "" when it wrote nothing: `saved` and
/// `also_saved` list objects with a `path`, `scripts_saved` paths.
fn saved_first(answer: &Value) -> String {
    const NAMED: usize = 8;
    let paths: Vec<&str> = ["saved", "also_saved", "scripts_saved"]
        .iter()
        .filter_map(|key| answer.get(*key).and_then(Value::as_array))
        .flatten()
        .filter_map(|entry| entry.as_str().or_else(|| entry.get("path")?.as_str()))
        .collect();
    if paths.is_empty() {
        return String::new();
    }
    let more = paths.len().saturating_sub(NAMED);
    format!(
        "; before asking, the restart saved {}{}",
        paths[..paths.len().min(NAMED)].join(", "),
        if more > 0 {
            format!(" and {more} more")
        } else {
            String::new()
        }
    )
}

fn route_identity(access: &AccessContext) -> RouterIdentity {
    RouterIdentity {
        transport_mode: access.transport_mode(),
        principal: access.mode_principal().to_journal_string(),
        session: access.connection_session().to_owned(),
    }
}

const fn journal_transport(mode: TransportAuthMode) -> TransportMode {
    match mode {
        TransportAuthMode::Stdio => TransportMode::Stdio,
        TransportAuthMode::TrustedLocal => TransportMode::TrustedLocal,
        TransportAuthMode::ProxiedOidc => TransportMode::ProxiedOidc,
        TransportAuthMode::Open => TransportMode::Open,
    }
}

/// The tool result for one routed editor mutation.
pub(crate) fn editor_answer(report: &RouteReport) -> Result<Value, OutcomeError> {
    if report.replayed() {
        let status = report.recorded_status().ok_or_else(|| {
            outcome_error(ErrorKind::Internal, "a replay carried no recorded status")
        })?;
        let transaction_id = report
            .transaction_id()
            .ok_or_else(|| outcome_error(ErrorKind::Internal, "a replay carried no transaction"))?;
        let verdict = report.recorded_verdict().ok_or_else(|| {
            outcome_error(
                ErrorKind::Unavailable,
                "a replay carried no terminal outcome",
            )
        })?;
        let mut value = replay_outcome(transaction_id, status, report.recorded_result(), verdict)?;
        if let Value::Object(object) = &mut value {
            object.insert("replayed".to_owned(), Value::Bool(true));
        }
        return Ok(value);
    }
    let outcome = report
        .editor_outcome()
        .ok_or_else(|| outcome_error(ErrorKind::Internal, "the editor route returned no answer"))?;
    let transaction_id = report.transaction_id().unwrap_or_default().to_owned();
    let mut details = BTreeMap::new();
    details.insert(
        "transaction_id".to_owned(),
        GodotValue::String(transaction_id.clone()),
    );
    details.insert(
        "mutation_status".to_owned(),
        GodotValue::String(outcome.status.as_str().to_owned()),
    );
    match (outcome.status, &outcome.error) {
        (MutationStatus::Committed, None) => {
            let mut result = match &outcome.payload {
                Value::Object(object) => object.clone(),
                _ => Map::new(),
            };
            result.remove("transaction_id");
            result.insert("transaction_id".to_owned(), Value::String(transaction_id));
            result.insert(
                "mutation_status".to_owned(),
                Value::String(outcome.status.as_str().to_owned()),
            );
            result.insert("replayed".to_owned(), Value::Bool(false));
            result.insert(
                "workspace".to_owned(),
                report.workspace().cloned().unwrap_or(Value::Null),
            );
            Ok(Value::Object(result))
        }
        (MutationStatus::Unresolved, error) => Err(OutcomeError::new(
            ErrorKind::Uncertain,
            format!(
                "whether the editor made the change is not decidable ({}); it is recorded as \
                 unresolved and will not be sent again",
                error
                    .as_ref()
                    .map_or("no answer", |(_, message)| message.as_str())
            ),
            false,
            details,
        )),
        (_, error) => {
            let (kind, message) = error.clone().unwrap_or((
                ErrorKind::Internal,
                "the editor reported no outcome".to_owned(),
            ));
            Err(OutcomeError::new(kind, message, false, details))
        }
    }
}

#[async_trait]
impl KernelToolHandler for EditorHandler {
    async fn call(&self, call: KernelToolCall) -> Result<Value, OutcomeError> {
        if call.cancellation.is_cancelled() {
            return Err(outcome_error(
                ErrorKind::Cancelled,
                "the request was cancelled before it started",
            ));
        }
        let runtime = self.runtime.as_ref();
        let arguments = &call.arguments;
        let operation = self.operation;
        if operation.is_read() {
            return match operation {
                EditorOperation::StatusGet => {
                    let status = runtime.bridge.status();
                    let mut answer = status.to_value();
                    if status.connected {
                        let mut state = runtime.ask("editor.state", json!({})).await?;
                        if let Value::Object(state) = &mut state {
                            let version = status.plugin_version.clone();
                            state.insert(
                                "addon".to_owned(),
                                json!({
                                    "version": version,
                                    "bundled_version": godot_mcp_bridge::addon::ADDON_VERSION,
                                    "update_available": version.as_deref()
                                        != Some(godot_mcp_bridge::addon::ADDON_VERSION),
                                }),
                            );
                        }
                        answer["editor"] = state;
                    } else {
                        answer["editor"] = Value::Null;
                    }
                    Ok(answer)
                }
                EditorOperation::SceneTreeGet => {
                    let scene = optional_scene(arguments)?;
                    runtime
                        .ask(
                            operation.bridge_method(),
                            json!({ "scene": scene.unwrap_or_default() }),
                        )
                        .await
                }
                EditorOperation::NodeGet => {
                    runtime
                        .ask(operation.bridge_method(), node_read(arguments)?)
                        .await
                }
                EditorOperation::TypesSearch => {
                    let text = |key: &str, most: usize| match arguments.get(key) {
                        None => Ok(None),
                        Some(Value::String(text)) if text.len() <= most => Ok(Some(text.clone())),
                        Some(_) => Err(outcome_error(
                            ErrorKind::InvalidInput,
                            format!("`{key}` must be a string of at most {most} bytes"),
                        )),
                    };
                    let query = text("query", 128)?.unwrap_or_default();
                    let base = text("base", 256)?.filter(|base| !base.is_empty());
                    let instantiable = match arguments.get("instantiable") {
                        None => None,
                        Some(Value::Bool(flag)) => Some(*flag),
                        Some(_) => {
                            return Err(outcome_error(
                                ErrorKind::InvalidInput,
                                "`instantiable` must be a boolean",
                            ));
                        }
                    };
                    let limit = match arguments.get("limit") {
                        None => 100,
                        Some(value) => value
                            .as_u64()
                            .filter(|limit| (1..=MAX_CLASSES).contains(limit))
                            .ok_or_else(|| {
                                outcome_error(
                                    ErrorKind::InvalidInput,
                                    format!("`limit` must be from 1 to {MAX_CLASSES}"),
                                )
                            })?,
                    };
                    runtime
                        .ask(
                            operation.bridge_method(),
                            json!({
                                "query": query,
                                "base": base,
                                "instantiable": instantiable,
                                "limit": limit,
                            }),
                        )
                        .await
                }
                EditorOperation::PreferencesGet => {
                    runtime
                        .ask(operation.bridge_method(), preferences_read(arguments)?)
                        .await
                }
                EditorOperation::ShortcutsGet => {
                    let payload = shortcuts_read(arguments)?;
                    let status = runtime.bridge.status();
                    if status.connected
                        && !status
                            .methods
                            .iter()
                            .any(|method| method == operation.bridge_method())
                    {
                        return Err(no_shortcuts(
                            &status,
                            operation.bridge_method(),
                            "Nothing was read.",
                        ));
                    }
                    runtime.ask(operation.bridge_method(), payload).await
                }
                EditorOperation::LogGet => {
                    let status = runtime.bridge.status();
                    if status.connected
                        && !status
                            .methods
                            .iter()
                            .any(|method| method == "editor.log.read")
                    {
                        return Err(no_log_capture(&status));
                    }
                    let after = match arguments.get("after") {
                        None => 0,
                        Some(value) => value.as_u64().ok_or_else(|| {
                            outcome_error(
                                ErrorKind::InvalidInput,
                                "`after` must be a sequence number, 0 or more",
                            )
                        })?,
                    };
                    let epoch = match arguments.get("epoch") {
                        None => None,
                        Some(Value::String(epoch)) if !epoch.is_empty() && epoch.len() <= 64 => {
                            Some(epoch.clone())
                        }
                        Some(_) => {
                            return Err(outcome_error(
                                ErrorKind::InvalidInput,
                                "`epoch` must be the epoch of a previous read",
                            ));
                        }
                    };
                    let limit = match arguments.get("limit") {
                        None => 100,
                        Some(value) => value
                            .as_u64()
                            .filter(|limit| (1..=MAX_LOG_ENTRIES).contains(limit))
                            .ok_or_else(|| {
                                outcome_error(
                                    ErrorKind::InvalidInput,
                                    format!("`limit` must be from 1 to {MAX_LOG_ENTRIES}"),
                                )
                            })?,
                    };
                    let kinds = match arguments.get("kinds") {
                        None => Value::Null,
                        Some(Value::Array(kinds))
                            if !kinds.is_empty()
                                && kinds.iter().all(|kind| {
                                    kind.as_str().is_some_and(|kind| LOG_KINDS.contains(&kind))
                                }) =>
                        {
                            Value::Array(kinds.clone())
                        }
                        Some(_) => {
                            return Err(outcome_error(
                                ErrorKind::InvalidInput,
                                format!("`kinds` lists entry kinds from {}", LOG_KINDS.join(", ")),
                            ));
                        }
                    };
                    runtime
                        .ask(
                            operation.bridge_method(),
                            json!({ "after": after, "epoch": epoch, "limit": limit, "kinds": kinds }),
                        )
                        .await
                }
                EditorOperation::CameraGet => {
                    runtime
                        .ask(operation.bridge_method(), camera_read(arguments)?)
                        .await
                }
                EditorOperation::DialogsGet => {
                    runtime
                        .ask(operation.bridge_method(), dialogs_read(arguments)?)
                        .await
                }
                EditorOperation::PerformanceGet => {
                    runtime
                        .ask(operation.bridge_method(), performance_read(arguments)?)
                        .await
                }
                EditorOperation::SceneRead => {
                    let asked = super::scene_read::request(arguments, optional_scene(arguments)?)?;
                    let answer = runtime
                        .ask(operation.bridge_method(), asked.clone())
                        .await?;
                    let calls = asked["calls"].as_array().cloned().unwrap_or_default();
                    super::scene_read::answer(&calls, &answer)
                }
                _ => {
                    let class = arguments
                        .get("class")
                        .and_then(Value::as_str)
                        .filter(|class| !class.is_empty() && class.len() <= 256)
                        .ok_or_else(|| {
                            outcome_error(ErrorKind::InvalidInput, "`class` names an engine class")
                        })?;
                    runtime
                        .ask(operation.bridge_method(), json!({ "class": class }))
                        .await
                }
            };
        }
        let tool = operation.tool_name();
        if operation == EditorOperation::ScriptRun && !runtime.allow_scripts {
            return Err(crate::scripts::scripts_off());
        }
        let idempotency_id = call.idempotency_id.clone().ok_or_else(|| {
            outcome_error(
                ErrorKind::InvalidInput,
                format!("{tool} requires `idempotencyId`, so a retried call can never apply twice"),
            )
        })?;
        if operation == EditorOperation::PlayStart && runtime.trust == ProcessTrust::Untrusted {
            return Err(outcome_error(ErrorKind::PermissionDenied, UNTRUSTED_PLAY));
        }
        if !runtime.bridge.status().connected {
            return Err(not_connected());
        }
        if runtime.bridge.restarting() {
            return Err(if operation == EditorOperation::Restart {
                outcome_error(
                    ErrorKind::Conflict,
                    "a restart of the editor is already awaited; nothing else was restarted",
                )
            } else {
                outcome_error(ErrorKind::Unavailable, BridgeFailure::Restarting.message())
            });
        }
        let canonical = Value::Object(arguments.clone());
        let status = runtime.bridge.status();
        if !status
            .methods
            .iter()
            .any(|method| method == operation.bridge_method())
        {
            return Err(match operation {
                EditorOperation::SceneClose => no_scene_close(&status),
                EditorOperation::ShortcutsSet => {
                    no_shortcuts(&status, operation.bridge_method(), "Nothing was changed.")
                }
                EditorOperation::CameraSet => no_camera_set(&status),
                EditorOperation::ScriptClose => no_script_close(&status),
                _ => plugin_lacks(&status, operation.bridge_method()),
            });
        }
        if operation == EditorOperation::Restart {
            return runtime
                .restart(tool, arguments, &canonical, &call, &idempotency_id)
                .await;
        }
        let mut reimported: Vec<String> = Vec::new();
        let (payload, scope) = match operation {
            EditorOperation::FilesystemScan => {
                reimported = match arguments.get("reimport") {
                    None => Vec::new(),
                    Some(Value::Array(paths)) if paths.len() <= MAX_REIMPORTS => paths
                        .iter()
                        .map(|path| {
                            path.as_str()
                                .ok_or_else(|| {
                                    outcome_error(
                                        ErrorKind::InvalidInput,
                                        "every `reimport` entry is a project path",
                                    )
                                })
                                .and_then(res_path)
                        })
                        .collect::<Result<_, _>>()?,
                    Some(_) => {
                        return Err(outcome_error(
                            ErrorKind::InvalidInput,
                            format!("`reimport` lists at most {MAX_REIMPORTS} project paths"),
                        ));
                    }
                };
                let timeout_ms = match arguments.get("timeoutMs") {
                    None => SCAN_TIMEOUT_MS.1,
                    Some(value) => value
                        .as_u64()
                        .filter(|ms| (SCAN_TIMEOUT_MS.0..=SCAN_TIMEOUT_MS.2).contains(ms))
                        .ok_or_else(|| {
                            outcome_error(
                                ErrorKind::InvalidInput,
                                format!(
                                    "`timeoutMs` is from {} to {}",
                                    SCAN_TIMEOUT_MS.0, SCAN_TIMEOUT_MS.2
                                ),
                            )
                        })?,
                };
                // The editor's imports write `.import`, `.uid` and cache files
                // nobody can list in advance.
                (
                    json!({ "reimport": reimported, "timeout_ms": timeout_ms }),
                    EditorScope::project_wide(Vec::new()).within(Duration::from_millis(timeout_ms)),
                )
            }
            EditorOperation::ScriptOpen => {
                let path = res_path(arguments.get("path").and_then(Value::as_str).ok_or_else(
                    || outcome_error(ErrorKind::InvalidInput, "`path` is required"),
                )?)?;
                if !path.ends_with(".gd") {
                    return Err(outcome_error(
                        ErrorKind::InvalidInput,
                        format!("`{path}` must name a .gd script"),
                    ));
                }
                let position = |key: &str| match arguments.get(key) {
                    None => Ok(1),
                    Some(value) => value
                        .as_u64()
                        .filter(|position| (1..=MAX_SCRIPT_POSITION).contains(position))
                        .ok_or_else(|| {
                            outcome_error(
                                ErrorKind::InvalidInput,
                                format!("`{key}` is from 1 to {MAX_SCRIPT_POSITION}"),
                            )
                        }),
                };
                (
                    json!({ "path": path, "line": position("line")?, "column": position("column")? }),
                    EditorScope::state(),
                )
            }
            EditorOperation::MainScreenSet => {
                let screen = arguments
                    .get("screen")
                    .and_then(Value::as_str)
                    .filter(|screen| MAIN_SCREENS.contains(screen))
                    .ok_or_else(|| {
                        outcome_error(ErrorKind::InvalidInput, "`screen` is 2D, 3D or Script")
                    })?;
                (json!({ "screen": screen }), EditorScope::state())
            }
            EditorOperation::ScreenshotCapture => {
                let view = match arguments.get("view") {
                    None => "window",
                    Some(Value::String(view)) if CAPTURE_VIEWS.contains(&view.as_str()) => {
                        view.as_str()
                    }
                    Some(_) => {
                        return Err(outcome_error(
                            ErrorKind::InvalidInput,
                            "`view` is window, 2d or 3d",
                        ));
                    }
                };
                let viewport = match arguments.get("viewport") {
                    None => 0,
                    Some(value) => value.as_u64().filter(|index| *index <= 3).ok_or_else(|| {
                        outcome_error(ErrorKind::InvalidInput, "`viewport` is from 0 to 3")
                    })?,
                };
                let max_width = match arguments.get("maxWidth") {
                    None => 0,
                    Some(value) => value
                        .as_u64()
                        .filter(|width| (1..=MAX_CAPTURE_WIDTH).contains(width))
                        .ok_or_else(|| {
                            outcome_error(
                                ErrorKind::InvalidInput,
                                format!("`maxWidth` is from 1 to {MAX_CAPTURE_WIDTH}"),
                            )
                        })?,
                };
                (
                    json!({
                        "view": view,
                        "viewport": viewport,
                        "max_width": max_width,
                        // The boundary keeps the artifact for this caller and
                        // removes the owner before the editor is asked.
                        "owner": {
                            "principal": call.access.principal().as_str(),
                        },
                    }),
                    EditorScope::state(),
                )
            }
            EditorOperation::SceneOpen => {
                let path = scene_path(arguments.get("path").and_then(Value::as_str).ok_or_else(
                    || outcome_error(ErrorKind::InvalidInput, "`path` is required"),
                )?)?;
                let reload = match arguments.get("reload") {
                    None => false,
                    Some(Value::Bool(reload)) => *reload,
                    Some(_) => {
                        return Err(outcome_error(
                            ErrorKind::InvalidInput,
                            "`reload` must be a boolean",
                        ));
                    }
                };
                (
                    json!({ "path": path, "reload": reload }),
                    EditorScope::state(),
                )
            }
            EditorOperation::SelectionSet => (selection_request(arguments)?, EditorScope::state()),
            EditorOperation::Undo | EditorOperation::Redo => {
                let scene = optional_scene(arguments)?;
                let save = match arguments.get("save") {
                    None => false,
                    Some(Value::Bool(save)) => *save,
                    Some(_) => {
                        return Err(outcome_error(
                            ErrorKind::InvalidInput,
                            "`save` must be a boolean",
                        ));
                    }
                };
                // A save writes the scene file, so that file is in the
                // transaction's scope with the content it holds now.
                let files = if save {
                    let path = match &scene {
                        Some(scene) => scene.clone(),
                        None => runtime.edited_scene().await?,
                    };
                    let relative = path.trim_start_matches("res://").to_owned();
                    vec![(relative, runtime.disk_precondition(&path)?)]
                } else {
                    Vec::new()
                };
                (
                    json!({ "scene": scene.unwrap_or_default(), "save": save }),
                    EditorScope::files(files),
                )
            }
            EditorOperation::SceneEdit => (edit_request(arguments, tool)?, EditorScope::state()),
            EditorOperation::SceneSave => {
                let scene = optional_scene(arguments)?;
                let all = match arguments.get("all") {
                    None => false,
                    Some(Value::Bool(all)) => *all,
                    Some(_) => {
                        return Err(outcome_error(
                            ErrorKind::InvalidInput,
                            "`all` must be a boolean",
                        ));
                    }
                };
                if all && scene.is_some() {
                    return Err(outcome_error(
                        ErrorKind::InvalidInput,
                        "name one open `scene` or save `all` of them, not both",
                    ));
                }
                let declared = runtime.scenes_to_save(scene, all).await?;
                let mut files = Vec::with_capacity(declared.len());
                let mut scenes = Vec::with_capacity(declared.len());
                for path in &declared {
                    let precondition = runtime.disk_precondition(path)?;
                    scenes.push(json!({
                        "path": path,
                        "sha256": match &precondition {
                            WritePrecondition::Sha256(sha256) => Value::String(sha256.clone()),
                            _ => Value::Null,
                        },
                    }));
                    files.push((path.trim_start_matches("res://").to_owned(), precondition));
                }
                // Saving a scene can also write the external resources it
                // changed, which nobody can list in advance.
                (
                    json!({ "scenes": scenes, "all": all }),
                    EditorScope::project_wide(files),
                )
            }
            EditorOperation::AddonReload => (json!({}), EditorScope::state()),
            // The code may write any project file, and the editor may still be
            // running it when the deadline ends the wait.
            EditorOperation::ScriptRun => {
                let request = crate::live_script::request(arguments)?;
                (
                    request.payload,
                    EditorScope::project_wide(Vec::new()).within(request.deadline),
                )
            }
            // An editor setting lives in the user's own settings file, outside
            // the project: no project file is in scope.
            EditorOperation::PreferencesSet => {
                (preferences_change(arguments)?, EditorScope::state())
            }
            EditorOperation::ShortcutsSet => (shortcuts_change(arguments)?, EditorScope::state()),
            EditorOperation::SceneClose => (
                json!({ "scene": optional_scene(arguments)?.unwrap_or_default() }),
                EditorScope::state(),
            ),
            EditorOperation::PlayStart => {
                let scene = play_scene(arguments)?;
                let mut files = Vec::new();
                let mut scenes = Vec::new();
                for path in runtime.open_scene_files().await? {
                    let precondition = runtime.disk_precondition(&path)?;
                    scenes.push(json!({
                        "path": path,
                        "sha256": match &precondition {
                            WritePrecondition::Sha256(sha256) => Value::String(sha256.clone()),
                            _ => Value::Null,
                        },
                    }));
                    files.push((path.trim_start_matches("res://").to_owned(), precondition));
                }
                // The editor saves the open scenes before it runs one, with
                // whatever external resources they changed.
                (
                    json!({ "scene": scene, "scenes": scenes }),
                    EditorScope::project_wide(files),
                )
            }
            EditorOperation::PlayStop => (json!({}), EditorScope::state()),
            EditorOperation::CameraSet => (camera_move(arguments)?, EditorScope::state()),
            EditorOperation::ScriptClose => (script_tab(arguments)?, EditorScope::state()),
            // The button does whatever the editor does for it: it may save,
            // discard or delete any project file.
            EditorOperation::DialogRespond => (
                dialog_press(arguments)?,
                EditorScope::project_wide(Vec::new()),
            ),
            EditorOperation::ResourcePreview => {
                let (path, timeout_ms) = preview_request(arguments)?;
                (
                    json!({
                        "path": path,
                        "timeout_ms": timeout_ms,
                        // The boundary keeps the artifacts for this caller and
                        // removes the owner before the editor is asked.
                        "owner": {
                            "principal": call.access.principal().as_str(),
                        },
                    }),
                    EditorScope::state().within(Duration::from_millis(timeout_ms) + PREVIEW_MARGIN),
                )
            }
            EditorOperation::Restart => unreachable!("a restart is answered above"),
            EditorOperation::StatusGet
            | EditorOperation::SceneTreeGet
            | EditorOperation::TypesGet
            | EditorOperation::NodeGet
            | EditorOperation::TypesSearch
            | EditorOperation::LogGet
            | EditorOperation::PreferencesGet
            | EditorOperation::ShortcutsGet
            | EditorOperation::CameraGet
            | EditorOperation::DialogsGet
            | EditorOperation::PerformanceGet
            | EditorOperation::SceneRead => unreachable!("reads are answered above"),
        };
        // Read from disk now, not from the last snapshot: the editor writes
        // files of its own (`.uid` sidecars, imports) the snapshot may predate.
        let before = if operation == EditorOperation::ScriptRun {
            crate::scripts::contents_of_inventory(&runtime.root, &runtime.workspace.rescan())?
        } else {
            None
        };
        let result = runtime
            .mutate(
                tool,
                operation.bridge_method(),
                payload,
                scope,
                &canonical,
                &call,
                &idempotency_id,
            )
            .await;
        if operation == EditorOperation::ScriptRun {
            let after =
                crate::scripts::contents_of_inventory(&runtime.root, &runtime.workspace.rescan());
            return crate::live_script::finish(
                crate::live_script::Place::Editor,
                result,
                before.as_ref(),
                after,
            );
        }
        let mut answer = result?;
        if !reimported.is_empty() {
            // The import state the editor left, read as asset.status reads it.
            answer["assets"] = Value::Array(
                reimported
                    .iter()
                    .map(|path| {
                        let relative = path.trim_start_matches("res://");
                        crate::authoring::assets::import_status_value(&runtime.root, relative)
                            .unwrap_or_else(
                                |error| json!({ "path": relative, "error": error.message() }),
                            )
                    })
                    .collect(),
            );
        }
        Ok(answer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An editor tool's schema requires an idempotency id exactly when the
    /// tool is a mutation, which is when its handler refuses a call without
    /// one: every operation that is not a read.
    #[test]
    fn exactly_the_editor_mutations_require_an_idempotency_id() {
        for &operation in EditorOperation::ALL {
            assert_eq!(
                operation.is_read(),
                !operation.descriptor().is_mutation(),
                "{}",
                operation.tool_name()
            );
            let schema = crate::kernel::tool_schema(
                operation.tool_name(),
                crate::core::CoreLimits::default(),
            );
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

    /// Hold the advertised schema of `operation` and the handler's argument
    /// check to the same verdict on every case. A case is given without the
    /// idempotency id, which the kernel takes out before a handler runs.
    fn agree(
        operation: EditorOperation,
        cases: &[(Value, bool)],
        check: impl Fn(&Map<String, Value>) -> bool,
    ) {
        let schema = Value::Object(crate::kernel::tool_schema(
            operation.tool_name(),
            crate::core::CoreLimits::default(),
        ));
        for (case, expected) in cases {
            let mut arguments = case.as_object().cloned().expect("arguments are an object");
            if !operation.is_read() {
                arguments.insert("idempotencyId".to_owned(), json!("call-1"));
            }
            assert_eq!(
                crate::schema::conformance::conforms(&schema, &Value::Object(arguments.clone())),
                *expected,
                "the {} schema's verdict on {case}",
                operation.tool_name()
            );
            arguments.remove("idempotencyId");
            assert_eq!(
                check(&arguments),
                *expected,
                "the {} handler's verdict on {case}",
                operation.tool_name()
            );
        }
    }

    fn integer(value: i64) -> Value {
        json!({ "type": "integer", "value": value })
    }

    /// The name cases of `rule`, each as the arguments `wrap` makes of it.
    fn named(
        rule: crate::schema::conformance::NameRule,
        wrap: impl Fn(String) -> Value,
    ) -> Vec<(Value, bool)> {
        crate::schema::conformance::name_cases(rule, "", "")
            .into_iter()
            .map(|(name, expected)| (wrap(name), expected))
            .collect()
    }

    /// A node read's node is 1 to 1024 characters, whatever the bytes each
    /// takes, and a path relative to the scene root; its property names are
    /// 1 to 256 characters. The check takes a control character in either,
    /// so the schema does too.
    #[test]
    fn the_node_read_schema_and_its_check_agree() {
        use crate::schema::conformance::NameRule;
        let node = NameRule {
            limit: MAX_NODE_PATH_CHARS,
            empty: false,
            control: false,
        };
        let property = NameRule { limit: 256, ..node };
        let mut cases = named(node, |node| json!({ "node": node }));
        cases.extend(named(property, |name| json!({ "properties": [name] })));
        for (node, expected) in [
            (".", true),
            ("Player/Sprite", true),
            ("..a/b..", true),
            ("/root", false),
            ("a/", false),
            ("a//b", false),
            ("..", false),
            ("../a", false),
            ("a/../b", false),
        ] {
            cases.push((json!({ "node": node }), expected));
        }
        cases.extend([
            (json!({}), true),
            (json!({ "properties": [] }), true),
            (json!({ "node": 3 }), false),
            (json!({ "properties": [3] }), false),
        ]);
        agree(EditorOperation::NodeGet, &cases, |arguments| {
            node_read(arguments).is_ok()
        });
    }

    /// A selected node is 1 to 1024 characters, whatever the bytes each
    /// takes. The check takes any such string, so the schema does too.
    #[test]
    fn the_selection_schema_and_its_check_agree() {
        let rule = crate::schema::conformance::NameRule {
            limit: MAX_NODE_PATH_CHARS,
            empty: false,
            control: false,
        };
        let mut cases = named(rule, |node| json!({ "nodes": [node] }));
        cases.extend([
            (json!({ "nodes": [] }), true),
            (json!({ "nodes": vec!["a"; MAX_SELECTION] }), true),
            (json!({ "nodes": vec!["a"; MAX_SELECTION + 1] }), false),
            (json!({ "nodes": [3] }), false),
            (json!({}), false),
        ]);
        agree(EditorOperation::SelectionSet, &cases, |arguments| {
            selection_request(arguments).is_ok()
        });
    }

    /// A live edit's label is 1 to 96 characters without a control
    /// character, and its operations are scene batch operations naming nodes
    /// by path, signal callbacks included.
    #[test]
    fn the_live_edit_schema_and_its_check_agree() {
        let rule = crate::schema::conformance::NameRule {
            limit: MAX_LABEL_CHARS,
            empty: false,
            control: true,
        };
        let digest = "0".repeat(64);
        let add = json!({ "op": "add_node", "name": "Enemy" });
        let mut cases = named(
            rule,
            |label| json!({ "expectedDigest": digest, "operations": [add], "label": label }),
        );
        let connect = json!({ "op": "connect_signal", "from": "Area", "signal": "body_entered",
                              "to": ".", "method": "_on_body_entered" });
        cases.extend([
            (
                json!({ "expectedDigest": digest, "operations": [add] }),
                true,
            ),
            (
                json!({ "expectedDigest": digest, "operations": [connect] }),
                true,
            ),
            (json!({ "expectedDigest": digest, "operations": [] }), false),
            (json!({ "expectedDigest": "0", "operations": [add] }), false),
        ]);
        agree(EditorOperation::SceneEdit, &cases, |arguments| {
            edit_request(arguments, "editor.scene.edit").is_ok()
        });
    }

    #[test]
    fn the_settings_read_schema_and_its_check_agree() {
        let names = |count: usize| -> Vec<String> {
            (0..count)
                .map(|index| format!("a/setting_{index}"))
                .collect()
        };
        let cases = [
            (json!({}), true),
            (
                json!({ "settings": ["text_editor/behavior/indent/size"] }),
                true,
            ),
            (json!({ "settings": ["a/b", "a/b"] }), true),
            (json!({ "settings": ["a/"] }), true),
            (json!({ "settings": ["é/ü"] }), true),
            (
                json!({ "settings": [format!("{}/b", "x".repeat(254))] }),
                true,
            ),
            (json!({ "settings": names(256) }), true),
            (
                json!({ "settings": [format!("{}/b", "x".repeat(255))] }),
                false,
            ),
            (json!({ "settings": names(257) }), false),
            (json!({ "settings": [] }), false),
            (json!({ "settings": ["no_slash"] }), false),
            (json!({ "settings": ["/a"] }), false),
            (json!({ "settings": ["a/b\n"] }), false),
            (json!({ "settings": ["a/\u{85}"] }), false),
            (json!({ "settings": ["a/\u{7f}"] }), false),
            (json!({ "settings": [3] }), false),
            (json!({ "settings": "a/b" }), false),
            (json!({ "prefix": "" }), true),
            (json!({ "prefix": "text_editor/" }), true),
            (json!({ "prefix": "x".repeat(256) }), true),
            (json!({ "prefix": "x".repeat(257) }), false),
            (json!({ "prefix": "a\u{1}" }), false),
            (json!({ "prefix": 1 }), false),
            (json!({ "offset": 0 }), true),
            (json!({ "offset": 10_000 }), true),
            (json!({ "offset": -1 }), false),
            (json!({ "offset": "1" }), false),
            (json!({ "limit": 1 }), true),
            (json!({ "limit": 500 }), true),
            (json!({ "limit": 0 }), false),
            (json!({ "limit": 501 }), false),
            (
                json!({ "settings": ["a/b"], "prefix": "a/", "offset": 1, "limit": 1 }),
                true,
            ),
        ];
        agree(EditorOperation::PreferencesGet, &cases, |arguments| {
            preferences_read(arguments).is_ok()
        });
    }

    /// Which settings may not be changed is the handler's to decide by name,
    /// and the editor's by hint: the schema lists no setting and says so in
    /// words. Every other case gets one verdict from both.
    #[test]
    fn the_settings_change_schema_and_its_check_agree() {
        let resource = json!({ "type": "resource", "value": {
            "resource": "res://theme.tres", "resource_type": "Theme", "uid": null
        } });
        let many = |count: usize| -> Value {
            Value::Object(
                (0..count)
                    .map(|index| (format!("a/setting_{index}"), integer(1)))
                    .collect(),
            )
        };
        let cases = [
            (json!({ "settings": { "a/b": integer(1) } }), true),
            (
                json!({ "settings": { "a/b": { "type": "bool", "value": true } } }),
                true,
            ),
            (
                json!({ "settings": { "a/b": { "type": "color", "value": {
                    "red": 1.0, "green": 0.5, "blue": 0.0, "alpha": 1.0
                } } } }),
                true,
            ),
            (
                json!({ "settings": { "a/b": { "type": "packed_string_array", "value": ["x"] } } }),
                true,
            ),
            (
                json!({ "settings": { "a/b": { "type": "dictionary", "value": { "k": integer(1) } } } }),
                true,
            ),
            (json!({ "settings": many(64) }), true),
            (json!({ "settings": many(65) }), false),
            (json!({ "settings": {} }), false),
            (json!({ "settings": [] }), false),
            (json!({}), false),
            (json!({ "settings": { "a/b": 2 } }), false),
            (
                json!({ "settings": { "a/b": { "type": "integer", "value": 2, "x": 1 } } }),
                false,
            ),
            (json!({ "settings": { "a/b": resource.clone() } }), false),
            (
                json!({ "settings": { "a/b": { "type": "array", "value": [resource.clone()] } } }),
                false,
            ),
            (
                json!({ "settings": { "a/b": { "type": "dictionary", "value": { "k": resource } } } }),
                false,
            ),
        ];
        agree(EditorOperation::PreferencesSet, &cases, |arguments| {
            preferences_change(arguments).is_ok()
        });
        let refused = json!({ "settings": { "export/android/debug_keystore_pass": { "type": "string", "value": "x" } } });
        let error = preferences_change(refused.as_object().expect("arguments"))
            .expect_err("a setting refused by its name");
        assert_eq!(error.kind(), ErrorKind::PermissionDenied, "{error:?}");
    }

    /// A scene's path syntax and the scene it names are the handler's to
    /// check, as for every editor tool that takes a `scene`.
    #[test]
    fn the_scene_close_schema_and_its_check_agree() {
        let cases = [
            (json!({}), true),
            (json!({ "scene": "main.tscn" }), true),
            (json!({ "scene": "res://levels/one.tscn" }), true),
            (json!({ "scene": "" }), false),
            (json!({ "scene": 3 }), false),
        ];
        agree(EditorOperation::SceneClose, &cases, |arguments| {
            optional_scene(arguments).is_ok()
        });
    }

    #[test]
    fn the_shortcuts_read_schema_and_its_check_agree() {
        let names = |count: usize| -> Value {
            Value::Array(
                (0..count)
                    .map(|index| json!(format!("a/s_{index}")))
                    .collect(),
            )
        };
        let cases = [
            (json!({}), true),
            (
                json!({ "shortcuts": ["script_editor/save", "ui_accept"] }),
                true,
            ),
            (json!({ "shortcuts": names(256) }), true),
            (json!({ "shortcuts": [&"x".repeat(256)] }), true),
            (json!({ "shortcuts": [&"x".repeat(257)] }), false),
            (json!({ "shortcuts": names(257) }), false),
            (json!({ "shortcuts": [] }), false),
            (json!({ "shortcuts": [""] }), false),
            (json!({ "shortcuts": ["a\n"] }), false),
            (json!({ "shortcuts": ["a\u{85}"] }), false),
            (json!({ "shortcuts": [3] }), false),
            (json!({ "shortcuts": "ui_accept" }), false),
            (json!({ "prefix": "" }), true),
            (json!({ "prefix": "ui_" }), true),
            (json!({ "prefix": "x".repeat(257) }), false),
            (json!({ "prefix": "a\u{1}" }), false),
            (json!({ "changedOnly": true }), true),
            (json!({ "changedOnly": false }), true),
            (json!({ "changedOnly": "true" }), false),
            (json!({ "offset": 0 }), true),
            (json!({ "offset": -1 }), false),
            (json!({ "limit": 1 }), true),
            (json!({ "limit": 500 }), true),
            (json!({ "limit": 0 }), false),
            (json!({ "limit": 501 }), false),
            (
                json!({ "shortcuts": ["a/b"], "prefix": "a/", "changedOnly": true, "offset": 1, "limit": 1 }),
                true,
            ),
        ];
        agree(EditorOperation::ShortcutsGet, &cases, |arguments| {
            shortcuts_read(arguments).is_ok()
        });
    }

    #[test]
    fn the_shortcuts_change_schema_and_its_check_agree() {
        let many = |count: usize| -> Value {
            Value::Object(
                (0..count)
                    .map(|index| (format!("a/s_{index}"), json!([])))
                    .collect(),
            )
        };
        let events = |count: usize| -> Value {
            Value::Array((0..count).map(|_| json!({ "key": "F9" })).collect())
        };
        let cases = [
            (json!({ "shortcuts": { "script_editor/save": [] } }), true),
            (json!({ "shortcuts": { "ui_cancel": "default" } }), true),
            (
                json!({ "shortcuts": { "script_editor/save": [
                    { "key": "S", "command_or_control": true, "alt": true },
                    { "key": "F9", "match": "physical", "shift": true, "ctrl": false }
                ] } }),
                true,
            ),
            (
                json!({ "shortcuts": { "a/b": [{
                    "key": "A", "match": "label", "shift": true, "ctrl": true,
                    "alt": true, "meta": true, "command_or_control": false
                }] } }),
                true,
            ),
            (
                json!({ "shortcuts": { "a/b": [{ "key": "A", "match": "keycode" }] } }),
                true,
            ),
            (
                json!({ "shortcuts": { "a/b": [{ "key": "Kp Add" }] } }),
                true,
            ),
            (
                json!({ "shortcuts": { "a/b": [{ "key": &"x".repeat(64) }] } }),
                true,
            ),
            (json!({ "shortcuts": { "a/b": events(16) } }), true),
            (json!({ "shortcuts": many(64) }), true),
            (json!({ "shortcuts": many(65) }), false),
            (json!({ "shortcuts": {} }), false),
            (json!({ "shortcuts": [] }), false),
            (json!({}), false),
            (json!({ "shortcuts": { "": [] } }), false),
            (json!({ "shortcuts": { "a/\u{7}": [] } }), false),
            (json!({ "shortcuts": { &"x".repeat(257): [] } }), false),
            (json!({ "shortcuts": { "a/b": "reset" } }), false),
            (json!({ "shortcuts": { "a/b": 3 } }), false),
            (json!({ "shortcuts": { "a/b": null } }), false),
            (json!({ "shortcuts": { "a/b": events(17) } }), false),
            (json!({ "shortcuts": { "a/b": [{}] } }), false),
            (json!({ "shortcuts": { "a/b": ["S"] } }), false),
            (json!({ "shortcuts": { "a/b": [{ "key": "" }] } }), false),
            (json!({ "shortcuts": { "a/b": [{ "key": 83 }] } }), false),
            (
                json!({ "shortcuts": { "a/b": [{ "key": "Ctrl+S" }] } }),
                false,
            ),
            (json!({ "shortcuts": { "a/b": [{ "key": "S\t" }] } }), false),
            (
                json!({ "shortcuts": { "a/b": [{ "key": &"x".repeat(65) }] } }),
                false,
            ),
            (
                json!({ "shortcuts": { "a/b": [{ "key": "S", "shift": "yes" }] } }),
                false,
            ),
            (
                json!({ "shortcuts": { "a/b": [{ "key": "S", "text": "S" }] } }),
                true,
            ),
            (
                json!({ "shortcuts": { "a/b": [{
                    "key": "S", "match": "keycode", "shift": false, "ctrl": false, "alt": true,
                    "meta": false, "command_or_control": true, "text": "Ctrl+Alt+S"
                }] } }),
                true,
            ),
            (
                json!({ "shortcuts": { "a/b": [{ "key": "S", "text": 3 }] } }),
                false,
            ),
            (
                json!({ "shortcuts": { "a/b": [{ "key": "S", "text": "x".repeat(257) }] } }),
                false,
            ),
            (
                json!({ "shortcuts": { "a/b": [{ "key": "S", "match": "position" }] } }),
                false,
            ),
            (
                json!({ "shortcuts": { "a/b": [{ "key": "S", "match": true }] } }),
                false,
            ),
            (
                json!({ "shortcuts": { "a/b": [{ "key": "S", "physical": true }] } }),
                false,
            ),
            (
                json!({ "shortcuts": { "a/b": [{ "key": "S", "command_or_control": true, "ctrl": true }] } }),
                false,
            ),
            (
                json!({ "shortcuts": { "a/b": [{ "key": "S", "command_or_control": true, "meta": true }] } }),
                false,
            ),
            (
                json!({ "shortcuts": { "a/b": [{ "key": "S", "command_or_control": true, "ctrl": false, "meta": false }] } }),
                true,
            ),
            (
                json!({ "shortcuts": { "a/b": [{ "key": "S", "command_or_control": false, "ctrl": true, "meta": true }] } }),
                true,
            ),
        ];
        agree(EditorOperation::ShortcutsSet, &cases, |arguments| {
            shortcuts_change(arguments).is_ok()
        });
    }

    /// That a 3D viewport is not part of the 2D view is the handler's to
    /// check, as every rule across top-level fields is. Every other case gets
    /// one verdict from both.
    #[test]
    fn the_camera_read_schema_and_its_check_agree() {
        let cases = [
            (json!({}), true),
            (json!({ "view": "2d" }), true),
            (json!({ "view": "3d" }), true),
            (json!({ "viewport": 0 }), true),
            (json!({ "viewport": 3 }), true),
            (json!({ "view": "3d", "viewport": 2 }), true),
            (json!({ "view": "4d" }), false),
            (json!({ "view": 2 }), false),
            (json!({ "viewport": 4 }), false),
            (json!({ "viewport": -1 }), false),
            (json!({ "viewport": 1.5 }), false),
            (json!({ "viewport": "1" }), false),
        ];
        agree(EditorOperation::CameraGet, &cases, |arguments| {
            camera_read(arguments).is_ok()
        });
        // A rule across two fields is the handler's: a schema's top level
        // stays one plain object.
        let both = json!({ "view": "2d", "viewport": 0 });
        let error = camera_read(both.as_object().expect("arguments"))
            .expect_err("a 3D viewport of the 2D view");
        assert_eq!(error.kind(), ErrorKind::InvalidInput, "{error:?}");
        assert_eq!(
            camera_read(json!({ "viewport": 1 }).as_object().expect("arguments")).expect("read"),
            json!({ "view": "3d", "viewport": 1 }),
            "a 3D viewport alone reads the 3D view"
        );
    }

    /// That a camera cannot look at the point it is at is the handler's to
    /// check: no schema keyword compares two properties. Every other case
    /// gets one verdict from both.
    #[test]
    fn the_camera_move_schema_and_its_check_agree() {
        let at = json!({ "x": 6, "y": 4, "z": 8 });
        let origin = json!({ "x": 0, "y": 0, "z": 0 });
        let cases = [
            (json!({ "position": at, "target": origin }), true),
            (
                json!({ "viewport": 3, "position": at, "target": origin }),
                true,
            ),
            (
                json!({ "position": { "x": 1.0e9, "y": -1.0e9, "z": 0.5 }, "target": origin }),
                true,
            ),
            (
                json!({ "position": { "x": 1.5e9, "y": 0, "z": 0 }, "target": origin }),
                false,
            ),
            (
                json!({ "position": { "x": 1, "y": 2 }, "target": origin }),
                false,
            ),
            (
                json!({ "position": { "x": 1, "y": 2, "w": 3 }, "target": origin }),
                false,
            ),
            (
                json!({ "position": { "x": 1, "y": 2, "z": 3, "w": 4 }, "target": origin }),
                false,
            ),
            (
                json!({ "position": { "x": "1", "y": 2, "z": 3 }, "target": origin }),
                false,
            ),
            (json!({ "position": [1, 2, 3], "target": origin }), false),
            (json!({ "position": at }), false),
            (json!({ "target": origin }), false),
            (
                json!({ "viewport": 4, "position": at, "target": origin }),
                false,
            ),
        ];
        agree(EditorOperation::CameraSet, &cases, |arguments| {
            camera_move(arguments).is_ok()
        });
        let same = json!({ "position": at, "target": at });
        let error = camera_move(same.as_object().expect("arguments"))
            .expect_err("a camera looking at its own position");
        assert_eq!(error.kind(), ErrorKind::InvalidInput, "{error:?}");
    }

    /// Which path names a script is the handler's to check, as for
    /// editor.script.open.
    #[test]
    fn the_script_close_schema_and_its_check_agree() {
        let cases = [
            (json!({ "path": "player.gd" }), true),
            (json!({ "path": "res://levels/one.gd" }), true),
            (json!({ "path": "" }), false),
            (json!({ "path": 3 }), false),
            (json!({}), false),
        ];
        agree(EditorOperation::ScriptClose, &cases, |arguments| {
            script_tab(arguments).is_ok()
        });
    }

    #[test]
    fn the_dialog_schemas_and_their_checks_agree() {
        let cases = [
            (json!({}), true),
            (json!({ "limit": 1 }), true),
            (json!({ "limit": 64 }), true),
            (json!({ "limit": 0 }), false),
            (json!({ "limit": 65 }), false),
            (json!({ "limit": "2" }), false),
        ];
        agree(EditorOperation::DialogsGet, &cases, |arguments| {
            dialogs_read(arguments).is_ok()
        });
        let cases = [
            (json!({ "dialog": "77", "button": "ok" }), true),
            (json!({ "dialog": "1", "button": "discard" }), true),
            (
                json!({ "dialog": "1".repeat(20), "button": "x".repeat(256) }),
                true,
            ),
            (json!({ "dialog": "1".repeat(21), "button": "ok" }), false),
            (json!({ "dialog": "01", "button": "ok" }), false),
            (json!({ "dialog": "0", "button": "ok" }), false),
            (json!({ "dialog": "", "button": "ok" }), false),
            (json!({ "dialog": "1a", "button": "ok" }), false),
            (json!({ "dialog": 77, "button": "ok" }), false),
            (json!({ "dialog": "77", "button": "" }), false),
            (json!({ "dialog": "77", "button": "x".repeat(257) }), false),
            (json!({ "dialog": "77", "button": "a\u{7}" }), false),
            (json!({ "dialog": "77", "button": "a\u{85}" }), false),
            (json!({ "dialog": "77" }), false),
            (json!({ "button": "ok" }), false),
        ];
        agree(EditorOperation::DialogRespond, &cases, |arguments| {
            dialog_press(arguments).is_ok()
        });
    }

    #[test]
    fn the_restart_schema_and_its_check_agree() {
        let cases = [
            (json!({}), true),
            (json!({ "save": true }), true),
            (json!({ "save": false, "timeoutMs": 1000 }), true),
            (json!({ "timeoutMs": 600_000 }), true),
            (json!({ "timeoutMs": 999 }), false),
            (json!({ "timeoutMs": 600_001 }), false),
            (json!({ "save": "yes" }), false),
        ];
        agree(EditorOperation::Restart, &cases, |arguments| {
            restart_request(arguments).is_ok()
        });
    }

    #[test]
    fn the_performance_schema_and_its_check_agree() {
        let names = |count: usize| -> Vec<String> {
            (0..count).map(|index| format!("MONITOR_{index}")).collect()
        };
        let cases = [
            (json!({}), true),
            (json!({ "monitors": [] }), true),
            (json!({ "monitors": ["TIME_FPS"] }), true),
            (json!({ "monitors": names(256) }), true),
            (json!({ "monitors": names(257) }), false),
            (json!({ "monitors": [""] }), false),
            (json!({ "monitors": ["x".repeat(257)] }), false),
            (json!({ "monitors": [3] }), false),
            (json!({ "monitors": "TIME_FPS" }), false),
        ];
        agree(EditorOperation::PerformanceGet, &cases, |arguments| {
            performance_read(arguments).is_ok()
        });
    }

    /// The calls are scene.read's, checked by the same code.
    #[test]
    fn the_scene_read_schema_and_its_check_agree() {
        let cells = json!({ "node": "Layer", "method": "get_used_cells" });
        let calls = |count: usize| -> Vec<Value> { (0..count).map(|_| cells.clone()).collect() };
        let cases = [
            (json!({ "calls": [cells] }), true),
            (
                json!({ "scene": "res://level.tscn", "calls": calls(64) }),
                true,
            ),
            (
                json!({ "calls": [{ "node": "Player", "resource": { "library": "" },
                                    "method": "get_animation_list" }] }),
                true,
            ),
            (
                json!({ "calls": [{ "method": "get_point_position",
                                    "arguments": [{ "type": "integer", "value": 1 }] }] }),
                true,
            ),
            (json!({}), false),
            (json!({ "calls": [] }), false),
            (json!({ "calls": calls(65) }), false),
            (json!({ "calls": [{ "method": "set_cell" }] }), false),
            (json!({ "calls": [{ "node": "Layer" }] }), false),
            (
                json!({ "calls": [{ "method": "get_used_cells", "extra": 1 }] }),
                false,
            ),
            (
                json!({ "calls": [{ "method": "get_length",
                                    "resource": { "property": "a", "animation": "b" } }] }),
                false,
            ),
            (json!({ "scene": 3, "calls": [cells] }), false),
            (json!({ "calls": [cells], "path": "level.tscn" }), false),
        ];
        agree(EditorOperation::SceneRead, &cases, |arguments| {
            optional_scene(arguments)
                .and_then(|scene| crate::editor::scene_read::request(arguments, scene))
                .is_ok()
        });
    }

    /// Which path names a project file is the handler's to check.
    #[test]
    fn the_preview_schema_and_its_check_agree() {
        let cases = [
            (json!({ "path": "icon.svg" }), true),
            (json!({ "path": "res://box.tres", "timeoutMs": 100 }), true),
            (json!({ "path": "a.png", "timeoutMs": 60_000 }), true),
            (json!({ "path": "a.png", "timeoutMs": 99 }), false),
            (json!({ "path": "a.png", "timeoutMs": 60_001 }), false),
            (json!({ "path": "" }), false),
            (json!({}), false),
        ];
        agree(EditorOperation::ResourcePreview, &cases, |arguments| {
            preview_request(arguments).is_ok()
        });
    }
}
