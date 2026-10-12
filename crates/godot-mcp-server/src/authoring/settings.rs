//! `project.settings.get` and `project.settings.patch`: typed project
//! settings over `project.godot`.
//!
//! The file is edited as a document, not regenerated: each typed operation
//! replaces, inserts or removes exactly the lines of the one setting it names,
//! so unrelated keys, comments and ordering survive. The result is re-parsed
//! and validated before it is published with compare-and-swap, and the
//! read-your-writes barrier reloads the parsed settings into the revision the
//! patch returns.

use godot_mcp_analysis::project::ProjectSettings;
use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::catalog::AuthoringOperation;
use godot_mcp_domain::contracts::OutcomeError;
use serde_json::{Map, Value, json};

use super::diff::unified;
use super::paths;
use super::{
    AuthoringRuntime, CallContext, Envelope, Mutation, object_schema, outcome_error,
    precondition_mismatch, sha256_argument, sha256_hex, sha256_property, utf8_text,
};
use crate::effect::{PlannedWrite, WritePrecondition};
use crate::journal::FileOperation;

const MANIFEST: &str = "project.godot";
/// Most operations one patch may apply.
const MAX_OPERATIONS: usize = 64;
/// Most events one input action may bind.
const MAX_EVENTS: usize = 16;
/// The deadzone Godot gives a new action.
const DEFAULT_DEADZONE: f64 = 0.2;

// ---------------------------------------------------------------------------
// The document model
// ---------------------------------------------------------------------------

/// One `key=value` assignment and the lines it spans.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    section: Option<String>,
    key: String,
    start: usize,
    end: usize,
    value: String,
}

/// `project.godot` as lines, with every section header and assignment located.
#[derive(Debug, Clone)]
struct Document {
    lines: Vec<String>,
    headers: Vec<(String, usize)>,
    entries: Vec<Entry>,
    eol: &'static str,
}

