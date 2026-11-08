//! `runtime.time_scale.set`, `runtime.image.compare` and `test.scenario.run`:
//! their schemas, their checks and the journaled command each sends.
//!
//! Each tool's advertised schema and the check here give the same verdict on
//! the same arguments, as the other session tools' do. What a schema cannot
//! state is refused here before anything is journaled: that step ids are
//! unique, that a comparison names an earlier screenshot, that no input step
//! waits longer than a sequence may, how many image steps a scenario has, and
//! that a reference image exists and is a PNG. A scenario step takes the
//! schema and the check of the tool it mirrors, so the two cannot drift.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

use godot_mcp_bridge::protocol::mutation::MutationStatus;
use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::contracts::{GodotValue, OutcomeError};
use godot_mcp_domain::lease::PROJECT_STATE_DIR_NAME;
use godot_mcp_engine::canonical::helper::HELPER_ROOT;
use serde_json::{Map, Value, json};

use super::{
    MAX_SEQUENCE_MS, PROBE_TIMEOUT, SessionCall, SessionHandler, SessionOperation, Withdrawal,
    boolean, control, integer, invalid, outcome_error, required_string, res_scene, sequence_step,
    session_answer, string,
};
use crate::effect::boundary::EditorMutation;
use crate::effect::gateway::EditorEffectIntent;
use crate::engine_plane::authored::tagged_value;
use crate::engine_plane::runtime::journal_transport;
use crate::engine_plane::session::scenario::{
    Action, DEFAULT_DURATION_MS, DEFAULT_STRESS_INTERVAL_MS, DEFAULT_STRESS_MS, MAX_DURATION_MS,
    MAX_IMAGE_STEPS, MAX_REFERENCE_BYTES, MAX_REFERENCE_TOTAL, MAX_STEPS, MAX_STRESS_INSTANCES,
    MAX_STRESS_INTERVAL_MS, MAX_STRESS_MS, MAX_TIME_SCALE, Plan, Planned, Reference,
};
use crate::journal::OperationFamily;
use crate::kernel::KernelToolCall;
use crate::router::RouteEffect;

/// The longest step id.
const MAX_ID: usize = 64;
/// The longest node path a step names.
const MAX_NODE: usize = 1024;
/// The longest property name an assertion names.
const MAX_PROPERTY: usize = 256;
/// The longest text a text assertion expects.
const MAX_TEXT: usize = 1024;
/// The largest tolerance of a property comparison, as `runtime.wait` takes.
const MAX_TOLERANCE: f64 = 1.0e9;
/// The longest project path of a reference image.
const MAX_PATH: usize = 1024;
/// The first bytes of every PNG file.
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

/// Every scenario step kind, as the schema lists it and the check reads it.
const STEP_KINDS: &[&str] = &[
    "input",
    "wait",
    "set_property",
    "press",
    "time_scale",
    "assert_property",
    "assert_ui_text",
    "screenshot",
    "compare_image",
    "stress",
];

/// The comparisons of a property assertion and the probe's spelling of each,
/// the one `runtime.wait`'s property condition uses.
const COMPARISONS: &[(&str, &str)] = &[
    ("equals", "=="),
    ("not_equals", "!="),
    ("less", "<"),
    ("greater", ">"),
    ("approx", "approx"),
];

/// The Engine properties an assertion may read, as a write may change them.
const ENGINE_PROPERTIES: &[&str] = &["time_scale", "physics_ticks_per_second", "max_fps"];

/// Whether `operation` is one of this module's tools.
pub(super) const fn handles(operation: SessionOperation) -> bool {
    matches!(
        operation,
        SessionOperation::TimeScaleSet
            | SessionOperation::ImageCompare
            | SessionOperation::ScenarioRun
    )
}

/// Whether `operation`'s schema refers to the tagged value definitions.
pub(super) const fn uses_values(operation: SessionOperation) -> bool {
    matches!(operation, SessionOperation::ScenarioRun)
}

/// What the journaled command of `operation` asks the runtime boundary for.
pub(super) const fn action(operation: SessionOperation) -> &'static str {
    match operation {
        SessionOperation::TimeScaleSet => "runtime.time_scale.set",
        SessionOperation::ImageCompare => "runtime.image.compare",
        _ => "test.scenario.run",
    }
}

// --- schemas -------------------------------------------------------------------

fn time_scale_schema() -> Value {
    json!({
        "type": "number",
        "minimum": 0,
        "maximum": MAX_TIME_SCALE,
        "description": "The scale of game time, 0 to 100: 1 is real time, 0.5 half speed, 2 \
                        double speed and 0 stops game time."
    })
}

fn image_tolerance_schema() -> Value {
    json!({
        "type": "integer",
        "minimum": 0,
        "maximum": 255,
        "description": "How far one channel of a pixel may differ, 0 to 255, before the pixel \
                        counts as differing (default 0)."
    })
}

fn difference_schema() -> Value {
    json!({
        "type": "boolean",
        "description": "Keep a difference image when pixels differ: the capture darkened, \
                        with every differing pixel red (default true)."
    })
}

fn path_schema() -> Value {
    json!({
        "type": "string",
        "minLength": 1,
        "maxLength": MAX_PATH,
        "pattern": crate::schema::project_png_pattern(),
        "description": "A PNG image of the project, res:// or project-relative."
    })
}

fn node_schema(description: &str) -> Value {
    json!({
        "type": "string",
        "minLength": 1,
        "maxLength": MAX_NODE,
        "description": description
    })
}

fn id_schema() -> Value {
    json!({
        "type": "string",
        "minLength": 1,
        "maxLength": MAX_ID,
        "pattern": "^[A-Za-z0-9][A-Za-z0-9_.-]*$",
        "description": "The step's id, unique in the scenario: the report and its artifacts \
                        name the step by it, and a comparison names an earlier screenshot by \
                        it (default step-<n>, its position from 1)."
    })
}

/// The field schemas `test.scenario.run` defines once when it repeats them,
/// beside those every session tool that takes tagged values shares.
pub(super) fn shared_fields() -> Vec<(&'static str, Value)> {
    vec![("StepId", id_schema()), ("ProjectPng", path_schema())]
}

/// The schema of the `property` named field of one of the mirrored tools'
/// schemas, as that tool writes it, before any definition is shared.
fn mirrored(operation: SessionOperation, field: &str) -> Value {
    operation.input_schema()["properties"][field].clone()
}

