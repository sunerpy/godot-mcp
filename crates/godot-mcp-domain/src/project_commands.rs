//! Commands a Godot project declares for MCP clients to call.
//!
//! A project lists its commands in [`DECLARATION_FILE`] at its root. Each
//! provider names one GDScript file whose class implements
//! `func mcp_call(command: String, args: Dictionary)`; each command has a name, a
//! title, a description, a JSON Schema for its arguments and a `mutates` flag.
//! The server reads the declaration without running any project code, checks
//! a call's arguments against the declared schema, and spells them as the
//! tagged values the editor bridge carries.
//!
//! [`Declaration::parse`] never fails: it returns every provider and command it
//! could read together with every problem it found, so a listing can show both.
//! A declaration with any problem is not callable at all. A command whose
//! input schema leaves the [`crate::jsonschema`] subset is a different case:
//! the declaration stays valid, the command is listed with the reason, and only
//! that command refuses calls.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value, json};

use crate::contracts::GodotValue;
use crate::jsonschema;

/// The declaration's file name, at the project root.
pub const DECLARATION_FILE: &str = "godot_mcp_commands.json";
/// The only declaration format version.
pub const DECLARATION_VERSION: u64 = 1;
/// The largest declaration file read, in bytes.
pub const MAX_DECLARATION_BYTES: usize = 262_144;
/// The most providers one declaration may hold.
pub const MAX_PROVIDERS: usize = 16;
/// The most commands one declaration may hold, over all its providers.
pub const MAX_COMMANDS: usize = 64;
/// The longest provider id or command name, in characters.
pub const MAX_NAME_CHARS: usize = 64;
/// The longest command title, in characters.
pub const MAX_TITLE_CHARS: usize = 128;
/// The longest command description, in characters.
pub const MAX_DESCRIPTION_CHARS: usize = 1_024;
/// The largest input schema of one command, serialized as compact JSON, in
/// bytes.
pub const MAX_INPUT_SCHEMA_BYTES: usize = 16_384;
/// The longest provider script path, in characters.
pub const MAX_SCRIPT_PATH_CHARS: usize = 1_024;
/// The largest arguments object of one call, serialized as compact JSON, in
/// bytes.
pub const MAX_ARGUMENTS_BYTES: usize = 262_144;
/// The deepest argument value the bridge carries: the arguments object is
/// level 0. The add-on's value codec holds the same bound.
pub const MAX_ARGUMENT_DEPTH: usize = 16;
/// The most items of one argument array, or entries of one argument object.
pub const MAX_ARGUMENT_ITEMS: usize = 4_096;
/// The longest argument string, in characters.
pub const MAX_ARGUMENT_STRING_CHARS: usize = 65_536;
/// The largest integer magnitude a JSON number carries exactly: the editor
/// reads every JSON number as a double.
pub const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_992;

/// One declared command.
#[derive(Debug, Clone, PartialEq)]
pub struct Command {
    /// Its name, unique within its provider.
    pub name: String,
    /// A short human title.
    pub title: String,
    /// What it does, for a client deciding whether to call it.
    pub description: String,
    /// The JSON Schema its arguments object must match.
    pub input_schema: Value,
    /// Whether it says it changes project files.
    pub mutates: bool,
    /// Why it cannot be called although the declaration is valid: its input
    /// schema uses a construct the server cannot check arguments against.
    pub unavailable: Option<String>,
}

/// One declared provider: a script and the commands it answers.
#[derive(Debug, Clone, PartialEq)]
pub struct Provider {
    /// Its id, unique within the declaration.
    pub id: String,
    /// The `res://` path of its GDScript file.
    pub script: String,
    /// Its commands.
    pub commands: Vec<Command>,
}

/// One rule a declaration breaks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// JSON pointer to the place in the declaration; `/` is the whole file.
    pub pointer: String,
    /// What is wrong there.
    pub message: String,
}

impl Problem {
    fn new(pointer: &str, message: impl Into<String>) -> Self {
        Self {
            pointer: if pointer.is_empty() {
                "/".to_owned()
            } else {
                pointer.to_owned()
            },
            message: message.into(),
        }
    }
}

/// A read declaration: what could be read, and what is wrong with it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Declaration {
    /// The providers, and of each the commands, that broke no rule.
    pub providers: Vec<Provider>,
    /// Every rule the declaration breaks, in document order.
    pub problems: Vec<Problem>,
}

