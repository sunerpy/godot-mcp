//! Stepping a running game by physics frames (ADR 0017).
//!
//! `runtime.frames.step` advances a managed game session by a counted number
//! of physics frames and then holds it paused or lets it run on. Its
//! advertised schema and the check here give the same verdict on the same
//! arguments; the probe decides what the game's own pause allows and answers
//! what it advanced.

use std::time::Duration;

use godot_mcp_domain::contracts::OutcomeError;
use serde_json::{Map, Value, json};

use super::{SessionOperation, invalid};

/// The most physics frames one step advances: 10 s at Godot's default 60
/// physics frames per second.
const MAX_FRAMES: u64 = 600;
/// The longest a step may take, and its default. 600 frames take 10 s at 60
/// per second and 20 s at 30, so the default leaves a full step room on a
/// game that ticks at half the default rate.
const MAX_STEP_MS: u64 = 30_000;
const DEFAULT_STEP_MS: u64 = 20_000;
/// What a step does once its frames have advanced.
const THEN: &[&str] = &["hold", "resume"];

/// Whether `operation` is this module's tool.
pub(super) const fn handles(operation: SessionOperation) -> bool {
    matches!(operation, SessionOperation::FramesStep)
}

/// The schema properties of `runtime.frames.step` and the ones it requires,
/// beside the idempotency id a command also requires.
pub(super) fn properties(session: &Value) -> (Value, &'static [&'static str]) {
    (
        json!({
            "session": session,
            "frames": {
                "type": "integer",
                "minimum": 0,
                "maximum": MAX_FRAMES,
                "description": "How many physics frames the game advances, 0 to 600. 0 holds a \
                                running game at its next physics frame, or, with `then` resume, \
                                releases a hold."
            },
            "then": {
                "type": "string",
                "enum": THEN,
                "description": "hold (the default) keeps the game paused from the start of the \
                                physics frame after the last one; resume lets it run on."
            },
            "timeoutMs": {
                "type": "integer",
                "minimum": 1,
                "maximum": MAX_STEP_MS,
                "description": "End the step after this many ms (default 20000), holding the \
                                game, with the frames it advanced by then."
            }
        }),
        &["session", "frames"],
    )
}

/// A whole number the schema's `integer` accepts, within `low..=high`.
fn bounded(
    arguments: &Map<String, Value>,
    key: &str,
    low: u64,
    high: u64,
) -> Result<Option<u64>, OutcomeError> {
    let Some(value) = arguments.get(key) else {
        return Ok(None);
    };
    let whole = value.as_u64().or_else(|| {
        value
            .as_f64()
            .filter(|number| number.fract() == 0.0 && *number >= 0.0 && *number <= 1.0e15)
            .map(|number| number as u64)
    });
    whole
        .filter(|number| (low..=high).contains(number))
        .map(Some)
        .ok_or_else(|| invalid(format!("`{key}` must be an integer from {low} to {high}")))
}