fn assert_target_schema() -> Value {
    control::closed_union(
        vec![
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "node": super::node_schema(),
                    "property": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": MAX_PROPERTY
                    }
                },
                "required": ["node", "property"]
            }),
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "singleton": { "type": "string", "enum": ["Engine"] },
                    "property": { "type": "string", "enum": ENGINE_PROPERTIES }
                },
                "required": ["singleton", "property"]
            }),
        ],
        "What to read: {node, property} of a running node, or {singleton: \"Engine\", \
         property} for time_scale, physics_ticks_per_second or max_fps.",
    )
}

fn reference_schema() -> Value {
    control::closed_union(
        vec![
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": { "path": path_schema() },
                "required": ["path"]
            }),
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "step": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": MAX_ID,
                        "pattern": "^[A-Za-z0-9][A-Za-z0-9_.-]*$"
                    }
                },
                "required": ["step"]
            }),
        ],
        "What to compare with: {path} names a PNG image of the project; {step} names an \
         earlier screenshot step of this scenario by its id.",
    )
}

fn step_schema(kind: &str) -> Value {
    let id = ("id", id_schema());
    let (fields, required): (Vec<(&str, Value)>, &[&str]) = match kind {
        "input" => (
            vec![
                id,
                ("steps", mirrored(SessionOperation::SequenceSend, "steps")),
            ],
            &["steps"],
        ),
        "wait" => (
            vec![
                id,
                ("condition", mirrored(SessionOperation::Wait, "condition")),
                ("timeoutMs", mirrored(SessionOperation::Wait, "timeoutMs")),
            ],
            &["condition"],
        ),
        "set_property" => (
            vec![
                id,
                (
                    "target",
                    mirrored(SessionOperation::PropertiesSet, "target"),
                ),
            ],
            &["target"],
        ),
        "press" => (
            vec![
                id,
                ("target", mirrored(SessionOperation::UiPress, "target")),
                ("holdMs", mirrored(SessionOperation::UiPress, "holdMs")),
            ],
            &["target"],
        ),
        "time_scale" => (vec![id, ("timeScale", time_scale_schema())], &["timeScale"]),
        "assert_property" => (
            vec![
                id,
                ("target", assert_target_schema()),
                (
                    "op",
                    json!({
                        "type": "string",
                        "enum": COMPARISONS.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
                        "description": "How the live value compares with `value`, as \
                                        runtime.wait compares: equals and not_equals take any \
                                        value; less and greater take numbers; approx takes \
                                        numbers, vectors, colors and quaternions within \
                                        `tolerance`."
                    }),
                ),
                ("value", crate::schema::writable_value()),
                (
                    "tolerance",
                    json!({
                        "type": "number",
                        "minimum": 0,
                        "maximum": MAX_TOLERANCE,
                        "description": "How far approx may be off: the difference of two \
                                        numbers, the distance of two vectors, the largest \
                                        component difference of two colors or quaternions \
                                        (default 0)."
                    }),
                ),
            ],
            &["target", "op", "value"],
        ),
        "assert_ui_text" => (
            vec![
                id,
                (
                    "node",
                    node_schema(
                        "A Control of the running game: an absolute path or a path \
                                 relative to the current scene.",
                    ),
                ),
                (
                    "op",
                    json!({
                        "type": "string",
                        "enum": ["equals", "contains"],
                        "description": "Whether the Control's text is `text` (equals, the \
                                        default) or contains it."
                    }),
                ),
                (
                    "text",
                    json!({ "type": "string", "minLength": 0, "maxLength": MAX_TEXT }),
                ),
            ],
            &["node", "text"],
        ),
        "screenshot" => (vec![id], &[]),
        "compare_image" => (
            vec![
                id,
                ("reference", reference_schema()),
                ("tolerance", image_tolerance_schema()),
                ("differenceImage", difference_schema()),
            ],
            &["reference"],
        ),
        _ => (
            vec![
                id,
                (
                    "scene",
                    json!({
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 1024,
                        "pattern": crate::schema::project_scene_pattern(),
                        "description": "The project scene to instance (.tscn or .scn), res:// \
                                        or project-relative."
                    }),
                ),
                (
                    "count",
                    json!({
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_STRESS_INSTANCES,
                        "description": "How many instances to add."
                    }),
                ),
                (
                    "parent",
                    node_schema(
                        "The node the instances are added under (default the current \
                                 scene).",
                    ),
                ),
                (
                    "durationMs",
                    json!({
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_STRESS_MS,
                        "description": "How long the instances stay, sampled, in ms (default \
                                        1000)."
                    }),
                ),
                (
                    "intervalMs",
                    json!({
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_STRESS_INTERVAL_MS,
                        "description": "The least time between two samples, in ms (default \
                                        100)."
                    }),
                ),
            ],
            &["scene", "count"],
        ),
    };
    crate::schema::variant("kind", kind, &fields, required)
}

/// The schema properties of one of this module's tools and the ones it
/// requires, beside the idempotency id each also requires.
pub(super) fn properties(
    operation: SessionOperation,
    session: &Value,
) -> (Value, &'static [&'static str]) {
    match operation {
        SessionOperation::TimeScaleSet => (
            json!({ "session": session, "timeScale": time_scale_schema() }),
            &["session", "timeScale"],
        ),
        SessionOperation::ImageCompare => (
            json!({
                "session": session,
                "reference": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": { "path": path_schema() },
                    "required": ["path"],
                    "description": "The PNG image of the project to compare with: {path}."
                },
                "tolerance": image_tolerance_schema(),
                "differenceImage": difference_schema()
            }),
            &["session", "reference"],
        ),
        SessionOperation::ScenarioRun => (
            json!({
                "session": session,
                "steps": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": MAX_STEPS,
                    "items": crate::schema::one_of(
                        STEP_KINDS.iter().map(|kind| step_schema(kind)).collect()
                    ),
                    "description": "The steps, run in order, keyed by `kind`: input, wait, \
                                    set_property, press, time_scale, assert_property, \
                                    assert_ui_text, screenshot, compare_image or stress. At \
                                    most 16 are screenshots or comparisons."
                },
                "stopOnFailure": {
                    "type": "boolean",
                    "description": "Skip the steps after the first that fails or cannot run \
                                    (default true). A step whose outcome cannot be decided \
                                    always ends the run."
                },
                "maxDurationMs": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_DURATION_MS,
                    "description": "The longest the scenario may run, in ms (default 60000): \
                                    the step running then is withdrawn and the rest are \
                                    skipped."
                }
            }),
            &["session", "steps"],
        ),
        other => unreachable!("{} is not a scenario tool", other.tool_name()),
    }
}