/// Net bracket depth a line contributes outside string literals.
fn depth_delta(line: &str) -> i64 {
    let mut depth = 0_i64;
    let mut in_string = false;
    let mut escaped = false;
    for character in line.chars() {
        if in_string {
            match character {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match character {
            '"' => in_string = true,
            '{' | '[' | '(' => depth += 1,
            '}' | ']' | ')' => depth -= 1,
            _ => {}
        }
    }
    depth
}

fn strip_eol(line: &str) -> &str {
    let line = line.strip_suffix('\n').unwrap_or(line);
    line.strip_suffix('\r').unwrap_or(line)
}

impl Document {
    fn parse(text: &str) -> Self {
        let lines: Vec<String> = text.split_inclusive('\n').map(str::to_owned).collect();
        let terminated = text.matches('\n').count();
        let eol = if terminated > 0 && text.matches("\r\n").count() == terminated {
            "\r\n"
        } else {
            "\n"
        };
        let mut headers = Vec::new();
        let mut entries = Vec::new();
        let mut section: Option<String> = None;
        let mut index = 0;
        while index < lines.len() {
            let trimmed = strip_eol(&lines[index]).trim();
            if trimmed.is_empty() || trimmed.starts_with(';') || trimmed.starts_with('#') {
                index += 1;
                continue;
            }
            if trimmed.starts_with('[') && trimmed.ends_with(']') {
                let name = trimmed[1..trimmed.len() - 1].trim().to_owned();
                headers.push((name.clone(), index));
                section = Some(name);
                index += 1;
                continue;
            }
            let Some((key, first)) = trimmed.split_once('=') else {
                index += 1;
                continue;
            };
            let mut value = first.trim().to_owned();
            let mut depth = depth_delta(first);
            let start = index;
            index += 1;
            while depth > 0 && index < lines.len() {
                let continuation = strip_eol(&lines[index]);
                value.push('\n');
                value.push_str(continuation);
                depth += depth_delta(continuation);
                index += 1;
            }
            entries.push(Entry {
                section: section.clone(),
                key: key.trim().to_owned(),
                start,
                end: index,
                value,
            });
        }
        Self {
            lines,
            headers,
            entries,
            eol,
        }
    }

    fn render(&self) -> String {
        self.lines.concat()
    }

    fn find(&self, section: &str, key: &str) -> Option<&Entry> {
        self.entries
            .iter()
            .find(|entry| entry.section.as_deref() == Some(section) && entry.key == key)
    }

    fn assignment_lines(&self, key: &str, value: &str) -> Vec<String> {
        format!("{key}={value}")
            .split('\n')
            .map(|line| format!("{line}{}", self.eol))
            .collect()
    }

    /// Replace or insert one assignment, then re-locate everything.
    fn set(&mut self, section: &str, key: &str, value: &str) {
        let replacement = self.assignment_lines(key, value);
        if let Some(entry) = self.find(section, key).cloned() {
            self.lines.splice(entry.start..entry.end, replacement);
        } else if let Some(&(_, header)) = self.headers.iter().find(|(name, _)| name == section) {
            let last = self
                .entries
                .iter()
                .filter(|entry| entry.section.as_deref() == Some(section))
                .map(|entry| entry.end)
                .max();
            let at = match last {
                Some(end) => end,
                None => {
                    let next = header + 1;
                    if self
                        .lines
                        .get(next)
                        .is_some_and(|line| strip_eol(line).trim().is_empty())
                    {
                        next + 1
                    } else {
                        self.lines.insert(next, self.eol.to_owned());
                        next + 1
                    }
                }
            };
            self.lines.splice(at..at, replacement);
        } else {
            if let Some(last) = self.lines.last_mut()
                && !last.ends_with('\n')
            {
                last.push_str(self.eol);
            }
            if !self.lines.is_empty() {
                self.lines.push(self.eol.to_owned());
            }
            self.lines.push(format!("[{section}]{}", self.eol));
            self.lines.push(self.eol.to_owned());
            self.lines.extend(replacement);
        }
        *self = Self::parse(&self.render());
    }

    /// Remove one assignment; `false` when it was absent.
    fn remove(&mut self, section: &str, key: &str) -> bool {
        let Some(entry) = self.find(section, key).cloned() else {
            return false;
        };
        self.lines.drain(entry.start..entry.end);
        *self = Self::parse(&self.render());
        true
    }
}

// ---------------------------------------------------------------------------
// Godot value spelling
// ---------------------------------------------------------------------------

/// A Godot string literal.
fn godot_string(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('"');
    for character in text.chars() {
        match character {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            '\t' => quoted.push_str("\\t"),
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    quoted
}

/// The unquoted text of a simple Godot string literal.
fn unquote(raw: &str) -> Option<String> {
    let inner = raw.trim().strip_prefix('"')?.strip_suffix('"')?;
    let mut text = String::with_capacity(inner.len());
    let mut characters = inner.chars();
    while let Some(character) = characters.next() {
        if character == '\\' {
            match characters.next()? {
                'n' => text.push('\n'),
                't' => text.push('\t'),
                other => text.push(other),
            }
        } else {
            text.push(character);
        }
    }
    Some(text)
}

fn godot_float(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{value:.1}")
    } else {
        format!("{value}")
    }
}

/// The strings of a `PackedStringArray(...)` literal.
fn packed_strings(raw: &str) -> Vec<String> {
    let Some(inner) = raw
        .trim()
        .strip_prefix("PackedStringArray(")
        .and_then(|rest| rest.strip_suffix(')'))
    else {
        return Vec::new();
    };
    let mut values = Vec::new();
    let mut current = String::new();
    let mut in_string = false;
    let mut escaped = false;
    for character in inner.chars() {
        if in_string {
            current.push(character);
            match character {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
        } else if character == '"' {
            in_string = true;
            current.push(character);
        } else if character == ',' {
            if let Some(value) = unquote(&current) {
                values.push(value);
            }
            current.clear();
        } else if !character.is_whitespace() {
            current.push(character);
        }
    }
    if let Some(value) = unquote(&current) {
        values.push(value);
    }
    values
}

fn packed_string_array(values: &[String]) -> String {
    let quoted: Vec<String> = values.iter().map(|value| godot_string(value)).collect();
    format!("PackedStringArray({})", quoted.join(", "))
}

// ---------------------------------------------------------------------------
// Keys, buttons and input events
// ---------------------------------------------------------------------------

/// Godot's `KEY_SPECIAL`: special keys sit above it, printable keys are their
/// Unicode code point.
const KEY_SPECIAL: i64 = 1 << 22;

/// Every named key this tool accepts beside letters, digits and F1-F12, by
/// lowercase name, with its Godot `Key` code.
const NAMED_KEYS: &[(&str, i64)] = &[
    ("space", 32),
    ("escape", KEY_SPECIAL + 0x01),
    ("esc", KEY_SPECIAL + 0x01),
    ("tab", KEY_SPECIAL + 0x02),
    ("backspace", KEY_SPECIAL + 0x04),
    ("enter", KEY_SPECIAL + 0x05),
    ("return", KEY_SPECIAL + 0x05),
    ("kp_enter", KEY_SPECIAL + 0x06),
    ("insert", KEY_SPECIAL + 0x07),
    ("delete", KEY_SPECIAL + 0x08),
    ("home", KEY_SPECIAL + 0x0D),
    ("end", KEY_SPECIAL + 0x0E),
    ("left", KEY_SPECIAL + 0x0F),
    ("up", KEY_SPECIAL + 0x10),
    ("right", KEY_SPECIAL + 0x11),
    ("down", KEY_SPECIAL + 0x12),
    ("pageup", KEY_SPECIAL + 0x13),
    ("page_up", KEY_SPECIAL + 0x13),
    ("pagedown", KEY_SPECIAL + 0x14),
    ("page_down", KEY_SPECIAL + 0x14),
    ("shift", KEY_SPECIAL + 0x15),
    ("ctrl", KEY_SPECIAL + 0x16),
    ("control", KEY_SPECIAL + 0x16),
    ("meta", KEY_SPECIAL + 0x17),
    ("alt", KEY_SPECIAL + 0x18),
];

/// Every mouse button this tool names, by lowercase name, with its Godot
/// `MouseButton` index.
const MOUSE_BUTTONS: &[(&str, i64)] = &[
    ("left", 1),
    ("right", 2),
    ("middle", 3),
    ("wheel_up", 4),
    ("wheel_down", 5),
    ("wheel_left", 6),
    ("wheel_right", 7),
];

/// Godot `Key` codes for the named keys this tool accepts, in any letter case:
/// a letter or digit, F1-F12, or one of [`NAMED_KEYS`].
fn key_code(name: &str) -> Option<i64> {
    let lower = name.to_ascii_lowercase();
    if lower.len() == 1 {
        let character = lower.as_bytes()[0];
        return match character {
            b'a'..=b'z' => Some(i64::from(character.to_ascii_uppercase())),
            b'0'..=b'9' => Some(i64::from(character)),
            _ => None,
        };
    }
    if let Some(number) = lower.strip_prefix('f')
        && let Some(index) = [
            "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11", "12",
        ]
        .iter()
        .position(|spelled| *spelled == number)
    {
        return Some(KEY_SPECIAL + 0x1C + i64::try_from(index).expect("twelve keys"));
    }
    named(NAMED_KEYS, &lower)
}

/// The code `table` gives the lowercase `name`.
fn named(table: &[(&str, i64)], lower: &str) -> Option<i64> {
    table
        .iter()
        .find(|(entry, _)| *entry == lower)
        .map(|(_, code)| *code)
}

/// `name` as a pattern fragment that matches it in any letter case.
fn caseless(name: &str) -> String {
    name.chars()
        .map(|character| {
            if character.is_ascii_alphabetic() {
                format!(
                    "[{}{}]",
                    character.to_ascii_uppercase(),
                    character.to_ascii_lowercase()
                )
            } else {
                character.to_string()
            }
        })
        .collect()
}

/// A pattern matching exactly the names [`key_code`] accepts.
fn key_name_pattern() -> String {
    let mut names = vec!["[A-Za-z0-9]".to_owned(), "[Ff](?:[1-9]|1[0-2])".to_owned()];
    names.extend(NAMED_KEYS.iter().map(|(name, _)| caseless(name)));
    format!("^(?:{})$", names.join("|"))
}

/// A pattern matching exactly the names [`mouse_button`] accepts.
fn mouse_button_pattern() -> String {
    let names: Vec<String> = MOUSE_BUTTONS
        .iter()
        .map(|(name, _)| caseless(name))
        .collect();
    format!("^(?:{})$", names.join("|"))
}

/// The reverse of [`key_code`], for presenting bound events.
fn key_name(code: i64) -> Option<String> {
    const NAMES: &[&str] = &[
        "Space",
        "Escape",
        "Tab",
        "Backspace",
        "Enter",
        "Kp_Enter",
        "Insert",
        "Delete",
        "Home",
        "End",
        "Left",
        "Up",
        "Right",
        "Down",
        "PageUp",
        "PageDown",
        "Shift",
        "Ctrl",
        "Meta",
        "Alt",
        "F1",
        "F2",
        "F3",
        "F4",
        "F5",
        "F6",
        "F7",
        "F8",
        "F9",
        "F10",
        "F11",
        "F12",
    ];
    if (65..=90).contains(&code) || (48..=57).contains(&code) {
        return u8::try_from(code)
            .ok()
            .map(|byte| char::from(byte).to_string());
    }
    NAMES
        .iter()
        .find(|name| key_code(name) == Some(code))
        .map(|name| (*name).to_owned())
}

/// The index of a named mouse button, in any letter case.
fn mouse_button(name: &str) -> Option<i64> {
    named(MOUSE_BUTTONS, &name.to_ascii_lowercase())
}

fn integer_field(event: &Map<String, Value>, key: &str) -> Result<Option<i64>, OutcomeError> {
    match event.get(key) {
        None => Ok(None),
        Some(value) => value
            .as_i64()
            .map(Some)
            .ok_or_else(|| invalid(&format!("an input event's `{key}` must be an integer"))),
    }
}

fn flag(event: &Map<String, Value>, key: &str) -> Result<bool, OutcomeError> {
    match event.get(key) {
        None => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(invalid(&format!(
            "an input event's `{key}` must be a boolean"
        ))),
    }
}

fn check_event_fields(event: &Map<String, Value>, allowed: &[&str]) -> Result<(), OutcomeError> {
    if let Some(unknown) = event
        .keys()
        .find(|key| *key != "type" && !allowed.contains(&key.as_str()))
    {
        return Err(invalid(&format!(
            "this input event does not accept `{unknown}`"
        )));
    }
    Ok(())
}

/// One typed input event as a Godot `Object(...)` literal.
fn event_literal(event: &Value) -> Result<String, OutcomeError> {
    let event = event
        .as_object()
        .ok_or_else(|| invalid("every input event must be an object"))?;
    let kind = event
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("every input event needs a `type`"))?;
    match kind {
        "key" => {
            check_event_fields(event, declared_fields(EVENT_TYPES, kind).unwrap_or(&[]))?;
            let named = match event.get("key") {
                None => None,
                Some(Value::String(name)) => {
                    Some(key_code(name).ok_or_else(|| invalid(&format!("unknown key `{name}`")))?)
                }
                Some(_) => return Err(invalid("a key event's `key` must be a string")),
            };
            let physical = integer_field(event, "physicalKeycode")?.or(named);
            let logical = integer_field(event, "keycode")?;
            if physical.is_none() && logical.is_none() {
                return Err(invalid(
                    "a key event needs `key`, `physicalKeycode` or `keycode`",
                ));
            }
            Ok(format!(
                "Object(InputEventKey,\"resource_local_to_scene\":false,\"resource_name\":\"\",\
                 \"device\":-1,\"window_id\":0,\"alt_pressed\":{},\"shift_pressed\":{},\
                 \"ctrl_pressed\":{},\"meta_pressed\":{},\"pressed\":false,\"keycode\":{},\
                 \"physical_keycode\":{},\"key_label\":0,\"unicode\":0,\"location\":0,\
                 \"echo\":false,\"script\":null)",
                flag(event, "alt")?,
                flag(event, "shift")?,
                flag(event, "ctrl")?,
                flag(event, "meta")?,
                logical.unwrap_or(0),
                physical.unwrap_or(0),
            ))
        }
        "mouse_button" => {
            check_event_fields(event, declared_fields(EVENT_TYPES, kind).unwrap_or(&[]))?;
            let index = match (event.get("button"), integer_field(event, "buttonIndex")?) {
                (Some(Value::String(name)), None) => mouse_button(name)
                    .ok_or_else(|| invalid(&format!("unknown mouse button `{name}`")))?,
                (None, Some(index)) if (1..=9).contains(&index) => index,
                _ => {
                    return Err(invalid(
                        "a mouse event needs a `button` name or a `buttonIndex` from 1 to 9",
                    ));
                }
            };
            Ok(format!(
                "Object(InputEventMouseButton,\"resource_local_to_scene\":false,\
                 \"resource_name\":\"\",\"device\":-1,\"window_id\":0,\"alt_pressed\":false,\
                 \"shift_pressed\":false,\"ctrl_pressed\":false,\"meta_pressed\":false,\
                 \"button_mask\":0,\"position\":Vector2(0, 0),\"global_position\":Vector2(0, 0),\
                 \"factor\":1.0,\"button_index\":{index},\"canceled\":false,\"pressed\":false,\
                 \"double_click\":false,\"script\":null)"
            ))
        }
        "joypad_button" => {
            check_event_fields(event, declared_fields(EVENT_TYPES, kind).unwrap_or(&[]))?;
            let index = integer_field(event, "buttonIndex")?
                .filter(|index| (0..=127).contains(index))
                .ok_or_else(|| invalid("a joypad button event needs `buttonIndex` 0-127"))?;
            Ok(format!(
                "Object(InputEventJoypadButton,\"resource_local_to_scene\":false,\
                 \"resource_name\":\"\",\"device\":-1,\"button_index\":{index},\"pressure\":0.0,\
                 \"pressed\":false,\"script\":null)"
            ))
        }
        "joypad_motion" => {
            check_event_fields(event, declared_fields(EVENT_TYPES, kind).unwrap_or(&[]))?;
            let axis = integer_field(event, "axis")?
                .filter(|axis| (0..=9).contains(axis))
                .ok_or_else(|| invalid("a joypad motion event needs `axis` 0-9"))?;
            let value = event
                .get("axisValue")
                .and_then(Value::as_f64)
                .filter(|value| (-1.0..=1.0).contains(value) && *value != 0.0)
                .ok_or_else(|| {
                    invalid("a joypad motion event needs a non-zero `axisValue` in [-1, 1]")
                })?;
            Ok(format!(
                "Object(InputEventJoypadMotion,\"resource_local_to_scene\":false,\
                 \"resource_name\":\"\",\"device\":-1,\"axis\":{axis},\"axis_value\":{},\
                 \"script\":null)",
                godot_float(value)
            ))
        }
        other => Err(invalid(&format!(
            "unknown input event type `{other}`; use key, mouse_button, joypad_button or \
             joypad_motion"
        ))),
    }
}

/// The value of one `"name":value` property inside an `Object(...)` literal.
fn object_property<'a>(object: &'a str, name: &str) -> Option<&'a str> {
    let marker = format!("\"{name}\":");
    let start = object.find(&marker)? + marker.len();
    let rest = &object[start..];
    let end = rest.find([',', ')']).unwrap_or(rest.len());
    Some(rest[..end].trim())
}

/// Present the events of one action's dictionary literal.
fn action_value(name: &str, raw: &str) -> Value {
    let deadzone = raw
        .find("\"deadzone\":")
        .map(|start| &raw[start + "\"deadzone\":".len()..])
        .and_then(|rest| rest.split([',', '\n', '}']).next())
        .and_then(|number| number.trim().parse::<f64>().ok());
    let mut events = Vec::new();
    let mut rest = raw;
    while let Some(start) = rest.find("Object(") {
        let body = &rest[start + "Object(".len()..];
        let mut depth = 1_i64;
        let mut end = body.len();
        for (offset, character) in body.char_indices() {
            match character {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = offset;
                        break;
                    }
                }
                _ => {}
            }
        }
        let object = &body[..end];
        let class = object.split(',').next().unwrap_or_default().trim();
        let integer =
            |property: &str| object_property(object, property).and_then(|v| v.parse::<i64>().ok());
        events.push(match class {
            "InputEventKey" => {
                let physical = integer("physical_keycode").unwrap_or(0);
                let logical = integer("keycode").unwrap_or(0);
                json!({
                    "type": "key",
                    "physical_keycode": physical,
                    "keycode": logical,
                    "key": key_name(if physical != 0 { physical } else { logical }),
                })
            }
            "InputEventMouseButton" => json!({
                "type": "mouse_button",
                "button_index": integer("button_index"),
            }),
            "InputEventJoypadButton" => json!({
                "type": "joypad_button",
                "button_index": integer("button_index"),
            }),
            "InputEventJoypadMotion" => json!({
                "type": "joypad_motion",
                "axis": integer("axis"),
                "axis_value": object_property(object, "axis_value")
                    .and_then(|value| value.parse::<f64>().ok()),
            }),
            other => json!({ "type": "other", "class": other }),
        });
        rest = &body[end.min(body.len())..];
    }
    json!({ "name": name, "deadzone": deadzone, "events": events })
}