/// The probe payload of `runtime.frames.step`, checked, and the time the
/// probe may take beyond an ordinary request.
pub(super) fn payload(
    arguments: &Map<String, Value>,
) -> Result<(Map<String, Value>, Duration), OutcomeError> {
    if let Some(unknown) = arguments
        .keys()
        .find(|key| !["session", "frames", "then", "timeoutMs"].contains(&key.as_str()))
    {
        return Err(invalid(format!(
            "runtime.frames.step does not accept `{unknown}`"
        )));
    }
    let frames = bounded(arguments, "frames", 0, MAX_FRAMES)?
        .ok_or_else(|| invalid("`frames` is required: how many physics frames to advance"))?;
    let then = match arguments.get("then") {
        None => "hold",
        Some(value) => value
            .as_str()
            .filter(|then| THEN.contains(then))
            .ok_or_else(|| invalid("`then` is hold or resume"))?,
    };
    let timeout = bounded(arguments, "timeoutMs", 1, MAX_STEP_MS)?.unwrap_or(DEFAULT_STEP_MS);
    let mut out = Map::new();
    out.insert("frames".to_owned(), json!(frames));
    out.insert("then".to_owned(), json!(then));
    out.insert("timeout_ms".to_owned(), json!(timeout));
    Ok((out, Duration::from_millis(timeout)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: &str = "00000000-0000-0000-0000-000000000000";

    fn advertised() -> Value {
        Value::Object(crate::kernel::tool_schema(
            "runtime.frames.step",
            crate::core::CoreLimits::default(),
        ))
    }

    /// The advertised schema and the check give each case the stated
    /// verdict: (arguments without the session and the idempotency id, the
    /// verdict both give).
    fn agree(cases: &[(Value, bool)]) {
        let schema = advertised();
        for (case, verdict) in cases {
            let mut arguments = case.as_object().cloned().expect("a case is an object");
            arguments.insert("session".to_owned(), json!(SESSION));
            let mut advertised_arguments = arguments.clone();
            advertised_arguments.insert("idempotencyId".to_owned(), json!("step-1"));
            assert_eq!(
                crate::schema::conformance::conforms(&schema, &Value::Object(advertised_arguments)),
                *verdict,
                "the runtime.frames.step schema's verdict on {case}"
            );
            let checked = payload(&arguments);
            assert_eq!(
                checked.is_ok(),
                *verdict,
                "the runtime.frames.step check's verdict on {case}: {checked:?}"
            );
        }
    }

    #[test]
    fn the_step_tool_is_strict_and_typed() {
        let schema = advertised();
        assert_eq!(schema["additionalProperties"], json!(false));
        assert_eq!(
            schema["required"],
            json!(["session", "frames", "idempotencyId"])
        );
        for (key, property) in schema["properties"].as_object().expect("properties") {
            assert!(property["type"].is_string(), "{key} needs a type");
            assert!(
                property["description"].is_string(),
                "{key} needs a description"
            );
        }
        for keyword in ["oneOf", "anyOf", "allOf", "if", "$defs"] {
            assert!(
                schema.get(keyword).is_none(),
                "the schema carries {keyword}"
            );
        }
    }

    #[test]
    fn a_step_advances_0_to_600_frames_and_holds_or_resumes() {
        agree(&[
            (json!({ "frames": 0 }), true),
            (json!({ "frames": 1 }), true),
            (
                json!({ "frames": 600, "then": "hold", "timeoutMs": 30000 }),
                true,
            ),
            (
                json!({ "frames": 3.0, "then": "resume", "timeoutMs": 1 }),
                true,
            ),
            (json!({}), false),
            (json!({ "frames": -1 }), false),
            (json!({ "frames": 601 }), false),
            (json!({ "frames": 1.5 }), false),
            (json!({ "frames": "3" }), false),
            (json!({ "frames": 1, "then": "pause" }), false),
            (json!({ "frames": 1, "then": null }), false),
            (json!({ "frames": 1, "timeoutMs": 0 }), false),
            (json!({ "frames": 1, "timeoutMs": 30001 }), false),
            (json!({ "frames": 1, "intervalMs": 10 }), false),
        ]);
    }

    #[test]
    fn a_step_is_sent_with_its_defaults_and_waits_for_its_timeout() {
        let mut arguments = Map::new();
        arguments.insert("session".to_owned(), json!(SESSION));
        arguments.insert("frames".to_owned(), json!(8));
        let (sent, extra) = payload(&arguments).expect("valid");
        assert_eq!(
            Value::Object(sent),
            json!({ "frames": 8, "then": "hold", "timeout_ms": 20000 })
        );
        assert_eq!(extra, Duration::from_millis(20_000));

        arguments.insert("then".to_owned(), json!("resume"));
        arguments.insert("timeoutMs".to_owned(), json!(500));
        let (sent, extra) = payload(&arguments).expect("valid");
        assert_eq!(
            Value::Object(sent),
            json!({ "frames": 8, "then": "resume", "timeout_ms": 500 })
        );
        assert_eq!(extra, Duration::from_millis(500));
    }
}