// --- checks ----------------------------------------------------------------------

/// The fields `fields` declares, and nothing else.
fn only(fields: &Map<String, Value>, allowed: &[&str], context: &str) -> Result<(), OutcomeError> {
    match fields.keys().find(|key| !allowed.contains(&key.as_str())) {
        Some(unknown) => Err(invalid(format!("{context} does not accept `{unknown}`"))),
        None => Ok(()),
    }
}

fn object<'a>(value: &'a Value, context: &str) -> Result<&'a Map<String, Value>, OutcomeError> {
    value
        .as_object()
        .ok_or_else(|| invalid(format!("{context} is an object")))
}

/// A string field of 1 to `high` characters.
fn text_of<'a>(
    fields: &'a Map<String, Value>,
    key: &str,
    low: usize,
    high: usize,
    context: &str,
) -> Result<&'a str, OutcomeError> {
    fields
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| (low..=high).contains(&text.chars().count()))
        .ok_or_else(|| {
            invalid(format!(
                "{context} needs `{key}`: a string of {low} to {high} characters"
            ))
        })
}

fn valid_id(id: &str) -> bool {
    let mut characters = id.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric())
        && characters.all(|later| later.is_ascii_alphanumeric() || matches!(later, '_' | '.' | '-'))
        && id.len() <= MAX_ID
}

/// The project-relative path of a reference image, checked as its schema
/// pattern states.
pub(super) fn reference_path(raw: &str) -> Result<String, OutcomeError> {
    let refuse = || {
        invalid(format!(
            "`{raw}` must name a PNG image of the project (res:// or project-relative), \
             outside .godot, {PROJECT_STATE_DIR_NAME} and {HELPER_ROOT}"
        ))
    };
    if raw.is_empty() || raw.chars().count() > MAX_PATH {
        return Err(refuse());
    }
    let relative = raw.strip_prefix("res://").unwrap_or(raw);
    let parts: Vec<&str> = relative.split('/').collect();
    let last = parts.last().copied().unwrap_or_default();
    let segment =
        |part: &str| !part.is_empty() && part != "." && part != ".." && !part.contains('\\');
    if !parts.iter().all(|part| segment(part))
        || last.len() < 4
        || !last.is_char_boundary(last.len() - 4)
        || !last[last.len() - 4..].eq_ignore_ascii_case(".png")
        || (parts.len() > 1 && matches!(parts[0], ".godot" | PROJECT_STATE_DIR_NAME | HELPER_ROOT))
    {
        return Err(refuse());
    }
    Ok(relative.to_owned())
}

/// A tolerance of an image comparison, 0 to 255.
fn image_tolerance(fields: &Map<String, Value>) -> Result<u8, OutcomeError> {
    Ok(integer(fields, "tolerance", 0, 255)?
        .and_then(|tolerance| u8::try_from(tolerance).ok())
        .unwrap_or(0))
}

fn time_scale(fields: &Map<String, Value>, context: &str) -> Result<f64, OutcomeError> {
    fields
        .get("timeScale")
        .and_then(Value::as_f64)
        .filter(|scale| (0.0..=MAX_TIME_SCALE).contains(scale))
        .ok_or_else(|| {
            invalid(format!(
                "{context} needs `timeScale`, a number from 0 to 100"
            ))
        })
}

/// The payload a mirrored control tool's own check makes of `fields`.
fn mirrored_payload(
    operation: SessionOperation,
    fields: &Map<String, Value>,
    keys: &[&str],
) -> Result<(Map<String, Value>, Duration), OutcomeError> {
    let mut arguments = Map::new();
    for key in keys {
        if let Some(value) = fields.get(*key) {
            arguments.insert((*key).to_owned(), value.clone());
        }
    }
    control::payload(operation, &arguments)
}

