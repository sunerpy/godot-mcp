//! Typed control and observation of a running game: property writes and
//! batch reads, waits, timed samples, node searches, navigation paths and
//! moves, button presses, and input recording and replay.
//!
//! Each tool's advertised schema and the check here give the same verdict on
//! the same arguments: every constraint the check enforces is one the schema
//! states, so a call the schema accepts is one the server sends. The probe
//! applies the semantic rules — whether a node or property exists, whether a
//! value converts without loss — and answers why it refuses one.

use std::time::Duration;

use godot_mcp_domain::contracts::OutcomeError;
use serde_json::{Map, Value, json};

use super::{SessionOperation, boolean, invalid};
use crate::engine_plane::authored::tagged_value;

/// The most targets one batch read names.
const MAX_BATCH_TARGETS: usize = 32;
/// The most properties a batch read names for one node.
const MAX_BATCH_PROPERTIES: usize = 64;
/// The most properties one write changes.
const MAX_SET_PROPERTIES: usize = 32;
/// The longest property name.
const MAX_NAME: usize = 256;
/// The longest wait.
const MAX_WAIT_MS: u64 = 30_000;
const DEFAULT_WAIT_MS: u64 = 5_000;
/// The most emissions a signal condition counts to.
const MAX_SIGNAL_COUNT: u64 = 1_000;
/// The longest text a UI condition or a press names.
const MAX_TEXT: usize = 1_024;
/// The longest sample, its widest interval and its most samples and series.
const MAX_SAMPLE_MS: u64 = 10_000;
const DEFAULT_SAMPLE_MS: u64 = 1_000;
const MAX_SAMPLE_INTERVAL_MS: u64 = 1_000;
const MAX_SAMPLES: u64 = 600;
const MAX_SERIES: usize = 32;
/// The most nodes a search answers.
const MAX_FOUND: u64 = 500;
/// The longest a path query waits for its map to synchronize.
const MAX_SYNC_MS: u64 = 5_000;
const DEFAULT_SYNC_MS: u64 = 1_000;
/// The longest walk, and its default.
const MAX_MOVE_MS: u64 = 30_000;
const DEFAULT_MOVE_MS: u64 = 10_000;
/// The fastest walk, in units per second.
const MAX_SPEED: f64 = 100_000.0;
/// The largest world coordinate a point names.
const MAX_COORDINATE: f64 = 1.0e9;
/// The longest a press holds the button down.
const MAX_PRESS_HOLD_MS: u64 = 5_000;
/// The most events a recording keeps or a replay sends, and the longest
/// either lasts.
const MAX_EVENTS: usize = 3_000;
const MAX_INPUT_MS: u64 = 30_000;
/// The largest window coordinate a replayed mouse event names.
const MAX_WINDOW_COORDINATE: f64 = 1.0e6;
/// The navigation layer mask: 32 layers.
const MAX_LAYERS: u64 = 4_294_967_295;

/// The singleton a write, a batch read or a sample may name, and its
/// properties with the tagged types each takes and its range.
const ENGINE: &str = "Engine";
const ENGINE_PROPERTIES: &[(&str, bool, f64, f64)] = &[
    ("time_scale", false, 0.0, 100.0),
    ("physics_ticks_per_second", true, 1.0, 1000.0),
    ("max_fps", true, 0.0, 1000.0),
];

/// Every comparison a property condition makes.
const COMPARISONS: &[&str] = &["==", "!=", "<", "<=", ">", ">=", "contains", "approx"];

/// The mouse buttons a replayed event names.
const MOUSE_BUTTONS: &[&str] = &[
    "left",
    "right",
    "middle",
    "wheel_up",
    "wheel_down",
    "wheel_left",
    "wheel_right",
    "xbutton1",
    "xbutton2",
];

/// Every replayable event kind, the fields it declares beside `t_ms`, `kind`,
/// `source` and `actions`, and the fields it requires. The schema and
/// [`event`] both read it.
const EVENT_KINDS: &[(&str, &[&str], &[&str])] = &[
    (
        "key",
        &["key", "physical", "unicode", "pressed", "echo", "modifiers"],
        &[],
    ),
    (
        "mouse_button",
        &["button", "position", "pressed", "double_click"],
        &["position"],
    ),
    (
        "mouse_motion",
        &["position", "relative", "button_mask"],
        &["position"],
    ),
    (
        "joypad_button",
        &["device", "button", "pressed"],
        &["button"],
    ),
    (
        "joypad_motion",
        &["device", "axis", "value"],
        &["axis", "value"],
    ),
    ("action", &["action", "pressed", "strength"], &["action"]),
    ("other", &["class"], &[]),
];

/// The fields every recorded event carries: its time, its kind, and what the
/// recording observed about it, which a replay does not read.
const EVENT_COMMON: &[&str] = &["t_ms", "kind", "source", "actions"];

/// Whether `operation` is one of this module's tools.
pub(super) const fn handles(operation: SessionOperation) -> bool {
    matches!(
        operation,
        SessionOperation::PropertiesBatchGet
            | SessionOperation::NodesFind
            | SessionOperation::NavigationPathGet
            | SessionOperation::Wait
            | SessionOperation::Sample
            | SessionOperation::InputRecordingStart
            | SessionOperation::InputRecordingStop
            | SessionOperation::InputReplay
            | SessionOperation::UiPress
            | SessionOperation::PropertiesSet
            | SessionOperation::NodeMove
    )
}

/// Whether `operation`'s schema refers to the tagged value definitions.
pub(super) const fn uses_values(operation: SessionOperation) -> bool {
    matches!(
        operation,
        SessionOperation::PropertiesSet | SessionOperation::Wait
    )
}

// --- schemas -------------------------------------------------------------------

fn name_schema(description: &str) -> Value {
    json!({
        "type": "string",
        "minLength": 1,
        "maxLength": MAX_NAME,
        "description": description
    })
}

fn point_schema() -> Value {
    let coordinate = json!({
        "type": "number",
        "minimum": -MAX_COORDINATE,
        "maximum": MAX_COORDINATE
    });
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": { "x": coordinate, "y": coordinate, "z": coordinate },
        "required": ["x", "y"],
        "description": "A global position: {x, y} for a 2D node, {x, y, z} for a 3D one."
    })
}

fn window_point_schema() -> Value {
    let coordinate = json!({
        "type": "number",
        "minimum": -MAX_WINDOW_COORDINATE,
        "maximum": MAX_WINDOW_COORDINATE
    });
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": { "x": coordinate, "y": coordinate },
        "required": ["x", "y"]
    })
}

fn engine_property_schema() -> Value {
    json!({
        "type": "string",
        "enum": ENGINE_PROPERTIES.iter().map(|(name, ..)| *name).collect::<Vec<_>>()
    })
}

fn engine_schema() -> Value {
    json!({ "type": "string", "enum": [ENGINE] })
}

fn layers_schema() -> Value {
    json!({
        "type": "integer",
        "minimum": 1,
        "maximum": MAX_LAYERS,
        "description": "The navigation layers to path over, as a bit mask (default: the \
                        NavigationAgent's, or layer 1)."
    })
}

fn optimize_schema() -> Value {
    json!({
        "type": "boolean",
        "description": "Smooth the path through polygon corners (default true)."
    })
}

fn sync_schema() -> Value {
    json!({
        "type": "integer",
        "minimum": 1,
        "maximum": MAX_SYNC_MS,
        "description": "How long to wait for the navigation map to synchronize its regions, \
                        in ms (default 1000)."
    })
}

/// One tagged value written to an Engine property: a number in its range,
/// tagged `float` or `integer`; a whole-number property takes whole values.
fn engine_value_schema(whole: bool, low: f64, high: f64) -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "type": { "type": "string", "enum": ["float", "integer"] },
            "value": { "type": if whole { "integer" } else { "number" }, "minimum": low, "maximum": high }
        },
        "required": ["type", "value"]
    })
}

fn set_target_schema(node: &Value) -> Value {
    let mut engine = Map::new();
    for (name, whole, low, high) in ENGINE_PROPERTIES {
        engine.insert((*name).to_owned(), engine_value_schema(*whole, *low, *high));
    }
    closed_union(
        vec![
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "node": node,
                    "properties": {
                        "type": "object",
                        "minProperties": 1,
                        "maxProperties": MAX_SET_PROPERTIES,
                        "propertyNames": { "minLength": 1, "maxLength": MAX_NAME },
                        "additionalProperties": crate::schema::writable_value()
                    }
                },
                "required": ["node", "properties"]
            }),
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "singleton": engine_schema(),
                    "properties": {
                        "type": "object",
                        "minProperties": 1,
                        "additionalProperties": false,
                        "properties": engine
                    }
                },
                "required": ["singleton", "properties"]
            }),
        ],
        "What to write: {node, properties} maps property names of a running node to tagged \
         values; {singleton: \"Engine\", properties} writes time_scale (0 to 100), \
         physics_ticks_per_second (1 to 1000) or max_fps (0 to 1000, 0 is unlimited).",
    )
}