// ---------------------------------------------------------------------------
// Allowlisted scalar values
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ValueKind {
    Text,
    ResourcePath,
    Integer { min: i64, max: i64 },
    Float,
    Bool,
    Choice(&'static [&'static str]),
}

const RENDERING_METHODS: &[&str] = &["forward_plus", "mobile", "gl_compatibility"];

/// The scalar settings `set_value` may change, by full setting name.
const ALLOWLIST: &[(&str, ValueKind)] = &[
    ("application/config/name", ValueKind::Text),
    ("application/config/description", ValueKind::Text),
    ("application/config/version", ValueKind::Text),
    ("application/config/icon", ValueKind::ResourcePath),
    (
        "application/run/max_fps",
        ValueKind::Integer { min: 0, max: 1000 },
    ),
    (
        "display/window/size/viewport_width",
        ValueKind::Integer { min: 1, max: 16384 },
    ),
    (
        "display/window/size/viewport_height",
        ValueKind::Integer { min: 1, max: 16384 },
    ),
    (
        "display/window/size/mode",
        ValueKind::Integer { min: 0, max: 4 },
    ),
    ("display/window/size/resizable", ValueKind::Bool),
    (
        "display/window/stretch/mode",
        ValueKind::Choice(&["disabled", "canvas_items", "viewport"]),
    ),
    (
        "display/window/stretch/aspect",
        ValueKind::Choice(&["ignore", "keep", "keep_width", "keep_height", "expand"]),
    ),
    (
        "rendering/renderer/rendering_method",
        ValueKind::Choice(RENDERING_METHODS),
    ),
    (
        "rendering/renderer/rendering_method.mobile",
        ValueKind::Choice(RENDERING_METHODS),
    ),
    (
        "rendering/textures/canvas_textures/default_texture_filter",
        ValueKind::Integer { min: 0, max: 3 },
    ),
    // A macOS universal or arm64 export needs ETC2/ASTC imports, a universal
    // or x86_64 one S3TC/BPTC imports.
    (
        "rendering/textures/vram_compression/import_etc2_astc",
        ValueKind::Bool,
    ),
    (
        "rendering/textures/vram_compression/import_s3tc_bptc",
        ValueKind::Bool,
    ),
    (
        "physics/common/physics_ticks_per_second",
        ValueKind::Integer { min: 1, max: 1000 },
    ),
    ("physics/2d/default_gravity", ValueKind::Float),
    ("physics/3d/default_gravity", ValueKind::Float),
    ("audio/buses/default_bus_layout", ValueKind::ResourcePath),
    ("gui/theme/custom", ValueKind::ResourcePath),
    (
        "input_devices/pointing/emulate_touch_from_mouse",
        ValueKind::Bool,
    ),
];