impl Declaration {
    /// Read a declaration file's bytes.
    #[must_use]
    pub fn parse(bytes: &[u8]) -> Self {
        let mut declaration = Self::default();
        if bytes.len() > MAX_DECLARATION_BYTES {
            declaration.problems.push(Problem::new(
                "",
                format!(
                    "the file is {} bytes; the limit is {MAX_DECLARATION_BYTES}",
                    bytes.len()
                ),
            ));
            return declaration;
        }
        let value: Value = match serde_json::from_slice(bytes) {
            Ok(value) => value,
            Err(error) => {
                declaration
                    .problems
                    .push(Problem::new("", format!("the file is not JSON: {error}")));
                return declaration;
            }
        };
        declaration.read_root(&value);
        declaration
    }

    /// Whether the declaration breaks no rule, so its commands may be called.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.problems.is_empty()
    }

    /// How many commands were read.
    #[must_use]
    pub fn command_count(&self) -> usize {
        self.providers
            .iter()
            .map(|provider| provider.commands.len())
            .sum()
    }

    /// The provider `provider` and its command `command`, when both were read.
    #[must_use]
    pub fn find(&self, provider: &str, command: &str) -> Option<(&Provider, &Command)> {
        let provider = self.providers.iter().find(|found| found.id == provider)?;
        let command = provider
            .commands
            .iter()
            .find(|found| found.name == command)?;
        Some((provider, command))
    }

    fn problem(&mut self, pointer: &str, message: impl Into<String>) {
        self.problems.push(Problem::new(pointer, message));
    }

    fn read_root(&mut self, value: &Value) {
        let Value::Object(root) = value else {
            self.problem("", "the declaration must be a JSON object");
            return;
        };
        self.unknown_keys(root, "", &["$schema", "version", "providers"]);
        if let Some(schema) = root.get("$schema")
            && !schema.is_string()
        {
            self.problem("/$schema", "`$schema` must be a string");
        }
        match root.get("version") {
            Some(version) if version.as_u64() == Some(DECLARATION_VERSION) => {}
            Some(_) => self.problem("/version", "`version` must be 1"),
            None => self.problem("/version", "`version` is required"),
        }
        let providers = match root.get("providers") {
            Some(Value::Array(providers)) => providers,
            Some(_) => {
                self.problem("/providers", "`providers` must be an array");
                return;
            }
            None => {
                self.problem("/providers", "`providers` is required");
                return;
            }
        };
        if providers.len() > MAX_PROVIDERS {
            self.problem(
                "/providers",
                format!(
                    "the declaration holds {} providers; the limit is {MAX_PROVIDERS}",
                    providers.len()
                ),
            );
            return;
        }
        let mut ids = BTreeSet::new();
        for (index, provider) in providers.iter().enumerate() {
            let pointer = format!("/providers/{index}");
            if let Some(read) = self.read_provider(&pointer, provider) {
                if ids.insert(read.id.clone()) {
                    self.providers.push(read);
                } else {
                    self.problem(
                        &format!("{pointer}/id"),
                        format!("provider `{}` is declared more than once", read.id),
                    );
                }
            }
        }
        let total = self.command_count();
        if total > MAX_COMMANDS {
            self.problem(
                "/providers",
                format!("the declaration holds {total} commands; the limit is {MAX_COMMANDS}"),
            );
        }
    }

    fn read_provider(&mut self, pointer: &str, value: &Value) -> Option<Provider> {
        let Value::Object(provider) = value else {
            self.problem(pointer, "a provider must be a JSON object");
            return None;
        };
        self.unknown_keys(provider, pointer, &["id", "script", "commands"]);
        let id = self.read_name(pointer, provider, "id");
        let script = match provider.get("script") {
            Some(Value::String(script)) if valid_script(script) => Some(script.clone()),
            Some(_) => {
                self.problem(
                    &format!("{pointer}/script"),
                    format!(
                        "`script` must be a `res://` path to a `.gd` file of at most \
                         {MAX_SCRIPT_PATH_CHARS} characters, with no empty segment, no \
                         segment starting with `.` and no backslash"
                    ),
                );
                None
            }
            None => {
                self.problem(&format!("{pointer}/script"), "`script` is required");
                None
            }
        };
        let commands_pointer = format!("{pointer}/commands");
        let mut commands = Vec::new();
        match provider.get("commands") {
            Some(Value::Array(declared)) if declared.is_empty() => {
                self.problem(
                    &commands_pointer,
                    "a provider declares at least one command",
                );
            }
            Some(Value::Array(declared)) if declared.len() > MAX_COMMANDS => {
                self.problem(
                    &commands_pointer,
                    format!(
                        "the provider holds {} commands; the limit is {MAX_COMMANDS}",
                        declared.len()
                    ),
                );
            }
            Some(Value::Array(declared)) => {
                let mut names = BTreeSet::new();
                for (index, command) in declared.iter().enumerate() {
                    let at = format!("{commands_pointer}/{index}");
                    if let Some(read) = self.read_command(&at, command) {
                        if names.insert(read.name.clone()) {
                            commands.push(read);
                        } else {
                            self.problem(
                                &format!("{at}/name"),
                                format!("command `{}` is declared more than once", read.name),
                            );
                        }
                    }
                }
            }
            Some(_) => self.problem(&commands_pointer, "`commands` must be an array"),
            None => self.problem(&commands_pointer, "`commands` is required"),
        }
        // A provider whose id or script is broken is not listed. Any problem
        // leaves the whole declaration invalid, so nothing it lists is callable.
        Some(Provider {
            id: id?,
            script: script?,
            commands,
        })
    }

    fn read_command(&mut self, pointer: &str, value: &Value) -> Option<Command> {
        let Value::Object(command) = value else {
            self.problem(pointer, "a command must be a JSON object");
            return None;
        };
        self.unknown_keys(
            command,
            pointer,
            &["name", "title", "description", "input_schema", "mutates"],
        );
        let name = self.read_name(pointer, command, "name");
        let title = self.read_text(pointer, command, "title", MAX_TITLE_CHARS);
        let description = self.read_text(pointer, command, "description", MAX_DESCRIPTION_CHARS);
        let input_schema = match command.get("input_schema") {
            Some(schema @ Value::Object(_)) => {
                let bytes = serde_json::to_vec(schema).map_or(usize::MAX, |bytes| bytes.len());
                if bytes > MAX_INPUT_SCHEMA_BYTES {
                    self.problem(
                        &format!("{pointer}/input_schema"),
                        format!(
                            "the input schema is {bytes} bytes as compact JSON; the limit is \
                             {MAX_INPUT_SCHEMA_BYTES}"
                        ),
                    );
                    None
                } else {
                    Some(schema.clone())
                }
            }
            Some(_) => {
                self.problem(
                    &format!("{pointer}/input_schema"),
                    "`input_schema` must be a JSON object",
                );
                None
            }
            None => {
                self.problem(
                    &format!("{pointer}/input_schema"),
                    "`input_schema` is required",
                );
                None
            }
        };
        let mutates = match command.get("mutates") {
            Some(Value::Bool(mutates)) => Some(*mutates),
            Some(_) => {
                self.problem(
                    &format!("{pointer}/mutates"),
                    "`mutates` must be true or false",
                );
                None
            }
            None => {
                self.problem(&format!("{pointer}/mutates"), "`mutates` is required");
                None
            }
        };
        let input_schema = input_schema?;
        let unavailable = unavailable_reason(&input_schema);
        Some(Command {
            name: name?,
            title: title?,
            description: description?,
            input_schema,
            mutates: mutates?,
            unavailable,
        })
    }

    fn read_name(
        &mut self,
        pointer: &str,
        object: &Map<String, Value>,
        key: &str,
    ) -> Option<String> {
        match object.get(key) {
            Some(Value::String(name)) if valid_name(name) => Some(name.clone()),
            Some(_) => {
                self.problem(
                    &format!("{pointer}/{key}"),
                    format!(
                        "`{key}` must be 1 to {MAX_NAME_CHARS} characters: a lowercase ASCII \
                         letter, then lowercase letters, digits, `_`, `.` or `-`"
                    ),
                );
                None
            }
            None => {
                self.problem(&format!("{pointer}/{key}"), format!("`{key}` is required"));
                None
            }
        }
    }

    fn read_text(
        &mut self,
        pointer: &str,
        object: &Map<String, Value>,
        key: &str,
        limit: usize,
    ) -> Option<String> {
        match object.get(key) {
            Some(Value::String(text)) if !text.is_empty() && text.chars().count() <= limit => {
                Some(text.clone())
            }
            Some(_) => {
                self.problem(
                    &format!("{pointer}/{key}"),
                    format!("`{key}` must be a string of 1 to {limit} characters"),
                );
                None
            }
            None => {
                self.problem(&format!("{pointer}/{key}"), format!("`{key}` is required"));
                None
            }
        }
    }

    fn unknown_keys(&mut self, object: &Map<String, Value>, pointer: &str, known: &[&str]) {
        for key in object.keys() {
            if !known.contains(&key.as_str()) {
                self.problem(
                    &format!("{pointer}/{}", escape_pointer(key)),
                    "this key is not part of the declaration format",
                );
            }
        }
    }
}