/// An object that is exactly one of `variants`, each itself a closed object.
/// The fields of every variant are declared at the top as well, so the object
/// names its fields and refuses any other wherever a client reads it, and each
/// variant then says which of them it takes.
pub(super) fn closed_union(variants: Vec<Value>, description: &str) -> Value {
    let mut fields = Map::new();
    for variant in &variants {
        if let Some(Value::Object(declared)) = variant.get("properties") {
            for (name, field) in declared {
                fields.entry(name.clone()).or_insert_with(|| field.clone());
            }
        }
    }
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": fields,
        "oneOf": variants,
        "description": description
    })
}

fn read_targets_schema(node: &Value) -> Value {
    json!({
        "type": "array",
        "minItems": 1,
        "maxItems": MAX_BATCH_TARGETS,
        "items": {
            "oneOf": [
                {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "node": node,
                        "properties": {
                            "type": "array",
                            "minItems": 1,
                            "maxItems": MAX_BATCH_PROPERTIES,
                            "items": { "type": "string", "minLength": 1, "maxLength": MAX_NAME }
                        }
                    },
                    "required": ["node"]
                },
                {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "singleton": engine_schema(),
                        "properties": {
                            "type": "array",
                            "minItems": 1,
                            "maxItems": ENGINE_PROPERTIES.len(),
                            "items": engine_property_schema()
                        }
                    },
                    "required": ["singleton"]
                }
            ]
        },
        "description": "Up to 32 targets: {node, properties?} reads a running node's named \
                        properties (its stored and editor-visible ones when omitted, at most \
                        64); {singleton: \"Engine\", properties?} reads time_scale, \
                        physics_ticks_per_second and max_fps."
    })
}

fn series_schema(node: &Value) -> Value {
    json!({
        "type": "array",
        "minItems": 1,
        "maxItems": MAX_SERIES,
        "items": {
            "oneOf": [
                {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "node": node,
                        "property": { "type": "string", "minLength": 1, "maxLength": MAX_NAME }
                    },
                    "required": ["node", "property"]
                },
                {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "singleton": engine_schema(),
                        "property": engine_property_schema()
                    },
                    "required": ["singleton", "property"]
                },
                {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "monitor": { "type": "string", "minLength": 1, "maxLength": MAX_NAME }
                    },
                    "required": ["monitor"]
                }
            ]
        },
        "description": "Up to 32 series, answered in this order: {node, property} of a running \
                        node, {singleton: \"Engine\", property}, or {monitor} by the name \
                        runtime.performance.get answers."
    })
}

fn condition_schema(node: &Value) -> Value {
    let only_node = |description: &str| {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": { "node": node },
            "required": ["node"],
            "description": description
        })
    };
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "node_exists": only_node("Holds once the node is in the running game."),
            "node_gone": only_node("Holds once the node is no longer in the running game."),
            "property": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "node": node,
                    "property": { "type": "string", "minLength": 1, "maxLength": MAX_NAME },
                    "op": { "type": "string", "enum": COMPARISONS },
                    "value": crate::schema::writable_value(),
                    "tolerance": { "type": "number", "minimum": 0, "maximum": MAX_COORDINATE }
                },
                "required": ["node", "property", "op", "value"],
                "description": "Holds once the node's property compares with `value` by `op`; \
                                approx allows `tolerance` (default 0), contains asks a string, \
                                array or dictionary property for `value`."
            },
            "signal": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "node": node,
                    "signal": { "type": "string", "minLength": 1, "maxLength": MAX_NAME },
                    "count": { "type": "integer", "minimum": 1, "maximum": MAX_SIGNAL_COUNT }
                },
                "required": ["node", "signal"],
                "description": "Holds once the node emitted `signal` `count` times (default 1) \
                                after the wait began."
            },
            "ui_text": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "text": { "type": "string", "minLength": 1, "maxLength": MAX_TEXT },
                    "match": { "type": "string", "enum": ["exact", "contains"] }
                },
                "required": ["text"],
                "description": "Holds once a visible Control shows `text` (`match` exact by \
                                default, or contains)."
            }
        },
        "minProperties": 1,
        "maxProperties": 1,
        "description": "One condition, keyed by its kind: node_exists, node_gone, property, \
                        signal or ui_text."
    })
}

fn near_schema(node: &Value) -> Value {
    let radius = json!({ "type": "number", "minimum": 0, "maximum": MAX_COORDINATE });
    closed_union(
        vec![
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": { "point": point_schema(), "radius": radius },
                "required": ["point", "radius"]
            }),
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": { "node": node, "radius": radius },
                "required": ["node", "radius"]
            }),
        ],
        "Only nodes within `radius` of a `point` or of another Node2D or Node3D `node`, of the \
         same dimension, nearest first.",
    )
}

fn mode_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "teleport": {
                "type": "object",
                "additionalProperties": false,
                "properties": {},
                "description": "Set the node's global position to `to` at once."
            },
            "along_path": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "speed": {
                        "type": "number",
                        "exclusiveMinimum": 0,
                        "maximum": MAX_SPEED,
                        "description": "Units per second; the node advances speed times the \
                                        physics step each physics frame."
                    },
                    "timeoutMs": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_MOVE_MS,
                        "description": "Stop walking after this many ms (default 10000)."
                    },
                    "layers": layers_schema(),
                    "optimize": optimize_schema(),
                    "syncTimeoutMs": sync_schema()
                },
                "required": ["speed"],
                "description": "Walk the node to `to` along its navigation map's path."
            }
        },
        "minProperties": 1,
        "maxProperties": 1,
        "description": "How to move, keyed by its kind: teleport or along_path."
    })
}

fn press_target_schema(node: &Value) -> Value {
    closed_union(
        vec![
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": { "node": node },
                "required": ["node"]
            }),
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "text": { "type": "string", "minLength": 1, "maxLength": MAX_TEXT },
                    "match": { "type": "string", "enum": ["exact", "contains"] }
                },
                "required": ["text"]
            }),
        ],
        "The button: {node} by its path, or {text, match?} by the text it shows (exact by \
         default, or contains); exactly one visible button may match.",
    )
}

fn event_field_schema(kind: &str, field: &str) -> Value {
    let flag = json!({ "type": "boolean" });
    match (kind, field) {
        (_, "t_ms") => json!({ "type": "integer", "minimum": 0, "maximum": MAX_INPUT_MS }),
        (_, "kind") => json!({ "const": kind }),
        (_, "source") => json!({ "type": "string", "enum": ["probe", "external"] }),
        (_, "actions") => json!({
            "type": "array",
            "maxItems": 8,
            "items": { "type": "string", "minLength": 1, "maxLength": MAX_NAME }
        }),
        (_, "key" | "physical") => json!({ "type": "string", "minLength": 1, "maxLength": 64 }),
        (_, "unicode") => json!({ "type": "integer", "minimum": 0, "maximum": 1_114_111 }),
        (_, "pressed" | "echo" | "double_click") => flag,
        (_, "modifiers") => json!({
            "type": "object",
            "additionalProperties": false,
            "properties": { "shift": flag, "ctrl": flag, "alt": flag, "meta": flag }
        }),
        ("mouse_button", "button") => json!({
            "oneOf": [
                { "type": "string", "enum": MOUSE_BUTTONS },
                { "type": "integer", "minimum": 1, "maximum": 9 }
            ]
        }),
        (_, "position" | "relative") => window_point_schema(),
        (_, "button_mask") => json!({ "type": "integer", "minimum": 0, "maximum": 511 }),
        (_, "device") => json!({ "type": "integer", "minimum": 0, "maximum": 255 }),
        (_, "button") => json!({ "type": "integer", "minimum": 0, "maximum": 127 }),
        (_, "axis") => json!({ "type": "integer", "minimum": 0, "maximum": 9 }),
        (_, "value") => json!({ "type": "number", "minimum": -1, "maximum": 1 }),
        (_, "action" | "class") => {
            json!({ "type": "string", "minLength": 1, "maxLength": MAX_NAME })
        }
        (_, "strength") => json!({ "type": "number", "minimum": 0, "maximum": 1 }),
        (kind, field) => unreachable!("{kind} declares no field {field}"),
    }
}