fn split_setting(name: &str) -> Option<(&str, &str)> {
    name.split_once('/')
}

/// The project-relative path a `res://` value names.
fn res_path(raw: &str, field: &str) -> Result<(String, String), OutcomeError> {
    let relative = paths::normalize(raw, field)?;
    paths::readable(&relative)?;
    Ok((format!("res://{relative}"), relative))
}

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

/// What an applied operation needs to exist for the result to be valid.
struct Requirements {
    files: Vec<(String, &'static str)>,
}

fn invalid(message: &str) -> OutcomeError {
    outcome_error(ErrorKind::InvalidInput, message.to_owned())
}

fn field<'a>(operation: &'a Map<String, Value>, key: &str) -> Result<&'a str, OutcomeError> {
    operation
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(&format!("this operation needs a string `{key}`")))
}

fn only_fields(operation: &Map<String, Value>, allowed: &[&str]) -> Result<(), OutcomeError> {
    if let Some(unknown) = operation
        .keys()
        .find(|key| *key != "op" && !allowed.contains(&key.as_str()))
    {
        return Err(invalid(&format!(
            "this operation does not accept `{unknown}`"
        )));
    }
    Ok(())
}

fn identifier(name: &str, what: &str) -> Result<(), OutcomeError> {
    let mut characters = name.chars();
    let valid_start = characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_');
    if !valid_start
        || name.len() > 128
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
    {
        return Err(invalid(&format!(
            "{what} `{name}` must be an identifier of letters, digits and underscores"
        )));
    }
    Ok(())
}

/// Every settings operation: its name, every field it declares and the fields
/// it requires. The schema and the field check both read it.
const OPERATIONS: &[(&str, &[&str], &[&str])] = &[
    ("set_main_scene", &["scene"], &["scene"]),
    ("clear_main_scene", &[], &[]),
    (
        "set_autoload",
        &["name", "path", "singleton"],
        &["name", "path"],
    ),
    ("remove_autoload", &["name"], &["name"]),
    (
        "set_input_action",
        &["name", "deadzone", "events"],
        &["name", "events"],
    ),
    ("remove_input_action", &["name"], &["name"]),
    ("set_value", &["setting", "value"], &["setting", "value"]),
    ("enable_plugin", &["plugin"], &["plugin"]),
    ("disable_plugin", &["plugin"], &["plugin"]),
];

/// Every input event type and its fields, as [`OPERATIONS`].
const EVENT_TYPES: &[(&str, &[&str], &[&str])] = &[
    (
        "key",
        &[
            "key",
            "keycode",
            "physicalKeycode",
            "shift",
            "ctrl",
            "alt",
            "meta",
        ],
        &[],
    ),
    ("mouse_button", &["button", "buttonIndex"], &[]),
    ("joypad_button", &["buttonIndex"], &["buttonIndex"]),
    (
        "joypad_motion",
        &["axis", "axisValue"],
        &["axis", "axisValue"],
    ),
];

fn declared_fields(
    table: &[(&str, &'static [&'static str], &[&str])],
    name: &str,
) -> Option<&'static [&'static str]> {
    table
        .iter()
        .find(|(entry, _, _)| *entry == name)
        .map(|(_, fields, _)| *fields)
}