/// One scenario step, checked and spelled for the boundary: its id and what
/// it does.
fn step(value: &Value, index: usize) -> Result<Planned, OutcomeError> {
    let context = format!("step {}", index + 1);
    let fields = object(value, &context)?;
    let kind = fields
        .get("kind")
        .and_then(Value::as_str)
        .filter(|kind| STEP_KINDS.contains(kind))
        .ok_or_else(|| {
            invalid(format!(
                "{context} needs `kind`, one of {}",
                STEP_KINDS.join(", ")
            ))
        })?;
    let context = format!("{context} ({kind})");
    let id = match fields.get("id") {
        None => format!("step-{}", index + 1),
        Some(Value::String(id)) if valid_id(id) => id.clone(),
        Some(_) => {
            return Err(invalid(format!(
                "{context}: `id` is 1 to {MAX_ID} letters, digits, `_`, `.` or `-`, starting with a \
                 letter or digit"
            )));
        }
    };
    let action = match kind {
        "input" => {
            only(fields, &["kind", "id", "steps"], &context)?;
            let listed = fields
                .get("steps")
                .and_then(Value::as_array)
                .filter(|listed| !listed.is_empty() && listed.len() <= 64)
                .ok_or_else(|| {
                    invalid(format!("{context}: `steps` is an array of 1 to 64 steps"))
                })?;
            let mut steps = Vec::with_capacity(listed.len());
            let mut waited = 0_u64;
            for entry in listed {
                let converted = sequence_step(object(entry, &context)?)?;
                waited += converted.get("ms").and_then(Value::as_u64).unwrap_or(0);
                steps.push(converted);
            }
            if waited > MAX_SEQUENCE_MS {
                return Err(invalid(format!(
                    "{context}: an input step waits at most {MAX_SEQUENCE_MS} ms in total"
                )));
            }
            Action::Input {
                steps,
                wait_ms: waited,
            }
        }
        "wait" => {
            only(fields, &["kind", "id", "condition", "timeoutMs"], &context)?;
            let (request, extra) =
                mirrored_payload(SessionOperation::Wait, fields, &["condition", "timeoutMs"])?;
            Action::Wait {
                request,
                timeout_ms: u64::try_from(extra.as_millis()).unwrap_or(u64::MAX),
            }
        }
        "set_property" => {
            only(fields, &["kind", "id", "target"], &context)?;
            let (request, _) =
                mirrored_payload(SessionOperation::PropertiesSet, fields, &["target"])?;
            Action::SetProperty { request }
        }
        "press" => {
            only(fields, &["kind", "id", "target", "holdMs"], &context)?;
            let (request, extra) =
                mirrored_payload(SessionOperation::UiPress, fields, &["target", "holdMs"])?;
            Action::Press {
                request,
                hold_ms: u64::try_from(extra.as_millis()).unwrap_or(u64::MAX),
            }
        }
        "time_scale" => {
            only(fields, &["kind", "id", "timeScale"], &context)?;
            Action::TimeScale {
                time_scale: time_scale(fields, &context)?,
            }
        }
        "assert_property" => assert_property(fields, &context)?,
        "assert_ui_text" => {
            only(fields, &["kind", "id", "node", "op", "text"], &context)?;
            let node = text_of(fields, "node", 1, MAX_NODE, &context)?;
            let op = string(fields, "op")?.unwrap_or("equals");
            if !matches!(op, "equals" | "contains") {
                return Err(invalid(format!("{context}: `op` is equals or contains")));
            }
            let text = text_of(fields, "text", 0, MAX_TEXT, &context)?;
            Action::AssertUiText {
                request: json!({
                    "kind": "ui_text",
                    "node": node,
                    "match": if op == "contains" { "contains" } else { "exact" },
                    "text": text,
                })
                .as_object()
                .cloned()
                .unwrap_or_default(),
                expected: json!({ "op": op, "text": text }),
            }
        }
        "screenshot" => {
            only(fields, &["kind", "id"], &context)?;
            Action::Screenshot
        }
        "compare_image" => {
            only(
                fields,
                &["kind", "id", "reference", "tolerance", "differenceImage"],
                &context,
            )?;
            let named = object(
                fields
                    .get("reference")
                    .ok_or_else(|| invalid(format!("{context} needs `reference`")))?,
                &format!("{context}: `reference`"),
            )?;
            let reference = match (named.get("path"), named.get("step"), named.len()) {
                (Some(Value::String(path)), None, 1) => Reference::Path {
                    path: format!("res://{}", reference_path(path)?),
                },
                (None, Some(Value::String(step)), 1) if valid_id(step) => {
                    Reference::Step { step: step.clone() }
                }
                _ => {
                    return Err(invalid(format!(
                        "{context}: `reference` is {{path}}, a project PNG, or {{step}}, an \
                         earlier screenshot step's id"
                    )));
                }
            };
            Action::CompareImage {
                reference,
                tolerance: image_tolerance(fields)?,
                difference: boolean(fields, "differenceImage")?.unwrap_or(true),
            }
        }
        _ => {
            only(
                fields,
                &[
                    "kind",
                    "id",
                    "scene",
                    "count",
                    "parent",
                    "durationMs",
                    "intervalMs",
                ],
                &context,
            )?;
            let scene = res_scene(required_string(fields, "scene")?)?;
            let count = integer(fields, "count", 1, MAX_STRESS_INSTANCES)?
                .ok_or_else(|| invalid(format!("{context} needs `count`")))?;
            let parent = match fields.get("parent") {
                None => ".",
                Some(_) => text_of(fields, "parent", 1, MAX_NODE, &context)?,
            };
            let duration =
                integer(fields, "durationMs", 1, MAX_STRESS_MS)?.unwrap_or(DEFAULT_STRESS_MS);
            let interval = integer(fields, "intervalMs", 1, MAX_STRESS_INTERVAL_MS)?
                .unwrap_or(DEFAULT_STRESS_INTERVAL_MS);
            Action::Stress {
                request: json!({
                    "scene": scene,
                    "count": count,
                    "parent": parent,
                    "duration_ms": duration,
                    "interval_ms": interval,
                })
                .as_object()
                .cloned()
                .unwrap_or_default(),
                count,
                duration_ms: duration,
            }
        }
    };
    Ok(Planned { id, action })
}