fn events_schema() -> Value {
    let variants = EVENT_KINDS
        .iter()
        .map(|(kind, fields, required)| {
            let mut properties = Map::new();
            for field in EVENT_COMMON.iter().chain(fields.iter()) {
                properties.insert((*field).to_owned(), event_field_schema(kind, field));
            }
            let mut needed = vec![json!("t_ms"), json!("kind")];
            needed.extend(required.iter().map(|field| json!(field)));
            let mut variant = json!({
                "type": "object",
                "additionalProperties": false,
                "properties": properties,
                "required": needed
            });
            if *kind == "key" {
                variant["anyOf"] = json!([{ "required": ["key"] }, { "required": ["physical"] }]);
            }
            variant
        })
        .collect();
    json!({
        "type": "array",
        "minItems": 1,
        "maxItems": MAX_EVENTS,
        "items": crate::schema::one_of(variants),
        "description": "Up to 3000 events, as input.recording.stop answers them: each its \
                        `t_ms` from the replay's start (at most 30000; delivered in time \
                        order), its `kind` and that kind's fields. `source` and `actions` are \
                        read by no one; `other` events are skipped. A key names `key` or \
                        `physical`."
    })
}

/// The schema properties of one of this module's tools and the ones it
/// requires, beside the idempotency id a command also requires.
pub(super) fn properties(
    operation: SessionOperation,
    session: &Value,
    node: &Value,
) -> (Value, &'static [&'static str]) {
    match operation {
        SessionOperation::PropertiesBatchGet => (
            json!({ "session": session, "targets": read_targets_schema(node) }),
            &["session", "targets"],
        ),
        SessionOperation::PropertiesSet => (
            json!({ "session": session, "target": set_target_schema(node) }),
            &["session", "target"],
        ),
        SessionOperation::Wait => (
            json!({
                "session": session,
                "condition": condition_schema(node),
                "timeoutMs": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_WAIT_MS,
                    "description": "How long to wait, in ms (default 5000)."
                }
            }),
            &["session", "condition"],
        ),
        SessionOperation::Sample => (
            json!({
                "session": session,
                "series": series_schema(node),
                "durationMs": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_SAMPLE_MS,
                    "description": "How long to sample, in ms (default 1000)."
                },
                "intervalMs": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_SAMPLE_INTERVAL_MS,
                    "description": "The least time between two samples, in ms (default 100); \
                                    a sample is taken at the first frame after it."
                },
                "maxSamples": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_SAMPLES,
                    "description": "Stop after this many samples (default 600)."
                }
            }),
            &["session", "series"],
        ),
        SessionOperation::NodesFind => (
            json!({
                "session": session,
                "root": node,
                "class": name_schema(
                    "Only nodes of this engine class or script class (class_name), or one \
                     inheriting it."
                ),
                "script": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 1024,
                    "description": "Only nodes whose script is this project script \
                                    (project-relative or res://) or extends it."
                },
                "group": name_schema("Only nodes in this group."),
                "name": name_schema(
                    "Only nodes whose name matches this glob: * matches any run of \
                     characters, ? one character."
                ),
                "near": near_schema(node),
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_FOUND,
                    "description": "The most nodes to answer (default 100)."
                }
            }),
            &["session"],
        ),
        SessionOperation::NavigationPathGet => (
            json!({
                "session": session,
                "node": node,
                "to": point_schema(),
                "from": point_schema(),
                "layers": layers_schema(),
                "optimize": optimize_schema(),
                "syncTimeoutMs": sync_schema()
            }),
            &["session", "node", "to"],
        ),
        SessionOperation::NodeMove => (
            json!({
                "session": session,
                "node": node,
                "to": point_schema(),
                "mode": mode_schema()
            }),
            &["session", "node", "to", "mode"],
        ),
        SessionOperation::UiPress => (
            json!({
                "session": session,
                "target": press_target_schema(node),
                "holdMs": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_PRESS_HOLD_MS,
                    "description": "How long the button is held down, in ms (default 100)."
                }
            }),
            &["session", "target"],
        ),
        SessionOperation::InputRecordingStart => (
            json!({
                "session": session,
                "maxEvents": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_EVENTS,
                    "description": "Keep at most this many events (default 3000); the \
                                    recording ends at the next one."
                },
                "maxDurationMs": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_INPUT_MS,
                    "description": "Record for at most this many ms (default 30000)."
                }
            }),
            &["session"],
        ),
        SessionOperation::InputRecordingStop => (json!({ "session": session }), &["session"]),
        SessionOperation::InputReplay => (
            json!({ "session": session, "events": events_schema() }),
            &["session", "events"],
        ),
        other => unreachable!("{} is not a runtime control tool", other.tool_name()),
    }
}

// --- checks ----------------------------------------------------------------------

/// The arguments `operation` accepts, and nothing else.
fn only(
    arguments: &Map<String, Value>,
    allowed: &[&str],
    context: &str,
) -> Result<(), OutcomeError> {
    match arguments
        .keys()
        .find(|key| !allowed.contains(&key.as_str()))
    {
        Some(unknown) => Err(invalid(format!("{context} does not accept `{unknown}`"))),
        None => Ok(()),
    }
}

fn object<'a>(value: &'a Value, context: &str) -> Result<&'a Map<String, Value>, OutcomeError> {
    value
        .as_object()
        .ok_or_else(|| invalid(format!("{context} is an object")))
}

fn text(value: Option<&Value>, low: usize, high: usize) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .filter(|text| (low..=high).contains(&text.chars().count()))
}

fn bounded_text<'a>(
    arguments: &'a Map<String, Value>,
    key: &str,
    high: usize,
    context: &str,
) -> Result<Option<&'a str>, OutcomeError> {
    match arguments.get(key) {
        None => Ok(None),
        value => text(value, 1, high).map(Some).ok_or_else(|| {
            invalid(format!(
                "{context}: `{key}` is a string of 1 to {high} characters"
            ))
        }),
    }
}

fn node_path<'a>(value: Option<&'a Value>, context: &str) -> Result<&'a str, OutcomeError> {
    text(value, 1, 1024).ok_or_else(|| {
        invalid(format!(
            "{context}: `node` is a node path of 1 to 1024 characters"
        ))
    })
}

fn number(
    arguments: &Map<String, Value>,
    key: &str,
    low: f64,
    high: f64,
) -> Result<Option<f64>, OutcomeError> {
    match arguments.get(key) {
        None => Ok(None),
        Some(value) => value
            .as_f64()
            .filter(|value| (low..=high).contains(value))
            .map(Some)
            .ok_or_else(|| invalid(format!("`{key}` is a number from {low} to {high}"))),
    }
}

fn whole(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        value
            .as_f64()
            .filter(|value| value.fract() == 0.0 && *value >= 0.0 && *value <= 9.0e15)
            .map(|value| value as u64)
    })
}

fn bounded_integer(
    arguments: &Map<String, Value>,
    key: &str,
    low: u64,
    high: u64,
) -> Result<Option<u64>, OutcomeError> {
    match arguments.get(key) {
        None => Ok(None),
        Some(value) => whole(value)
            .filter(|value| (low..=high).contains(value))
            .map(Some)
            .ok_or_else(|| invalid(format!("`{key}` must be an integer from {low} to {high}"))),
    }
}

/// A point the schema's `point_schema` accepts, spelled for the probe.
fn point(value: &Value, key: &str) -> Result<Value, OutcomeError> {
    let invalid_point = || {
        invalid(format!(
            "`{key}` is {{x, y}} or {{x, y, z}}, each within ±{MAX_COORDINATE}"
        ))
    };
    let point = value.as_object().ok_or_else(invalid_point)?;
    if !point.contains_key("x")
        || !point.contains_key("y")
        || point.iter().any(|(axis, coordinate)| {
            !matches!(axis.as_str(), "x" | "y" | "z")
                || !coordinate
                    .as_f64()
                    .is_some_and(|coordinate| coordinate.abs() <= MAX_COORDINATE)
        })
    {
        return Err(invalid_point());
    }
    Ok(value.clone())
}

fn engine_property(value: Option<&Value>, context: &str) -> Result<String, OutcomeError> {
    value
        .and_then(Value::as_str)
        .filter(|name| ENGINE_PROPERTIES.iter().any(|(known, ..)| known == name))
        .map(str::to_owned)
        .ok_or_else(|| {
            invalid(format!(
                "{context}: the Engine's properties here are time_scale, \
                 physics_ticks_per_second and max_fps"
            ))
        })
}

fn singleton(value: Option<&Value>, context: &str) -> Result<(), OutcomeError> {
    if value.and_then(Value::as_str) == Some(ENGINE) {
        Ok(())
    } else {
        Err(invalid(format!(
            "{context}: the only singleton here is Engine"
        )))
    }
}