/// Why a command with this input schema cannot be called, if it cannot.
fn unavailable_reason(input_schema: &Value) -> Option<String> {
    if let Err(error) = jsonschema::check_schema(input_schema) {
        return Some(format!(
            "the input schema uses a construct the server cannot check arguments against, at \
             {}: {}",
            error.pointer, error.reason
        ));
    }
    if input_schema.get("type") != Some(&json!("object")) {
        return Some(
            "the input schema must have \"type\": \"object\" at its top, since the arguments \
             of a call are one JSON object"
                .to_owned(),
        );
    }
    None
}

/// Whether `name` is a valid provider id or command name: 1 to
/// [`MAX_NAME_CHARS`] characters, a lowercase ASCII letter first, then
/// lowercase letters, digits, `_`, `.` or `-`.
#[must_use]
pub fn valid_name(name: &str) -> bool {
    let mut characters = name.chars();
    name.len() <= MAX_NAME_CHARS
        && characters
            .next()
            .is_some_and(|first| first.is_ascii_lowercase())
        && characters.all(|rest| {
            rest.is_ascii_lowercase() || rest.is_ascii_digit() || matches!(rest, '_' | '.' | '-')
        })
}

/// Whether `script` is a `res://` path to a `.gd` file the add-on may load:
/// within [`MAX_SCRIPT_PATH_CHARS`], every segment non-empty and not starting
/// with `.` (so neither `..` nor a hidden directory such as `.godot`), and no
/// backslash or control character.
fn valid_script(script: &str) -> bool {
    let Some(rest) = script.strip_prefix("res://") else {
        return false;
    };
    script.chars().count() <= MAX_SCRIPT_PATH_CHARS
        && rest.ends_with(".gd")
        && !rest
            .chars()
            .any(|character| character == '\\' || character.is_control())
        && rest
            .split('/')
            .all(|segment| !segment.is_empty() && !segment.starts_with('.'))
}