fn assert_property(fields: &Map<String, Value>, context: &str) -> Result<Action, OutcomeError> {
    only(
        fields,
        &["kind", "id", "target", "op", "value", "tolerance"],
        context,
    )?;
    let target = object(
        fields
            .get("target")
            .ok_or_else(|| invalid(format!("{context} needs `target`")))?,
        &format!("{context}: `target`"),
    )?;
    let mut request = Map::new();
    request.insert("kind".to_owned(), json!("property"));
    match (target.get("node"), target.get("singleton")) {
        (Some(_), None) => {
            only(
                target,
                &["node", "property"],
                &format!("{context}: `target`"),
            )?;
            let node = text_of(target, "node", 1, MAX_NODE, context)?;
            request.insert("node".to_owned(), json!(node));
            request.insert(
                "property".to_owned(),
                json!(text_of(target, "property", 1, MAX_PROPERTY, context)?),
            );
        }
        (None, Some(Value::String(singleton))) if singleton == "Engine" => {
            only(
                target,
                &["singleton", "property"],
                &format!("{context}: `target`"),
            )?;
            let property = target
                .get("property")
                .and_then(Value::as_str)
                .filter(|property| ENGINE_PROPERTIES.contains(property))
                .ok_or_else(|| {
                    invalid(format!(
                        "{context}: an Engine assertion reads {}",
                        ENGINE_PROPERTIES.join(", ")
                    ))
                })?;
            request.insert("singleton".to_owned(), json!("Engine"));
            request.insert("property".to_owned(), json!(property));
        }
        _ => {
            return Err(invalid(format!(
                "{context}: `target` is {{node, property}} or {{singleton: \"Engine\", property}}"
            )));
        }
    }
    let op = string(fields, "op")?.unwrap_or_default();
    let probe_op = COMPARISONS
        .iter()
        .find(|(name, _)| *name == op)
        .map(|(_, spelled)| *spelled)
        .ok_or_else(|| {
            invalid(format!(
                "{context}: `op` is one of {}",
                COMPARISONS
                    .iter()
                    .map(|(name, _)| *name)
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?;
    let value = fields
        .get("value")
        .ok_or_else(|| invalid(format!("{context} needs `value`")))?;
    tagged_value(value, &format!("{context}: `value`"))?;
    let tolerance = match fields.get("tolerance") {
        None => 0.0,
        Some(tolerance) => tolerance
            .as_f64()
            .filter(|tolerance| (0.0..=MAX_TOLERANCE).contains(tolerance))
            .ok_or_else(|| {
                invalid(format!(
                    "{context}: `tolerance` is a number from 0 to {MAX_TOLERANCE}"
                ))
            })?,
    };
    request.insert("op".to_owned(), json!(probe_op));
    request.insert("value".to_owned(), value.clone());
    request.insert("tolerance".to_owned(), json!(tolerance));
    let mut expected = json!({ "op": op, "value": value });
    if op == "approx" || fields.contains_key("tolerance") {
        expected["tolerance"] = json!(tolerance);
    }
    Ok(Action::AssertProperty { request, expected })
}

/// A scenario's arguments, checked: the plan the boundary runs. Every
/// semantic rule its schema cannot state is checked here too.
pub(super) fn plan(arguments: &Map<String, Value>) -> Result<Plan, OutcomeError> {
    only(
        arguments,
        &["session", "steps", "stopOnFailure", "maxDurationMs"],
        "test.scenario.run",
    )?;
    let listed = arguments
        .get("steps")
        .and_then(Value::as_array)
        .filter(|listed| !listed.is_empty() && listed.len() <= MAX_STEPS)
        .ok_or_else(|| invalid(format!("`steps` is an array of 1 to {MAX_STEPS} steps")))?;
    let steps = listed
        .iter()
        .enumerate()
        .map(|(index, entry)| step(entry, index))
        .collect::<Result<Vec<_>, _>>()?;
    let mut seen = BTreeSet::new();
    let mut screenshots = BTreeSet::new();
    let mut images = 0_usize;
    for planned in &steps {
        if !seen.insert(planned.id.as_str()) {
            return Err(invalid(format!(
                "two steps are named {}; step ids are unique",
                planned.id
            )));
        }
        match &planned.action {
            Action::Screenshot => {
                images += 1;
                screenshots.insert(planned.id.as_str());
            }
            Action::CompareImage { reference, .. } => {
                images += 1;
                if let Reference::Step { step } = reference
                    && !screenshots.contains(step.as_str())
                {
                    return Err(invalid(format!(
                        "step {} compares with {step}, which is not an earlier screenshot step",
                        planned.id
                    )));
                }
            }
            _ => {}
        }
    }
    if images > MAX_IMAGE_STEPS {
        return Err(invalid(format!(
            "a scenario takes at most {MAX_IMAGE_STEPS} screenshots and comparisons"
        )));
    }
    Ok(Plan {
        stop_on_failure: boolean(arguments, "stopOnFailure")?.unwrap_or(true),
        max_duration_ms: integer(arguments, "maxDurationMs", 1, MAX_DURATION_MS)?
            .unwrap_or(DEFAULT_DURATION_MS),
        steps,
    })
}

/// An image comparison's arguments, checked: the reference's path, the
/// tolerance and whether a difference image is kept.
pub(super) fn comparison(
    arguments: &Map<String, Value>,
) -> Result<(String, u8, bool), OutcomeError> {
    only(
        arguments,
        &["session", "reference", "tolerance", "differenceImage"],
        "runtime.image.compare",
    )?;
    let reference = object(
        arguments
            .get("reference")
            .ok_or_else(|| invalid("`reference` is required"))?,
        "`reference`",
    )?;
    only(reference, &["path"], "`reference`")?;
    let path = reference
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("`reference` is {path}, a PNG image of the project"))?;
    Ok((
        reference_path(path)?,
        image_tolerance(arguments)?,
        boolean(arguments, "differenceImage")?.unwrap_or(true),
    ))
}

// --- the commands ------------------------------------------------------------------

impl SessionHandler {
    /// The bytes of the project PNG at `relative`, read through the held
    /// lease and checked to be a PNG of at most the largest reference size.
    fn reference_bytes(&self, relative: &str) -> Result<Vec<u8>, OutcomeError> {
        let bytes = self
            .engine
            .root()?
            .read(Path::new(relative))
            .map_err(|error| match error {
                godot_mcp_domain::safe_root::SafeRootError::StalePreimage { .. } => outcome_error(
                    ErrorKind::NotFound,
                    format!("res://{relative} does not exist"),
                ),
                error => outcome_error(
                    error.error_kind(),
                    format!("res://{relative} could not be read from the project"),
                ),
            })?;
        if bytes.len() as u64 > MAX_REFERENCE_BYTES {
            return Err(invalid(format!(
                "res://{relative} is {} bytes; a reference image is at most {MAX_REFERENCE_BYTES}",
                bytes.len()
            )));
        }
        if !bytes.starts_with(PNG_SIGNATURE) {
            return Err(invalid(format!("res://{relative} is not a PNG image")));
        }
        Ok(bytes)
    }

    /// Send one of this module's commands, journaled like every command to a
    /// game session.
    pub(super) async fn scenario_command(
        &self,
        call: &KernelToolCall,
    ) -> Result<Value, OutcomeError> {
        let tool = self.operation.tool_name();
        let idempotency_id = self.idempotency(call)?;
        let arguments = &call.arguments;
        let session = required_string(arguments, "session")?.to_owned();
        // The arguments are checked first; nothing they name is read yet.
        let mut payload = Map::new();
        let mut methods: BTreeSet<&str> = BTreeSet::new();
        let mut timeout = PROBE_TIMEOUT;
        let mut comparison_of = None;
        let mut planned = None;
        match self.operation {
            SessionOperation::TimeScaleSet => {
                only(arguments, &["session", "timeScale"], tool)?;
                payload.insert("time_scale".to_owned(), json!(time_scale(arguments, tool)?));
                methods.insert("runtime.properties.set");
            }
            SessionOperation::ImageCompare => {
                comparison_of = Some(comparison(arguments)?);
                methods.extend([
                    "runtime.image.reference",
                    "runtime.image.compare",
                    "runtime.capture.chunk",
                ]);
            }
            _ => {
                let plan = plan(arguments)?;
                for step in &plan.steps {
                    methods.extend(step.action.methods());
                }
                timeout = Duration::from_millis(plan.max_duration_ms) + PROBE_TIMEOUT;
                planned = Some(plan);
            }
        }
        // What the probe does not implement, or a session that renders
        // nothing for a comparison, is refused before anything is journaled.
        let bridge = self.sessions().running_game(&session)?;
        let status = bridge.status();
        if status.connected
            && let Some(missing) = methods
                .iter()
                .find(|method| !status.methods.iter().any(|known| known == *method))
        {
            return Err(outcome_error(
                ErrorKind::Unsupported,
                crate::editor::bridge::BridgeFailure::Unsupported((*missing).to_owned())
                    .probe_message(),
            ));
        }
        if comparison_of.is_some()
            && status
                .description
                .as_ref()
                .and_then(|description| description.get("renders"))
                .and_then(Value::as_bool)
                == Some(false)
        {
            return Err(outcome_error(
                ErrorKind::Unavailable,
                "this session renders nothing: it runs headless; start it with display window \
                 to compare images",
            ));
        }
        // Then every reference image is read, before anything is journaled.
        if let Some((relative, tolerance, difference)) = comparison_of {
            let bytes = self.reference_bytes(&relative)?;
            payload.insert(
                "reference".to_owned(),
                json!({
                    "path": format!("res://{relative}"),
                    "data_base64": crate::capture::base64_encode(&bytes),
                }),
            );
            payload.insert("tolerance".to_owned(), json!(tolerance));
            payload.insert("difference".to_owned(), json!(difference));
        }
        if let Some(plan) = planned {
            let mut references = Map::new();
            let mut total = 0_u64;
            for step in &plan.steps {
                if let Action::CompareImage {
                    reference: Reference::Path { path },
                    ..
                } = &step.action
                    && !references.contains_key(path)
                {
                    let relative = path.strip_prefix("res://").unwrap_or(path);
                    let bytes = self.reference_bytes(relative)?;
                    total += bytes.len() as u64;
                    if total > MAX_REFERENCE_TOTAL {
                        return Err(invalid(format!(
                            "the reference images of one scenario are at most \
                             {MAX_REFERENCE_TOTAL} bytes together"
                        )));
                    }
                    references.insert(path.clone(), json!(crate::capture::base64_encode(&bytes)));
                }
            }
            payload.insert(
                "plan".to_owned(),
                serde_json::to_value(&plan)
                    .map_err(|error| outcome_error(ErrorKind::Internal, error.to_string()))?,
            );
            payload.insert("references".to_owned(), Value::Object(references));
        }
        payload.insert("session".to_owned(), Value::String(session.clone()));
        payload.insert(
            "owner".to_owned(),
            json!({
                "principal": call.access.principal().as_str(),
            }),
        );
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
            project_wide: false,
            observe_all: false,
            expected_revision: None,
            opened_at_unix_ms: observed,
        };
        // What runs on the probe is withdrawn when the caller cancels or the
        // session is told to stop.
        let withdrawal = Withdrawal::new(&call.cancellation, self.sessions().stop_signal(&session));
        let command = Box::new(EditorMutation {
            action: action(self.operation).to_owned(),
            payload: Value::Object(payload).to_string(),
            idempotency_id,
            timeout,
            cancellation: withdrawal.signal(),
        });
        let report = self
            .route_report(
                call,
                transaction_id,
                canonical,
                RouteEffect::Runtime { intent, command },
                &session,
            )
            .await;
        drop(withdrawal);
        let report = report?;
        if self.operation == SessionOperation::TimeScaleSet
            && let Some(uncertain) = uncertain_scale(&report)
        {
            return Err(uncertain);
        }
        session_answer(&report, session)
    }
}

/// A time-scale write that was committed and reads back otherwise, as an
/// uncertain error that names the scale before, the one asked for and the one
/// read back.
fn uncertain_scale(report: &crate::router::RouteReport) -> Option<OutcomeError> {
    if report.replayed() {
        return None;
    }
    let outcome = report.editor_outcome()?;
    let (ErrorKind::Uncertain, message) = outcome.error.as_ref()? else {
        return None;
    };
    if outcome.status != MutationStatus::Committed {
        return None;
    }
    let mut details = BTreeMap::new();
    details.insert(
        "transaction_id".to_owned(),
        GodotValue::String(report.transaction_id().unwrap_or_default().to_owned()),
    );
    details.insert(
        "mutation_status".to_owned(),
        GodotValue::String(outcome.status.as_str().to_owned()),
    );
    for key in ["before", "requested", "after"] {
        details.insert(
            key.to_owned(),
            outcome
                .payload
                .get(key)
                .and_then(Value::as_f64)
                .map_or(GodotValue::Null, GodotValue::Float),
        );
    }
    Some(OutcomeError::new(
        ErrorKind::Uncertain,
        message.clone(),
        false,
        details,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(value: Value) -> Map<String, Value> {
        value.as_object().cloned().expect("object")
    }

    fn advertised(tool: &str) -> Value {
        Value::Object(crate::kernel::tool_schema(
            tool,
            crate::core::CoreLimits::default(),
        ))
    }

    const SESSION: &str = "00000000-0000-0000-0000-000000000000";

    /// Each case: arguments without the session and the idempotency id, the
    /// schema's verdict and the handler's verdict.
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
            let mut handed = map(arguments.clone());
            handed.remove("idempotencyId");
            assert_eq!(
                handler(&handed),
                *handler_verdict,
                "the {tool} handler's verdict on {arguments}"
            );
        }
    }

    fn scenario(steps: Value) -> Value {
        json!({ "steps": steps })
    }

    #[test]
    fn the_time_scale_schema_and_check_agree() {
        verdicts(
            "runtime.time_scale.set",
            &[
                (json!({ "timeScale": 1 }), true, true),
                (json!({ "timeScale": 0 }), true, true),
                (json!({ "timeScale": 0.25 }), true, true),
                (json!({ "timeScale": 100 }), true, true),
                (json!({ "timeScale": 100.5 }), false, false),
                (json!({ "timeScale": -1 }), false, false),
                (json!({ "timeScale": "2" }), false, false),
                (json!({}), false, false),
                (json!({ "timeScale": 1, "persist": true }), false, false),
            ],
            |arguments| {
                only(arguments, &["session", "timeScale"], "t").is_ok()
                    && time_scale(arguments, "t").is_ok()
            },
        );
    }

    #[test]
    fn the_comparison_schema_and_check_agree() {
        let reference = |path: &str| json!({ "reference": { "path": path } });
        verdicts(
            "runtime.image.compare",
            &[
                (reference("res://ref/menu.png"), true, true),
                (reference("ref/MENU.PNG"), true, true),
                (reference("res://a b/.hidden.png"), true, true),
                (
                    json!({ "reference": { "path": "res://a.png" }, "tolerance": 8, "differenceImage": false }),
                    true,
                    true,
                ),
                (
                    json!({ "reference": { "path": "res://a.png" }, "tolerance": 256 }),
                    false,
                    false,
                ),
                (reference("res://a.jpg"), false, false),
                (reference("res://../escape.png"), false, false),
                (reference("res://a//b.png"), false, false),
                (reference("res://.godot/imported/a.png"), false, false),
                (reference("res://.godot-mcp/a.png"), false, false),
                (reference("godot_mcp_helpers/a.png"), false, false),
                (reference("res://.godotish/a.png"), true, true),
                (reference("a\\b.png"), false, false),
                (reference("res://res://a.png"), false, false),
                (
                    json!({ "reference": { "path": "res://a.png", "step": "x" } }),
                    false,
                    false,
                ),
                (json!({ "reference": { "step": "x" } }), false, false),
                (json!({}), false, false),
            ],
            |arguments| comparison(arguments).is_ok(),
        );
    }

    fn property_assertion(op: &str, value: Value) -> Value {
        json!({ "kind": "assert_property", "target": { "node": "Player", "property": "position" }, "op": op, "value": value })
    }

    #[test]
    fn the_scenario_schema_and_check_agree() {
        let vector = json!({ "type": "vector2", "value": { "x": 1.0, "y": 2.0 } });
        let cases = [
            (scenario(json!([{ "kind": "screenshot" }])), true, true),
            (
                scenario(
                    json!([{ "kind": "screenshot", "id": "menu" }, { "kind": "compare_image", "reference": { "step": "menu" } }]),
                ),
                true,
                true,
            ),
            (
                scenario(
                    json!([{ "kind": "compare_image", "reference": { "path": "res://menu.png" }, "tolerance": 2 }]),
                ),
                true,
                true,
            ),
            (
                scenario(
                    json!([{ "kind": "input", "steps": [{ "kind": "action", "action": "jump" }, { "kind": "wait", "ms": 100 }] }]),
                ),
                true,
                true,
            ),
            (
                scenario(
                    json!([{ "kind": "wait", "condition": { "node_exists": { "node": "Boss" } }, "timeoutMs": 2000 }]),
                ),
                true,
                true,
            ),
            (
                scenario(
                    json!([{ "kind": "set_property", "target": { "node": "Player", "properties": { "visible": { "type": "bool", "value": false } } } }]),
                ),
                true,
                true,
            ),
            (
                scenario(json!([{ "kind": "press", "target": { "text": "Start" }, "holdMs": 50 }])),
                true,
                true,
            ),
            (
                scenario(json!([{ "kind": "time_scale", "timeScale": 0.5 }])),
                true,
                true,
            ),
            (
                scenario(json!([property_assertion("approx", vector.clone())])),
                true,
                true,
            ),
            (
                scenario(
                    json!([{ "kind": "assert_property", "target": { "singleton": "Engine", "property": "time_scale" }, "op": "equals", "value": { "type": "float", "value": 2.0 } }]),
                ),
                true,
                true,
            ),
            (
                scenario(
                    json!([{ "kind": "assert_ui_text", "node": "HUD/Score", "op": "contains", "text": "10" }]),
                ),
                true,
                true,
            ),
            (
                scenario(json!([{ "kind": "assert_ui_text", "node": "HUD/Score", "text": "" }])),
                true,
                true,
            ),
            (
                scenario(
                    json!([{ "kind": "stress", "scene": "res://bullet.tscn", "count": 100, "parent": "Bullets", "durationMs": 500, "intervalMs": 50 }]),
                ),
                true,
                true,
            ),
            (
                json!({ "steps": [{ "kind": "screenshot" }], "stopOnFailure": false, "maxDurationMs": 1000 }),
                true,
                true,
            ),
            // Refused by both.
            (scenario(json!([])), false, false),
            (scenario(json!([{ "kind": "teleport" }])), false, false),
            (
                scenario(json!([{ "kind": "screenshot", "colour": "red" }])),
                false,
                false,
            ),
            (
                scenario(json!([{ "kind": "screenshot", "id": "-bad" }])),
                false,
                false,
            ),
            (
                scenario(json!([{ "kind": "time_scale", "timeScale": 101 }])),
                false,
                false,
            ),
            (
                scenario(json!([property_assertion("about", vector.clone())])),
                false,
                false,
            ),
            (
                scenario(json!([property_assertion(
                    "equals",
                    json!({ "kind": "float", "value": 1 })
                )])),
                false,
                false,
            ),
            (
                scenario(
                    json!([{ "kind": "assert_property", "target": { "singleton": "Engine", "property": "fps" }, "op": "equals", "value": { "type": "float", "value": 2.0 } }]),
                ),
                false,
                false,
            ),
            (
                scenario(
                    json!([{ "kind": "assert_property", "target": { "node": "A", "singleton": "Engine", "property": "time_scale" }, "op": "equals", "value": { "type": "float", "value": 2.0 } }]),
                ),
                false,
                false,
            ),
            (
                scenario(
                    json!([{ "kind": "assert_ui_text", "node": "HUD/Score", "op": "starts_with", "text": "1" }]),
                ),
                false,
                false,
            ),
            (
                scenario(
                    json!([{ "kind": "compare_image", "reference": { "path": "res://menu.jpg" } }]),
                ),
                false,
                false,
            ),
            (
                scenario(json!([{ "kind": "stress", "scene": "res://bullet.gd", "count": 1 }])),
                false,
                false,
            ),
            (
                scenario(
                    json!([{ "kind": "stress", "scene": "res://../bullet.tscn", "count": 1 }]),
                ),
                false,
                false,
            ),
            (
                scenario(json!([{ "kind": "stress", "scene": "levels/bullet.scn", "count": 1 }])),
                true,
                true,
            ),
            (
                scenario(
                    json!([{ "kind": "stress", "scene": "res://bullet.tscn", "count": 1001 }]),
                ),
                false,
                false,
            ),
            (
                scenario(json!([{ "kind": "wait", "condition": {} }])),
                false,
                false,
            ),
            (
                json!({ "steps": [{ "kind": "screenshot" }], "maxDurationMs": 600001 }),
                false,
                false,
            ),
            // What a schema cannot state: unique ids, a comparison with an
            // earlier screenshot, the total wait of an input step and the
            // number of image steps.
            (
                scenario(
                    json!([{ "kind": "screenshot", "id": "same" }, { "kind": "screenshot", "id": "same" }]),
                ),
                true,
                false,
            ),
            (
                scenario(
                    json!([{ "kind": "compare_image", "reference": { "step": "later" } }, { "kind": "screenshot", "id": "later" }]),
                ),
                true,
                false,
            ),
            (
                scenario(
                    json!([{ "kind": "time_scale", "id": "speed", "timeScale": 1 }, { "kind": "compare_image", "reference": { "step": "speed" } }]),
                ),
                true,
                false,
            ),
            (
                scenario(
                    json!([{ "kind": "input", "steps": [{ "kind": "wait", "ms": 6000 }, { "kind": "wait", "ms": 6000 }] }]),
                ),
                true,
                false,
            ),
            (
                scenario(json!(vec![json!({ "kind": "screenshot" }); 17])),
                true,
                false,
            ),
        ];
        verdicts("test.scenario.run", &cases, |arguments| {
            plan(arguments).is_ok()
        });
    }

    /// An input step of a scenario names its action and key as
    /// `input.sequence.send`'s steps do: an action in 1 to 256 characters and
    /// a key in 1 to 32, whatever the bytes each takes, neither with a
    /// control character.
    #[test]
    fn a_scenario_input_step_names_actions_and_keys_as_a_sequence_does() {
        use crate::schema::conformance::{NameRule, name_cases};
        let action = NameRule {
            limit: 256,
            empty: false,
            control: true,
        };
        let key = NameRule {
            limit: 32,
            ..action
        };
        let input = |step: Value| scenario(json!([{ "kind": "input", "steps": [step] }]));
        let mut cases = Vec::new();
        for (name, expected) in name_cases(action, "", "") {
            cases.push((
                input(json!({ "kind": "action", "action": name })),
                expected,
                expected,
            ));
        }
        for (name, expected) in name_cases(key, "", "") {
            cases.push((
                input(json!({ "kind": "key", "key": name })),
                expected,
                expected,
            ));
        }
        verdicts("test.scenario.run", &cases, |arguments| {
            plan(arguments).is_ok()
        });
    }

    #[test]
    fn a_plan_is_spelled_for_the_probe() {
        let plan = plan(&map(json!({
            "session": SESSION,
            "steps": [
                { "kind": "input", "steps": [{ "kind": "action", "action": "jump", "holdMs": 100 }] },
                { "kind": "assert_property", "id": "near", "target": { "node": "Player", "property": "position" }, "op": "approx", "value": { "type": "vector2", "value": { "x": 1, "y": 2 } }, "tolerance": 0.5 },
                { "kind": "assert_ui_text", "node": "HUD/Score", "op": "contains", "text": "10" },
                { "kind": "stress", "scene": "bullet.tscn", "count": 3 },
                { "kind": "wait", "condition": { "node_gone": { "node": "Boss" } } },
            ],
            "stopOnFailure": false
        })))
        .expect("a valid plan");
        assert!(!plan.stop_on_failure);
        assert_eq!(plan.max_duration_ms, DEFAULT_DURATION_MS);
        let ids: Vec<&str> = plan
            .steps
            .iter()
            .map(|planned| planned.id.as_str())
            .collect();
        assert_eq!(ids, ["step-1", "near", "step-3", "step-4", "step-5"]);
        let Action::Input { steps, wait_ms } = &plan.steps[0].action else {
            panic!("an input step");
        };
        assert_eq!(
            steps[0],
            json!({ "kind": "action", "action": "jump", "state": "tap", "hold_ms": 100 })
        );
        assert_eq!(*wait_ms, 0);
        let Action::AssertProperty { request, expected } = &plan.steps[1].action else {
            panic!("a property assertion");
        };
        assert_eq!(request["op"], json!("approx"), "the probe's spelling");
        assert_eq!(request["kind"], json!("property"));
        assert_eq!(expected["op"], json!("approx"), "the caller's spelling");
        assert_eq!(expected["tolerance"], json!(0.5));
        let Action::AssertUiText { request, .. } = &plan.steps[2].action else {
            panic!("a text assertion");
        };
        assert_eq!(request["match"], json!("contains"));
        let Action::Stress {
            request,
            count,
            duration_ms,
        } = &plan.steps[3].action
        else {
            panic!("a stress window");
        };
        assert_eq!(request["scene"], json!("res://bullet.tscn"));
        assert_eq!(request["parent"], json!("."));
        assert_eq!((*count, *duration_ms), (3, DEFAULT_STRESS_MS));
        let Action::Wait {
            request,
            timeout_ms,
        } = &plan.steps[4].action
        else {
            panic!("a wait");
        };
        assert_eq!(
            request["timeout_ms"],
            json!(5000),
            "runtime.wait's own default"
        );
        assert_eq!(*timeout_ms, 5000);
    }

    #[test]
    fn the_bounds_are_the_documented_ones() {
        assert_eq!(MAX_STEPS, 64);
        assert_eq!(MAX_IMAGE_STEPS, 16);
        assert_eq!(MAX_DURATION_MS, 600_000);
        let schema = advertised("test.scenario.run");
        assert_eq!(schema["properties"]["steps"]["maxItems"], json!(MAX_STEPS));
        assert_eq!(
            schema["properties"]["maxDurationMs"]["maximum"],
            json!(MAX_DURATION_MS)
        );
        let too_many = json!({ "session": SESSION, "steps": vec![json!({ "kind": "time_scale", "timeScale": 1 }); MAX_STEPS + 1] });
        assert!(plan(&map(too_many)).is_err());
        let most = json!({ "session": SESSION, "steps": (0..MAX_STEPS).map(|index| json!({ "kind": "time_scale", "id": format!("s{index}"), "timeScale": 1 })).collect::<Vec<_>>() });
        assert!(plan(&map(most)).is_ok());
        assert_eq!(
            advertised("runtime.time_scale.set")["properties"]["timeScale"]["maximum"],
            json!(MAX_TIME_SCALE)
        );
    }

    #[test]
    fn every_scenario_step_kind_has_a_schema_variant() {
        let schema = advertised("test.scenario.run");
        let kinds: Vec<&str> = schema["properties"]["steps"]["items"]["oneOf"]
            .as_array()
            .expect("variants")
            .iter()
            .map(|variant| {
                variant["properties"]["kind"]["const"]
                    .as_str()
                    .expect("kind")
            })
            .collect();
        assert_eq!(kinds, STEP_KINDS);
    }
}