fn apply_operation(
    document: &mut Document,
    operation: &Value,
    requirements: &mut Requirements,
) -> Result<Value, OutcomeError> {
    let operation = operation
        .as_object()
        .ok_or_else(|| invalid("every operation must be an object"))?;
    let op = field(operation, "op")?;
    if let Some(fields) = declared_fields(OPERATIONS, op) {
        only_fields(operation, fields)?;
    }
    match op {
        "set_main_scene" => {
            let (uri, relative) = res_path(field(operation, "scene")?, "scene")?;
            if !matches!(paths::extension(&relative).as_str(), "tscn" | "scn") {
                return Err(invalid("the main scene must be a .tscn or .scn scene"));
            }
            requirements.files.push((relative, "main scene"));
            document.set("application", "run/main_scene", &godot_string(&uri));
            Ok(json!({ "op": op, "scene": uri }))
        }
        "clear_main_scene" => {
            let removed = document.remove("application", "run/main_scene");
            Ok(json!({ "op": op, "changed": removed }))
        }
        "set_autoload" => {
            let name = field(operation, "name")?;
            identifier(name, "an autoload name")?;
            let (uri, relative) = res_path(field(operation, "path")?, "path")?;
            if !matches!(paths::extension(&relative).as_str(), "gd" | "tscn" | "scn") {
                return Err(invalid("an autoload must be a .gd script or a scene"));
            }
            let singleton = match operation.get("singleton") {
                None => true,
                Some(Value::Bool(value)) => *value,
                Some(_) => return Err(invalid("`singleton` must be a boolean")),
            };
            requirements.files.push((relative, "autoload"));
            let value = if singleton {
                godot_string(&format!("*{uri}"))
            } else {
                godot_string(&uri)
            };
            document.set("autoload", name, &value);
            Ok(json!({ "op": op, "name": name, "path": uri, "singleton": singleton }))
        }
        "remove_autoload" => {
            let name = field(operation, "name")?;
            let removed = document.remove("autoload", name);
            Ok(json!({ "op": op, "name": name, "changed": removed }))
        }
        "set_input_action" => {
            let name = field(operation, "name")?;
            if name.is_empty()
                || name.len() > 128
                || name
                    .chars()
                    .any(|character| character.is_control() || "\"=[]{}:".contains(character))
            {
                return Err(invalid(
                    "an input action name must be 1-128 characters without quotes, =, :, \
                     brackets or braces",
                ));
            }
            let deadzone = match operation.get("deadzone") {
                None => DEFAULT_DEADZONE,
                Some(value) => value
                    .as_f64()
                    .filter(|value| (0.0..=1.0).contains(value))
                    .ok_or_else(|| invalid("`deadzone` must be a number from 0 to 1"))?,
            };
            let events = operation
                .get("events")
                .and_then(Value::as_array)
                .ok_or_else(|| invalid("`events` must be an array"))?;
            if events.len() > MAX_EVENTS {
                return Err(invalid(&format!(
                    "an action binds at most {MAX_EVENTS} events"
                )));
            }
            let literals = events
                .iter()
                .map(event_literal)
                .collect::<Result<Vec<_>, _>>()?;
            let value = format!(
                "{{\n\"deadzone\": {},\n\"events\": [{}]\n}}",
                godot_float(deadzone),
                literals.join(", ")
            );
            document.set("input", name, &value);
            Ok(json!({ "op": op, "name": name, "events": literals.len() }))
        }
        "remove_input_action" => {
            let name = field(operation, "name")?;
            let removed = document.remove("input", name);
            Ok(json!({ "op": op, "name": name, "changed": removed }))
        }
        "set_value" => {
            let setting = field(operation, "setting")?;
            let kind = ALLOWLIST
                .iter()
                .find(|(name, _)| *name == setting)
                .map(|(_, kind)| *kind)
                .ok_or_else(|| {
                    invalid(&format!(
                        "`{setting}` is not a setting set_value may change; the allowlist is {}",
                        ALLOWLIST
                            .iter()
                            .map(|(name, _)| *name)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                })?;
            let value = operation
                .get("value")
                .ok_or_else(|| invalid("set_value needs a `value`"))?;
            let literal = scalar_literal(setting, kind, value, requirements)?;
            let (section, key) = split_setting(setting).expect("allowlisted names are sectioned");
            document.set(section, key, &literal);
            Ok(json!({ "op": op, "setting": setting, "value": value }))
        }
        "enable_plugin" | "disable_plugin" => {
            let (uri, relative) = res_path(field(operation, "plugin")?, "plugin")?;
            if !(relative.starts_with("addons/") && relative.ends_with("/plugin.cfg")) {
                return Err(invalid(
                    "a plugin is named by its res://addons/<name>/plugin.cfg file",
                ));
            }
            if op == "enable_plugin" {
                requirements.files.push((relative, "plugin"));
            }
            let changed = toggle_plugin(document, &uri, op == "enable_plugin");
            Ok(json!({ "op": op, "plugin": uri, "changed": changed }))
        }
        other => Err(invalid(&format!(
            "unknown settings operation `{other}`; use set_main_scene, clear_main_scene, \
             set_autoload, remove_autoload, set_input_action, remove_input_action, set_value, \
             enable_plugin or disable_plugin"
        ))),
    }
}

/// Enable or disable the plugin `uri` in `[editor_plugins] enabled`; `true`
/// when the list changed.
fn toggle_plugin(document: &mut Document, uri: &str, enable: bool) -> bool {
    let mut enabled = document
        .find("editor_plugins", "enabled")
        .map(|entry| packed_strings(&entry.value))
        .unwrap_or_default();
    let before = enabled.len();
    if enable {
        if !enabled.iter().any(|plugin| plugin == uri) {
            enabled.push(uri.to_owned());
        }
    } else {
        enabled.retain(|plugin| plugin != uri);
    }
    let changed = enabled.len() != before;
    if changed {
        if enabled.is_empty() {
            document.remove("editor_plugins", "enabled");
        } else {
            document.set("editor_plugins", "enabled", &packed_string_array(&enabled));
        }
    }
    changed
}

/// `project.godot` text with the plugin `uri` enabled or disabled, preserving
/// everything else; `None` when nothing changes.
pub(super) fn with_plugin(text: &str, uri: &str, enable: bool) -> Option<String> {
    let mut document = Document::parse(text);
    toggle_plugin(&mut document, uri, enable).then(|| document.render())
}

/// Whether `project.godot` text enables the plugin `uri`.
pub(super) fn plugin_enabled(text: &str, uri: &str) -> bool {
    Document::parse(text)
        .find("editor_plugins", "enabled")
        .is_some_and(|entry| {
            packed_strings(&entry.value)
                .iter()
                .any(|plugin| plugin == uri)
        })
}

fn scalar_literal(
    setting: &str,
    kind: ValueKind,
    value: &Value,
    requirements: &mut Requirements,
) -> Result<String, OutcomeError> {
    let mismatch = |expected: &str| invalid(&format!("`{setting}` takes {expected}"));
    match kind {
        ValueKind::Text => value
            .as_str()
            .filter(|text| text.len() <= 4096)
            .map(godot_string)
            .ok_or_else(|| mismatch("a string of at most 4096 bytes")),
        ValueKind::ResourcePath => {
            let raw = value.as_str().ok_or_else(|| mismatch("a res:// path"))?;
            let (uri, relative) = res_path(raw, setting)?;
            requirements.files.push((relative, "resource"));
            Ok(godot_string(&uri))
        }
        ValueKind::Integer { min, max } => value
            .as_i64()
            .filter(|number| (min..=max).contains(number))
            .map(|number| number.to_string())
            .ok_or_else(|| mismatch(&format!("an integer from {min} to {max}"))),
        ValueKind::Float => value
            .as_f64()
            .filter(|number| number.is_finite())
            .map(godot_float)
            .ok_or_else(|| mismatch("a finite number")),
        ValueKind::Bool => value
            .as_bool()
            .map(|flag| flag.to_string())
            .ok_or_else(|| mismatch("a boolean")),
        ValueKind::Choice(choices) => value
            .as_str()
            .filter(|choice| choices.contains(choice))
            .map(godot_string)
            .ok_or_else(|| mismatch(&format!("one of {}", choices.join(", ")))),
    }
}

// ---------------------------------------------------------------------------
// The typed view
// ---------------------------------------------------------------------------

fn typed_view(document: &Document, settings: &ProjectSettings) -> Value {
    let application = settings.application();
    let main_scene = settings
        .main_scene()
        .and_then(|binding| binding.target().project_relative_path())
        .map(|path| format!("res://{path}"))
        .or_else(|| {
            document
                .find("application", "run/main_scene")
                .and_then(|entry| unquote(&entry.value))
        });
    let autoloads: Vec<Value> = settings
        .autoloads()
        .iter()
        .map(|autoload| {
            json!({
                "name": autoload.name(),
                "path": autoload
                    .target()
                    .project_relative_path()
                    .map(|path| format!("res://{path}")),
                "singleton": autoload.is_singleton(),
            })
        })
        .collect();
    let input_actions: Vec<Value> = settings
        .input_actions()
        .iter()
        .map(|action| action_value(action.name(), action.raw_value()))
        .collect();
    let plugins: Vec<String> = document
        .find("editor_plugins", "enabled")
        .map(|entry| packed_strings(&entry.value))
        .unwrap_or_default();
    let values: Map<String, Value> = ALLOWLIST
        .iter()
        .filter_map(|(name, _)| {
            let (section, key) = split_setting(name)?;
            let entry = document.find(section, key)?;
            let value = unquote(&entry.value).map_or_else(
                || {
                    entry
                        .value
                        .parse::<i64>()
                        .map(Value::from)
                        .or_else(|_| entry.value.parse::<f64>().map(Value::from))
                        .or_else(|_| entry.value.parse::<bool>().map(Value::from))
                        .unwrap_or_else(|_| Value::String(entry.value.clone()))
                },
                Value::String,
            );
            Some(((*name).to_owned(), value))
        })
        .collect();
    json!({
        "name": application.name(),
        "description": application.description(),
        "version": application.version(),
        "features": application.features(),
        "config_version": settings.config_version(),
        "main_scene": main_scene,
        "autoloads": autoloads,
        "input_actions": input_actions,
        "enabled_plugins": plugins,
        "values": values,
        "manifest_diagnostics": settings.diagnostics().len(),
    })
}

pub(super) fn get_schema() -> Value {
    object_schema(json!({}), &[], false)
}

/// The schema of one settings operation field.
fn settings_field_schema(field: &str) -> Value {
    match field {
        "scene" | "path" | "plugin" => json!({ "type": "string", "pattern": "^res://" }),
        "singleton" => json!({ "type": "boolean" }),
        "deadzone" => json!({ "type": "number", "minimum": 0, "maximum": 1 }),
        "events" => json!({
            "type": "array",
            "maxItems": MAX_EVENTS,
            "items": crate::schema::one_of(
                EVENT_TYPES
                    .iter()
                    .map(|(kind, fields, required)| event_variant(kind, fields, required))
                    .collect()
            )
        }),
        _ => json!({ "type": "string", "minLength": 1 }),
    }
}

/// The schema of one input event type, exactly as [`event_literal`] reads it:
/// a key event names a key, a physical key code or a key code; a mouse event
/// names exactly one of a button and a button index.
fn event_variant(kind: &str, fields: &[&str], required: &[&str]) -> Value {
    let field_schema = |field: &str| match (kind, field) {
        (_, "key") => json!({
            "type": "string",
            "pattern": key_name_pattern(),
            "description": "A letter, a digit, F1-F12 or a named key (space, escape, tab, \
                            backspace, enter, insert, delete, home, end, the arrows, page_up, \
                            page_down, shift, ctrl, meta, alt), in any letter case."
        }),
        (_, "keycode" | "physicalKeycode") => json!({ "type": "integer" }),
        (_, "shift" | "ctrl" | "alt" | "meta") => json!({ "type": "boolean" }),
        (_, "button") => json!({
            "type": "string",
            "pattern": mouse_button_pattern(),
            "description": "left, right, middle, wheel_up, wheel_down, wheel_left or \
                            wheel_right, in any letter case."
        }),
        ("mouse_button", "buttonIndex") => json!({ "type": "integer", "minimum": 1, "maximum": 9 }),
        ("joypad_button", "buttonIndex") => {
            json!({ "type": "integer", "minimum": 0, "maximum": 127 })
        }
        (_, "axis") => json!({ "type": "integer", "minimum": 0, "maximum": 9 }),
        (_, "axisValue") => json!({
            "type": "number",
            "minimum": -1,
            "maximum": 1,
            "not": { "const": 0 }
        }),
        (kind, field) => unreachable!("{kind} events declare no field {field}"),
    };
    let fields: Vec<(&str, Value)> = fields
        .iter()
        .map(|field| (*field, field_schema(field)))
        .collect();
    let mut variant = crate::schema::variant("type", kind, &fields, required);
    match kind {
        "key" => {
            variant["anyOf"] = json!([
                { "required": ["key"] },
                { "required": ["keycode"] },
                { "required": ["physicalKeycode"] }
            ]);
        }
        "mouse_button" => {
            variant["oneOf"] = json!([
                { "required": ["button"] },
                { "required": ["buttonIndex"] }
            ]);
        }
        _ => {}
    }
    variant
}

/// The schema of one allowlisted setting's value, as [`scalar_literal`]
/// reads it. A text's bound is in bytes, which a schema counts as characters.
fn value_kind_schema(kind: ValueKind) -> Value {
    match kind {
        ValueKind::Text => json!({ "type": "string", "maxLength": 4096 }),
        ValueKind::ResourcePath => json!({ "type": "string", "pattern": "^res://" }),
        ValueKind::Integer { min, max } => {
            json!({ "type": "integer", "minimum": min, "maximum": max })
        }
        ValueKind::Float => json!({ "type": "number" }),
        ValueKind::Bool => json!({ "type": "boolean" }),
        ValueKind::Choice(choices) => json!({ "type": "string", "enum": choices }),
    }
}

/// Every settings operation; `set_value` is one variant per allowlisted
/// setting, each with that setting's value type.
fn operation_items() -> Value {
    let mut variants = Vec::new();
    for (op, fields, required) in OPERATIONS {
        if *op == "set_value" {
            variants.extend(ALLOWLIST.iter().map(|(setting, kind)| {
                crate::schema::variant(
                    "op",
                    op,
                    &[
                        ("setting", json!({ "const": setting })),
                        ("value", value_kind_schema(*kind)),
                    ],
                    required,
                )
            }));
            continue;
        }
        let fields: Vec<(&str, Value)> = fields
            .iter()
            .map(|field| (*field, settings_field_schema(field)))
            .collect();
        variants.push(crate::schema::variant("op", op, &fields, required));
    }
    crate::schema::one_of(variants)
}

pub(super) fn patch_schema() -> Value {
    object_schema(
        json!({
            "expectedSha256": sha256_property("SHA-256 of project.godot's current bytes."),
            "operations": {
                "type": "array",
                "minItems": 1,
                "maxItems": MAX_OPERATIONS,
                "description": "Typed operations applied in order: set_main_scene {scene}, \
                    clear_main_scene, set_autoload {name, path, singleton?}, remove_autoload \
                    {name}, set_input_action {name, deadzone?, events[]}, remove_input_action \
                    {name}, set_value {setting, value}, enable_plugin {plugin}, disable_plugin \
                    {plugin}. Key events take key|physicalKeycode|keycode and shift/ctrl/alt/meta; \
                    mouse_button events take button|buttonIndex; joypad_button events take \
                    buttonIndex; joypad_motion events take axis and axisValue.",
                "items": operation_items()
            }
        }),
        &["expectedSha256", "operations"],
        true,
    )
}

pub(super) fn get(runtime: &AuthoringRuntime) -> Result<Value, OutcomeError> {
    let (snapshot, contents) = runtime.coherent_read(&[MANIFEST.to_owned()])?;
    let bytes = contents
        .into_iter()
        .next()
        .flatten()
        .ok_or_else(|| outcome_error(ErrorKind::NotFound, "project.godot does not exist"))?;
    let text = utf8_text(&bytes, MANIFEST)?;
    let settings = ProjectSettings::parse(&text, runtime.generation)
        .map_err(|error| outcome_error(error.kind(), error.to_string()))?;
    let mut view = typed_view(&Document::parse(&text), &settings);
    if let Value::Object(object) = &mut view {
        object.insert("sha256".to_owned(), Value::String(sha256_hex(&bytes)));
        object.insert("workspace".to_owned(), snapshot.to_value());
    }
    Ok(view)
}

pub(super) async fn patch(
    runtime: &AuthoringRuntime,
    arguments: &Map<String, Value>,
    envelope: &Envelope,
    canonical: &Value,
    context: &CallContext<'_>,
) -> Result<Value, OutcomeError> {
    let expected = sha256_argument(arguments, "expectedSha256")?;
    let operations = arguments
        .get("operations")
        .and_then(Value::as_array)
        .filter(|operations| !operations.is_empty() && operations.len() <= MAX_OPERATIONS)
        .ok_or_else(|| {
            invalid(&format!(
                "`operations` must hold between 1 and {MAX_OPERATIONS} operations"
            ))
        })?;
    let (snapshot, contents) = runtime.coherent_read(&[MANIFEST.to_owned()])?;
    envelope.check_revision(&snapshot)?;
    let bytes = match contents.into_iter().next().flatten() {
        Some(bytes) if sha256_hex(&bytes) == expected => bytes,
        other => return Err(precondition_mismatch(MANIFEST, &expected, other.as_deref())),
    };
    let before = utf8_text(&bytes, MANIFEST)?;
    let mut document = Document::parse(&before);
    let mut requirements = Requirements { files: Vec::new() };
    let applied = operations
        .iter()
        .map(|operation| apply_operation(&mut document, operation, &mut requirements))
        .collect::<Result<Vec<_>, _>>()?;
    let after = document.render();

    // Every file an operation points at must exist in this snapshot.
    for (relative, role) in &requirements.files {
        if !snapshot.inventory().contains(relative) {
            return Err(outcome_error(
                ErrorKind::InvalidInput,
                format!("the {role} res://{relative} does not exist in the project"),
            ));
        }
    }
    let before_diagnostics = ProjectSettings::parse(&before, runtime.generation)
        .map_err(|error| outcome_error(error.kind(), error.to_string()))?
        .diagnostics()
        .len();
    let settings = ProjectSettings::parse(&after, runtime.generation)
        .map_err(|error| outcome_error(error.kind(), error.to_string()))?;
    if settings.diagnostics().len() > before_diagnostics {
        return Err(outcome_error(
            ErrorKind::Internal,
            "the edited project.godot no longer parses cleanly; nothing was written",
        ));
    }

    let mut result = Map::new();
    result.insert("path".to_owned(), Value::String(MANIFEST.to_owned()));
    result.insert("operations".to_owned(), Value::Array(applied));
    result.insert("before_sha256".to_owned(), Value::String(expected.clone()));
    result.insert(
        "after_sha256".to_owned(),
        Value::String(sha256_hex(after.as_bytes())),
    );
    result.insert("changed".to_owned(), Value::Bool(after != before));
    result.insert(
        "diff".to_owned(),
        unified(Some(MANIFEST), Some(MANIFEST), &before, &after).to_value(),
    );
    result.insert("settings".to_owned(), typed_view(&document, &settings));
    if envelope.dry_run || after == before {
        result.insert("dry_run".to_owned(), Value::Bool(envelope.dry_run));
        result.insert("workspace".to_owned(), snapshot.to_value());
        return Ok(Value::Object(result));
    }
    let committed = runtime
        .commit(
            Mutation {
                tool: AuthoringOperation::SettingsPatch.tool_name(),
                primary_path: MANIFEST.to_owned(),
                writes: vec![PlannedWrite {
                    path: MANIFEST.to_owned(),
                    operation: FileOperation::Update,
                    bytes: Some(after.into_bytes()),
                    precondition: WritePrecondition::Sha256(expected),
                }],
                expected_revision: envelope.expected_revision,
            },
            canonical,
            context,
        )
        .await?;
    Ok(committed.into_value(result))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST_TEXT: &str = "; Engine configuration file.\n\
        ; keep this comment\n\
        \n\
        config_version=5\n\
        \n\
        [application]\n\
        \n\
        config/name=\"Demo\"\n\
        config/features=PackedStringArray(\"4.7\", \"Forward Plus\")\n\
        \n\
        [rendering]\n\
        \n\
        textures/vram_compression/import_etc2_astc=true\n";

    fn apply(text: &str, operations: Value) -> String {
        let mut document = Document::parse(text);
        let mut requirements = Requirements { files: Vec::new() };
        for operation in operations.as_array().expect("operations") {
            apply_operation(&mut document, operation, &mut requirements).expect("operation");
        }
        document.render()
    }

    /// `schema` as a standalone schema carrying the definitions of the patch
    /// schema it was taken from.
    fn standalone(schema: &Value) -> Value {
        let root = patch_schema();
        let mut standalone = json!({ "allOf": [schema] });
        if let Some(definitions) = root.get("$defs") {
            standalone["$defs"] = definitions.clone();
        }
        standalone
    }

    /// The schema of one settings operation item.
    fn advertised_operation_schema() -> Value {
        standalone(&patch_schema()["properties"]["operations"]["items"])
    }

    /// The schema of one input event of `set_input_action`.
    fn advertised_event_schema() -> Value {
        let root = patch_schema();
        let variant = root["properties"]["operations"]["items"]["oneOf"]
            .as_array()
            .expect("operation variants")
            .iter()
            .find(|variant| variant["properties"]["op"]["const"] == json!("set_input_action"))
            .cloned()
            .expect("set_input_action is an operation");
        standalone(&variant["properties"]["events"]["items"])
    }

    /// The event schema and the event check give the same verdict on every
    /// case, and it is the intended one: a key event names a key, a physical
    /// key code or a key code; a mouse event names exactly one of a button and
    /// a button index; every index, axis and axis value is in the range the
    /// handler accepts.
    #[test]
    fn the_input_event_schema_and_the_event_check_agree() {
        let cases = [
            (json!({ "type": "key", "key": "Space" }), true),
            (json!({ "type": "key", "key": "space" }), true),
            (json!({ "type": "key", "key": "W" }), true),
            (json!({ "type": "key", "key": "7" }), true),
            (json!({ "type": "key", "key": "f12" }), true),
            (json!({ "type": "key", "key": "Page_Up" }), true),
            (json!({ "type": "key", "key": "ESC", "shift": true }), true),
            (json!({ "type": "key", "keycode": 65 }), true),
            (
                json!({ "type": "key", "physicalKeycode": 87, "ctrl": false }),
                true,
            ),
            (json!({ "type": "key", "key": "A", "keycode": 65 }), true),
            (json!({ "type": "mouse_button", "button": "left" }), true),
            (
                json!({ "type": "mouse_button", "button": "WHEEL_UP" }),
                true,
            ),
            (json!({ "type": "mouse_button", "buttonIndex": 9 }), true),
            (json!({ "type": "joypad_button", "buttonIndex": 0 }), true),
            (json!({ "type": "joypad_button", "buttonIndex": 127 }), true),
            (
                json!({ "type": "joypad_motion", "axis": 9, "axisValue": -1 }),
                true,
            ),
            (
                json!({ "type": "joypad_motion", "axis": 0, "axisValue": 0.5 }),
                true,
            ),
            (json!({ "type": "key" }), false),
            (json!({ "type": "key", "shift": true }), false),
            (json!({ "type": "key", "key": "NoSuchKey" }), false),
            (json!({ "type": "key", "key": "F13" }), false),
            (json!({ "type": "key", "key": "" }), false),
            (json!({ "type": "key", "key": "ab" }), false),
            (json!({ "type": "key", "key": 5 }), false),
            (json!({ "type": "key", "keycode": "65" }), false),
            (json!({ "type": "key", "key": "A", "shift": "yes" }), false),
            (
                json!({ "type": "key", "key": "A", "button": "left" }),
                false,
            ),
            (json!({ "type": "mouse_button" }), false),
            (
                json!({ "type": "mouse_button", "button": "left", "buttonIndex": 1 }),
                false,
            ),
            (json!({ "type": "mouse_button", "button": "thumb" }), false),
            (json!({ "type": "mouse_button", "buttonIndex": 0 }), false),
            (json!({ "type": "mouse_button", "buttonIndex": 10 }), false),
            (json!({ "type": "joypad_button" }), false),
            (
                json!({ "type": "joypad_button", "buttonIndex": 128 }),
                false,
            ),
            (json!({ "type": "joypad_button", "buttonIndex": -1 }), false),
            (
                json!({ "type": "joypad_motion", "axis": 10, "axisValue": 1 }),
                false,
            ),
            (
                json!({ "type": "joypad_motion", "axis": 1, "axisValue": 0 }),
                false,
            ),
            (
                json!({ "type": "joypad_motion", "axis": 1, "axisValue": 1.5 }),
                false,
            ),
            (json!({ "type": "joypad_motion", "axis": 1 }), false),
            (json!({ "type": "touch" }), false),
            (json!({ "key": "A" }), false),
            (json!("key"), false),
        ];
        let schema = advertised_event_schema();
        for (case, expected) in &cases {
            assert_eq!(
                crate::schema::conformance::conforms(&schema, case),
                *expected,
                "the schema's verdict on {case}"
            );
            assert_eq!(
                event_literal(case).is_ok(),
                *expected,
                "the handler's verdict on {case}"
            );
        }
    }

    /// The `set_value` schema and the operation check give the same verdict on
    /// every case, and it is the intended one: an allowlisted setting with a
    /// value of that setting's type and range. A path value's syntax and the
    /// file it names are the handler's to check, as for every path.
    #[test]
    fn the_set_value_schema_and_the_operation_check_agree() {
        let set = |setting: &str, value: Value| json!({ "op": "set_value", "setting": setting, "value": value });
        let cases = [
            (set("application/config/name", json!("Game")), true),
            (set("application/config/description", json!("")), true),
            (
                set("application/config/icon", json!("res://icon.svg")),
                true,
            ),
            (set("application/run/max_fps", json!(0)), true),
            (set("application/run/max_fps", json!(1000)), true),
            (
                set("display/window/size/viewport_width", json!(16384)),
                true,
            ),
            (set("display/window/size/mode", json!(4)), true),
            (set("display/window/size/resizable", json!(false)), true),
            (set("display/window/stretch/mode", json!("viewport")), true),
            (
                set("display/window/stretch/aspect", json!("keep_width")),
                true,
            ),
            (
                set("rendering/renderer/rendering_method", json!("mobile")),
                true,
            ),
            (set("physics/2d/default_gravity", json!(980)), true),
            (set("physics/3d/default_gravity", json!(-9.8)), true),
            (set("application/config/name", json!(5)), false),
            (
                set("application/config/name", json!("x".repeat(4097))),
                false,
            ),
            (set("application/config/icon", json!(5)), false),
            (set("application/run/max_fps", json!(1001)), false),
            (set("application/run/max_fps", json!(-1)), false),
            (set("application/run/max_fps", json!("60")), false),
            (set("application/run/max_fps", json!(60.5)), false),
            (set("display/window/size/viewport_width", json!(0)), false),
            (set("display/window/size/resizable", json!("yes")), false),
            (set("display/window/stretch/mode", json!("fill")), false),
            (set("physics/2d/default_gravity", json!("9.8")), false),
            (
                set("application/run/main_scene", json!("res://main.tscn")),
                false,
            ),
            (
                json!({ "op": "set_value", "setting": "application/config/name" }),
                false,
            ),
            (
                json!({ "op": "set_value", "setting": "application/config/name", "value": "x", "x": 1 }),
                false,
            ),
        ];
        let schema = advertised_operation_schema();
        for (case, expected) in &cases {
            assert_eq!(
                crate::schema::conformance::conforms(&schema, case),
                *expected,
                "the schema's verdict on {case}"
            );
            let mut document = Document::parse(MANIFEST_TEXT);
            let mut requirements = Requirements { files: Vec::new() };
            assert_eq!(
                apply_operation(&mut document, case, &mut requirements).is_ok(),
                *expected,
                "the handler's verdict on {case}"
            );
        }
    }

    #[test]
    fn operations_preserve_comments_and_unrelated_keys() {
        let after = apply(
            MANIFEST_TEXT,
            json!([
                { "op": "set_main_scene", "scene": "res://main.tscn" },
                { "op": "set_autoload", "name": "Game", "path": "res://autoload/game.gd" },
                { "op": "set_value", "setting": "application/config/name", "value": "Renamed" },
                { "op": "set_input_action", "name": "jump", "events": [{ "type": "key", "key": "Space" }] }
            ]),
        );
        assert!(after.starts_with("; Engine configuration file.\n; keep this comment\n"));
        assert!(after.contains("textures/vram_compression/import_etc2_astc=true\n"));
        assert!(after.contains("config/name=\"Renamed\"\n"));
        assert!(!after.contains("\"Demo\""));
        assert!(after.contains("run/main_scene=\"res://main.tscn\"\n"));
        assert!(after.contains("[autoload]\n\nGame=\"*res://autoload/game.gd\"\n"));
        assert!(after.contains("jump={\n\"deadzone\": 0.2,\n\"events\": [Object(InputEventKey,"));
        assert!(after.contains("\"physical_keycode\":32,"));

        let settings = ProjectSettings::parse(&after, 1).expect("parses");
        assert!(
            settings.diagnostics().is_empty(),
            "{:?}",
            settings.diagnostics()
        );
        assert_eq!(settings.autoloads().len(), 1);
        assert_eq!(settings.input_actions().len(), 1);
        assert_eq!(
            settings
                .main_scene()
                .and_then(|scene| scene.target().project_relative_path()),
            Some("main.tscn")
        );
    }

    #[test]
    fn replacing_a_multi_line_value_replaces_all_of_its_lines() {
        let first = apply(
            MANIFEST_TEXT,
            json!([{ "op": "set_input_action", "name": "jump", "events": [
                { "type": "key", "key": "Space" }, { "type": "mouse_button", "button": "left" }
            ] }]),
        );
        let second = apply(
            &first,
            json!([{ "op": "set_input_action", "name": "jump", "deadzone": 0.5, "events": [
                { "type": "key", "key": "W" }
            ] }]),
        );
        assert_eq!(second.matches("jump={").count(), 1);
        assert!(!second.contains("InputEventMouseButton"));
        assert!(second.contains("\"physical_keycode\":87,"));
        let removed = apply(
            &second,
            json!([{ "op": "remove_input_action", "name": "jump" }]),
        );
        assert!(!removed.contains("jump="));
        assert!(!removed.contains("deadzone"));
    }

    #[test]
    fn plugins_are_a_set_and_values_are_allowlisted_and_typed() {
        let enabled = apply(
            MANIFEST_TEXT,
            json!([
                { "op": "enable_plugin", "plugin": "res://addons/a/plugin.cfg" },
                { "op": "enable_plugin", "plugin": "res://addons/b/plugin.cfg" },
                { "op": "enable_plugin", "plugin": "res://addons/a/plugin.cfg" }
            ]),
        );
        assert!(enabled.contains(
            "enabled=PackedStringArray(\"res://addons/a/plugin.cfg\", \"res://addons/b/plugin.cfg\")"
        ));
        let disabled = apply(
            &enabled,
            json!([
                { "op": "disable_plugin", "plugin": "res://addons/a/plugin.cfg" },
                { "op": "disable_plugin", "plugin": "res://addons/b/plugin.cfg" }
            ]),
        );
        assert!(!disabled.contains("enabled="));

        let mut document = Document::parse(MANIFEST_TEXT);
        let mut requirements = Requirements { files: Vec::new() };
        for refused in [
            json!({ "op": "set_value", "setting": "application/run/main_scene", "value": "x" }),
            json!({ "op": "set_value", "setting": "display/window/size/viewport_width", "value": "wide" }),
            json!({ "op": "set_value", "setting": "display/window/size/viewport_width", "value": 0 }),
            json!({ "op": "set_autoload", "name": "bad name", "path": "res://a.gd" }),
            json!({ "op": "set_input_action", "name": "x", "events": [{ "type": "key", "key": "NoSuchKey" }] }),
            json!({ "op": "drop_everything" }),
        ] {
            assert!(
                apply_operation(&mut document, &refused, &mut requirements).is_err(),
                "{refused}"
            );
        }
    }

    #[test]
    fn the_texture_formats_an_export_needs_are_typed_settings() {
        let after = apply(
            MANIFEST_TEXT,
            json!([
                { "op": "set_value", "setting": "rendering/textures/vram_compression/import_etc2_astc", "value": false },
                { "op": "set_value", "setting": "rendering/textures/vram_compression/import_s3tc_bptc", "value": true }
            ]),
        );
        assert!(
            after.contains("textures/vram_compression/import_etc2_astc=false\n"),
            "{after}"
        );
        assert!(
            after.contains("textures/vram_compression/import_s3tc_bptc=true\n"),
            "{after}"
        );
        let mut document = Document::parse(MANIFEST_TEXT);
        let mut requirements = Requirements { files: Vec::new() };
        let refused = json!({
            "op": "set_value",
            "setting": "rendering/textures/vram_compression/import_etc2_astc",
            "value": "yes"
        });
        assert!(apply_operation(&mut document, &refused, &mut requirements).is_err());
    }

    #[test]
    fn named_keys_follow_the_godot_key_enum() {
        assert_eq!(key_code("A"), Some(65));
        assert_eq!(key_code("space"), Some(32));
        assert_eq!(key_code("Escape"), Some(4_194_305));
        assert_eq!(key_code("Enter"), Some(4_194_309));
        assert_eq!(key_code("Left"), Some(4_194_319));
        assert_eq!(key_code("Down"), Some(4_194_322));
        assert_eq!(key_code("Shift"), Some(4_194_325));
        assert_eq!(key_code("F1"), Some(4_194_332));
        assert_eq!(key_code("F12"), Some(4_194_343));
        assert_eq!(key_name(4_194_320).as_deref(), Some("Up"));
    }

    #[test]
    fn the_typed_view_presents_bound_events() {
        let after = apply(
            MANIFEST_TEXT,
            json!([{ "op": "set_input_action", "name": "jump", "events": [
                { "type": "key", "key": "Space" }, { "type": "joypad_motion", "axis": 1, "axisValue": -1.0 }
            ] }]),
        );
        let settings = ProjectSettings::parse(&after, 1).expect("parses");
        let view = typed_view(&Document::parse(&after), &settings);
        assert_eq!(view["input_actions"][0]["name"], "jump");
        assert_eq!(view["input_actions"][0]["events"][0]["key"], "Space");
        assert_eq!(view["input_actions"][0]["events"][1]["axis"], 1);
        assert_eq!(view["values"]["application/config/name"], "Demo");
    }
}