/// A JSON pointer reference token for `name` (RFC 6901).
fn escape_pointer(name: &str) -> String {
    name.replace('~', "~0").replace('/', "~1")
}

/// Check `arguments` for a call of `command` and spell them for the bridge:
/// within [`MAX_ARGUMENTS_BYTES`], matching the command's input schema, and
/// carried exactly by the bridge's tagged values. The caller checks
/// [`Command::unavailable`] first.
///
/// # Errors
/// Says which rule the arguments break.
pub fn prepare_arguments(
    command: &Command,
    arguments: &Map<String, Value>,
) -> Result<GodotValue, String> {
    let bytes = serde_json::to_vec(arguments).map_or(usize::MAX, |bytes| bytes.len());
    if bytes > MAX_ARGUMENTS_BYTES {
        return Err(format!(
            "the arguments are {bytes} bytes as compact JSON; the limit is {MAX_ARGUMENTS_BYTES}"
        ));
    }
    jsonschema::validate(&command.input_schema, &Value::Object(arguments.clone())).map_err(
        |error| {
            format!(
                "the arguments do not match the command's input schema at {}: {}",
                error.pointer, error.reason
            )
        },
    )?;
    tagged_arguments(arguments)
}

/// Spell an arguments object as the tagged dictionary the editor bridge
/// carries, refusing what it cannot carry exactly.
///
/// # Errors
/// Names the argument the bridge cannot carry, and why.
pub fn tagged_arguments(arguments: &Map<String, Value>) -> Result<GodotValue, String> {
    tagged_object(arguments, 0, "")
}

fn tagged_object(
    object: &Map<String, Value>,
    depth: usize,
    pointer: &str,
) -> Result<GodotValue, String> {
    if object.len() > MAX_ARGUMENT_ITEMS {
        return Err(format!(
            "the argument at {} holds more than {MAX_ARGUMENT_ITEMS} entries",
            shown(pointer)
        ));
    }
    let mut entries = BTreeMap::new();
    for (key, value) in object {
        let at = format!("{pointer}/{}", escape_pointer(key));
        entries.insert(key.clone(), tagged(value, depth + 1, &at)?);
    }
    Ok(GodotValue::Dictionary(entries))
}

