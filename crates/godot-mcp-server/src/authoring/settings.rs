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
/// The longest text an allowlisted text setting takes, in characters.
const MAX_TEXT_CHARS: usize = 4096;
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

/// Where a value's text stands after some of its lines: its bracket depth
/// outside string literals, and whether it ends inside one. Godot writes a
/// string's newlines as they are, so a string may span lines.
#[derive(Debug, Clone, Copy, Default)]
struct Scan {
    depth: i64,
    in_string: bool,
    escaped: bool,
}

impl Scan {
    fn feed(&mut self, text: &str) {
        for character in text.chars() {
            if self.in_string {
                match character {
                    _ if self.escaped => self.escaped = false,
                    '\\' => self.escaped = true,
                    '"' => self.in_string = false,
                    _ => {}
                }
                continue;
            }
            match character {
                '"' => self.in_string = true,
                '{' | '[' | '(' => self.depth += 1,
                '}' | ']' | ')' => self.depth -= 1,
                _ => {}
            }
        }
    }

    /// Whether the value goes on to the next line.
    const fn open(&self) -> bool {
        self.depth > 0 || self.in_string
    }
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
            let mut scan = Scan::default();
            scan.feed(first);
            let start = index;
            index += 1;
            while scan.open() && index < lines.len() {
                let continuation = strip_eol(&lines[index]);
                value.push('\n');
                value.push_str(continuation);
                scan.feed("\n");
                scan.feed(continuation);
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
    godot_mcp_domain::variant_text::string_literal(text)
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

/// A pattern matching exactly the names [`key_code`] accepts.
fn key_name_pattern() -> String {
    let mut names = vec!["[A-Za-z0-9]".to_owned(), "[Ff](?:[1-9]|1[0-2])".to_owned()];
    names.extend(
        NAMED_KEYS
            .iter()
            .map(|(name, _)| crate::schema::caseless(name)),
    );
    format!("^(?:{})$", names.join("|"))
}

/// A pattern matching exactly the names [`mouse_button`] accepts.
fn mouse_button_pattern() -> String {
    let names: Vec<String> = MOUSE_BUTTONS
        .iter()
        .map(|(name, _)| crate::schema::caseless(name))
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

/// The `name` of an entry to remove: any string but the empty one, which
/// names no entry.
fn removed_name<'a>(
    operation: &'a Map<String, Value>,
    what: &str,
) -> Result<&'a str, OutcomeError> {
    let name = field(operation, "name")?;
    if name.is_empty() {
        return Err(invalid(&format!(
            "the `name` of {what} to remove must not be empty"
        )));
    }
    Ok(name)
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

/// Longest autoload or input action name, in characters.
const MAX_ENTRY_NAME_CHARS: usize = 128;
/// An autoload name, as [`identifier`] checks one.
const IDENTIFIER_PATTERN: &str = "^[A-Za-z_][A-Za-z0-9_]*$";
/// An input action name, as `set_input_action` checks one: no control
/// character, quote, `=`, `:`, bracket or brace.
const ACTION_NAME_PATTERN: &str = r#"^[^\x00-\x1F\x7F-\x9F"=\[\]{}:]*$"#;

fn identifier(name: &str, what: &str) -> Result<(), OutcomeError> {
    let mut characters = name.chars();
    let valid_start = characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_');
    if !valid_start
        || name.chars().count() > MAX_ENTRY_NAME_CHARS
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
    ("set_setting", &["setting", "value"], &["setting", "value"]),
    ("clear_setting", &["setting"], &["setting"]),
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

/// The settings a typed operation owns, by name or name prefix, with that
/// operation: `set_setting` and `clear_setting` refuse them.
const TYPED_SETTINGS: &[(&str, &str)] = &[
    (
        "application/run/main_scene",
        "set_main_scene or clear_main_scene",
    ),
    ("autoload/", "set_autoload or remove_autoload"),
    ("input/", "set_input_action or remove_input_action"),
    ("editor_plugins/", "enable_plugin or disable_plugin"),
];
/// Longest setting name `set_setting` and `clear_setting` take.
const MAX_SETTING_NAME: usize = 256;
/// Largest literal `set_setting` writes.
const MAX_SETTING_LITERAL: usize = 64 * 1024;
/// A setting name: `section/key`, of letters, digits, `_`, `-`, `.` and `/`.
const SETTING_NAME_PATTERN: &str = "^[A-Za-z0-9_.-]+(?:/[A-Za-z0-9_.-]+)+$";

/// A setting name `set_setting` and `clear_setting` may change.
fn generic_setting(setting: &str) -> Result<(&str, &str), OutcomeError> {
    let well_formed = setting.len() <= MAX_SETTING_NAME
        && setting.contains('/')
        && setting.split('/').all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || "_.-".contains(character))
        });
    if !well_formed {
        return Err(invalid(&format!(
            "`{setting}` is not a setting name: `section/key` of letters, digits, `_`, `-`, \
             `.` and `/`, at most {MAX_SETTING_NAME} bytes"
        )));
    }
    if let Some((_, operation)) = TYPED_SETTINGS.iter().find(|(owned, _)| {
        setting == *owned || (owned.ends_with('/') && setting.starts_with(owned))
    }) {
        return Err(invalid(&format!(
            "`{setting}` is changed with {operation}, which checks what it names"
        )));
    }
    Ok(split_setting(setting).expect("a setting name has a section"))
}

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
            let name = removed_name(operation, "an autoload")?;
            let removed = document.remove("autoload", name);
            Ok(json!({ "op": op, "name": name, "changed": removed }))
        }
        "set_input_action" => {
            let name = field(operation, "name")?;
            if name.is_empty()
                || name.chars().count() > MAX_ENTRY_NAME_CHARS
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
            let name = removed_name(operation, "an input action")?;
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
        "set_setting" => {
            let setting = field(operation, "setting")?;
            let (section, key) = generic_setting(setting)?;
            let tagged = operation
                .get("value")
                .ok_or_else(|| invalid("set_setting needs a `value`"))?;
            crate::engine_plane::authored::tagged_value(tagged, &format!("`{setting}`"))?;
            let value: godot_mcp_domain::contracts::GodotValue =
                serde_json::from_value(tagged.clone())
                    .map_err(|error| invalid(&format!("`{setting}`: {error}")))?;
            if value == godot_mcp_domain::contracts::GodotValue::Null {
                return Err(invalid(&format!(
                    "`{setting}`: a null value would remove the setting; use clear_setting"
                )));
            }
            let literal = godot_mcp_domain::variant_text::write(&value)
                .map_err(|error| invalid(&format!("`{setting}`: {error}")))?;
            if literal.len() > MAX_SETTING_LITERAL {
                return Err(invalid(&format!(
                    "`{setting}` would be {} bytes; a setting holds at most {MAX_SETTING_LITERAL}",
                    literal.len()
                )));
            }
            document.set(section, key, &literal);
            Ok(json!({ "op": op, "setting": setting, "literal": literal }))
        }
        "clear_setting" => {
            let setting = field(operation, "setting")?;
            let (section, key) = generic_setting(setting)?;
            let removed = document.remove(section, key);
            Ok(json!({ "op": op, "setting": setting, "changed": removed }))
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
             set_setting, clear_setting, enable_plugin or disable_plugin"
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
        // Free text: a description may hold a tab or a newline.
        ValueKind::Text => value
            .as_str()
            .filter(|text| text.chars().count() <= MAX_TEXT_CHARS)
            .map(godot_string)
            .ok_or_else(|| mismatch(&format!("a string of at most {MAX_TEXT_CHARS} characters"))),
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

/// Most settings one `project.settings.get` names or lists.
const MAX_SELECTED_SETTINGS: usize = 512;

pub(super) fn get_schema() -> Value {
    object_schema(
        json!({
            "settings": {
                "type": "array",
                "maxItems": MAX_SELECTED_SETTINGS,
                "items": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": MAX_SETTING_NAME
                },
                "description": "Also answer these settings, by full name, as `entries`: the \
                                literal project.godot holds and its tagged value."
            },
            "prefix": {
                "type": "string",
                "maxLength": MAX_SETTING_NAME,
                "description": "Also answer, as `entries`, every setting project.godot holds \
                                whose name starts with this, such as `layer_names/`; \"\" lists \
                                them all."
            }
        }),
        &[],
        false,
    )
}

/// The settings `arguments` select, as {setting, literal, value} entries in
/// name order: the literal project.godot holds, or null when it holds none,
/// and its tagged value, or null when the literal has no typed value here, as
/// an input event or a resource reference. `None` when nothing is selected.
fn selected_entries(
    document: &Document,
    arguments: &Map<String, Value>,
) -> Result<Option<Value>, OutcomeError> {
    let named = match arguments.get("settings") {
        None => None,
        Some(Value::Array(names)) if names.len() <= MAX_SELECTED_SETTINGS => Some(
            names
                .iter()
                .map(|name| {
                    name.as_str()
                        .filter(|name| !name.is_empty() && name.chars().count() <= MAX_SETTING_NAME)
                        .map(str::to_owned)
                        .ok_or_else(|| invalid("`settings` lists setting names"))
                })
                .collect::<Result<std::collections::BTreeSet<_>, _>>()?,
        ),
        Some(_) => {
            return Err(invalid(&format!(
                "`settings` lists at most {MAX_SELECTED_SETTINGS} setting names"
            )));
        }
    };
    let prefix = match arguments.get("prefix") {
        None => None,
        Some(Value::String(prefix)) if prefix.chars().count() <= MAX_SETTING_NAME => {
            Some(prefix.as_str())
        }
        Some(_) => return Err(invalid("`prefix` is a string")),
    };
    if named.is_none() && prefix.is_none() {
        return Ok(None);
    }
    let held = literals(document);
    let mut selected: std::collections::BTreeSet<String> = named.unwrap_or_default();
    if let Some(prefix) = prefix {
        selected.extend(held.keys().filter(|name| name.starts_with(prefix)).cloned());
    }
    let truncated = selected.len() > MAX_SELECTED_SETTINGS;
    let entries: Vec<Value> = selected
        .iter()
        .take(MAX_SELECTED_SETTINGS)
        .map(|name| {
            let literal = held.get(name);
            let value = literal
                .and_then(|literal| godot_mcp_domain::variant_text::parse(literal).ok())
                .and_then(|value| serde_json::to_value(value).ok());
            json!({ "setting": name, "literal": literal, "value": value })
        })
        .collect();
    Ok(Some(json!({ "entries": entries, "truncated": truncated })))
}

/// The schema of one field of settings operation `op`.
fn settings_field_schema(op: &str, field: &str) -> Value {
    match field {
        "name" if op == "set_autoload" => json!({
            "type": "string",
            "maxLength": MAX_ENTRY_NAME_CHARS,
            "pattern": IDENTIFIER_PATTERN,
            "description": "An identifier: letters, digits and underscores, not starting \
                            with a digit."
        }),
        "name" if op == "set_input_action" => json!({
            "type": "string",
            "minLength": 1,
            "maxLength": MAX_ENTRY_NAME_CHARS,
            "pattern": ACTION_NAME_PATTERN,
            "description": "The action's name, without a control character, a quote, `=`, \
                            `:`, a bracket or a brace."
        }),
        "scene" | "path" | "plugin" => json!({ "type": "string", "pattern": "^res://" }),
        "singleton" => json!({ "type": "boolean" }),
        "setting" => json!({
            "type": "string",
            "maxLength": MAX_SETTING_NAME,
            "pattern": SETTING_NAME_PATTERN,
            "not": typed_setting_names(),
            "description": "A setting's full name, `section/key`, such as \
                            `layer_names/2d_physics/layer_1`. The main scene, autoloads, \
                            input actions and plugins have their own operations."
        }),
        "value" => json!({
            "allOf": [
                crate::schema::plain_value(),
                { "not": {
                    "type": "object",
                    "properties": {
                        "type": { "const": "null" },
                        "value": { "type": "null" }
                    },
                    "required": ["type"],
                    "additionalProperties": false
                } }
            ],
            "description": "The setting's new value. A null value is refused: clear_setting \
                            removes a setting."
        }),
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

/// The setting names [`TYPED_SETTINGS`] gives to a typed operation.
fn typed_setting_names() -> Value {
    json!({
        "anyOf": TYPED_SETTINGS
            .iter()
            .map(|(owned, _)| {
                if owned.ends_with('/') {
                    json!({ "pattern": format!("^{owned}") })
                } else {
                    json!({ "const": owned })
                }
            })
            .collect::<Vec<_>>()
    })
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
/// reads it.
fn value_kind_schema(kind: ValueKind) -> Value {
    match kind {
        ValueKind::Text => json!({ "type": "string", "maxLength": MAX_TEXT_CHARS }),
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
            .map(|field| (*field, settings_field_schema(op, field)))
            .collect();
        variants.push(crate::schema::variant("op", op, &fields, required));
    }
    crate::schema::one_of(variants)
}

pub(super) fn patch_schema() -> Value {
    let mut schema = object_schema(
        json!({
            "expectedSha256": sha256_property("SHA-256 of project.godot's current bytes."),
            "operations": {
                "type": "array",
                "minItems": 1,
                "maxItems": MAX_OPERATIONS,
                "description": "Typed operations applied in order: set_main_scene {scene}, \
                    clear_main_scene, set_autoload {name, path, singleton?}, remove_autoload \
                    {name}, set_input_action {name, deadzone?, events[]}, remove_input_action \
                    {name}, set_value {setting, value} for an allowlisted setting with its plain \
                    JSON value, set_setting {setting, value} for any other setting with a tagged \
                    value, clear_setting {setting} back to Godot's default, enable_plugin \
                    {plugin}, disable_plugin {plugin}. Key events take \
                    key|physicalKeycode|keycode and shift/ctrl/alt/meta; \
                    mouse_button events take button|buttonIndex; joypad_button events take \
                    buttonIndex; joypad_motion events take axis and axisValue.",
                "items": operation_items()
            }
        }),
        &["expectedSha256", "operations"],
        true,
    );
    crate::schema::define_values(&mut schema);
    schema
}

pub(super) fn get(
    runtime: &AuthoringRuntime,
    arguments: &Map<String, Value>,
) -> Result<Value, OutcomeError> {
    let (snapshot, contents) = runtime.coherent_read(&[MANIFEST.to_owned()])?;
    let bytes = contents
        .into_iter()
        .next()
        .flatten()
        .ok_or_else(|| outcome_error(ErrorKind::NotFound, "project.godot does not exist"))?;
    let text = utf8_text(&bytes, MANIFEST)?;
    let settings = ProjectSettings::parse(&text, runtime.generation)
        .map_err(|error| outcome_error(error.kind(), error.to_string()))?;
    let document = Document::parse(&text);
    let selected = selected_entries(&document, arguments)?;
    let mut view = typed_view(&document, &settings);
    if let Value::Object(object) = &mut view {
        if let Some(Value::Object(selected)) = selected {
            object.extend(selected);
        }
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
    let editor = runtime
        .editor
        .as_ref()
        .filter(|bridge| bridge.status().connected);
    result.insert(
        "backend".to_owned(),
        Value::String(if editor.is_some() { "editor" } else { "file" }.to_owned()),
    );
    if envelope.dry_run || after == before {
        result.insert("dry_run".to_owned(), Value::Bool(envelope.dry_run));
        result.insert("workspace".to_owned(), snapshot.to_value());
        return Ok(Value::Object(result));
    }
    if let Some(bridge) = editor {
        let change = EditorChange::between(&Document::parse(&before), &document);
        return in_editor(
            runtime, bridge, &change, &before, &expected, result, envelope, canonical, context,
        )
        .await;
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

// ---------------------------------------------------------------------------
// The live-editor path
// ---------------------------------------------------------------------------

/// The bridge method a connected editor applies settings with.
const SETTINGS_APPLY: &str = "editor.settings.apply";
/// How long the editor may take to apply and save the settings.
const SETTINGS_APPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Every sectioned assignment of a document, by its Godot setting name
/// (`section/key`), with its raw literal. A repeated key keeps its last
/// assignment, as Godot reads it.
fn literals(document: &Document) -> std::collections::BTreeMap<String, String> {
    document
        .entries
        .iter()
        .filter_map(|entry| {
            entry
                .section
                .as_ref()
                .map(|section| (format!("{section}/{}", entry.key), entry.value.clone()))
        })
        .collect()
}

/// The enabled-plugin list of a document.
fn enabled_plugins(document: &Document) -> std::collections::BTreeSet<String> {
    document
        .find("editor_plugins", "enabled")
        .map(|entry| packed_strings(&entry.value).into_iter().collect())
        .unwrap_or_default()
}

/// The setting under which Godot keeps the enabled plugins. A connected editor
/// switches plugins itself, so this list is never sent as a raw value.
const ENABLED_PLUGINS: &str = "editor_plugins/enabled";

/// One patch as the connected editor applies it: each changed setting with
/// the literal the file holds before and after it (`None` for an absent key),
/// and each plugin switched on or off.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EditorChange {
    settings: Vec<(String, Option<String>, Option<String>)>,
    plugins: Vec<(String, bool)>,
}

impl EditorChange {
    fn between(before: &Document, after: &Document) -> Self {
        let old = literals(before);
        let new = literals(after);
        let names: std::collections::BTreeSet<&String> = old.keys().chain(new.keys()).collect();
        let settings = names
            .into_iter()
            .filter(|name| name.as_str() != ENABLED_PLUGINS)
            .filter(|name| old.get(*name) != new.get(*name))
            .map(|name| (name.clone(), old.get(name).cloned(), new.get(name).cloned()))
            .collect();
        let was = enabled_plugins(before);
        let now = enabled_plugins(after);
        let plugins = was
            .symmetric_difference(&now)
            .map(|plugin| (plugin.clone(), now.contains(plugin)))
            .collect();
        Self { settings, plugins }
    }

    /// The setting names this change asks for, as `changed_keys` names them.
    fn requested(&self) -> std::collections::BTreeSet<String> {
        let mut requested: std::collections::BTreeSet<String> = self
            .settings
            .iter()
            .map(|(name, _, _)| name.clone())
            .collect();
        if !self.plugins.is_empty() {
            requested.insert(ENABLED_PLUGINS.to_owned());
        }
        requested
    }

    fn payload(&self, expected_sha256: &str) -> Value {
        json!({
            "expected_sha256": expected_sha256,
            "changes": self.settings.iter().map(|(key, old, new)| json!({
                "key": key,
                "old": old,
                "new": new,
            })).collect::<Vec<_>>(),
            "plugins": self.plugins.iter().map(|(path, enabled)| json!({
                "path": path,
                "enabled": enabled,
            })).collect::<Vec<_>>(),
        })
    }
}

/// Every setting whose literal differs between two documents, by name.
fn changed_settings(before: &Document, after: &Document) -> Vec<String> {
    let old = literals(before);
    let new = literals(after);
    let names: std::collections::BTreeSet<&String> = old.keys().chain(new.keys()).collect();
    names
        .into_iter()
        .filter(|name| old.get(*name) != new.get(*name))
        .cloned()
        .collect()
}

/// The refusal while the connected plugin predates `editor.settings.apply`.
fn plugin_too_old(status: &crate::editor::bridge::BridgeStatus) -> OutcomeError {
    outcome_error(
        ErrorKind::Unsupported,
        format!(
            "the connected godot_mcp plugin ({}) cannot apply project settings, and \
             project.godot belongs to the running editor; update it with \
             editor.addon.update, then editor.addon.reload, and retry. Nothing was written.",
            status
                .plugin_version
                .as_deref()
                .unwrap_or("unknown version")
        ),
    )
}

/// Apply one settings patch in the connected editor, which owns
/// `project.godot` while it runs, and answer from the bytes it saved.
#[allow(clippy::too_many_arguments)]
async fn in_editor(
    runtime: &AuthoringRuntime,
    bridge: &std::sync::Arc<crate::editor::bridge::EditorBridge>,
    change: &EditorChange,
    before: &str,
    expected: &str,
    mut result: Map<String, Value>,
    envelope: &Envelope,
    canonical: &Value,
    context: &CallContext<'_>,
) -> Result<Value, OutcomeError> {
    use crate::effect::boundary::{CancelSignal, EditorMutation};
    use crate::effect::gateway::EditorEffectIntent;
    use crate::journal::OperationFamily;
    use crate::router::{RouteEffect, RouteRequest};

    let status = bridge.status();
    if !status.methods.iter().any(|method| method == SETTINGS_APPLY) {
        return Err(plugin_too_old(&status));
    }
    let tool = AuthoringOperation::SettingsPatch.tool_name();
    let access = context.access;
    let transaction_id = runtime
        .router
        .next_transaction_id(tool, access.observed_at_unix_ms());
    let mut payload = change.payload(expected);
    payload["transaction_id"] = Value::String(transaction_id.clone());
    payload["label"] = Value::String(format!("godot-mcp: {tool}"));
    // Switching a plugin runs that plugin's own code in the editor, which may
    // write any project file.
    let project_wide = !change.plugins.is_empty();
    let intent = EditorEffectIntent {
        transaction_id: transaction_id.clone(),
        tool_name: tool.to_owned(),
        operation_family: OperationFamily::ProjectMutation,
        idempotency_id: context.idempotency_id.to_owned(),
        transport_mode: super::journal_transport(access.transport_mode()),
        principal: Some(access.mode_principal().to_journal_string()),
        session: Some(access.connection_session().to_owned()),
        arguments: canonical.clone(),
        files: vec![(
            MANIFEST.to_owned(),
            WritePrecondition::Sha256(expected.to_owned()),
        )],
        project_wide,
        observe_all: project_wide,
        expected_revision: envelope.expected_revision,
        opened_at_unix_ms: access.observed_at_unix_ms(),
    };
    let request = RouteRequest {
        tool: tool.to_owned(),
        idempotency_id: context.idempotency_id.to_owned(),
        transaction_id: Some(transaction_id),
        path: MANIFEST.to_owned(),
        bytes: Vec::new(),
        arguments: canonical.clone(),
        observation: runtime.observation(),
        cancellation: context.cancellation.clone(),
        queued_at_unix_ms: access.observed_at_unix_ms(),
        identity: Some(AuthoringRuntime::route_identity(access)),
        effect: Some(RouteEffect::Editor {
            intent,
            mutation: Box::new(EditorMutation {
                action: SETTINGS_APPLY.to_owned(),
                payload: payload.to_string(),
                idempotency_id: context.idempotency_id.to_owned(),
                timeout: SETTINGS_APPLY_TIMEOUT,
                cancellation: CancelSignal::default(),
            }),
        }),
        audit: Some(context.audit.clone()),
    };
    let report = runtime
        .router
        .route(&request)
        .await
        .map_err(|refusal| refusal.to_outcome())?;
    let answer = crate::editor::tools::editor_answer(&report)?;
    if answer.get("replayed") == Some(&Value::Bool(true)) {
        return Ok(answer);
    }

    // The file the editor saved is the published result.
    let saved = runtime
        .read(MANIFEST)?
        .ok_or_else(|| outcome_error(ErrorKind::Internal, "the editor removed project.godot"))?;
    let after = utf8_text(&saved, MANIFEST)?;
    let after_document = Document::parse(&after);
    let settings = ProjectSettings::parse(&after, runtime.generation)
        .map_err(|error| outcome_error(error.kind(), error.to_string()))?;
    let changed = changed_settings(&Document::parse(before), &after_document);
    let requested = change.requested();
    let unrequested: Vec<String> = changed
        .iter()
        .filter(|name| !requested.contains(*name))
        .cloned()
        .collect();
    result.insert("after_sha256".to_owned(), Value::String(sha256_hex(&saved)));
    result.insert("changed".to_owned(), Value::Bool(after != before));
    result.insert(
        "diff".to_owned(),
        unified(Some(MANIFEST), Some(MANIFEST), before, &after).to_value(),
    );
    result.insert(
        "settings".to_owned(),
        typed_view(&after_document, &settings),
    );
    result.insert("changed_keys".to_owned(), json!(changed));
    result.insert("unrequested_keys".to_owned(), json!(unrequested));
    for key in ["transaction_id", "workspace", "mutation_status"] {
        if let Some(value) = answer.get(key) {
            result.insert(key.to_owned(), value.clone());
        }
    }
    result.insert(
        "editor".to_owned(),
        json!({ "action": answer.get("action"), "saved_sha256": answer.get("sha256") }),
    );
    result.insert(
        "phase".to_owned(),
        Value::String("committed_fsynced".to_owned()),
    );
    result.insert(
        "conclusion".to_owned(),
        Value::String("proven_post_state".to_owned()),
    );
    result.insert("dry_run".to_owned(), Value::Bool(false));
    result.insert("replayed".to_owned(), Value::Bool(false));
    Ok(Value::Object(result))
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
    fn removing_an_autoload_keeps_the_others() {
        let both = apply(
            MANIFEST_TEXT,
            json!([
                { "op": "set_autoload", "name": "Game", "path": "res://autoload/game.gd" },
                { "op": "set_autoload", "name": "Audio", "path": "res://autoload/audio.gd", "singleton": false }
            ]),
        );
        let removed = apply(&both, json!([{ "op": "remove_autoload", "name": "Game" }]));
        assert!(!removed.contains("Game="), "{removed}");
        assert!(
            removed.contains("Audio=\"res://autoload/audio.gd\"\n"),
            "{removed}"
        );
        let settings = ProjectSettings::parse(&removed, 1).expect("parses");
        assert_eq!(settings.autoloads().len(), 1);
        let absent = apply(
            &removed,
            json!([{ "op": "remove_autoload", "name": "Missing" }]),
        );
        assert_eq!(
            absent, removed,
            "removing an absent autoload changes nothing"
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

    /// The `set_setting` and `clear_setting` schemas and the operation check
    /// give the same verdict on every case, and it is the intended one: any
    /// sectioned setting but those a typed operation owns, set to a tagged
    /// value that is neither null nor a reference.
    #[test]
    fn the_generic_setting_schema_and_the_operation_check_agree() {
        let set = |setting: &str, value: Value| json!({ "op": "set_setting", "setting": setting, "value": value });
        let clear = |setting: &str| json!({ "op": "clear_setting", "setting": setting });
        let text = |text: &str| json!({ "type": "string", "value": text });
        let resource = json!({
            "type": "resource",
            "value": { "resource": "res://a.tres", "resource_type": "Resource" }
        });
        let longest = format!("section/{}", "k".repeat(MAX_SETTING_NAME - 8));
        let too_long = format!("{longest}k");
        let cases = [
            (set("layer_names/2d_physics/layer_1", text("player")), true),
            (set("application/config/name", text("Game")), true),
            (
                set("rendering/renderer/rendering_method.mobile", text("mobile")),
                true,
            ),
            (
                set(
                    "my-plugin/settings/table",
                    json!({ "type": "dictionary", "value": {
                        "speed": { "type": "integer", "value": 3 },
                        "tags": { "type": "packed_string_array", "value": ["a", "b"] }
                    } }),
                ),
                true,
            ),
            (
                set(
                    "rendering/environment/defaults/default_clear_color",
                    json!({ "type": "color", "value": { "red": 0.1, "green": 0.2, "blue": 0.3, "alpha": 1.0 } }),
                ),
                true,
            ),
            (
                set(
                    "physics/common/physics_jitter_fix",
                    json!({ "type": "float", "value": 0.5 }),
                ),
                true,
            ),
            (
                set(
                    "display/window/size/always_on_top",
                    json!({ "type": "bool", "value": true }),
                ),
                true,
            ),
            (
                set(
                    "my/list",
                    json!({ "type": "array", "value": [
                        { "type": "null" },
                        { "type": "vector2", "value": { "x": 1, "y": 2 } }
                    ] }),
                ),
                true,
            ),
            (set(&longest, text("x")), true),
            (clear("layer_names/2d_physics/layer_1"), true),
            (clear("application/config/name"), true),
            (set(&too_long, text("x")), false),
            (set("autoload/Game", text("*res://game.gd")), false),
            (set("input/jump", text("x")), false),
            (
                set("application/run/main_scene", text("res://main.tscn")),
                false,
            ),
            (
                set(
                    "editor_plugins/enabled",
                    json!({ "type": "packed_string_array", "value": [] }),
                ),
                false,
            ),
            (
                set("config_version", json!({ "type": "integer", "value": 5 })),
                false,
            ),
            (set("a//b", text("x")), false),
            (set("/a/b", text("x")), false),
            (set("a/b/", text("x")), false),
            (set("bad name/x", text("x")), false),
            (set("a/b", json!("plain")), false),
            (set("a/b", json!({ "type": "null" })), false),
            (set("a/b", json!({ "type": "null", "value": null })), false),
            (set("a/b", resource.clone()), false),
            (
                set("a/b", json!({ "type": "array", "value": [resource] })),
                false,
            ),
            (
                set("a/b", json!({ "type": "string", "value": "x", "extra": 1 })),
                false,
            ),
            (
                set("a/b", json!({ "type": "no_such_type", "value": 1 })),
                false,
            ),
            (json!({ "op": "set_setting", "setting": "a/b" }), false),
            (
                json!({ "op": "set_setting", "setting": "a/b", "value": text("x"), "x": 1 }),
                false,
            ),
            (clear("autoload/Game"), false),
            (clear("input/jump"), false),
            (clear("editor_plugins/enabled"), false),
            (clear("config_version"), false),
            (
                json!({ "op": "clear_setting", "setting": "a/b", "value": text("x") }),
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

    /// `set_setting` writes Godot's spelling of the value, which the file
    /// then holds and `project.settings.get` reads back as the same tagged
    /// value; comments and unrelated keys stay; `clear_setting` removes the
    /// key and says whether there was one.
    #[test]
    fn generic_settings_round_trip_and_clear() {
        let values = [
            (
                "layer_names/2d_physics/layer_1",
                json!({ "type": "string", "value": "player \"one\"\nsecond line" }),
                "\"player \\\"one\\\"\\nsecond line\"",
            ),
            (
                "rendering/environment/defaults/default_clear_color",
                json!({ "type": "color", "value": { "red": 0.25, "green": 0.5, "blue": 0.75, "alpha": 1.0 } }),
                "Color(0.25, 0.5, 0.75, 1)",
            ),
            (
                "application/config/features",
                json!({ "type": "packed_string_array", "value": ["4.7", "Mobile"] }),
                "PackedStringArray(\"4.7\", \"Mobile\")",
            ),
            (
                "physics/common/physics_ticks_per_second",
                json!({ "type": "integer", "value": 120 }),
                "120",
            ),
            (
                "physics/common/physics_jitter_fix",
                json!({ "type": "float", "value": 1.0 }),
                "1.0",
            ),
            (
                "my_plugin/config/table",
                json!({ "type": "dictionary", "value": {
                    "speed": { "type": "integer", "value": 3 },
                    "offset": { "type": "vector2", "value": { "x": 1.5, "y": -2.0 } }
                } }),
                "{\n\"offset\": Vector2(1.5, -2),\n\"speed\": 3\n}",
            ),
        ];
        let operations: Vec<Value> = values
            .iter()
            .map(|(setting, value, _)| json!({ "op": "set_setting", "setting": setting, "value": value }))
            .collect();
        let after = apply(MANIFEST_TEXT, Value::Array(operations));
        assert!(after.starts_with("; Engine configuration file.\n; keep this comment\n"));
        assert!(after.contains("textures/vram_compression/import_etc2_astc=true\n"));
        assert!(after.contains("config/name=\"Demo\"\n"));
        assert!(after.contains("[layer_names]\n"), "{after}");
        let settings = ProjectSettings::parse(&after, 1).expect("parses");
        assert!(
            settings.diagnostics().is_empty(),
            "{:?}",
            settings.diagnostics()
        );
        let document = Document::parse(&after);
        let held = literals(&document);
        let names: Vec<&str> = values.iter().map(|(setting, _, _)| *setting).collect();
        let selected = selected_entries(
            &document,
            json!({ "settings": names }).as_object().expect("arguments"),
        )
        .expect("selects")
        .expect("entries");
        for (setting, value, literal) in &values {
            assert_eq!(
                held.get(*setting).map(String::as_str),
                Some(*literal),
                "{setting}"
            );
            let entry = selected["entries"]
                .as_array()
                .expect("entries")
                .iter()
                .find(|entry| entry["setting"] == json!(setting))
                .expect("selected");
            assert_eq!(&entry["value"], value, "{setting}");
        }

        let mut document = Document::parse(&after);
        let mut requirements = Requirements { files: Vec::new() };
        let cleared = apply_operation(
            &mut document,
            &json!({ "op": "clear_setting", "setting": "my_plugin/config/table" }),
            &mut requirements,
        )
        .expect("clears");
        assert_eq!(cleared["changed"], true);
        let again = apply_operation(
            &mut document,
            &json!({ "op": "clear_setting", "setting": "my_plugin/config/table" }),
            &mut requirements,
        )
        .expect("clears");
        assert_eq!(again["changed"], false);
        let rendered = document.render();
        assert!(!rendered.contains("table="), "{rendered}");
        assert!(!rendered.contains("\"speed\""), "{rendered}");
        assert!(rendered.contains("physics_jitter_fix=1.0\n"), "{rendered}");
    }

    /// Godot writes a string's newlines as they are, so a string may span
    /// lines; the whole string is one assignment, even when a later line of
    /// it looks like one.
    #[test]
    fn a_string_spanning_lines_is_one_assignment() {
        let text = "[application]\n\nconfig/description=\"first line\nsecond = line [\"\nconfig/name=\"Demo\"\n";
        let document = Document::parse(text);
        let held = literals(&document);
        assert_eq!(
            held.get("application/config/description")
                .map(String::as_str),
            Some("\"first line\nsecond = line [\"")
        );
        assert_eq!(
            held.get("application/config/name").map(String::as_str),
            Some("\"Demo\"")
        );
        assert_eq!(held.len(), 2, "{held:?}");
        let selected = selected_entries(
            &document,
            json!({ "settings": ["application/config/description"] })
                .as_object()
                .expect("arguments"),
        )
        .expect("selects")
        .expect("entries");
        assert_eq!(
            selected["entries"][0]["value"],
            json!({ "type": "string", "value": "first line\nsecond = line [" })
        );
        let after = apply(
            text,
            json!([{ "op": "set_value", "setting": "application/config/description", "value": "one line" }]),
        );
        assert_eq!(
            after,
            "[application]\n\nconfig/description=\"one line\"\nconfig/name=\"Demo\"\n"
        );
    }

    /// `project.settings.get` answers the settings it names, and every
    /// setting under a prefix, in name order, each with the literal the file
    /// holds and its tagged value; a setting the file does not hold answers
    /// nulls, and a literal with no plain value, as an input action, keeps
    /// its text with a null value.
    #[test]
    fn get_selects_settings_by_name_and_prefix() {
        let text = apply(
            MANIFEST_TEXT,
            json!([
                { "op": "set_setting", "setting": "layer_names/2d_physics/layer_2", "value": { "type": "string", "value": "enemies" } },
                { "op": "set_setting", "setting": "layer_names/2d_physics/layer_1", "value": { "type": "string", "value": "player" } },
                { "op": "set_input_action", "name": "jump", "events": [{ "type": "key", "key": "Space" }] }
            ]),
        );
        let document = Document::parse(&text);
        let select = |arguments: Value| {
            selected_entries(&document, arguments.as_object().expect("arguments")).expect("selects")
        };
        assert_eq!(select(json!({})), None);
        let named = select(json!({ "settings": ["no/such_setting", "application/config/name"] }))
            .expect("entries");
        assert_eq!(named["truncated"], false);
        assert_eq!(
            named["entries"],
            json!([
                { "setting": "application/config/name", "literal": "\"Demo\"", "value": { "type": "string", "value": "Demo" } },
                { "setting": "no/such_setting", "literal": null, "value": null }
            ])
        );
        let layers = select(json!({ "prefix": "layer_names/" })).expect("entries");
        assert_eq!(
            layers["entries"],
            json!([
                { "setting": "layer_names/2d_physics/layer_1", "literal": "\"player\"", "value": { "type": "string", "value": "player" } },
                { "setting": "layer_names/2d_physics/layer_2", "literal": "\"enemies\"", "value": { "type": "string", "value": "enemies" } }
            ])
        );
        let input = select(json!({ "settings": ["input/jump"] })).expect("entries");
        assert!(
            input["entries"][0]["literal"]
                .as_str()
                .expect("literal")
                .contains("InputEventKey")
        );
        assert_eq!(input["entries"][0]["value"], Value::Null);
        let all = select(json!({ "prefix": "" })).expect("entries");
        let names: Vec<&str> = all["entries"]
            .as_array()
            .expect("entries")
            .iter()
            .map(|entry| entry["setting"].as_str().expect("name"))
            .collect();
        assert_eq!(
            names,
            [
                "application/config/features",
                "application/config/name",
                "input/jump",
                "layer_names/2d_physics/layer_1",
                "layer_names/2d_physics/layer_2",
                "rendering/textures/vram_compression/import_etc2_astc"
            ]
        );

        let many: String = std::iter::once("[many]\n".to_owned())
            .chain((0..=MAX_SELECTED_SETTINGS).map(|index| format!("k{index:04}={index}\n")))
            .collect();
        let selected = selected_entries(
            &Document::parse(&many),
            json!({ "prefix": "many/" }).as_object().expect("arguments"),
        )
        .expect("selects")
        .expect("entries");
        assert_eq!(selected["truncated"], true);
        assert_eq!(
            selected["entries"].as_array().expect("entries").len(),
            MAX_SELECTED_SETTINGS
        );
    }

    /// An autoload's name is an identifier of at most 128 characters, and an
    /// input action's name 1 to 128 characters, whatever the bytes each
    /// takes, without a control character, a quote, `=`, `:`, a bracket or a
    /// brace, in both the schema and the check.
    #[test]
    fn the_names_an_operation_sets_follow_one_rule_in_the_schema_and_the_check() {
        use crate::schema::conformance::{NameRule, name_cases};
        let key = json!({ "type": "key", "key": "Space" });
        let action =
            |name: &str| json!({ "op": "set_input_action", "name": name, "events": [key] });
        let autoload = |name: &str| json!({ "op": "set_autoload", "name": name, "path": "res://autoload/game.gd" });
        let rule = NameRule {
            limit: 128,
            empty: false,
            control: true,
        };
        let mut cases: Vec<(Value, bool)> = name_cases(rule, "", "")
            .into_iter()
            .map(|(name, expected)| (action(&name), expected))
            .collect();
        for (name, expected) in [
            ("jump", true),
            ("a b", true),
            ("a\"b", false),
            ("a=b", false),
            ("a:b", false),
            ("a[b", false),
            ("a]b", false),
            ("a{b", false),
            ("a}b", false),
        ] {
            cases.push((action(name), expected));
        }
        for (name, expected) in [
            ("Game".to_owned(), true),
            ("_Game2".to_owned(), true),
            ("A".repeat(128), true),
            ("A".repeat(129), false),
            ("2Game".to_owned(), false),
            ("bad name".to_owned(), false),
            ("Ga-me".to_owned(), false),
            ("\u{540D}".to_owned(), false),
            (String::new(), false),
        ] {
            cases.push((autoload(&name), expected));
        }
        let schema = advertised_operation_schema();
        for (case, expected) in &cases {
            let shown = format!("{} {:?}", case["op"], case["name"]);
            assert_eq!(
                crate::schema::conformance::conforms(&schema, case),
                *expected,
                "the schema's verdict on {shown}"
            );
            let mut document = Document::parse(MANIFEST_TEXT);
            let mut requirements = Requirements { files: Vec::new() };
            assert_eq!(
                apply_operation(&mut document, case, &mut requirements).is_ok(),
                *expected,
                "the handler's verdict on {shown}"
            );
        }
    }

    /// A text setting's value is at most 4096 characters, whatever the bytes
    /// each takes, and free text: it may be empty or hold a control
    /// character, a tab or a newline in a description. An autoload or input
    /// action to remove is named by a non-empty string, as the schema says.
    #[test]
    fn text_values_and_removed_names_follow_one_rule_in_the_schema_and_the_check() {
        use crate::schema::conformance::{NameRule, name_cases};
        let text = |value: &str| json!({ "op": "set_value", "setting": "application/config/description", "value": value });
        let rule = NameRule {
            limit: 4096,
            empty: true,
            control: false,
        };
        let mut cases: Vec<(Value, bool)> = name_cases(rule, "", "")
            .into_iter()
            .map(|(value, expected)| (text(&value), expected))
            .collect();
        cases.push((text("line one\nline two\tend"), true));
        for op in ["remove_autoload", "remove_input_action"] {
            for (name, expected) in [
                ("Game", true),
                ("\u{1F600}", true),
                ("a\u{1F}b", true),
                ("", false),
            ] {
                cases.push((json!({ "op": op, "name": name }), expected));
            }
            cases.push((json!({ "op": op, "name": 3 }), false));
            cases.push((json!({ "op": op }), false));
        }
        let schema = advertised_operation_schema();
        for (case, expected) in &cases {
            let shown = format!(
                "{} {}",
                case["op"],
                case.get("value").or_else(|| case.get("name")).map_or_else(
                    || "nothing".to_owned(),
                    |value| {
                        format!(
                            "{} characters",
                            value.as_str().map_or(0, |text| text.chars().count())
                        )
                    }
                )
            );
            assert_eq!(
                crate::schema::conformance::conforms(&schema, case),
                *expected,
                "the schema's verdict on {shown}: {case}"
            );
            let mut document = Document::parse(MANIFEST_TEXT);
            let mut requirements = Requirements { files: Vec::new() };
            assert_eq!(
                apply_operation(&mut document, case, &mut requirements).is_ok(),
                *expected,
                "the handler's verdict on {shown}: {case}"
            );
        }
    }

    /// The `project.settings.get` schema and the selection check give the
    /// same verdict on every case. A name's bound counts characters, as the
    /// schema does.
    #[test]
    fn the_get_schema_and_the_selection_check_agree() {
        let document = Document::parse(MANIFEST_TEXT);
        let many: Vec<String> = (0..=MAX_SELECTED_SETTINGS)
            .map(|index| format!("a/k{index}"))
            .collect();
        let cases = [
            (json!({}), true),
            (json!({ "settings": [] }), true),
            (
                json!({ "settings": ["application/config/name", "application/config/name"] }),
                true,
            ),
            (json!({ "settings": ["é".repeat(MAX_SETTING_NAME)] }), true),
            (json!({ "prefix": "" }), true),
            (
                json!({ "prefix": "layer_names/", "settings": ["a/b"] }),
                true,
            ),
            (json!({ "settings": [""] }), false),
            (json!({ "settings": [5] }), false),
            (json!({ "settings": "a/b" }), false),
            (json!({ "settings": many }), false),
            (
                json!({ "settings": ["é".repeat(MAX_SETTING_NAME + 1)] }),
                false,
            ),
            (json!({ "prefix": 5 }), false),
            (json!({ "prefix": "x".repeat(MAX_SETTING_NAME + 1) }), false),
        ];
        let schema = get_schema();
        for (case, expected) in &cases {
            assert_eq!(
                crate::schema::conformance::conforms(&schema, case),
                *expected,
                "the schema's verdict on {case}"
            );
            assert_eq!(
                selected_entries(&document, case.as_object().expect("object")).is_ok(),
                *expected,
                "the handler's verdict on {case}"
            );
        }
    }
}