/// The names of a `properties` list, each 1 to 256 characters.
fn names(value: &Value, high: usize, context: &str) -> Result<Vec<Value>, OutcomeError> {
    let list = value
        .as_array()
        .filter(|list| !list.is_empty() && list.len() <= high)
        .ok_or_else(|| invalid(format!("{context}: `properties` lists 1 to {high} names")))?;
    for name in list {
        text(Some(name), 1, MAX_NAME).ok_or_else(|| {
            invalid(format!(
                "{context}: a property name has 1 to {MAX_NAME} characters"
            ))
        })?;
    }
    Ok(list.clone())
}

fn batch_target(value: &Value, index: usize) -> Result<Value, OutcomeError> {
    let context = format!("target {index}");
    let target = object(value, &context)?;
    let mut out = Map::new();
    if target.contains_key("singleton") {
        only(target, &["singleton", "properties"], &context)?;
        singleton(target.get("singleton"), &context)?;
        out.insert("singleton".to_owned(), json!(ENGINE));
        if let Some(listed) = target.get("properties") {
            let listed = listed
                .as_array()
                .filter(|listed| !listed.is_empty() && listed.len() <= ENGINE_PROPERTIES.len())
                .ok_or_else(|| invalid(format!("{context}: `properties` lists 1 to 3 names")))?;
            let mut checked = Vec::with_capacity(listed.len());
            for name in listed {
                checked.push(Value::String(engine_property(Some(name), &context)?));
            }
            out.insert("properties".to_owned(), Value::Array(checked));
        }
    } else {
        only(target, &["node", "properties"], &context)?;
        out.insert(
            "node".to_owned(),
            json!(node_path(target.get("node"), &context)?),
        );
        if let Some(listed) = target.get("properties") {
            out.insert(
                "properties".to_owned(),
                Value::Array(names(listed, MAX_BATCH_PROPERTIES, &context)?),
            );
        }
    }
    Ok(Value::Object(out))
}

fn set_target(arguments: &Map<String, Value>) -> Result<Map<String, Value>, OutcomeError> {
    let target = object(
        arguments
            .get("target")
            .ok_or_else(|| invalid("`target` is required"))?,
        "`target`",
    )?;
    let mut out = Map::new();
    let written = target
        .get("properties")
        .and_then(Value::as_object)
        .filter(|written| !written.is_empty())
        .ok_or_else(|| invalid("`target.properties` maps at least one property to a value"))?;
    if target.contains_key("singleton") {
        only(target, &["singleton", "properties"], "`target`")?;
        singleton(target.get("singleton"), "`target`")?;
        out.insert("singleton".to_owned(), json!(ENGINE));
        for (name, value) in written {
            let (_, whole_only, low, high) = ENGINE_PROPERTIES
                .iter()
                .find(|(known, ..)| known == name)
                .ok_or_else(|| {
                    invalid(format!(
                        "Engine.{name} is not writable here; only time_scale, \
                         physics_ticks_per_second and max_fps are"
                    ))
                })?;
            let tagged = value
                .as_object()
                .filter(|tagged| tagged.len() == 2)
                .ok_or_else(|| invalid(format!("Engine.{name} takes a tagged number")))?;
            let kind = tagged.get("type").and_then(Value::as_str);
            let number = tagged.get("value").and_then(Value::as_f64);
            let fits = matches!(kind, Some("float" | "integer"))
                && number.is_some_and(|number| {
                    (*low..=*high).contains(&number) && (!whole_only || number.fract() == 0.0)
                });
            if !fits {
                return Err(invalid(format!(
                    "Engine.{name} takes a {} from {low} to {high}, tagged float or integer",
                    if *whole_only {
                        "whole number"
                    } else {
                        "number"
                    }
                )));
            }
        }
    } else {
        only(target, &["node", "properties"], "`target`")?;
        out.insert(
            "node".to_owned(),
            json!(node_path(target.get("node"), "`target`")?),
        );
        if written.len() > MAX_SET_PROPERTIES {
            return Err(invalid(format!(
                "`target.properties` writes at most {MAX_SET_PROPERTIES} properties"
            )));
        }
        for (name, value) in written {
            if text(Some(&Value::String(name.clone())), 1, MAX_NAME).is_none() {
                return Err(invalid(format!(
                    "a property name has 1 to {MAX_NAME} characters"
                )));
            }
            tagged_value(value, &format!("`target.properties.{name}`"))?;
        }
    }
    out.insert("properties".to_owned(), Value::Object(written.clone()));
    Ok(out)
}

fn condition(arguments: &Map<String, Value>) -> Result<Value, OutcomeError> {
    let keyed = arguments
        .get("condition")
        .and_then(Value::as_object)
        .filter(|keyed| keyed.len() == 1)
        .ok_or_else(|| invalid("`condition` is an object with one member, keyed by its kind"))?;
    let (kind, fields) = keyed.iter().next().expect("one member");
    let context = format!("a {kind} condition");
    let fields = object(fields, &context)?;
    match kind.as_str() {
        "node_exists" | "node_gone" => {
            only(fields, &["node"], &context)?;
            node_path(fields.get("node"), &context)?;
        }
        "property" => {
            only(
                fields,
                &["node", "property", "op", "value", "tolerance"],
                &context,
            )?;
            node_path(fields.get("node"), &context)?;
            text(fields.get("property"), 1, MAX_NAME)
                .ok_or_else(|| invalid(format!("{context}: `property` names a property")))?;
            fields
                .get("op")
                .and_then(Value::as_str)
                .filter(|op| COMPARISONS.contains(op))
                .ok_or_else(|| {
                    invalid(format!(
                        "{context}: `op` is one of {}",
                        COMPARISONS.join(", ")
                    ))
                })?;
            tagged_value(
                fields
                    .get("value")
                    .ok_or_else(|| invalid(format!("{context} needs `value`")))?,
                &format!("{context}: `value`"),
            )?;
            number(fields, "tolerance", 0.0, MAX_COORDINATE)?;
        }
        "signal" => {
            only(fields, &["node", "signal", "count"], &context)?;
            node_path(fields.get("node"), &context)?;
            text(fields.get("signal"), 1, MAX_NAME)
                .ok_or_else(|| invalid(format!("{context}: `signal` names a signal")))?;
            bounded_integer(fields, "count", 1, MAX_SIGNAL_COUNT)?;
        }
        "ui_text" => {
            only(fields, &["text", "match"], &context)?;
            text(fields.get("text"), 1, MAX_TEXT).ok_or_else(|| {
                invalid(format!("{context}: `text` has 1 to {MAX_TEXT} characters"))
            })?;
            match_mode(fields, &context)?;
        }
        other => {
            return Err(invalid(format!(
                "{other} is not a condition kind: node_exists, node_gone, property, signal or \
                 ui_text"
            )));
        }
    }
    Ok(Value::Object(keyed.clone()))
}

fn match_mode(fields: &Map<String, Value>, context: &str) -> Result<(), OutcomeError> {
    match fields.get("match") {
        None => Ok(()),
        Some(Value::String(mode)) if mode == "exact" || mode == "contains" => Ok(()),
        Some(_) => Err(invalid(format!("{context}: `match` is exact or contains"))),
    }
}

fn series(arguments: &Map<String, Value>) -> Result<Vec<Value>, OutcomeError> {
    let listed = arguments
        .get("series")
        .and_then(Value::as_array)
        .filter(|listed| !listed.is_empty() && listed.len() <= MAX_SERIES)
        .ok_or_else(|| invalid(format!("`series` lists 1 to {MAX_SERIES} series")))?;
    let mut out = Vec::with_capacity(listed.len());
    for (index, item) in listed.iter().enumerate() {
        let context = format!("series {index}");
        let item = object(item, &context)?;
        if item.contains_key("monitor") {
            only(item, &["monitor"], &context)?;
            text(item.get("monitor"), 1, MAX_NAME)
                .ok_or_else(|| invalid(format!("{context}: `monitor` names a monitor")))?;
        } else if item.contains_key("singleton") {
            only(item, &["singleton", "property"], &context)?;
            singleton(item.get("singleton"), &context)?;
            engine_property(item.get("property"), &context)?;
        } else {
            only(item, &["node", "property"], &context)?;
            node_path(item.get("node"), &context)?;
            text(item.get("property"), 1, MAX_NAME)
                .ok_or_else(|| invalid(format!("{context}: `property` names a property")))?;
        }
        out.push(Value::Object(item.clone()));
    }
    Ok(out)
}