fn tagged(value: &Value, depth: usize, pointer: &str) -> Result<GodotValue, String> {
    if depth > MAX_ARGUMENT_DEPTH {
        return Err(format!(
            "the argument at {pointer} nests deeper than {MAX_ARGUMENT_DEPTH} levels"
        ));
    }
    Ok(match value {
        Value::Null => GodotValue::Null,
        Value::Bool(flag) => GodotValue::Bool(*flag),
        Value::Number(number) => {
            if let Some(whole) = number.as_i64() {
                if whole.unsigned_abs() > MAX_SAFE_INTEGER.unsigned_abs() {
                    return Err(beyond_safe(pointer));
                }
                GodotValue::Integer(whole)
            } else if number.is_u64() {
                return Err(beyond_safe(pointer));
            } else {
                let float = number
                    .as_f64()
                    .ok_or_else(|| format!("the argument at {pointer} is not a finite number"))?;
                GodotValue::Float(float)
            }
        }
        Value::String(text) => {
            if text.chars().count() > MAX_ARGUMENT_STRING_CHARS {
                return Err(format!(
                    "the argument at {pointer} is longer than {MAX_ARGUMENT_STRING_CHARS} \
                     characters"
                ));
            }
            GodotValue::String(text.clone())
        }
        Value::Array(items) => {
            if items.len() > MAX_ARGUMENT_ITEMS {
                return Err(format!(
                    "the argument at {pointer} holds more than {MAX_ARGUMENT_ITEMS} items"
                ));
            }
            GodotValue::Array(
                items
                    .iter()
                    .enumerate()
                    .map(|(index, item)| tagged(item, depth + 1, &format!("{pointer}/{index}")))
                    .collect::<Result<_, _>>()?,
            )
        }
        Value::Object(object) => tagged_object(object, depth, pointer)?,
    })
}

fn beyond_safe(pointer: &str) -> String {
    format!(
        "the argument at {pointer} is an integer beyond 2^53, which the editor cannot read \
         exactly"
    )
}

fn shown(pointer: &str) -> &str {
    if pointer.is_empty() { "/" } else { pointer }
}

/// The JSON Schema of the declaration file, committed as
/// `schemas/domain/project-commands-v1.schema.json`. It holds the structural
/// rules; the server additionally refuses duplicate ids and names, more than
/// [`MAX_COMMANDS`] commands in total, a script path with an empty or
/// dot-leading segment, an input schema over [`MAX_INPUT_SCHEMA_BYTES`], and
/// it decides per command whether the input schema is checkable.
#[must_use]
pub fn declaration_schema() -> Value {
    let name = json!({
        "type": "string",
        "minLength": 1,
        "maxLength": MAX_NAME_CHARS,
        "pattern": "^[a-z][a-z0-9_.-]*$"
    });
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "https://godot-mcp.invalid/schemas/domain/project-commands-v1.schema.json",
        "title": "Project command declaration (v1)",
        "description": "The godot_mcp_commands.json file at a Godot project's root: the commands the project declares for MCP clients to call through a connected editor. Each provider names a GDScript file whose class implements mcp_call(command, args).",
        "type": "object",
        "additionalProperties": false,
        "required": ["version", "providers"],
        "properties": {
            "$schema": { "type": "string" },
            "version": { "const": DECLARATION_VERSION },
            "providers": {
                "type": "array",
                "maxItems": MAX_PROVIDERS,
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["id", "script", "commands"],
                    "properties": {
                        "id": name.clone(),
                        "script": {
                            "type": "string",
                            "minLength": 10,
                            "maxLength": MAX_SCRIPT_PATH_CHARS,
                            "pattern": "^res://[^\\\\]+\\.gd$"
                        },
                        "commands": {
                            "type": "array",
                            "minItems": 1,
                            "maxItems": MAX_COMMANDS,
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "required": ["name", "title", "description", "input_schema", "mutates"],
                                "properties": {
                                    "name": name,
                                    "title": { "type": "string", "minLength": 1, "maxLength": MAX_TITLE_CHARS },
                                    "description": { "type": "string", "minLength": 1, "maxLength": MAX_DESCRIPTION_CHARS },
                                    "input_schema": { "type": "object" },
                                    "mutates": { "type": "boolean" }
                                }
                            }
                        }
                    }
                }
            }
        }
    })
}