fn near(value: &Value) -> Result<Value, OutcomeError> {
    let near = object(value, "`near`")?;
    let mut out = Map::new();
    if near.contains_key("point") {
        only(near, &["point", "radius"], "`near`")?;
        out.insert("point".to_owned(), point(&near["point"], "near.point")?);
    } else {
        only(near, &["node", "radius"], "`near`")?;
        out.insert(
            "node".to_owned(),
            json!(node_path(near.get("node"), "`near`")?),
        );
    }
    let radius = number(near, "radius", 0.0, MAX_COORDINATE)?
        .ok_or_else(|| invalid("`near` needs `radius`"))?;
    out.insert("radius".to_owned(), json!(radius));
    Ok(Value::Object(out))
}

fn navigation_options(
    fields: &Map<String, Value>,
    out: &mut Map<String, Value>,
) -> Result<u64, OutcomeError> {
    if let Some(layers) = bounded_integer(fields, "layers", 1, MAX_LAYERS)? {
        out.insert("layers".to_owned(), json!(layers));
    }
    if let Some(optimize) = boolean(fields, "optimize")? {
        out.insert("optimize".to_owned(), json!(optimize));
    }
    let sync = bounded_integer(fields, "syncTimeoutMs", 1, MAX_SYNC_MS)?.unwrap_or(DEFAULT_SYNC_MS);
    out.insert("sync_timeout_ms".to_owned(), json!(sync));
    Ok(sync)
}

fn press_target(arguments: &Map<String, Value>) -> Result<Map<String, Value>, OutcomeError> {
    let target = object(
        arguments
            .get("target")
            .ok_or_else(|| invalid("`target` is required"))?,
        "`target`",
    )?;
    let mut out = Map::new();
    if target.contains_key("node") {
        only(target, &["node"], "`target`")?;
        out.insert(
            "node".to_owned(),
            json!(node_path(target.get("node"), "`target`")?),
        );
    } else {
        only(target, &["text", "match"], "`target`")?;
        let shown = text(target.get("text"), 1, MAX_TEXT).ok_or_else(|| {
            invalid(format!(
                "`target` names a button by `node` or by its `text` of 1 to {MAX_TEXT} characters"
            ))
        })?;
        match_mode(target, "`target`")?;
        out.insert("text".to_owned(), json!(shown));
        if let Some(mode) = target.get("match") {
            out.insert("match".to_owned(), mode.clone());
        }
    }
    Ok(out)
}

/// One replayed event the schema's variant for its kind accepts.
fn event(value: &Value, index: usize) -> Result<(u64, Value), OutcomeError> {
    let context = format!("event {index}");
    let entry = object(value, &context)?;
    let kind = entry
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("{context} names its `kind`")))?;
    let Some((_, fields, required)) = EVENT_KINDS.iter().find(|(name, ..)| *name == kind) else {
        return Err(invalid(format!(
            "{context}: {kind} is not an input event kind"
        )));
    };
    if let Some(unknown) = entry
        .keys()
        .find(|key| !EVENT_COMMON.contains(&key.as_str()) && !fields.contains(&key.as_str()))
    {
        return Err(invalid(format!(
            "{context}: a {kind} event does not declare `{unknown}`"
        )));
    }
    if let Some(missing) = required.iter().find(|field| !entry.contains_key(**field)) {
        return Err(invalid(format!(
            "{context}: a {kind} event needs `{missing}`"
        )));
    }
    let at = bounded_integer(entry, "t_ms", 0, MAX_INPUT_MS)?
        .ok_or_else(|| invalid(format!("{context} needs `t_ms`")))?;
    if entry
        .get("source")
        .is_some_and(|source| !matches!(source.as_str(), Some("probe" | "external")))
    {
        return Err(invalid(format!("{context}: `source` is probe or external")));
    }
    if let Some(actions) = entry.get("actions") {
        let valid = actions.as_array().is_some_and(|actions| {
            actions.len() <= 8
                && actions
                    .iter()
                    .all(|action| text(Some(action), 1, MAX_NAME).is_some())
        });
        if !valid {
            return Err(invalid(format!(
                "{context}: `actions` lists at most 8 action names"
            )));
        }
    }
    for field in *fields {
        let Some(value) = entry.get(*field) else {
            continue;
        };
        let fits = match (kind, *field) {
            (_, "key" | "physical") => text(Some(value), 1, 64).is_some(),
            (_, "unicode") => whole(value).is_some_and(|code| code <= 1_114_111),
            (_, "pressed" | "echo" | "double_click") => value.is_boolean(),
            (_, "modifiers") => value.as_object().is_some_and(|modifiers| {
                modifiers.iter().all(|(name, held)| {
                    matches!(name.as_str(), "shift" | "ctrl" | "alt" | "meta") && held.is_boolean()
                })
            }),
            ("mouse_button", "button") => {
                value
                    .as_str()
                    .is_some_and(|name| MOUSE_BUTTONS.contains(&name))
                    || whole(value).is_some_and(|index| (1..=9).contains(&index))
            }
            (_, "position" | "relative") => value.as_object().is_some_and(|point| {
                point.len() == 2
                    && ["x", "y"].iter().all(|axis| {
                        point
                            .get(*axis)
                            .and_then(Value::as_f64)
                            .is_some_and(|coordinate| coordinate.abs() <= MAX_WINDOW_COORDINATE)
                    })
            }),
            (_, "button_mask") => whole(value).is_some_and(|mask| mask <= 511),
            (_, "device") => whole(value).is_some_and(|device| device <= 255),
            (_, "button") => whole(value).is_some_and(|button| button <= 127),
            (_, "axis") => whole(value).is_some_and(|axis| axis <= 9),
            (_, "value") => value
                .as_f64()
                .is_some_and(|value| (-1.0..=1.0).contains(&value)),
            (_, "action" | "class") => text(Some(value), 1, MAX_NAME).is_some(),
            (_, "strength") => value
                .as_f64()
                .is_some_and(|value| (0.0..=1.0).contains(&value)),
            _ => false,
        };
        if !fits {
            return Err(invalid(format!(
                "{context}: `{field}` does not fit a {kind} event"
            )));
        }
    }
    if kind == "key"
        && !["key", "physical"]
            .iter()
            .any(|field| entry.get(*field).is_some_and(Value::is_string))
    {
        return Err(invalid(format!(
            "{context}: a key event names `key` or `physical`"
        )));
    }
    Ok((at, Value::Object(entry.clone())))
}

/// The probe payload of one of this module's tools, checked, and the time
/// the probe may take beyond an ordinary request.
pub(super) fn payload(
    operation: SessionOperation,
    arguments: &Map<String, Value>,
) -> Result<(Map<String, Value>, Duration), OutcomeError> {
    let mut out = Map::new();
    let mut extra = Duration::ZERO;
    match operation {
        SessionOperation::PropertiesBatchGet => {
            only(
                arguments,
                &["session", "targets"],
                "runtime.properties.batch.get",
            )?;
            let targets = arguments
                .get("targets")
                .and_then(Value::as_array)
                .filter(|targets| !targets.is_empty() && targets.len() <= MAX_BATCH_TARGETS)
                .ok_or_else(|| {
                    invalid(format!("`targets` lists 1 to {MAX_BATCH_TARGETS} targets"))
                })?;
            let checked = targets
                .iter()
                .enumerate()
                .map(|(index, target)| batch_target(target, index))
                .collect::<Result<Vec<_>, _>>()?;
            out.insert("targets".to_owned(), Value::Array(checked));
        }
        SessionOperation::PropertiesSet => {
            only(arguments, &["session", "target"], "runtime.properties.set")?;
            out.extend(set_target(arguments)?);
        }
        SessionOperation::Wait => {
            only(
                arguments,
                &["session", "condition", "timeoutMs"],
                "runtime.wait",
            )?;
            out.insert("condition".to_owned(), condition(arguments)?);
            let timeout =
                bounded_integer(arguments, "timeoutMs", 1, MAX_WAIT_MS)?.unwrap_or(DEFAULT_WAIT_MS);
            out.insert("timeout_ms".to_owned(), json!(timeout));
            extra = Duration::from_millis(timeout);
        }
        SessionOperation::Sample => {
            only(
                arguments,
                &[
                    "session",
                    "series",
                    "durationMs",
                    "intervalMs",
                    "maxSamples",
                ],
                "runtime.sample",
            )?;
            out.insert("series".to_owned(), Value::Array(series(arguments)?));
            let duration = bounded_integer(arguments, "durationMs", 1, MAX_SAMPLE_MS)?
                .unwrap_or(DEFAULT_SAMPLE_MS);
            out.insert("duration_ms".to_owned(), json!(duration));
            if let Some(interval) =
                bounded_integer(arguments, "intervalMs", 1, MAX_SAMPLE_INTERVAL_MS)?
            {
                out.insert("interval_ms".to_owned(), json!(interval));
            }
            if let Some(most) = bounded_integer(arguments, "maxSamples", 1, MAX_SAMPLES)? {
                out.insert("max_samples".to_owned(), json!(most));
            }
            extra = Duration::from_millis(duration);
        }
        SessionOperation::NodesFind => {
            only(
                arguments,
                &[
                    "session", "root", "class", "script", "group", "name", "near", "limit",
                ],
                "runtime.nodes.find",
            )?;
            if arguments.contains_key("root") {
                out.insert(
                    "root".to_owned(),
                    json!(
                        text(arguments.get("root"), 1, 1024).ok_or_else(|| invalid(
                            "`root` is a node path of 1 to 1024 characters"
                        ))?
                    ),
                );
            }
            for (key, high) in [
                ("class", MAX_NAME),
                ("script", 1024),
                ("group", MAX_NAME),
                ("name", MAX_NAME),
            ] {
                if let Some(value) = bounded_text(arguments, key, high, "runtime.nodes.find")? {
                    out.insert(key.to_owned(), json!(value));
                }
            }
            if let Some(value) = arguments.get("near") {
                out.insert("near".to_owned(), near(value)?);
            }
            if let Some(limit) = bounded_integer(arguments, "limit", 1, MAX_FOUND)? {
                out.insert("limit".to_owned(), json!(limit));
            }
        }
        SessionOperation::NavigationPathGet => {
            only(
                arguments,
                &[
                    "session",
                    "node",
                    "to",
                    "from",
                    "layers",
                    "optimize",
                    "syncTimeoutMs",
                ],
                "runtime.navigation.path.get",
            )?;
            out.insert(
                "node".to_owned(),
                json!(node_path(
                    arguments.get("node"),
                    "runtime.navigation.path.get"
                )?),
            );
            out.insert(
                "to".to_owned(),
                point(
                    arguments
                        .get("to")
                        .ok_or_else(|| invalid("`to` is required"))?,
                    "to",
                )?,
            );
            if let Some(from) = arguments.get("from") {
                out.insert("from".to_owned(), point(from, "from")?);
            }
            extra = Duration::from_millis(navigation_options(arguments, &mut out)?);
        }
        SessionOperation::NodeMove => {
            only(
                arguments,
                &["session", "node", "to", "mode"],
                "runtime.node.move",
            )?;
            out.insert(
                "node".to_owned(),
                json!(node_path(arguments.get("node"), "runtime.node.move")?),
            );
            out.insert(
                "to".to_owned(),
                point(
                    arguments
                        .get("to")
                        .ok_or_else(|| invalid("`to` is required"))?,
                    "to",
                )?,
            );
            let keyed = arguments
                .get("mode")
                .and_then(Value::as_object)
                .filter(|keyed| keyed.len() == 1)
                .ok_or_else(|| {
                    invalid("`mode` is an object with one member: teleport or along_path")
                })?;
            let (mode, fields) = keyed.iter().next().expect("one member");
            let fields = object(fields, &format!("`mode.{mode}`"))?;
            match mode.as_str() {
                "teleport" => {
                    only(fields, &[], "`mode.teleport`")?;
                    out.insert("mode".to_owned(), json!("teleport"));
                }
                "along_path" => {
                    only(
                        fields,
                        &["speed", "timeoutMs", "layers", "optimize", "syncTimeoutMs"],
                        "`mode.along_path`",
                    )?;
                    let speed = fields
                        .get("speed")
                        .and_then(Value::as_f64)
                        .filter(|speed| *speed > 0.0 && *speed <= MAX_SPEED)
                        .ok_or_else(|| {
                            invalid(format!(
                                "along_path needs `speed` in (0, {MAX_SPEED}] units per second"
                            ))
                        })?;
                    out.insert("mode".to_owned(), json!("along_path"));
                    out.insert("speed".to_owned(), json!(speed));
                    let timeout = bounded_integer(fields, "timeoutMs", 1, MAX_MOVE_MS)?
                        .unwrap_or(DEFAULT_MOVE_MS);
                    out.insert("timeout_ms".to_owned(), json!(timeout));
                    let sync = navigation_options(fields, &mut out)?;
                    extra = Duration::from_millis(timeout + sync);
                }
                other => {
                    return Err(invalid(format!(
                        "{other} is not a way to move: teleport or along_path"
                    )));
                }
            }
        }
        SessionOperation::UiPress => {
            only(
                arguments,
                &["session", "target", "holdMs"],
                "runtime.ui.press",
            )?;
            out.extend(press_target(arguments)?);
            let hold = bounded_integer(arguments, "holdMs", 1, MAX_PRESS_HOLD_MS)?.unwrap_or(100);
            out.insert("hold_ms".to_owned(), json!(hold));
            extra = Duration::from_millis(hold);
        }
        SessionOperation::InputRecordingStart => {
            only(
                arguments,
                &["session", "maxEvents", "maxDurationMs"],
                "input.recording.start",
            )?;
            if let Some(most) = bounded_integer(arguments, "maxEvents", 1, MAX_EVENTS as u64)? {
                out.insert("max_events".to_owned(), json!(most));
            }
            if let Some(longest) = bounded_integer(arguments, "maxDurationMs", 1, MAX_INPUT_MS)? {
                out.insert("max_duration_ms".to_owned(), json!(longest));
            }
        }
        SessionOperation::InputRecordingStop => {
            only(arguments, &["session"], "input.recording.stop")?;
        }
        SessionOperation::InputReplay => {
            only(arguments, &["session", "events"], "input.replay")?;
            let listed = arguments
                .get("events")
                .and_then(Value::as_array)
                .filter(|listed| !listed.is_empty() && listed.len() <= MAX_EVENTS)
                .ok_or_else(|| invalid(format!("`events` lists 1 to {MAX_EVENTS} events")))?;
            let mut timed = listed
                .iter()
                .enumerate()
                .map(|(index, entry)| event(entry, index))
                .collect::<Result<Vec<_>, _>>()?;
            // Each event is delivered at its own time: the list is put in time
            // order, keeping the order of events of the same time.
            timed.sort_by_key(|(at, _)| *at);
            let last = timed.last().map_or(0, |(at, _)| *at);
            out.insert(
                "events".to_owned(),
                Value::Array(timed.into_iter().map(|(_, entry)| entry).collect()),
            );
            extra = Duration::from_millis(last);
        }
        other => unreachable!("{} is not a runtime control tool", other.tool_name()),
    }
    Ok((out, extra))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: &str = "00000000-0000-0000-0000-000000000000";

    fn advertised(tool: &str) -> Value {
        Value::Object(crate::kernel::tool_schema(
            tool,
            crate::core::CoreLimits::default(),
        ))
    }

    /// Hold `operation`'s advertised schema and its check to the stated
    /// verdict on each case: (arguments without the session and the
    /// idempotency id, the verdict both give).
    fn agree(operation: SessionOperation, cases: &[(Value, bool)]) {
        let tool = operation.tool_name();
        let schema = advertised(tool);
        for (case, verdict) in cases {
            let mut arguments = case.as_object().cloned().expect("a case is an object");
            arguments.insert("session".to_owned(), json!(SESSION));
            let mut advertised_arguments = arguments.clone();
            if operation.descriptor().is_mutation() {
                advertised_arguments.insert("idempotencyId".to_owned(), json!("call-1"));
            }
            assert_eq!(
                crate::schema::conformance::conforms(&schema, &Value::Object(advertised_arguments)),
                *verdict,
                "the {tool} schema's verdict on {case}"
            );
            let checked = payload(operation, &arguments);
            assert_eq!(
                checked.is_ok(),
                *verdict,
                "the {tool} check's verdict on {case}: {checked:?}"
            );
        }
    }

    fn tagged(kind: &str, value: Value) -> Value {
        json!({ "type": kind, "value": value })
    }

    #[test]
    fn every_control_tool_is_handled_and_strict() {
        for &operation in SessionOperation::ALL {
            if !handles(operation) {
                continue;
            }
            let schema = advertised(operation.tool_name());
            assert_eq!(
                schema["additionalProperties"],
                json!(false),
                "{}",
                operation.tool_name()
            );
            for (key, property) in schema["properties"].as_object().expect("properties") {
                assert!(
                    property["type"].is_string(),
                    "{}.{key} needs a type",
                    operation.tool_name()
                );
                assert!(
                    property["description"].is_string(),
                    "{}.{key} needs a description",
                    operation.tool_name()
                );
            }
            assert_eq!(
                schema.get("$defs").is_some(),
                uses_values(operation),
                "{} carries the value definitions exactly when it names a tagged value",
                operation.tool_name()
            );
        }
    }

    #[test]
    fn a_property_write_names_one_target_and_values_it_can_carry() {
        let int = tagged("integer", json!(3));
        agree(
            SessionOperation::PropertiesSet,
            &[
                (
                    json!({ "target": { "node": "Player", "properties": { "speed": int } } }),
                    true,
                ),
                (
                    json!({ "target": { "node": "Player", "properties": {
                        "tags": tagged("array", json!([tagged("string", json!("a"))]))
                    } } }),
                    true,
                ),
                (
                    json!({ "target": { "node": "Player", "properties": {} } }),
                    false,
                ),
                (json!({ "target": { "node": "Player" } }), false),
                (
                    json!({ "target": { "node": "", "properties": { "speed": int } } }),
                    false,
                ),
                (
                    json!({ "target": { "node": "Player", "properties": { "speed": { "type": "integer", "value": 1, "extra": 2 } } } }),
                    false,
                ),
                (
                    json!({ "target": { "node": "Player", "properties": { "speed": 3 } } }),
                    false,
                ),
                (
                    json!({ "target": { "node": "Player", "properties": { "": int } } }),
                    false,
                ),
                (
                    json!({ "target": { "node": "Player", "singleton": "Engine", "properties": { "speed": int } } }),
                    false,
                ),
                (
                    json!({ "target": { "singleton": "Engine", "properties": {
                        "time_scale": tagged("float", json!(0.5)),
                        "max_fps": tagged("integer", json!(60))
                    } } }),
                    true,
                ),
                (
                    json!({ "target": { "singleton": "Engine", "properties": {
                        "physics_ticks_per_second": tagged("float", json!(30.0))
                    } } }),
                    true,
                ),
                (
                    json!({ "target": { "singleton": "Engine", "properties": {
                        "physics_ticks_per_second": tagged("float", json!(30.5))
                    } } }),
                    false,
                ),
                (
                    json!({ "target": { "singleton": "Engine", "properties": {
                        "physics_ticks_per_second": tagged("integer", json!(0))
                    } } }),
                    false,
                ),
                (
                    json!({ "target": { "singleton": "Engine", "properties": {
                        "time_scale": tagged("string", json!("1"))
                    } } }),
                    false,
                ),
                (
                    json!({ "target": { "singleton": "Engine", "properties": {
                        "print_error_messages": tagged("bool", json!(false))
                    } } }),
                    false,
                ),
                (
                    json!({ "target": { "singleton": "OS", "properties": { "time_scale": int } } }),
                    false,
                ),
                (
                    json!({ "target": { "singleton": "Engine", "properties": {} } }),
                    false,
                ),
                (json!({ "properties": { "speed": int } }), false),
                (
                    json!({ "target": { "node": "Player", "properties": { "speed": int } }, "extra": 1 }),
                    false,
                ),
            ],
        );
    }

    #[test]
    fn a_batch_read_names_up_to_32_targets() {
        let many: Vec<Value> = (0..33)
            .map(|index| json!({ "node": format!("N{index}") }))
            .collect();
        agree(
            SessionOperation::PropertiesBatchGet,
            &[
                (json!({ "targets": [{ "node": "Player" }] }), true),
                (
                    json!({ "targets": [
                        { "node": "Player", "properties": ["position", "speed"] },
                        { "singleton": "Engine" },
                        { "singleton": "Engine", "properties": ["time_scale"] }
                    ] }),
                    true,
                ),
                (json!({ "targets": [] }), false),
                (json!({ "targets": many }), false),
                (
                    json!({ "targets": [{ "node": "Player", "properties": [] }] }),
                    false,
                ),
                (
                    json!({ "targets": [{ "singleton": "Engine", "properties": ["print_error_messages"] }] }),
                    false,
                ),
                (json!({ "targets": [{ "singleton": "Input" }] }), false),
                (
                    json!({ "targets": [{ "node": "Player", "singleton": "Engine" }] }),
                    false,
                ),
                (json!({ "targets": [{}] }), false),
            ],
        );
    }

    #[test]
    fn a_wait_names_one_condition_of_a_known_kind() {
        let value = tagged("integer", json!(3));
        agree(
            SessionOperation::Wait,
            &[
                (
                    json!({ "condition": { "node_exists": { "node": "Spawned" } } }),
                    true,
                ),
                (
                    json!({ "condition": { "node_gone": { "node": "Doomed" } }, "timeoutMs": 30000 }),
                    true,
                ),
                (
                    json!({ "condition": { "property": { "node": ".", "property": "beats", "op": ">=", "value": value } } }),
                    true,
                ),
                (
                    json!({ "condition": { "property": { "node": ".", "property": "p", "op": "approx", "value": value, "tolerance": 0.5 } } }),
                    true,
                ),
                (
                    json!({ "condition": { "signal": { "node": ".", "signal": "beat", "count": 2 } } }),
                    true,
                ),
                (
                    json!({ "condition": { "ui_text": { "text": "Go!", "match": "contains" } } }),
                    true,
                ),
                (json!({ "condition": {} }), false),
                (
                    json!({ "condition": { "node_exists": { "node": "A" }, "node_gone": { "node": "B" } } }),
                    false,
                ),
                (json!({ "condition": { "frame": { "count": 3 } } }), false),
                (
                    json!({ "condition": { "property": { "node": ".", "property": "p", "op": "~", "value": value } } }),
                    false,
                ),
                (
                    json!({ "condition": { "property": { "node": ".", "property": "p", "op": "==", "value": 3 } } }),
                    false,
                ),
                (
                    json!({ "condition": { "property": { "node": ".", "property": "p", "op": "==" } } }),
                    false,
                ),
                (
                    json!({ "condition": { "property": { "node": ".", "property": "p", "op": "approx", "value": value, "tolerance": -1 } } }),
                    false,
                ),
                (
                    json!({ "condition": { "signal": { "node": ".", "signal": "beat", "count": 0 } } }),
                    false,
                ),
                (json!({ "condition": { "ui_text": { "text": "" } } }), false),
                (
                    json!({ "condition": { "ui_text": { "text": "x", "match": "regex" } } }),
                    false,
                ),
                (
                    json!({ "condition": { "node_exists": { "node": "A" } }, "timeoutMs": 30001 }),
                    false,
                ),
                (
                    json!({ "condition": { "node_exists": { "node": "A" } }, "timeoutMs": 0 }),
                    false,
                ),
                (
                    json!({ "condition": { "node_exists": { "node": "A", "extra": 1 } } }),
                    false,
                ),
            ],
        );
    }

    #[test]
    fn a_sample_names_up_to_32_series_for_at_most_10_seconds() {
        let many: Vec<Value> = (0..33).map(|_| json!({ "monitor": "TIME_FPS" })).collect();
        agree(
            SessionOperation::Sample,
            &[
                (
                    json!({ "series": [
                        { "node": ".", "property": "beats" },
                        { "singleton": "Engine", "property": "time_scale" },
                        { "monitor": "TIME_FPS" }
                    ], "durationMs": 10000, "intervalMs": 1000, "maxSamples": 600 }),
                    true,
                ),
                (json!({ "series": [] }), false),
                (json!({ "series": many }), false),
                (json!({ "series": [{ "node": "." }] }), false),
                (
                    json!({ "series": [{ "singleton": "Engine", "property": "print_error_messages" }] }),
                    false,
                ),
                (
                    json!({ "series": [{ "monitor": "TIME_FPS", "node": "." }] }),
                    false,
                ),
                (
                    json!({ "series": [{ "monitor": "TIME_FPS" }], "durationMs": 10001 }),
                    false,
                ),
                (
                    json!({ "series": [{ "monitor": "TIME_FPS" }], "intervalMs": 0 }),
                    false,
                ),
                (
                    json!({ "series": [{ "monitor": "TIME_FPS" }], "maxSamples": 601 }),
                    false,
                ),
            ],
        );
    }

    #[test]
    fn a_search_takes_filters_and_one_kind_of_centre() {
        agree(
            SessionOperation::NodesFind,
            &[
                (json!({}), true),
                (
                    json!({ "root": "/root/Main", "class": "CharacterBody2D", "script": "res://hero.gd", "group": "coins", "name": "Coin*", "limit": 500 }),
                    true,
                ),
                (
                    json!({ "near": { "point": { "x": 1, "y": 2 }, "radius": 10 } }),
                    true,
                ),
                (
                    json!({ "near": { "point": { "x": 1, "y": 2, "z": 3 }, "radius": 0 } }),
                    true,
                ),
                (
                    json!({ "near": { "node": "Player", "radius": 200.5 } }),
                    true,
                ),
                (json!({ "near": { "node": "Player" } }), false),
                (
                    json!({ "near": { "node": "Player", "point": { "x": 1, "y": 2 }, "radius": 1 } }),
                    false,
                ),
                (
                    json!({ "near": { "point": { "x": 1 }, "radius": 1 } }),
                    false,
                ),
                (
                    json!({ "near": { "point": { "x": 1, "y": 2, "w": 3 }, "radius": 1 } }),
                    false,
                ),
                (
                    json!({ "near": { "point": { "x": 1, "y": 2 }, "radius": -1 } }),
                    false,
                ),
                (json!({ "class": "" }), false),
                (json!({ "limit": 501 }), false),
                (json!({ "type": "Node2D" }), false),
            ],
        );
    }

    #[test]
    fn a_path_query_names_a_node_and_a_destination() {
        agree(
            SessionOperation::NavigationPathGet,
            &[
                (
                    json!({ "node": "Player", "to": { "x": 50, "y": 350 } }),
                    true,
                ),
                (
                    json!({ "node": "Walker", "to": { "x": 9, "y": 0, "z": 9 }, "from": { "x": 1, "y": 0, "z": 1 }, "layers": 1, "optimize": false, "syncTimeoutMs": 5000 }),
                    true,
                ),
                (json!({ "node": "Player" }), false),
                (json!({ "to": { "x": 1, "y": 2 } }), false),
                (
                    json!({ "node": "Player", "to": { "x": 1, "y": 2 }, "layers": 0 }),
                    false,
                ),
                (
                    json!({ "node": "Player", "to": { "x": 1, "y": 2 }, "syncTimeoutMs": 5001 }),
                    false,
                ),
                (
                    json!({ "node": "Player", "to": { "x": 1, "y": 2e10 } }),
                    false,
                ),
            ],
        );
    }

    #[test]
    fn a_move_names_one_mode_with_only_its_own_fields() {
        let to = json!({ "x": 50, "y": 350 });
        agree(
            SessionOperation::NodeMove,
            &[
                (
                    json!({ "node": "Player", "to": to, "mode": { "teleport": {} } }),
                    true,
                ),
                (
                    json!({ "node": "Player", "to": to, "mode": { "along_path": { "speed": 400 } } }),
                    true,
                ),
                (
                    json!({ "node": "Player", "to": to, "mode": { "along_path": { "speed": 0.5, "timeoutMs": 30000, "layers": 3, "optimize": true, "syncTimeoutMs": 10 } } }),
                    true,
                ),
                (
                    json!({ "node": "Player", "to": to, "mode": { "teleport": { "speed": 1 } } }),
                    false,
                ),
                (
                    json!({ "node": "Player", "to": to, "mode": { "along_path": {} } }),
                    false,
                ),
                (
                    json!({ "node": "Player", "to": to, "mode": { "along_path": { "speed": 0 } } }),
                    false,
                ),
                (
                    json!({ "node": "Player", "to": to, "mode": { "along_path": { "speed": 100001 } } }),
                    false,
                ),
                (
                    json!({ "node": "Player", "to": to, "mode": { "fly": {} } }),
                    false,
                ),
                (json!({ "node": "Player", "to": to, "mode": {} }), false),
                (
                    json!({ "node": "Player", "to": to, "mode": { "teleport": {}, "along_path": { "speed": 1 } } }),
                    false,
                ),
                (
                    json!({ "node": "Player", "mode": { "teleport": {} } }),
                    false,
                ),
            ],
        );
    }

    #[test]
    fn a_press_names_its_button_one_way() {
        agree(
            SessionOperation::UiPress,
            &[
                (json!({ "target": { "node": "HUD/StartButton" } }), true),
                (
                    json!({ "target": { "text": "Start" }, "holdMs": 5000 }),
                    true,
                ),
                (
                    json!({ "target": { "text": "Sta", "match": "contains" } }),
                    true,
                ),
                (
                    json!({ "target": { "node": "HUD/StartButton", "text": "Start" } }),
                    false,
                ),
                (
                    json!({ "target": { "node": "HUD/StartButton", "match": "exact" } }),
                    false,
                ),
                (json!({ "target": {} }), false),
                (json!({ "target": { "text": "" } }), false),
                (
                    json!({ "target": { "text": "Start" }, "holdMs": 5001 }),
                    false,
                ),
                (json!({}), false),
            ],
        );
    }

    #[test]
    fn a_recording_is_bounded() {
        agree(
            SessionOperation::InputRecordingStart,
            &[
                (json!({}), true),
                (json!({ "maxEvents": 3000, "maxDurationMs": 30000 }), true),
                (json!({ "maxEvents": 0 }), false),
                (json!({ "maxDurationMs": 30001 }), false),
            ],
        );
        agree(
            SessionOperation::InputRecordingStop,
            &[(json!({}), true), (json!({ "keep": true }), false)],
        );
    }

    #[test]
    fn a_replay_takes_recorded_events_of_known_kinds() {
        let key = json!({
            "t_ms": 0, "kind": "key", "key": "D", "physical": "D", "unicode": 100, "pressed": true,
            "echo": false, "modifiers": { "shift": false, "ctrl": false, "alt": false, "meta": false },
            "source": "probe", "actions": ["move_right"]
        });
        let events = |events: Value| json!({ "events": events });
        agree(
            SessionOperation::InputReplay,
            &[
                (events(json!([key])), true),
                (
                    events(json!([
                        { "t_ms": 300, "kind": "joypad_button", "device": 0, "button": 0, "pressed": true },
                        { "t_ms": 10, "kind": "joypad_motion", "axis": 1, "value": -0.5 },
                        { "t_ms": 20, "kind": "mouse_button", "button": "left", "position": { "x": 5, "y": 6 }, "double_click": false },
                        { "t_ms": 30, "kind": "mouse_button", "button": 9, "position": { "x": 5, "y": 6 } },
                        { "t_ms": 40, "kind": "mouse_motion", "position": { "x": 5, "y": 6 }, "relative": { "x": 1, "y": 0 }, "button_mask": 1 },
                        { "t_ms": 50, "kind": "action", "action": "jump", "strength": 1, "pressed": false },
                        { "t_ms": 60, "kind": "other", "class": "InputEventMagnifyGesture", "source": "external" },
                        { "t_ms": 70, "kind": "key", "physical": "Space" }
                    ])),
                    true,
                ),
                (events(json!([])), false),
                (events(json!([{ "t_ms": 0, "kind": "key" }])), false),
                (
                    events(json!([{ "t_ms": 0, "kind": "key", "key": null, "physical": null }])),
                    false,
                ),
                (
                    events(json!([{ "t_ms": 0, "kind": "key", "key": null, "physical": "Space" }])),
                    false,
                ),
                (
                    events(json!([{ "t_ms": 0, "kind": "key", "key": "D", "button": 1 }])),
                    false,
                ),
                (events(json!([{ "kind": "key", "key": "D" }])), false),
                (
                    events(json!([{ "t_ms": 30001, "kind": "key", "key": "D" }])),
                    false,
                ),
                (
                    events(json!([{ "t_ms": 0, "kind": "touch", "position": { "x": 1, "y": 1 } }])),
                    false,
                ),
                (
                    events(json!([{ "t_ms": 0, "kind": "mouse_button", "button": "left" }])),
                    false,
                ),
                (
                    events(
                        json!([{ "t_ms": 0, "kind": "mouse_button", "button": 10, "position": { "x": 1, "y": 1 } }]),
                    ),
                    false,
                ),
                (
                    events(json!([{ "t_ms": 0, "kind": "joypad_button", "button": 128 }])),
                    false,
                ),
                (
                    events(
                        json!([{ "t_ms": 0, "kind": "joypad_motion", "axis": 1, "value": 1.5 }]),
                    ),
                    false,
                ),
                (
                    events(
                        json!([{ "t_ms": 0, "kind": "action", "action": "jump", "source": "device" }]),
                    ),
                    false,
                ),
                (
                    events(
                        json!([{ "t_ms": 0, "kind": "mouse_motion", "position": { "x": 1, "y": 1, "z": 1 } }]),
                    ),
                    false,
                ),
            ],
        );
    }

    #[test]
    fn a_replay_is_sent_in_time_order_and_waits_for_its_last_event() {
        let mut arguments = Map::new();
        arguments.insert(
            "events".to_owned(),
            json!([
                { "t_ms": 300, "kind": "action", "action": "b" },
                { "t_ms": 10, "kind": "action", "action": "a" },
                { "t_ms": 300, "kind": "action", "action": "c" }
            ]),
        );
        let (payload, extra) = payload(SessionOperation::InputReplay, &arguments).expect("valid");
        let order: Vec<&str> = payload["events"]
            .as_array()
            .expect("events")
            .iter()
            .map(|event| event["action"].as_str().expect("an action"))
            .collect();
        assert_eq!(order, ["a", "b", "c"]);
        assert_eq!(extra, Duration::from_millis(300));
    }
}
