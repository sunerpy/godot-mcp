//! Named setups: a recipe turns a name and a few parameters into the typed
//! operations an authoring batch takes.
//!
//! A scene recipe adds nodes below one `parent` node of an existing scene: its
//! `.` is that parent. The operations are exactly those `scene.batch.apply`
//! accepts, so `scene.recipe.apply` applies them through the same journaled
//! batch, and the same array works with `editor.scene.edit`, `scene.create` and
//! `project.batch.apply`. The `theme` recipe makes a resource instead, for
//! `resource.create`.
//!
//! Parameters are plain JSON values. A *variant* parameter (a choice or a
//! boolean) has a default; a *property* parameter has none, and when it is
//! omitted no operation is written for it, so Godot's own default stands. A
//! parameter that does not apply to the chosen variant is refused, never
//! ignored.
//!
//! This module is the one source of the recipes: the tool's input schema, its
//! parser, the MCP prompts and the recipes page are all derived from
//! [`RECIPES`]. Every limit below is stated the same way in the schema and in
//! the parser, so the two give the same verdict on every request.

use std::collections::BTreeMap;
use std::fmt;

use serde_json::{Map, Value, json};

use crate::contracts::ProjectResource;
use crate::editing_methods::is_class_name;

/// The public name of the tool that applies a scene recipe.
pub const RECIPE_TOOL: &str = "scene.recipe.apply";

/// Longest `parent` node path, in characters.
pub const MAX_PARENT_CHARS: usize = 80;
/// Longest node name, in characters. With [`MAX_PARENT_CHARS`] and the longest
/// fixed child path a recipe adds, every node path stays within the 512
/// characters an authoring batch accepts.
pub const MAX_NAME_CHARS: usize = 40;
/// Longest label text, in characters.
const MAX_TEXT_CHARS: usize = 256;
/// Longest bus, action or item name, in characters.
const MAX_LABEL_CHARS: usize = 128;
/// Longest `res://` path, in characters.
const MAX_RESOURCE_PATH_CHARS: usize = 1024;
/// Longest shader uniform name, in characters.
const MAX_UNIFORM_NAME_CHARS: usize = 64;
/// Most cells one grid map recipe paints: the batch's own `repeat` limit.
pub const MAX_RECIPE_CELLS: usize = crate::editing_methods::MAX_REPEATED_CALLS;
/// Most shader uniforms one recipe sets.
pub const MAX_RECIPE_UNIFORMS: usize = 64;
/// Largest collision or navigation layer bit field.
const MAX_LAYERS: i64 = 4_294_967_295;
/// The largest whole number a JSON number holds exactly.
const MAX_EXACT_INTEGER: i64 = 9_007_199_254_740_991;

/// Characters a node name or a node path segment never holds: Godot refuses
/// `. : @ / " %` in a name, and no field of a batch holds a control character.
const CONTROL: &str = r"\x00-\x1F\x7F-\x9F";

/// One segment of a normalized project path, as `ProjectResource` parses one:
/// not empty, not `.` or `..`, and without a slash or a backslash.
const RESOURCE_SEGMENT: &str = r"(?:[^/\\.][^/\\]*|\.[^/\\.][^/\\]*|\.\.[^/\\]+)";

/// What a recipe makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecipeTarget {
    /// Nodes below `parent` in an existing scene.
    Scene,
    /// The operations of a new resource of this class, for `resource.create`.
    Resource {
        /// The resource class to create.
        class: &'static str,
    },
}

/// The JSON shape one parameter takes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ParameterKind {
    /// A whole number in `minimum..=maximum`.
    Integer { minimum: i64, maximum: i64 },
    /// A number in the range; `exclusive_minimum` excludes `minimum` itself.
    Number {
        minimum: f64,
        maximum: f64,
        exclusive_minimum: bool,
    },
    /// `true` or `false`.
    Boolean,
    /// A string of `min_chars` to `max_chars` characters.
    Text { min_chars: usize, max_chars: usize },
    /// A node name: 1 to 40 characters, none of `. : @ / " %` or a control.
    NodeName,
    /// A `res://` path to a project resource, loaded as `resource_type`, whose
    /// last segment ends in `extension` when one is given.
    ResourcePath {
        resource_type: &'static str,
        extension: Option<&'static str>,
    },
    /// One of the listed strings.
    Choice(&'static [&'static str]),
    /// `{ "x": n, "y": n }`; every component above zero when `positive`.
    Vector2 { positive: bool },
    /// `{ "x": n, "y": n, "z": n }`; every component above zero when `positive`.
    Vector3 { positive: bool },
    /// `{ "red": n, "green": n, "blue": n, "alpha": n }`, each from 0 to 16.
    Color,
    /// An array of `{ "x": i, "y": i, "z": i }` grid cells.
    Cells,
    /// `{ "left", "right", "up", "down" }` input action names, at least one.
    Actions,
    /// Shader uniform names mapped to tagged values.
    Uniforms,
}

/// When a parameter applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applies {
    /// In every variant.
    Always,
    /// Only while the choice parameter holds one of these values.
    When(&'static str, &'static [&'static str]),
    /// Only while the boolean parameter is `true`.
    WhenTrue(&'static str),
}

/// The value an omitted parameter takes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Default {
    /// A choice.
    Choice(&'static str),
    /// A boolean.
    Boolean(bool),
    /// A string.
    Text(&'static str),
    /// A two-component vector.
    Vector2(f64, f64),
    /// A value derived from other parameters, which the recipe fills in and
    /// the answer reports; the text says how, for the schema and the page.
    Derived(&'static str),
}

impl Default {
    /// The fixed value, or `None` for a derived default.
    fn value(self) -> Option<Value> {
        match self {
            Self::Choice(text) | Self::Text(text) => Some(Value::String(text.to_owned())),
            Self::Boolean(flag) => Some(Value::Bool(flag)),
            Self::Vector2(x, y) => Some(json!({ "x": x, "y": y })),
            Self::Derived(_) => None,
        }
    }

    /// The default as the parameter table writes it.
    #[must_use]
    pub fn display(self) -> String {
        match self {
            Self::Choice(text) => text.to_owned(),
            Self::Text(text) => format!("\"{text}\""),
            Self::Boolean(flag) => flag.to_string(),
            Self::Vector2(x, y) => format!("{{x: {x}, y: {y}}}"),
            Self::Derived(how) => how.to_owned(),
        }
    }
}

/// One parameter of a recipe.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Parameter {
    /// Its key in the recipe's parameters.
    pub name: &'static str,
    /// The JSON shape it takes.
    pub kind: ParameterKind,
    /// What it sets, in one sentence.
    pub description: &'static str,
    /// The value an omitted parameter takes; `None` writes nothing for it.
    pub default: Option<Default>,
    /// Whether the caller must give it.
    pub required: bool,
    /// The variants it applies to.
    pub applies: Applies,
}

impl Parameter {
    const fn knob(name: &'static str, kind: ParameterKind, description: &'static str) -> Self {
        Self {
            name,
            kind,
            description,
            default: None,
            required: false,
            applies: Applies::Always,
        }
    }

    const fn only(self, applies: Applies) -> Self {
        Self { applies, ..self }
    }

    const fn defaulting(self, default: Default) -> Self {
        Self {
            default: Some(default),
            ..self
        }
    }

    const fn required(self) -> Self {
        Self {
            required: true,
            ..self
        }
    }

    /// The shape it takes, as the parameter table writes it.
    #[must_use]
    pub fn shape(&self) -> String {
        match self.kind {
            ParameterKind::Integer { minimum, maximum } => {
                format!("integer, {minimum} to {maximum}")
            }
            ParameterKind::Number {
                minimum,
                maximum,
                exclusive_minimum,
            } => {
                if exclusive_minimum {
                    format!("number above {minimum}, at most {maximum}")
                } else {
                    format!("number, {minimum} to {maximum}")
                }
            }
            ParameterKind::Boolean => "boolean".to_owned(),
            ParameterKind::Text {
                min_chars,
                max_chars,
            } => format!("string, {min_chars} to {max_chars} characters"),
            ParameterKind::NodeName => format!("node name, 1 to {MAX_NAME_CHARS} characters"),
            ParameterKind::ResourcePath {
                resource_type,
                extension,
            } => {
                let article = if resource_type.starts_with(['A', 'E', 'I', 'O', 'U']) {
                    "an"
                } else {
                    "a"
                };
                match extension {
                    Some(extension) => {
                        format!("res:// path of {article} {resource_type} ({extension})")
                    }
                    None => format!("res:// path of {article} {resource_type}"),
                }
            }
            ParameterKind::Choice(choices) => choices.join(", "),
            ParameterKind::Vector2 { positive } => {
                if positive { "{x, y} above 0" } else { "{x, y}" }.to_owned()
            }
            ParameterKind::Vector3 { positive } => if positive {
                "{x, y, z} above 0"
            } else {
                "{x, y, z}"
            }
            .to_owned(),
            ParameterKind::Color => "{red, green, blue, alpha}, 0 to 16".to_owned(),
            ParameterKind::Cells => format!("1 to {MAX_RECIPE_CELLS} {{x, y, z}} cells"),
            ParameterKind::Actions => "{left, right, up, down} action names".to_owned(),
            ParameterKind::Uniforms => {
                format!("1 to {MAX_RECIPE_UNIFORMS} uniform names to tagged values")
            }
        }
    }

    /// When it applies, as the parameter table writes it.
    #[must_use]
    pub fn condition(&self) -> String {
        match self.applies {
            Applies::Always => "always".to_owned(),
            Applies::When(controller, allowed) => {
                format!("`{controller}` is {}", allowed.join(" or "))
            }
            Applies::WhenTrue(controller) => format!("`{controller}` is true"),
        }
    }
}

/// The parameters of one request, validated, with every default filled in.
#[derive(Debug, Clone, PartialEq)]
pub struct Values(BTreeMap<&'static str, Value>);

impl Values {
    fn get(&self, name: &str) -> Option<&Value> {
        self.0.get(name)
    }

    fn choice(&self, name: &str) -> &str {
        self.get(name).and_then(Value::as_str).unwrap_or_default()
    }

    fn flag(&self, name: &str) -> bool {
        self.get(name).and_then(Value::as_bool).unwrap_or(false)
    }

    fn number(&self, name: &str) -> Option<f64> {
        self.get(name).and_then(Value::as_f64)
    }

    fn integer(&self, name: &str) -> Option<i64> {
        self.get(name).and_then(Value::as_i64)
    }

    fn text(&self, name: &str) -> Option<&str> {
        self.get(name).and_then(Value::as_str)
    }

    /// The resolved parameters as a JSON object, in key order.
    #[must_use]
    pub fn to_map(&self) -> Map<String, Value> {
        self.0
            .iter()
            .map(|(name, value)| ((*name).to_owned(), value.clone()))
            .collect()
    }
}

/// Where a recipe's nodes go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    parent: String,
    name: Option<String>,
}

impl Placement {
    /// The parent node path, relative to the scene root.
    #[must_use]
    pub fn parent(&self) -> &str {
        &self.parent
    }

    /// The primary node's name, for a recipe that has one.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    fn primary_name(&self) -> &str {
        self.name.as_deref().unwrap_or_default()
    }

    /// The path of the primary node.
    fn primary(&self) -> String {
        join(&self.parent, self.primary_name())
    }

    /// The path of a node the recipe adds directly below `parent`.
    fn below(&self, name: &str) -> String {
        join(&self.parent, name)
    }
}

/// `child` below `parent`, where `.` is the scene root.
fn join(parent: &str, child: &str) -> String {
    if parent == "." {
        child.to_owned()
    } else {
        format!("{parent}/{child}")
    }
}

/// One named setup.
pub struct Recipe {
    /// Its name, as the `recipe` object keys it.
    pub id: &'static str,
    /// What it makes, in a few words.
    pub title: &'static str,
    /// What it makes and what to know before using it.
    pub summary: &'static str,
    /// What it makes.
    pub target: RecipeTarget,
    /// The class the parent node is expected to be.
    pub parent_class: &'static str,
    /// The oldest locked Godot build that has every class it uses.
    pub since: Option<&'static str>,
    /// Its parameters, in documentation order.
    pub parameters: &'static [Parameter],
    /// The values of its derived defaults, for the parameters a request left
    /// out: the primary node's name, a particle preset's amount, lifetime and
    /// strip width.
    derive: fn(&Values) -> Vec<(&'static str, Value)>,
    /// Checks across parameters the table cannot state.
    check: fn(&Values) -> Result<(), String>,
    expand: fn(&Values, &Placement) -> Vec<Value>,
}

impl fmt::Debug for Recipe {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Recipe")
            .field("id", &self.id)
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

/// One recipe request, resolved and expanded.
#[derive(Debug, Clone, PartialEq)]
pub struct Expansion {
    /// The recipe.
    pub id: &'static str,
    /// Where its nodes went.
    pub placement: Placement,
    /// The parameters with every default filled in, `name` included.
    pub values: Values,
    /// The typed operations of the batch.
    pub operations: Vec<Value>,
}

impl Expansion {
    /// The answer's `recipe` field: the recipe, its parent and its resolved
    /// parameters.
    #[must_use]
    pub fn describe(&self) -> Value {
        json!({
            "id": self.id,
            "parent": self.placement.parent,
            "parameters": self.values.to_map(),
        })
    }
}

/// Why a recipe request was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipeError(String);

impl RecipeError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }

    /// The refusal, naming the recipe and the parameter.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RecipeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for RecipeError {}

impl Recipe {
    /// Whether the recipe has a primary node, named by its `name` parameter.
    #[must_use]
    pub fn takes_name(&self) -> bool {
        self.parameter("name").is_some()
    }

    /// The parameter called `name`.
    #[must_use]
    pub fn parameter(&self, name: &str) -> Option<&'static Parameter> {
        self.parameters
            .iter()
            .find(|parameter| parameter.name == name)
    }

    /// Validate `parameters` and fill in every default, the primary node's
    /// name included.
    ///
    /// # Errors
    /// Returns [`RecipeError`] for an unknown parameter, a value of the wrong
    /// shape or out of range, a missing required parameter, or a parameter
    /// that does not apply to the variant the request chose.
    pub fn resolve(&self, parameters: &Map<String, Value>) -> Result<Values, RecipeError> {
        let mut values = BTreeMap::new();
        for (key, value) in parameters {
            let Some(parameter) = self.parameter(key) else {
                let accepted: Vec<&str> = self.parameters.iter().map(|p| p.name).collect();
                return Err(RecipeError::new(if accepted.is_empty() {
                    format!("recipe {} takes no parameters; `{key}` is not one", self.id)
                } else {
                    format!(
                        "recipe {} has no parameter `{key}`; it takes {}",
                        self.id,
                        accepted.join(", ")
                    )
                }));
            };
            let normalized = normalize(parameter, value)
                .map_err(|reason| RecipeError::new(format!("{}: `{key}` {reason}", self.id)))?;
            values.insert(parameter.name, normalized);
        }
        for parameter in self.parameters {
            if values.contains_key(parameter.name) {
                continue;
            }
            if parameter.required {
                return Err(RecipeError::new(format!(
                    "{}: `{}` is required: {}",
                    self.id, parameter.name, parameter.description
                )));
            }
            if let Some(value) = parameter.default.and_then(Default::value) {
                values.insert(parameter.name, value);
            }
        }
        for key in parameters.keys() {
            let parameter = self.parameter(key).expect("checked above");
            match parameter.applies {
                Applies::Always => {}
                Applies::When(controller, allowed) => {
                    let current = values
                        .get(controller)
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if !allowed.contains(&current) {
                        return Err(RecipeError::new(format!(
                            "{}: `{key}` applies only when `{controller}` is {}, and it is {current}",
                            self.id,
                            allowed.join(" or ")
                        )));
                    }
                }
                Applies::WhenTrue(controller) => {
                    if values.get(controller).and_then(Value::as_bool) != Some(true) {
                        return Err(RecipeError::new(format!(
                            "{}: `{key}` applies only when `{controller}` is true",
                            self.id
                        )));
                    }
                }
            }
        }
        let mut values = Values(values);
        (self.check)(&values)
            .map_err(|reason| RecipeError::new(format!("{}: {reason}", self.id)))?;
        for (name, value) in (self.derive)(&values) {
            values.0.entry(name).or_insert(value);
        }
        Ok(values)
    }

    /// Validate a request and expand it into its operations. `parent` is a
    /// node path relative to the scene root, `.` (the default) being the root.
    ///
    /// # Errors
    /// The refusals of [`Recipe::resolve`], and a `parent` that is not a node
    /// path or that a resource recipe cannot take.
    pub fn expand(
        &self,
        parameters: &Map<String, Value>,
        parent: Option<&str>,
    ) -> Result<Expansion, RecipeError> {
        let parent = parent.unwrap_or(".");
        validate_parent(parent)
            .map_err(|reason| RecipeError::new(format!("{}: `parent` {reason}", self.id)))?;
        if self.target != RecipeTarget::Scene && parent != "." {
            return Err(RecipeError::new(format!(
                "{} makes a resource and has no parent node",
                self.id
            )));
        }
        let values = self.resolve(parameters)?;
        let placement = Placement {
            parent: parent.to_owned(),
            name: values.text("name").map(str::to_owned),
        };
        let operations = (self.expand)(&values, &placement);
        Ok(Expansion {
            id: self.id,
            placement,
            values,
            operations,
        })
    }

    /// The recipe's parameters as a Markdown table: each one's name, what it
    /// takes, its default, when it applies and what it sets. The recipe's
    /// prompt and the recipes page both print it.
    #[must_use]
    pub fn parameter_table(&self) -> String {
        if self.parameters.is_empty() {
            return "It takes no parameters.\n".to_owned();
        }
        let cell = |text: &str| text.replace('|', "\\|");
        let mut table = String::from(
            "| Parameter | Takes | Default | Applies when | Sets |\n| --- | --- | --- | --- | --- |\n",
        );
        for parameter in self.parameters {
            let default = if parameter.required {
                "required".to_owned()
            } else {
                parameter
                    .default
                    .map_or_else(|| "Godot's".to_owned(), Default::display)
            };
            table.push_str(&format!(
                "| `{}` | {} | {} | {} | {} |\n",
                parameter.name,
                cell(&parameter.shape()),
                cell(&default),
                cell(&parameter.condition()),
                cell(parameter.description)
            ));
        }
        table
    }

    /// The JSON schema of this recipe's parameters object.
    #[must_use]
    pub fn parameters_schema(&self) -> Value {
        let mut properties = Map::new();
        for parameter in self.parameters {
            properties.insert(parameter.name.to_owned(), parameter_schema(parameter));
        }
        let required: Vec<&str> = self
            .parameters
            .iter()
            .filter(|parameter| parameter.required)
            .map(|parameter| parameter.name)
            .collect();
        let mut schema = json!({
            "type": "object",
            "description": self.summary,
            "additionalProperties": false,
            "properties": properties,
        });
        if !required.is_empty() {
            schema["required"] = json!(required);
        }
        let mut conditions = applicability_schema(self);
        if self.id == "light-3d" {
            conditions.push(json!({
                "not": {
                    "required": ["sun", "lamp", "sky", "camera"],
                    "properties": {
                        "sun": { "const": false },
                        "lamp": { "const": false },
                        "sky": { "const": false },
                        "camera": { "const": false }
                    }
                }
            }));
        }
        if !conditions.is_empty() {
            schema["allOf"] = Value::Array(conditions);
        }
        schema
    }
}

/// One condition per conditional parameter: it is absent, or its controlling
/// parameter holds an allowed value, or the controller is absent and its
/// default is allowed.
fn applicability_schema(recipe: &Recipe) -> Vec<Value> {
    let mut conditions = Vec::new();
    for parameter in recipe.parameters {
        let (controller, allowed, default_allowed) = match parameter.applies {
            Applies::Always => continue,
            Applies::When(controller, allowed) => (
                controller,
                json!({ "enum": allowed }),
                matches!(
                    recipe.parameter(controller).and_then(|c| c.default),
                    Some(Default::Choice(default)) if allowed.contains(&default)
                ),
            ),
            Applies::WhenTrue(controller) => (
                controller,
                json!({ "const": true }),
                recipe.parameter(controller).and_then(|c| c.default)
                    == Some(Default::Boolean(true)),
            ),
        };
        let mut options = vec![
            json!({ "not": { "required": [parameter.name] } }),
            json!({ "required": [controller], "properties": { controller: allowed } }),
        ];
        if default_allowed {
            options.push(json!({ "not": { "required": [controller] } }));
        }
        conditions.push(json!({ "anyOf": options }));
    }
    conditions
}

/// The `recipe` property of `scene.recipe.apply`: one scene recipe, keyed by
/// its name, with its parameters.
#[must_use]
pub fn recipe_schema() -> Value {
    let mut properties = Map::new();
    for recipe in scene_recipes() {
        properties.insert(recipe.id.to_owned(), recipe.parameters_schema());
    }
    let names: Vec<&str> = scene_recipes().map(|recipe| recipe.id).collect();
    json!({
        "type": "object",
        "description": format!(
            "One recipe, keyed by its name, with its parameters: {}. For example \
             {{\"particles-2d\": {{\"preset\": \"smoke\"}}}}. A parameter left out keeps the \
             recipe's default or Godot's own.",
            names.join(", ")
        ),
        "minProperties": 1,
        "maxProperties": 1,
        "additionalProperties": false,
        "properties": properties,
    })
}

/// The `parent` property of `scene.recipe.apply`.
#[must_use]
pub fn parent_schema() -> Value {
    let segment = format!(
        "(?:[^/:.{CONTROL}][^/:{CONTROL}]*|\\.[^/:.{CONTROL}][^/:{CONTROL}]*|\\.\\.[^/:{CONTROL}]+)"
    );
    json!({
        "type": "string",
        "minLength": 1,
        "maxLength": MAX_PARENT_CHARS,
        "pattern": format!("^(?:\\.|{segment}(?:/{segment})*)$"),
        "description": "The node the recipe's nodes go below, as a path relative to the scene \
                        root (default `.`, the root). Not a node handle.",
    })
}

fn node_name_pattern() -> String {
    format!("^[^.:@/\"%{CONTROL}]+$")
}

/// A normalized `res://` path, and with an extension, one whose last segment
/// ends in it. The extension is matched structurally rather than by a
/// lookahead, so no `.` wildcard stops at a line break the parser accepts.
fn resource_pattern(extension: Option<&str>) -> String {
    match extension {
        Some(extension) => format!(
            r"^res://(?:{RESOURCE_SEGMENT}/)*[^/\\]*{}$",
            extension.replace('.', r"\.")
        ),
        None => format!("^res://{RESOURCE_SEGMENT}(?:/{RESOURCE_SEGMENT})*$"),
    }
}

fn vector_schema(axes: &[&str], positive: bool) -> Value {
    let mut properties = Map::new();
    for axis in axes {
        let mut component = json!({ "type": "number" });
        if positive {
            component["exclusiveMinimum"] = json!(0);
        }
        properties.insert((*axis).to_owned(), component);
    }
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": properties,
        "required": axes,
    })
}

fn color_schema() -> Value {
    let mut properties = Map::new();
    for channel in ["red", "green", "blue", "alpha"] {
        properties.insert(
            channel.to_owned(),
            json!({ "type": "number", "minimum": 0, "maximum": 16 }),
        );
    }
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": properties,
        "required": ["red", "green", "blue", "alpha"],
    })
}

/// The tagged values a shader uniform may take.
fn uniform_value_schema() -> Value {
    let tagged = |kind: &str, value: Value| {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": { "type": { "const": kind }, "value": value },
            "required": ["type", "value"],
        })
    };
    json!({
        "oneOf": [
            tagged("bool", json!({ "type": "boolean" })),
            tagged("integer", json!({
                "type": "integer",
                "minimum": -MAX_EXACT_INTEGER,
                "maximum": MAX_EXACT_INTEGER
            })),
            tagged("float", json!({ "type": "number" })),
            tagged("vector2", vector_schema(&["x", "y"], false)),
            tagged("vector3", vector_schema(&["x", "y", "z"], false)),
            tagged("vector4", vector_schema(&["x", "y", "z", "w"], false)),
            tagged("color", color_schema()),
            tagged("resource", json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "resource": {
                        "type": "string",
                        "maxLength": MAX_RESOURCE_PATH_CHARS,
                        "pattern": resource_pattern(None)
                    },
                    "resource_type": {
                        "type": "string",
                        "maxLength": crate::editing_methods::MAX_CLASS_NAME_BYTES,
                        "pattern": "^[A-Za-z_][A-Za-z0-9_]*$"
                    }
                },
                "required": ["resource", "resource_type"],
            })),
        ]
    })
}

fn parameter_schema(parameter: &Parameter) -> Value {
    let mut schema = match parameter.kind {
        ParameterKind::Integer { minimum, maximum } => {
            json!({ "type": "integer", "minimum": minimum, "maximum": maximum })
        }
        ParameterKind::Number {
            minimum,
            maximum,
            exclusive_minimum,
        } => {
            let mut schema = json!({ "type": "number", "maximum": maximum });
            if exclusive_minimum {
                schema["exclusiveMinimum"] = json!(minimum);
            } else {
                schema["minimum"] = json!(minimum);
            }
            schema
        }
        ParameterKind::Boolean => json!({ "type": "boolean" }),
        ParameterKind::Text {
            min_chars,
            max_chars,
        } => json!({ "type": "string", "minLength": min_chars, "maxLength": max_chars }),
        ParameterKind::NodeName => json!({
            "type": "string",
            "minLength": 1,
            "maxLength": MAX_NAME_CHARS,
            "pattern": node_name_pattern(),
        }),
        ParameterKind::ResourcePath { extension, .. } => json!({
            "type": "string",
            "maxLength": MAX_RESOURCE_PATH_CHARS,
            "pattern": resource_pattern(extension),
        }),
        ParameterKind::Choice(choices) => json!({ "type": "string", "enum": choices }),
        ParameterKind::Vector2 { positive } => vector_schema(&["x", "y"], positive),
        ParameterKind::Vector3 { positive } => vector_schema(&["x", "y", "z"], positive),
        ParameterKind::Color => color_schema(),
        ParameterKind::Cells => json!({
            "type": "array",
            "minItems": 1,
            "maxItems": MAX_RECIPE_CELLS,
            "items": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "x": { "type": "integer", "minimum": -32768, "maximum": 32767 },
                    "y": { "type": "integer", "minimum": -32768, "maximum": 32767 },
                    "z": { "type": "integer", "minimum": -32768, "maximum": 32767 }
                },
                "required": ["x", "y", "z"],
            },
        }),
        ParameterKind::Actions => {
            let action = json!({ "type": "string", "minLength": 1, "maxLength": MAX_LABEL_CHARS });
            json!({
                "type": "object",
                "additionalProperties": false,
                "minProperties": 1,
                "properties": {
                    "left": action.clone(),
                    "right": action.clone(),
                    "up": action.clone(),
                    "down": action,
                },
            })
        }
        ParameterKind::Uniforms => json!({
            "type": "object",
            "minProperties": 1,
            "maxProperties": MAX_RECIPE_UNIFORMS,
            "propertyNames": {
                "pattern": "^[A-Za-z_][A-Za-z0-9_]*$",
                "maxLength": MAX_UNIFORM_NAME_CHARS
            },
            "additionalProperties": uniform_value_schema(),
        }),
    };
    let mut description = parameter.description.to_owned();
    match parameter.applies {
        Applies::Always => {}
        Applies::When(controller, allowed) => {
            description.push_str(&format!(
                " Only when `{controller}` is {}.",
                allowed.join(" or ")
            ));
        }
        Applies::WhenTrue(controller) => {
            description.push_str(&format!(" Only when `{controller}` is true."));
        }
    }
    match parameter.default {
        Some(Default::Derived(how)) => description.push_str(&format!(" Default: {how}.")),
        Some(_) => {}
        None if parameter.required => {}
        None => description.push_str(" Omitted: Godot's default."),
    }
    schema["description"] = Value::String(description);
    if let Some(value) = parameter.default.and_then(Default::value) {
        schema["default"] = value;
    }
    schema
}

/// Check `value` against `parameter` and return it in canonical form.
fn normalize(parameter: &Parameter, value: &Value) -> Result<Value, String> {
    match parameter.kind {
        ParameterKind::Integer { minimum, maximum } => {
            let whole = whole_number(value).ok_or("must be a whole number")?;
            if whole < minimum || whole > maximum {
                return Err(format!("must be from {minimum} to {maximum}, not {whole}"));
            }
            Ok(json!(whole))
        }
        ParameterKind::Number {
            minimum,
            maximum,
            exclusive_minimum,
        } => {
            let number = finite(value).ok_or("must be a number")?;
            let low = if exclusive_minimum {
                number <= minimum
            } else {
                number < minimum
            };
            if low || number > maximum {
                let bound = if exclusive_minimum {
                    "above"
                } else {
                    "at least"
                };
                return Err(format!(
                    "must be {bound} {minimum} and at most {maximum}, not {number}"
                ));
            }
            Ok(json!(number))
        }
        ParameterKind::Boolean => value
            .as_bool()
            .map(Value::Bool)
            .ok_or_else(|| "must be true or false".to_owned()),
        ParameterKind::Text {
            min_chars,
            max_chars,
        } => {
            let text = value.as_str().ok_or("must be a string")?;
            let characters = text.chars().count();
            if characters < min_chars || characters > max_chars {
                return Err(format!(
                    "must be {min_chars} to {max_chars} characters long"
                ));
            }
            Ok(Value::String(text.to_owned()))
        }
        ParameterKind::NodeName => {
            let name = value.as_str().ok_or("must be a string")?;
            validate_name(name)?;
            Ok(Value::String(name.to_owned()))
        }
        ParameterKind::ResourcePath { extension, .. } => {
            let path = value.as_str().ok_or("must be a res:// path")?;
            validate_resource_path(path)?;
            if let Some(extension) = extension
                && !path.ends_with(extension)
            {
                return Err(format!("must end in {extension}"));
            }
            Ok(Value::String(path.to_owned()))
        }
        ParameterKind::Choice(choices) => {
            let choice = value.as_str().ok_or("must be a string")?;
            if choices.contains(&choice) {
                Ok(Value::String(choice.to_owned()))
            } else {
                Err(format!(
                    "must be one of {}, not {choice:?}",
                    choices.join(", ")
                ))
            }
        }
        ParameterKind::Vector2 { positive } => vector(value, &["x", "y"], positive),
        ParameterKind::Vector3 { positive } => vector(value, &["x", "y", "z"], positive),
        ParameterKind::Color => color(value),
        ParameterKind::Cells => cells(value),
        ParameterKind::Actions => actions(value),
        ParameterKind::Uniforms => uniforms(value),
    }
}

/// A whole number, as the schema's `integer` type takes one: an integer, or a
/// number whose fraction is zero, within the range JSON holds exactly.
fn whole_number(value: &Value) -> Option<i64> {
    let whole = if let Some(whole) = value.as_i64() {
        whole
    } else {
        let number = value.as_f64()?;
        if !number.is_finite() || number.fract() != 0.0 || number.abs() > 9.007_199_254_740_991e15 {
            return None;
        }
        // Bounded just above: exact in an f64 and inside i64.
        #[allow(clippy::cast_possible_truncation)]
        let whole = number as i64;
        whole
    };
    (-MAX_EXACT_INTEGER..=MAX_EXACT_INTEGER)
        .contains(&whole)
        .then_some(whole)
}

fn finite(value: &Value) -> Option<f64> {
    value.as_f64().filter(|number| number.is_finite())
}

fn object_with<'a>(value: &'a Value, keys: &[&str]) -> Result<&'a Map<String, Value>, String> {
    let object = value
        .as_object()
        .ok_or_else(|| format!("must be an object with {}", keys.join(", ")))?;
    if let Some(unknown) = object.keys().find(|key| !keys.contains(&key.as_str())) {
        return Err(format!(
            "has no field `{unknown}`; it takes {}",
            keys.join(", ")
        ));
    }
    Ok(object)
}

fn vector(value: &Value, axes: &[&str], positive: bool) -> Result<Value, String> {
    let object = object_with(value, axes)?;
    let mut normalized = Map::new();
    for axis in axes {
        let component = object
            .get(*axis)
            .and_then(finite)
            .ok_or_else(|| format!("needs a number `{axis}`"))?;
        if positive && component <= 0.0 {
            return Err(format!("needs `{axis}` above zero, not {component}"));
        }
        normalized.insert((*axis).to_owned(), json!(component));
    }
    Ok(Value::Object(normalized))
}

fn color(value: &Value) -> Result<Value, String> {
    let channels = ["red", "green", "blue", "alpha"];
    let object = object_with(value, &channels)?;
    let mut normalized = Map::new();
    for channel in channels {
        let component = object
            .get(channel)
            .and_then(finite)
            .ok_or_else(|| format!("needs a number `{channel}`"))?;
        if !(0.0..=16.0).contains(&component) {
            return Err(format!("needs `{channel}` from 0 to 16, not {component}"));
        }
        normalized.insert(channel.to_owned(), json!(component));
    }
    Ok(Value::Object(normalized))
}

fn cells(value: &Value) -> Result<Value, String> {
    let listed = value.as_array().ok_or("must be an array of cells")?;
    if listed.is_empty() || listed.len() > MAX_RECIPE_CELLS {
        return Err(format!("must hold 1 to {MAX_RECIPE_CELLS} cells"));
    }
    let mut normalized = Vec::with_capacity(listed.len());
    for (index, cell) in listed.iter().enumerate() {
        let object = object_with(cell, &["x", "y", "z"])
            .map_err(|reason| format!("cell {index} {reason}"))?;
        let mut coordinates = Map::new();
        for axis in ["x", "y", "z"] {
            let coordinate = object
                .get(axis)
                .and_then(whole_number)
                .filter(|coordinate| (-32768..=32767).contains(coordinate))
                .ok_or_else(|| {
                    format!("cell {index} needs a whole `{axis}` from -32768 to 32767")
                })?;
            coordinates.insert(axis.to_owned(), json!(coordinate));
        }
        normalized.push(Value::Object(coordinates));
    }
    Ok(Value::Array(normalized))
}

fn actions(value: &Value) -> Result<Value, String> {
    let directions = ["left", "right", "up", "down"];
    let object = object_with(value, &directions)?;
    if object.is_empty() {
        return Err("must name at least one of left, right, up, down".to_owned());
    }
    let mut normalized = Map::new();
    for (direction, action) in object {
        let action = action
            .as_str()
            .filter(|action| (1..=MAX_LABEL_CHARS).contains(&action.chars().count()))
            .ok_or_else(|| {
                format!("needs `{direction}` to be an input action name of 1 to {MAX_LABEL_CHARS} characters")
            })?;
        normalized.insert(direction.clone(), Value::String(action.to_owned()));
    }
    Ok(Value::Object(normalized))
}

fn is_identifier(name: &str) -> bool {
    name.chars()
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn uniforms(value: &Value) -> Result<Value, String> {
    let object = value
        .as_object()
        .ok_or("must map uniform names to tagged values")?;
    if object.is_empty() || object.len() > MAX_RECIPE_UNIFORMS {
        return Err(format!("must set 1 to {MAX_RECIPE_UNIFORMS} uniforms"));
    }
    let mut normalized = Map::new();
    for (name, tagged) in object {
        if !is_identifier(name) || name.chars().count() > MAX_UNIFORM_NAME_CHARS {
            return Err(format!(
                "names {name:?}, which is not a shader uniform name of at most \
                 {MAX_UNIFORM_NAME_CHARS} characters"
            ));
        }
        normalized.insert(
            name.clone(),
            uniform_value(tagged).map_err(|reason| format!("`{name}` {reason}"))?,
        );
    }
    Ok(Value::Object(normalized))
}

fn uniform_value(tagged: &Value) -> Result<Value, String> {
    let object = object_with(tagged, &["type", "value"])?;
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or("needs a `type`")?;
    let value = object.get("value").ok_or("needs a `value`")?;
    let normalized = match kind {
        "bool" => value
            .as_bool()
            .map(Value::Bool)
            .ok_or("must be true or false")?,
        "integer" => json!(whole_number(value).ok_or("must be a whole number")?),
        "float" => json!(finite(value).ok_or("must be a number")?),
        "vector2" => vector(value, &["x", "y"], false)?,
        "vector3" => vector(value, &["x", "y", "z"], false)?,
        "vector4" => vector(value, &["x", "y", "z", "w"], false)?,
        "color" => color(value)?,
        "resource" => {
            let reference = object_with(value, &["resource", "resource_type"])?;
            let path = reference
                .get("resource")
                .and_then(Value::as_str)
                .ok_or("needs a `resource` path")?;
            validate_resource_path(path)?;
            let resource_type = reference
                .get("resource_type")
                .and_then(Value::as_str)
                .filter(|class| is_class_name(class))
                .ok_or("needs a `resource_type` class name")?;
            json!({ "resource": path, "resource_type": resource_type })
        }
        other => {
            return Err(format!(
                "has type {other:?}; a uniform takes bool, integer, float, vector2, vector3, \
                 vector4, color or resource"
            ));
        }
    };
    Ok(json!({ "type": kind, "value": normalized }))
}

fn validate_resource_path(path: &str) -> Result<(), String> {
    if path.chars().count() > MAX_RESOURCE_PATH_CHARS {
        return Err(format!(
            "must be at most {MAX_RESOURCE_PATH_CHARS} characters"
        ));
    }
    ProjectResource::parse(path)
        .map(|_| ())
        .map_err(|_| format!("{path:?} is not a normalized res:// project path"))
}

fn segment_character(character: char) -> bool {
    character != '/' && character != ':' && !character.is_control()
}

fn validate_parent(parent: &str) -> Result<(), String> {
    if parent == "." {
        return Ok(());
    }
    let characters = parent.chars().count();
    if characters == 0 || characters > MAX_PARENT_CHARS {
        return Err(format!(
            "must be `.` or a node path of 1 to {MAX_PARENT_CHARS} characters"
        ));
    }
    let well_formed = parent.split('/').all(|segment| {
        !segment.is_empty()
            && segment != "."
            && segment != ".."
            && segment.chars().all(segment_character)
    });
    if well_formed {
        Ok(())
    } else {
        Err(format!(
            "{parent:?} is not a node path below the scene root: `.` is the root, segments are \
             separated by `/`, and a node handle or a property path is not accepted"
        ))
    }
}

fn validate_name(name: &str) -> Result<(), String> {
    let characters = name.chars().count();
    if characters == 0 || characters > MAX_NAME_CHARS {
        return Err(format!("must be 1 to {MAX_NAME_CHARS} characters long"));
    }
    if let Some(invalid) = name.chars().find(|character| {
        matches!(character, '.' | ':' | '@' | '/' | '"' | '%') || character.is_control()
    }) {
        return Err(format!(
            "{name:?} holds {invalid:?}, which a node name cannot hold"
        ));
    }
    Ok(())
}

// --- tagged values and operations --------------------------------------------

fn float(value: f64) -> Value {
    json!({ "type": "float", "value": value })
}

fn integer(value: i64) -> Value {
    json!({ "type": "integer", "value": value })
}

fn boolean(value: bool) -> Value {
    json!({ "type": "bool", "value": value })
}

fn vector2(x: f64, y: f64) -> Value {
    json!({ "type": "vector2", "value": { "x": x, "y": y } })
}

fn vector3(x: f64, y: f64, z: f64) -> Value {
    json!({ "type": "vector3", "value": { "x": x, "y": y, "z": z } })
}

fn tagged(kind: &str, value: &Value) -> Value {
    json!({ "type": kind, "value": value })
}

fn color_value(red: f64, green: f64, blue: f64, alpha: f64) -> Value {
    json!({ "type": "color", "value": { "red": red, "green": green, "blue": blue, "alpha": alpha } })
}

fn text(value: &str) -> Value {
    json!({ "type": "string", "value": value })
}

fn string_name(value: &str) -> Value {
    json!({ "type": "string_name", "value": value })
}

fn resource(path: &str, resource_type: &str) -> Value {
    json!({ "type": "resource", "value": { "resource": path, "resource_type": resource_type } })
}

fn add_node(parent: &str, name: &str, class: &str) -> Value {
    json!({ "op": "add_node", "parent": parent, "name": name, "type": class })
}

fn set(node: &str, property: &str, value: Value) -> Value {
    json!({ "op": "set_property", "node": node, "property": property, "value": value })
}

fn subresource(node: &str, property: &str, class: &str, properties: Map<String, Value>) -> Value {
    let mut operation = json!({
        "op": "set_subresource", "node": node, "property": property, "class": class
    });
    if !properties.is_empty() {
        operation["properties"] = Value::Object(properties);
    }
    operation
}

fn call(node: &str, method: &str, arguments: Vec<Value>) -> Value {
    json!({ "op": "call_method", "node": node, "method": method, "arguments": arguments })
}

fn call_resource(node: &str, property: &str, method: &str, arguments: Vec<Value>) -> Value {
    json!({
        "op": "call_method", "node": node, "resource": { "property": property },
        "method": method, "arguments": arguments
    })
}

/// Properties for a sub-resource, from `(property, tagged value)` pairs.
fn properties(pairs: impl IntoIterator<Item = (&'static str, Value)>) -> Map<String, Value> {
    pairs
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value))
        .collect()
}

fn tagged_vector2(value: &Value) -> Value {
    tagged("vector2", value)
}

fn tagged_vector3(value: &Value) -> Value {
    tagged("vector3", value)
}

fn tagged_color(value: &Value) -> Value {
    tagged("color", value)
}

const fn number(minimum: f64, maximum: f64) -> ParameterKind {
    ParameterKind::Number {
        minimum,
        maximum,
        exclusive_minimum: false,
    }
}

const fn positive(maximum: f64) -> ParameterKind {
    ParameterKind::Number {
        minimum: 0.0,
        maximum,
        exclusive_minimum: true,
    }
}

const LAYERS: ParameterKind = ParameterKind::Integer {
    minimum: 0,
    maximum: MAX_LAYERS,
};

const NAVIGATION_LAYERS: ParameterKind = ParameterKind::Integer {
    minimum: 1,
    maximum: MAX_LAYERS,
};

fn no_check(_: &Values) -> Result<(), String> {
    Ok(())
}

// --- body-2d -----------------------------------------------------------------

const BODY_2D_PARAMETERS: &[Parameter] = &[
    Parameter::knob("name", ParameterKind::NodeName, "The body's name.").defaulting(
        Default::Derived("Player, RigidBody, StaticBody or Area, by `body`"),
    ),
    Parameter::knob(
        "body",
        ParameterKind::Choice(&["character", "rigid", "static", "area"]),
        "The body: CharacterBody2D, RigidBody2D, StaticBody2D or Area2D.",
    )
    .defaulting(Default::Choice("character")),
    Parameter::knob(
        "shape",
        ParameterKind::Choice(&["capsule", "circle", "rectangle"]),
        "The collision shape: CapsuleShape2D, CircleShape2D or RectangleShape2D.",
    )
    .defaulting(Default::Choice("capsule")),
    Parameter::knob(
        "radius",
        positive(100_000.0),
        "The shape's radius, in pixels.",
    )
    .only(Applies::When("shape", &["capsule", "circle"])),
    Parameter::knob(
        "height",
        positive(100_000.0),
        "The capsule's height, in pixels.",
    )
    .only(Applies::When("shape", &["capsule"])),
    Parameter::knob(
        "size",
        ParameterKind::Vector2 { positive: true },
        "The rectangle's size, in pixels.",
    )
    .only(Applies::When("shape", &["rectangle"])),
    Parameter::knob(
        "collision_layer",
        LAYERS,
        "The layers the body is on, as a bit field.",
    ),
    Parameter::knob(
        "collision_mask",
        LAYERS,
        "The layers the body collides with, as a bit field.",
    ),
    Parameter::knob(
        "mass",
        positive(100_000.0),
        "The body's mass, in kilograms.",
    )
    .only(Applies::When("body", &["rigid"])),
    Parameter::knob(
        "ground_ray",
        ParameterKind::Boolean,
        "Add a RayCast2D named GroundRay pointing down, for ground checks.",
    )
    .defaulting(Default::Boolean(false)),
    Parameter::knob(
        "ray_length",
        positive(100_000.0),
        "How far down the ray reaches, in pixels.",
    )
    .only(Applies::WhenTrue("ground_ray")),
    Parameter::knob(
        "ray_mask",
        LAYERS,
        "The layers the ray detects, as a bit field.",
    )
    .only(Applies::WhenTrue("ground_ray")),
];

fn body_name(values: &Values) -> &'static str {
    match values.choice("body") {
        "rigid" => "RigidBody",
        "static" => "StaticBody",
        "area" => "Area",
        _ => "Player",
    }
}

fn body_2d(values: &Values, at: &Placement) -> Vec<Value> {
    let class = match values.choice("body") {
        "rigid" => "RigidBody2D",
        "static" => "StaticBody2D",
        "area" => "Area2D",
        _ => "CharacterBody2D",
    };
    let body = at.primary();
    let mut operations = vec![add_node(at.parent(), at.primary_name(), class)];
    collision_layers(values, &body, &mut operations);
    if let Some(mass) = values.number("mass") {
        operations.push(set(&body, "mass", float(mass)));
    }
    let shape = join(&body, "Shape");
    operations.push(add_node(&body, "Shape", "CollisionShape2D"));
    let (class, shape_properties) = match values.choice("shape") {
        "circle" => (
            "CircleShape2D",
            properties(
                values
                    .number("radius")
                    .map(|radius| ("radius", float(radius))),
            ),
        ),
        "rectangle" => (
            "RectangleShape2D",
            properties(
                values
                    .get("size")
                    .map(|size| ("size", tagged_vector2(size))),
            ),
        ),
        _ => (
            "CapsuleShape2D",
            properties(
                [
                    values
                        .number("radius")
                        .map(|radius| ("radius", float(radius))),
                    values
                        .number("height")
                        .map(|height| ("height", float(height))),
                ]
                .into_iter()
                .flatten(),
            ),
        ),
    };
    operations.push(subresource(&shape, "shape", class, shape_properties));
    if values.flag("ground_ray") {
        let ray = join(&body, "GroundRay");
        operations.push(add_node(&body, "GroundRay", "RayCast2D"));
        if let Some(length) = values.number("ray_length") {
            operations.push(set(&ray, "target_position", vector2(0.0, length)));
        }
        if let Some(mask) = values.integer("ray_mask") {
            operations.push(set(&ray, "collision_mask", integer(mask)));
        }
    }
    operations
}

fn collision_layers(values: &Values, node: &str, operations: &mut Vec<Value>) {
    if let Some(layer) = values.integer("collision_layer") {
        operations.push(set(node, "collision_layer", integer(layer)));
    }
    if let Some(mask) = values.integer("collision_mask") {
        operations.push(set(node, "collision_mask", integer(mask)));
    }
}

// --- body-3d -----------------------------------------------------------------

const BODY_3D_PARAMETERS: &[Parameter] = &[
    Parameter::knob("name", ParameterKind::NodeName, "The body's name.").defaulting(
        Default::Derived("RigidBody, StaticBody, Player or Area, by `body`"),
    ),
    Parameter::knob(
        "body",
        ParameterKind::Choice(&["rigid", "static", "character", "area"]),
        "The body: RigidBody3D, StaticBody3D, CharacterBody3D or Area3D.",
    )
    .defaulting(Default::Choice("rigid")),
    Parameter::knob(
        "shape",
        ParameterKind::Choice(&["box", "sphere", "capsule", "cylinder"]),
        "The collision shape, and the mesh drawn with it: a box, a sphere, a capsule or a \
         cylinder.",
    )
    .defaulting(Default::Choice("box")),
    Parameter::knob(
        "size",
        ParameterKind::Vector3 { positive: true },
        "The box's size, in metres, for the shape and the mesh.",
    )
    .only(Applies::When("shape", &["box"])),
    Parameter::knob(
        "radius",
        positive(10_000.0),
        "The radius, in metres, for the shape and the mesh.",
    )
    .only(Applies::When("shape", &["sphere", "capsule", "cylinder"])),
    Parameter::knob(
        "height",
        positive(10_000.0),
        "The height, in metres, for the shape and the mesh.",
    )
    .only(Applies::When("shape", &["capsule", "cylinder"])),
    Parameter::knob(
        "mass",
        positive(100_000.0),
        "The body's mass, in kilograms.",
    )
    .only(Applies::When("body", &["rigid"])),
    Parameter::knob(
        "friction",
        number(0.0, 1.0),
        "The friction of the body's own PhysicsMaterial.",
    )
    .only(Applies::When("body", &["rigid", "static"])),
    Parameter::knob(
        "bounce",
        number(0.0, 1.0),
        "The bounce of the body's own PhysicsMaterial.",
    )
    .only(Applies::When("body", &["rigid", "static"])),
    Parameter::knob(
        "collision_layer",
        LAYERS,
        "The layers the body is on, as a bit field.",
    ),
    Parameter::knob(
        "collision_mask",
        LAYERS,
        "The layers the body collides with, as a bit field.",
    ),
    Parameter::knob(
        "mesh",
        ParameterKind::Boolean,
        "Add a MeshInstance3D named Mesh drawing the shape.",
    )
    .defaulting(Default::Boolean(true)),
    Parameter::knob(
        "color",
        ParameterKind::Color,
        "The albedo colour of the mesh's StandardMaterial3D.",
    )
    .only(Applies::WhenTrue("mesh")),
    Parameter::knob(
        "roughness",
        number(0.0, 1.0),
        "The roughness of the mesh's StandardMaterial3D.",
    )
    .only(Applies::WhenTrue("mesh")),
];

fn body_3d(values: &Values, at: &Placement) -> Vec<Value> {
    let class = match values.choice("body") {
        "static" => "StaticBody3D",
        "character" => "CharacterBody3D",
        "area" => "Area3D",
        _ => "RigidBody3D",
    };
    let body = at.primary();
    let mut operations = vec![add_node(at.parent(), at.primary_name(), class)];
    if let Some(mass) = values.number("mass") {
        operations.push(set(&body, "mass", float(mass)));
    }
    let material = properties(
        [
            values
                .number("friction")
                .map(|friction| ("friction", float(friction))),
            values
                .number("bounce")
                .map(|bounce| ("bounce", float(bounce))),
        ]
        .into_iter()
        .flatten(),
    );
    if !material.is_empty() {
        operations.push(subresource(
            &body,
            "physics_material_override",
            "PhysicsMaterial",
            material,
        ));
    }
    collision_layers(values, &body, &mut operations);
    let radius = values.number("radius");
    let height = values.number("height");
    let size = values.get("size").map(tagged_vector3);
    let (shape_class, shape_properties, mesh_class, mesh_properties) = match values.choice("shape")
    {
        "sphere" => (
            "SphereShape3D",
            properties(radius.map(|radius| ("radius", float(radius)))),
            "SphereMesh",
            properties(
                radius
                    .map(|radius| [("radius", float(radius)), ("height", float(radius * 2.0))])
                    .into_iter()
                    .flatten(),
            ),
        ),
        "capsule" => {
            let pairs = [
                radius.map(|radius| ("radius", float(radius))),
                height.map(|height| ("height", float(height))),
            ];
            (
                "CapsuleShape3D",
                properties(pairs.clone().into_iter().flatten()),
                "CapsuleMesh",
                properties(pairs.into_iter().flatten()),
            )
        }
        "cylinder" => (
            "CylinderShape3D",
            properties(
                [
                    radius.map(|radius| ("radius", float(radius))),
                    height.map(|height| ("height", float(height))),
                ]
                .into_iter()
                .flatten(),
            ),
            "CylinderMesh",
            properties(
                [
                    radius.map(|radius| ("top_radius", float(radius))),
                    radius.map(|radius| ("bottom_radius", float(radius))),
                    height.map(|height| ("height", float(height))),
                ]
                .into_iter()
                .flatten(),
            ),
        ),
        _ => (
            "BoxShape3D",
            properties(size.clone().map(|size| ("size", size))),
            "BoxMesh",
            properties(size.map(|size| ("size", size))),
        ),
    };
    let shape = join(&body, "Shape");
    operations.push(add_node(&body, "Shape", "CollisionShape3D"));
    operations.push(subresource(&shape, "shape", shape_class, shape_properties));
    if values.flag("mesh") {
        let mesh = join(&body, "Mesh");
        operations.push(add_node(&body, "Mesh", "MeshInstance3D"));
        operations.push(subresource(&mesh, "mesh", mesh_class, mesh_properties));
        let surface = properties(
            [
                values
                    .get("color")
                    .map(|color| ("albedo_color", tagged_color(color))),
                values
                    .number("roughness")
                    .map(|roughness| ("roughness", float(roughness))),
            ]
            .into_iter()
            .flatten(),
        );
        if !surface.is_empty() {
            operations.push(subresource(
                &mesh,
                "material_override",
                "StandardMaterial3D",
                surface,
            ));
        }
    }
    operations
}

// --- light-3d ----------------------------------------------------------------

const LIGHT_3D_PARAMETERS: &[Parameter] = &[
    Parameter::knob(
        "sun",
        ParameterKind::Boolean,
        "Add a DirectionalLight3D named Sun.",
    )
    .defaulting(Default::Boolean(true)),
    Parameter::knob(
        "sun_rotation",
        ParameterKind::Vector3 { positive: false },
        "The sun's rotation, in degrees.",
    )
    .only(Applies::WhenTrue("sun")),
    Parameter::knob("sun_energy", number(0.0, 1000.0), "The sun's light energy.")
        .only(Applies::WhenTrue("sun")),
    Parameter::knob(
        "shadows",
        ParameterKind::Boolean,
        "Whether the sun casts shadows.",
    )
    .only(Applies::WhenTrue("sun")),
    Parameter::knob(
        "lamp",
        ParameterKind::Boolean,
        "Add an OmniLight3D named Lamp.",
    )
    .defaulting(Default::Boolean(true)),
    Parameter::knob(
        "lamp_position",
        ParameterKind::Vector3 { positive: false },
        "The lamp's position, in metres.",
    )
    .only(Applies::WhenTrue("lamp")),
    Parameter::knob(
        "lamp_range",
        positive(10_000.0),
        "How far the lamp reaches, in metres.",
    )
    .only(Applies::WhenTrue("lamp")),
    Parameter::knob(
        "lamp_color",
        ParameterKind::Color,
        "The lamp's light colour.",
    )
    .only(Applies::WhenTrue("lamp")),
    Parameter::knob(
        "sky",
        ParameterKind::Boolean,
        "Add a WorldEnvironment named Environment whose Environment draws a procedural sky.",
    )
    .defaulting(Default::Boolean(true)),
    Parameter::knob(
        "sky_top_color",
        ParameterKind::Color,
        "The colour at the top of the procedural sky.",
    )
    .only(Applies::WhenTrue("sky")),
    Parameter::knob(
        "camera",
        ParameterKind::Boolean,
        "Add a Camera3D named Camera and make it the current camera.",
    )
    .defaulting(Default::Boolean(true)),
    Parameter::knob(
        "camera_position",
        ParameterKind::Vector3 { positive: false },
        "The camera's position, in metres.",
    )
    .only(Applies::WhenTrue("camera")),
    Parameter::knob(
        "fov",
        ParameterKind::Number {
            minimum: 1.0,
            maximum: 179.0,
            exclusive_minimum: false,
        },
        "The camera's vertical field of view, in degrees.",
    )
    .only(Applies::WhenTrue("camera")),
];

fn light_3d_check(values: &Values) -> Result<(), String> {
    if ["sun", "lamp", "sky", "camera"]
        .iter()
        .any(|part| values.flag(part))
    {
        Ok(())
    } else {
        Err("at least one of `sun`, `lamp`, `sky` and `camera` must be true".to_owned())
    }
}

fn light_3d(values: &Values, at: &Placement) -> Vec<Value> {
    let mut operations = Vec::new();
    if values.flag("sun") {
        let sun = at.below("Sun");
        operations.push(add_node(at.parent(), "Sun", "DirectionalLight3D"));
        if let Some(rotation) = values.get("sun_rotation") {
            operations.push(set(&sun, "rotation_degrees", tagged_vector3(rotation)));
        }
        if let Some(energy) = values.number("sun_energy") {
            operations.push(set(&sun, "light_energy", float(energy)));
        }
        if let Some(shadows) = values.get("shadows").and_then(Value::as_bool) {
            operations.push(set(&sun, "shadow_enabled", boolean(shadows)));
        }
    }
    if values.flag("lamp") {
        let lamp = at.below("Lamp");
        operations.push(add_node(at.parent(), "Lamp", "OmniLight3D"));
        if let Some(position) = values.get("lamp_position") {
            operations.push(set(&lamp, "position", tagged_vector3(position)));
        }
        if let Some(range) = values.number("lamp_range") {
            operations.push(set(&lamp, "omni_range", float(range)));
        }
        if let Some(color) = values.get("lamp_color") {
            operations.push(set(&lamp, "light_color", tagged_color(color)));
        }
    }
    if values.flag("sky") {
        let environment = at.below("Environment");
        operations.push(add_node(at.parent(), "Environment", "WorldEnvironment"));
        // `background_mode` 2 is `Environment.BG_SKY`.
        operations.push(subresource(
            &environment,
            "environment",
            "Environment",
            properties([("background_mode", integer(2))]),
        ));
        operations.push(subresource(
            &environment,
            "environment:sky",
            "Sky",
            Map::new(),
        ));
        operations.push(subresource(
            &environment,
            "environment:sky:sky_material",
            "ProceduralSkyMaterial",
            properties(
                values
                    .get("sky_top_color")
                    .map(|color| ("sky_top_color", tagged_color(color))),
            ),
        ));
    }
    if values.flag("camera") {
        let camera = at.below("Camera");
        operations.push(add_node(at.parent(), "Camera", "Camera3D"));
        if let Some(position) = values.get("camera_position") {
            operations.push(set(&camera, "position", tagged_vector3(position)));
        }
        if let Some(fov) = values.number("fov") {
            operations.push(set(&camera, "fov", float(fov)));
        }
        operations.push(set(&camera, "current", boolean(true)));
    }
    operations
}

// --- particles-2d ------------------------------------------------------------

/// The particle presets, in documentation order.
pub const PARTICLE_PRESETS: &[&str] = &["fire", "smoke", "sparks", "rain", "snow", "explosion"];

const PARTICLES_2D_PARAMETERS: &[Parameter] = &[
    Parameter::knob("name", ParameterKind::NodeName, "The emitter's name.").defaulting(
        Default::Derived("Fire, Smoke, Sparks, Rain, Snow or Explosion, by `preset`"),
    ),
    Parameter::knob(
        "preset",
        ParameterKind::Choice(PARTICLE_PRESETS),
        "The look: fire, smoke, sparks, rain, snow or a one-shot explosion.",
    )
    .defaulting(Default::Choice("fire")),
    Parameter::knob(
        "amount",
        ParameterKind::Integer {
            minimum: 1,
            maximum: 100_000,
        },
        "How many particles are alive at once.",
    )
    .defaulting(Default::Derived(
        "the preset's: fire 64, smoke 32, sparks 48, rain 200, snow 120, explosion 96",
    )),
    Parameter::knob(
        "lifetime",
        positive(600.0),
        "How long each particle lives, in seconds.",
    )
    .defaulting(Default::Derived(
        "the preset's: fire 0.8, smoke 2.5, sparks 0.6, rain 1.2, snow 6, explosion 0.9",
    )),
    Parameter::knob(
        "width",
        positive(100_000.0),
        "The width of the strip the drops fall from, in pixels, centred on the node.",
    )
    .defaulting(Default::Derived("640"))
    .only(Applies::When("preset", &["rain", "snow"])),
    Parameter::knob(
        "texture",
        ParameterKind::ResourcePath {
            resource_type: "Texture2D",
            extension: None,
        },
        "A Texture2D each particle draws; without one a particle is a small square.",
    ),
];

/// One particle preset: the emitter's settings and its process material.
struct Preset {
    amount: i64,
    lifetime: f64,
    one_shot: bool,
    /// The material's properties, without the colour ramp.
    material: fn(&Values) -> Map<String, Value>,
    /// A colour ramp as offsets and colours, or `None` for a flat colour.
    ramp: Option<(&'static [f64], &'static [[f64; 4]])>,
}

fn preset_name(values: &Values) -> &'static str {
    match values.choice("preset") {
        "smoke" => "Smoke",
        "sparks" => "Sparks",
        "rain" => "Rain",
        "snow" => "Snow",
        "explosion" => "Explosion",
        _ => "Fire",
    }
}

fn strip_extents(values: &Values) -> Value {
    let width = values.number("width").unwrap_or(640.0);
    vector3(width / 2.0, 1.0, 1.0)
}

fn fire_material(_: &Values) -> Map<String, Value> {
    properties([
        ("direction", vector3(0.0, -1.0, 0.0)),
        ("spread", float(25.0)),
        ("initial_velocity_min", float(80.0)),
        ("initial_velocity_max", float(140.0)),
        ("gravity", vector3(0.0, -40.0, 0.0)),
    ])
}

fn smoke_material(_: &Values) -> Map<String, Value> {
    properties([
        ("direction", vector3(0.0, -1.0, 0.0)),
        ("spread", float(15.0)),
        ("initial_velocity_min", float(20.0)),
        ("initial_velocity_max", float(40.0)),
        ("gravity", vector3(0.0, -15.0, 0.0)),
        ("scale_min", float(2.0)),
        ("scale_max", float(4.0)),
    ])
}

fn sparks_material(_: &Values) -> Map<String, Value> {
    properties([
        ("direction", vector3(0.0, -1.0, 0.0)),
        ("spread", float(60.0)),
        ("initial_velocity_min", float(150.0)),
        ("initial_velocity_max", float(300.0)),
        ("gravity", vector3(0.0, 400.0, 0.0)),
    ])
}

fn rain_material(values: &Values) -> Map<String, Value> {
    properties([
        // `emission_shape` 3 is `ParticleProcessMaterial.EMISSION_SHAPE_BOX`.
        ("emission_shape", integer(3)),
        ("emission_box_extents", strip_extents(values)),
        ("direction", vector3(0.0, 1.0, 0.0)),
        ("spread", float(2.0)),
        ("initial_velocity_min", float(400.0)),
        ("initial_velocity_max", float(500.0)),
        ("gravity", vector3(0.0, 800.0, 0.0)),
        ("scale_min", float(1.0)),
        ("scale_max", float(2.0)),
        ("color", color_value(0.6, 0.7, 1.0, 0.7)),
    ])
}

fn snow_material(values: &Values) -> Map<String, Value> {
    properties([
        ("emission_shape", integer(3)),
        ("emission_box_extents", strip_extents(values)),
        ("direction", vector3(0.0, 1.0, 0.0)),
        ("spread", float(20.0)),
        ("initial_velocity_min", float(20.0)),
        ("initial_velocity_max", float(40.0)),
        ("gravity", vector3(0.0, 10.0, 0.0)),
        ("scale_min", float(2.0)),
        ("scale_max", float(4.0)),
        ("color", color_value(1.0, 1.0, 1.0, 0.9)),
    ])
}

fn explosion_material(_: &Values) -> Map<String, Value> {
    properties([
        ("direction", vector3(0.0, -1.0, 0.0)),
        ("spread", float(180.0)),
        ("initial_velocity_min", float(200.0)),
        ("initial_velocity_max", float(350.0)),
        ("gravity", vector3(0.0, 98.0, 0.0)),
        ("damping_min", float(80.0)),
        ("damping_max", float(120.0)),
        ("scale_min", float(2.0)),
        ("scale_max", float(4.0)),
    ])
}

fn preset(name: &str) -> Preset {
    match name {
        "smoke" => Preset {
            amount: 32,
            lifetime: 2.5,
            one_shot: false,
            material: smoke_material,
            ramp: Some((&[0.0, 1.0], &[[0.6, 0.6, 0.6, 0.6], [0.3, 0.3, 0.3, 0.0]])),
        },
        "sparks" => Preset {
            amount: 48,
            lifetime: 0.6,
            one_shot: false,
            material: sparks_material,
            ramp: Some((
                &[0.0, 0.5, 1.0],
                &[
                    [1.0, 1.0, 0.8, 1.0],
                    [1.0, 0.55, 0.1, 1.0],
                    [1.0, 0.2, 0.0, 0.0],
                ],
            )),
        },
        "rain" => Preset {
            amount: 200,
            lifetime: 1.2,
            one_shot: false,
            material: rain_material,
            ramp: None,
        },
        "snow" => Preset {
            amount: 120,
            lifetime: 6.0,
            one_shot: false,
            material: snow_material,
            ramp: None,
        },
        "explosion" => Preset {
            amount: 96,
            lifetime: 0.9,
            one_shot: true,
            material: explosion_material,
            ramp: Some((
                &[0.0, 0.35, 1.0],
                &[
                    [1.0, 1.0, 0.75, 1.0],
                    [1.0, 0.45, 0.1, 0.9],
                    [0.25, 0.2, 0.2, 0.0],
                ],
            )),
        },
        _ => Preset {
            amount: 64,
            lifetime: 0.8,
            one_shot: false,
            material: fire_material,
            ramp: Some((
                &[0.0, 0.6, 1.0],
                &[
                    [1.0, 0.9, 0.3, 1.0],
                    [1.0, 0.3, 0.1, 0.8],
                    [0.2, 0.2, 0.2, 0.0],
                ],
            )),
        },
    }
}

fn particles_2d(values: &Values, at: &Placement) -> Vec<Value> {
    let chosen = preset(values.choice("preset"));
    let emitter = at.primary();
    let mut operations = vec![add_node(at.parent(), at.primary_name(), "GPUParticles2D")];
    operations.push(set(
        &emitter,
        "amount",
        integer(values.integer("amount").unwrap_or(chosen.amount)),
    ));
    operations.push(set(
        &emitter,
        "lifetime",
        float(values.number("lifetime").unwrap_or(chosen.lifetime)),
    ));
    if chosen.one_shot {
        operations.push(set(&emitter, "one_shot", boolean(true)));
        operations.push(set(&emitter, "explosiveness", float(1.0)));
    }
    if let Some(texture) = values.text("texture") {
        operations.push(set(&emitter, "texture", resource(texture, "Texture2D")));
    }
    operations.push(subresource(
        &emitter,
        "process_material",
        "ParticleProcessMaterial",
        (chosen.material)(values),
    ));
    if let Some((offsets, colors)) = chosen.ramp {
        operations.push(subresource(
            &emitter,
            "process_material:color_ramp",
            "GradientTexture1D",
            Map::new(),
        ));
        operations.push(subresource(
            &emitter,
            "process_material:color_ramp:gradient",
            "Gradient",
            properties([
                (
                    "offsets",
                    json!({ "type": "packed_float32_array", "value": offsets }),
                ),
                (
                    "colors",
                    json!({
                        "type": "packed_color_array",
                        "value": colors
                            .iter()
                            .map(|[red, green, blue, alpha]| json!({
                                "red": red, "green": green, "blue": blue, "alpha": alpha
                            }))
                            .collect::<Vec<_>>()
                    }),
                ),
            ]),
        ));
    }
    operations
}

// --- navigation-2d -----------------------------------------------------------

const NAVIGATION_2D_PARAMETERS: &[Parameter] = &[
    Parameter::knob(
        "size",
        ParameterKind::Vector2 { positive: true },
        "The floor's size, in pixels: the outline is a rectangle from the region's origin.",
    )
    .defaulting(Default::Vector2(640.0, 360.0)),
    Parameter::knob(
        "navigation_layers",
        NAVIGATION_LAYERS,
        "The navigation layers of the floor and the agent, as a bit field.",
    ),
    Parameter::knob(
        "agent_radius",
        number(0.0, 10_000.0),
        "The clearance the bake keeps from the outline's edges, in pixels.",
    ),
    Parameter::knob(
        "walker",
        ParameterKind::Boolean,
        "Add a CharacterBody2D named Walker with a NavigationAgent2D named Agent.",
    )
    .defaulting(Default::Boolean(true)),
    Parameter::knob(
        "walker_radius",
        positive(10_000.0),
        "The agent's radius, in pixels.",
    )
    .only(Applies::WhenTrue("walker")),
    Parameter::knob(
        "path_desired_distance",
        positive(10_000.0),
        "How close the agent gets to each path point before moving on, in pixels.",
    )
    .only(Applies::WhenTrue("walker")),
    Parameter::knob(
        "target_desired_distance",
        positive(10_000.0),
        "How close to the target the agent counts as arrived, in pixels.",
    )
    .only(Applies::WhenTrue("walker")),
    Parameter::knob(
        "avoidance",
        ParameterKind::Boolean,
        "Whether the agent avoids other agents.",
    )
    .only(Applies::WhenTrue("walker")),
];

fn navigation_2d(values: &Values, at: &Placement) -> Vec<Value> {
    let floor = at.below("Floor");
    let mut operations = vec![add_node(at.parent(), "Floor", "NavigationRegion2D")];
    let layers = values.integer("navigation_layers");
    if let Some(layers) = layers {
        operations.push(set(&floor, "navigation_layers", integer(layers)));
    }
    operations.push(subresource(
        &floor,
        "navigation_polygon",
        "NavigationPolygon",
        properties(
            values
                .number("agent_radius")
                .map(|radius| ("agent_radius", float(radius))),
        ),
    ));
    let size = values
        .get("size")
        .cloned()
        .unwrap_or_else(|| json!({ "x": 640.0, "y": 360.0 }));
    let width = size["x"].as_f64().unwrap_or(640.0);
    let height = size["y"].as_f64().unwrap_or(360.0);
    operations.push(call_resource(
        &floor,
        "navigation_polygon",
        "add_outline",
        vec![json!({
            "type": "packed_vector2_array",
            "value": [
                { "x": 0.0, "y": 0.0 },
                { "x": width, "y": 0.0 },
                { "x": width, "y": height },
                { "x": 0.0, "y": height }
            ]
        })],
    ));
    operations.push(call(
        &floor,
        "bake_navigation_polygon",
        vec![boolean(false)],
    ));
    if values.flag("walker") {
        let walker = at.below("Walker");
        let agent = join(&walker, "Agent");
        operations.push(add_node(at.parent(), "Walker", "CharacterBody2D"));
        operations.push(add_node(&walker, "Agent", "NavigationAgent2D"));
        if let Some(distance) = values.number("path_desired_distance") {
            operations.push(set(&agent, "path_desired_distance", float(distance)));
        }
        if let Some(distance) = values.number("target_desired_distance") {
            operations.push(set(&agent, "target_desired_distance", float(distance)));
        }
        if let Some(radius) = values.number("walker_radius") {
            operations.push(set(&agent, "radius", float(radius)));
        }
        if let Some(avoidance) = values.get("avoidance").and_then(Value::as_bool) {
            operations.push(set(&agent, "avoidance_enabled", boolean(avoidance)));
        }
        if let Some(layers) = layers {
            operations.push(set(&agent, "navigation_layers", integer(layers)));
        }
    }
    operations
}

// --- audio-2d ----------------------------------------------------------------

const AUDIO_2D_PARAMETERS: &[Parameter] = &[
    Parameter::knob("name", ParameterKind::NodeName, "The player's name.")
        .defaulting(Default::Text("Sound")),
    Parameter::knob(
        "stream",
        ParameterKind::ResourcePath {
            resource_type: "AudioStream",
            extension: None,
        },
        "The imported sound file the player plays, such as res://audio/step.wav.",
    )
    .required(),
    Parameter::knob("volume_db", number(-80.0, 24.0), "The volume, in decibels."),
    Parameter::knob(
        "max_distance",
        positive(1_000_000.0),
        "How far away the sound can be heard, in pixels.",
    ),
    Parameter::knob(
        "bus",
        ParameterKind::Text {
            min_chars: 1,
            max_chars: MAX_LABEL_CHARS,
        },
        "The audio bus; it must exist in the project's bus layout.",
    ),
    Parameter::knob(
        "autoplay",
        ParameterKind::Boolean,
        "Whether the sound starts when the scene does.",
    ),
];

fn audio_2d(values: &Values, at: &Placement) -> Vec<Value> {
    let player = at.primary();
    let mut operations = vec![add_node(
        at.parent(),
        at.primary_name(),
        "AudioStreamPlayer2D",
    )];
    if let Some(stream) = values.text("stream") {
        operations.push(set(&player, "stream", resource(stream, "AudioStream")));
    }
    if let Some(volume) = values.number("volume_db") {
        operations.push(set(&player, "volume_db", float(volume)));
    }
    if let Some(distance) = values.number("max_distance") {
        operations.push(set(&player, "max_distance", float(distance)));
    }
    if let Some(bus) = values.text("bus") {
        operations.push(set(&player, "bus", string_name(bus)));
    }
    if let Some(autoplay) = values.get("autoplay").and_then(Value::as_bool) {
        operations.push(set(&player, "autoplay", boolean(autoplay)));
    }
    operations
}

// --- shader-2d ---------------------------------------------------------------

const SHADER_2D_PARAMETERS: &[Parameter] = &[
    Parameter::knob("name", ParameterKind::NodeName, "The rectangle's name.")
        .defaulting(Default::Text("ShaderRect")),
    Parameter::knob(
        "shader",
        ParameterKind::ResourcePath {
            resource_type: "Shader",
            extension: Some(".gdshader"),
        },
        "The project's canvas_item shader the material draws with.",
    )
    .required(),
    Parameter::knob(
        "size",
        ParameterKind::Vector2 { positive: true },
        "The rectangle's size, in pixels.",
    ),
    Parameter::knob(
        "uniforms",
        ParameterKind::Uniforms,
        "Shader uniforms mapped to tagged values: bool, integer, float, vector2, vector3, \
         vector4, color, or resource for a texture.",
    ),
];

fn shader_2d(values: &Values, at: &Placement) -> Vec<Value> {
    let rectangle = at.primary();
    let mut operations = vec![add_node(at.parent(), at.primary_name(), "ColorRect")];
    if let Some(size) = values.get("size") {
        operations.push(set(&rectangle, "size", tagged_vector2(size)));
    }
    if let Some(shader) = values.text("shader") {
        operations.push(subresource(
            &rectangle,
            "material",
            "ShaderMaterial",
            properties([("shader", resource(shader, "Shader"))]),
        ));
    }
    if let Some(uniforms) = values.get("uniforms").and_then(Value::as_object) {
        for (name, value) in uniforms {
            operations.push(set(
                &rectangle,
                &format!("material:shader_parameter/{name}"),
                value.clone(),
            ));
        }
    }
    operations
}

// --- hud ---------------------------------------------------------------------

const HUD_PARAMETERS: &[Parameter] = &[
    Parameter::knob(
        "theme",
        ParameterKind::ResourcePath {
            resource_type: "Theme",
            extension: None,
        },
        "A Theme resource for the HUD and everything below it.",
    ),
    Parameter::knob(
        "score_text",
        ParameterKind::Text {
            min_chars: 0,
            max_chars: MAX_TEXT_CHARS,
        },
        "The score label's text.",
    )
    .defaulting(Default::Text("Score: 0")),
    Parameter::knob(
        "score_color",
        ParameterKind::Color,
        "The score label's font colour, overriding the theme for it alone.",
    ),
    Parameter::knob(
        "score_font_size",
        ParameterKind::Integer {
            minimum: 1,
            maximum: 1024,
        },
        "The score label's font size, overriding the theme for it alone.",
    ),
    Parameter::knob(
        "pause_text",
        ParameterKind::Text {
            min_chars: 0,
            max_chars: MAX_TEXT_CHARS,
        },
        "The pause button's text.",
    )
    .defaulting(Default::Text("Pause")),
];

/// Anchor a control the way the editor saves it: the layout mode, the preset
/// call that moves its anchors and offsets, and the recorded preset.
fn anchor(node: &str, layout_mode: i64, preset: i64, operations: &mut Vec<Value>) {
    operations.push(set(node, "layout_mode", integer(layout_mode)));
    operations.push(call(
        node,
        "set_anchors_and_offsets_preset",
        vec![integer(preset)],
    ));
    operations.push(set(node, "anchors_preset", integer(preset)));
}

fn hud(values: &Values, at: &Placement) -> Vec<Value> {
    let root = at.parent();
    let mut operations = Vec::new();
    if let Some(theme) = values.text("theme") {
        operations.push(set(root, "theme", resource(theme, "Theme")));
    }
    // Layout mode 3 is a scene root's (uncontrolled), 1 a child's (anchors);
    // preset 15 is full rect, 1 top right and 3 bottom right.
    anchor(root, if root == "." { 3 } else { 1 }, 15, &mut operations);
    let score = at.below("Score");
    operations.push(add_node(root, "Score", "Label"));
    operations.push(set(
        &score,
        "text",
        text(values.text("score_text").unwrap_or("Score: 0")),
    ));
    operations.push(set(&score, "horizontal_alignment", integer(2)));
    if let Some(color) = values.get("score_color") {
        operations.push(set(
            &score,
            "theme_override_colors/font_color",
            tagged_color(color),
        ));
    }
    if let Some(size) = values.integer("score_font_size") {
        operations.push(set(
            &score,
            "theme_override_font_sizes/font_size",
            integer(size),
        ));
    }
    anchor(&score, 1, 1, &mut operations);
    let pause = at.below("Pause");
    operations.push(add_node(root, "Pause", "Button"));
    operations.push(set(
        &pause,
        "text",
        text(values.text("pause_text").unwrap_or("Pause")),
    ));
    anchor(&pause, 1, 3, &mut operations);
    operations
}

// --- gridmap-3d --------------------------------------------------------------

const GRIDMAP_3D_PARAMETERS: &[Parameter] = &[
    Parameter::knob("name", ParameterKind::NodeName, "The grid map's name.")
        .defaulting(Default::Text("GridMap")),
    Parameter::knob(
        "cell_size",
        ParameterKind::Vector3 { positive: true },
        "The size of one cell, in metres.",
    ),
    Parameter::knob(
        "item_name",
        ParameterKind::Text {
            min_chars: 1,
            max_chars: MAX_LABEL_CHARS,
        },
        "The name of the library's one item, a box mesh.",
    )
    .defaulting(Default::Text("Block")),
    Parameter::knob(
        "cells",
        ParameterKind::Cells,
        "The cells painted with the item, as `{x, y, z}` grid coordinates. Omitted: none.",
    ),
];

fn gridmap_3d(values: &Values, at: &Placement) -> Vec<Value> {
    let grid = at.primary();
    let mut operations = vec![add_node(at.parent(), at.primary_name(), "GridMap")];
    if let Some(size) = values.get("cell_size") {
        operations.push(set(&grid, "cell_size", tagged_vector3(size)));
    }
    operations.push(subresource(
        &grid,
        "mesh_library",
        "MeshLibrary",
        Map::new(),
    ));
    operations.push(call_resource(
        &grid,
        "mesh_library",
        "create_item",
        vec![integer(0)],
    ));
    operations.push(call_resource(
        &grid,
        "mesh_library",
        "set_item_name",
        vec![
            integer(0),
            text(values.text("item_name").unwrap_or("Block")),
        ],
    ));
    operations.push(call_resource(
        &grid,
        "mesh_library",
        "set_item_mesh",
        vec![
            integer(0),
            json!({ "type": "new_resource", "value": { "class": "BoxMesh" } }),
        ],
    ));
    if let Some(cells) = values.get("cells").and_then(Value::as_array) {
        let repeat: Vec<Value> = cells
            .iter()
            .map(|cell| json!([tagged("vector3i", cell), integer(0)]))
            .collect();
        operations.push(json!({
            "op": "call_method", "node": grid, "method": "set_cell_item", "repeat": repeat
        }));
    }
    operations
}

// --- joystick ----------------------------------------------------------------

const JOYSTICK_PARAMETERS: &[Parameter] = &[
    Parameter::knob("name", ParameterKind::NodeName, "The joystick's name.")
        .defaulting(Default::Text("Joystick")),
    Parameter::knob(
        "mode",
        ParameterKind::Choice(&["fixed", "dynamic", "following"]),
        "Fixed in place, appearing where the touch starts (dynamic), or following the touch.",
    ),
    Parameter::knob(
        "deadzone",
        number(0.0, 1.0),
        "The part of the radius that counts as no input.",
    ),
    Parameter::knob(
        "actions",
        ParameterKind::Actions,
        "The input actions the joystick presses, as `left`, `right`, `up` and `down`.",
    ),
    Parameter::knob(
        "corner",
        ParameterKind::Choice(&["bottom_left", "bottom_right"]),
        "The screen corner the joystick is anchored to.",
    )
    .defaulting(Default::Choice("bottom_left")),
];

fn joystick(values: &Values, at: &Placement) -> Vec<Value> {
    let stick = at.primary();
    let mut operations = vec![add_node(at.parent(), at.primary_name(), "VirtualJoystick")];
    if let Some(mode) = values.text("mode") {
        // `VirtualJoystick.JoystickMode`: fixed 0, dynamic 1, following 2.
        let mode = match mode {
            "fixed" => 0,
            "following" => 2,
            _ => 1,
        };
        operations.push(set(&stick, "joystick_mode", integer(mode)));
    }
    if let Some(deadzone) = values.number("deadzone") {
        operations.push(set(&stick, "deadzone_ratio", float(deadzone)));
    }
    if let Some(actions) = values.get("actions").and_then(Value::as_object) {
        for direction in ["left", "right", "up", "down"] {
            if let Some(action) = actions.get(direction).and_then(Value::as_str) {
                operations.push(set(
                    &stick,
                    &format!("action_{direction}"),
                    string_name(action),
                ));
            }
        }
    }
    // Preset 2 is bottom left, 3 bottom right.
    let preset = if values.choice("corner") == "bottom_right" {
        3
    } else {
        2
    };
    anchor(&stick, 1, preset, &mut operations);
    operations
}

// --- theme -------------------------------------------------------------------

fn theme(_: &Values, _: &Placement) -> Vec<Value> {
    let label = || string_name("Label");
    vec![
        json!({ "op": "call_method", "method": "set_color", "arguments": [
            string_name("font_color"), label(), color_value(0.95, 0.95, 0.95, 1.0) ] }),
        json!({ "op": "call_method", "method": "set_font_size", "arguments": [
            string_name("font_size"), label(), integer(20) ] }),
        json!({ "op": "call_method", "method": "set_constant", "arguments": [
            string_name("outline_size"), label(), integer(2) ] }),
        json!({ "op": "call_method", "method": "set_stylebox", "arguments": [
            string_name("panel"), string_name("Panel"),
            { "type": "new_resource", "value": { "class": "StyleBoxFlat", "properties": {
                "bg_color": color_value(0.1, 0.12, 0.16, 0.9),
                "corner_radius_top_left": integer(8),
                "corner_radius_top_right": integer(8),
                "corner_radius_bottom_right": integer(8),
                "corner_radius_bottom_left": integer(8)
            } } } ] }),
    ]
}

fn no_derived(_: &Values) -> Vec<(&'static str, Value)> {
    Vec::new()
}

fn derive_body(values: &Values) -> Vec<(&'static str, Value)> {
    vec![("name", Value::String(body_name(values).to_owned()))]
}

fn derive_particles(values: &Values) -> Vec<(&'static str, Value)> {
    let chosen = preset(values.choice("preset"));
    let mut derived = vec![
        ("name", Value::String(preset_name(values).to_owned())),
        ("amount", json!(chosen.amount)),
        ("lifetime", json!(chosen.lifetime)),
    ];
    if matches!(values.choice("preset"), "rain" | "snow") {
        derived.push(("width", json!(640.0)));
    }
    derived
}

/// Every recipe, in documentation order.
pub const RECIPES: &[Recipe] = &[
    Recipe {
        id: "body-2d",
        title: "A 2D physics body with a collision shape",
        summary: "A CharacterBody2D, RigidBody2D, StaticBody2D or Area2D with a CollisionShape2D \
                  named Shape and, for ground checks, a RayCast2D named GroundRay.",
        target: RecipeTarget::Scene,
        parent_class: "Node2D",
        since: None,
        parameters: BODY_2D_PARAMETERS,
        derive: derive_body,
        check: no_check,
        expand: body_2d,
    },
    Recipe {
        id: "body-3d",
        title: "A 3D physics body with a shape and a mesh",
        summary: "A RigidBody3D, StaticBody3D, CharacterBody3D or Area3D with a CollisionShape3D \
                  named Shape and a MeshInstance3D named Mesh of the same shape and size.",
        target: RecipeTarget::Scene,
        parent_class: "Node3D",
        since: None,
        parameters: BODY_3D_PARAMETERS,
        derive: derive_body,
        check: no_check,
        expand: body_3d,
    },
    Recipe {
        id: "light-3d",
        title: "Lights, a sky and a camera",
        summary: "A DirectionalLight3D named Sun, an OmniLight3D named Lamp, a WorldEnvironment \
                  named Environment drawing a procedural sky, and a Camera3D named Camera made \
                  current; each part can be left out.",
        target: RecipeTarget::Scene,
        parent_class: "Node3D",
        since: None,
        parameters: LIGHT_3D_PARAMETERS,
        derive: no_derived,
        check: light_3d_check,
        expand: light_3d,
    },
    Recipe {
        id: "particles-2d",
        title: "2D particles from a preset",
        summary: "A GPUParticles2D with a ParticleProcessMaterial set up as fire, smoke, sparks, \
                  rain, snow or a one-shot explosion. The values are a starting point to tune, \
                  not a generated effect.",
        target: RecipeTarget::Scene,
        parent_class: "Node2D",
        since: None,
        parameters: PARTICLES_2D_PARAMETERS,
        derive: derive_particles,
        check: no_check,
        expand: particles_2d,
    },
    Recipe {
        id: "navigation-2d",
        title: "A baked 2D navigation floor and an agent",
        summary: "A NavigationRegion2D named Floor whose rectangular outline is baked before the \
                  batch goes on, and a CharacterBody2D named Walker with a NavigationAgent2D \
                  named Agent.",
        target: RecipeTarget::Scene,
        parent_class: "Node2D",
        since: None,
        parameters: NAVIGATION_2D_PARAMETERS,
        derive: no_derived,
        check: no_check,
        expand: navigation_2d,
    },
    Recipe {
        id: "audio-2d",
        title: "A positional sound",
        summary: "An AudioStreamPlayer2D playing a sound file of the project. The file must be \
                  imported first, and a named bus must exist in the project's bus layout.",
        target: RecipeTarget::Scene,
        parent_class: "Node2D",
        since: None,
        parameters: AUDIO_2D_PARAMETERS,
        derive: no_derived,
        check: no_check,
        expand: audio_2d,
    },
    Recipe {
        id: "shader-2d",
        title: "A rectangle drawn with a shader",
        summary: "A ColorRect drawn with a project shader through a ShaderMaterial, with \
                  uniforms set through property paths. The shader file must exist first.",
        target: RecipeTarget::Scene,
        parent_class: "Node2D",
        since: None,
        parameters: SHADER_2D_PARAMETERS,
        derive: no_derived,
        check: no_check,
        expand: shader_2d,
    },
    Recipe {
        id: "hud",
        title: "A HUD with a score and a pause button",
        summary: "Lays the parent Control out full screen, with a Label named Score in the top \
                  right corner and a Button named Pause in the bottom right one.",
        target: RecipeTarget::Scene,
        parent_class: "Control",
        since: None,
        parameters: HUD_PARAMETERS,
        derive: no_derived,
        check: no_check,
        expand: hud,
    },
    Recipe {
        id: "gridmap-3d",
        title: "A grid map with a box block",
        summary: "A GridMap whose embedded MeshLibrary holds one box item, with cells painted \
                  with it.",
        target: RecipeTarget::Scene,
        parent_class: "Node3D",
        since: None,
        parameters: GRIDMAP_3D_PARAMETERS,
        derive: no_derived,
        check: no_check,
        expand: gridmap_3d,
    },
    Recipe {
        id: "joystick",
        title: "An on-screen joystick",
        summary: "A VirtualJoystick anchored to a bottom corner that presses four input \
                  actions. The class exists from Godot 4.7.1; an older build refuses the batch.",
        target: RecipeTarget::Scene,
        parent_class: "Control",
        since: Some("4.7.1"),
        parameters: JOYSTICK_PARAMETERS,
        derive: no_derived,
        check: no_check,
        expand: joystick,
    },
    Recipe {
        id: "theme",
        title: "A theme",
        summary: "A Theme resource with a label colour and size, an outline constant and a flat \
                  panel style, for resource.create with class Theme.",
        target: RecipeTarget::Resource { class: "Theme" },
        parent_class: "",
        since: None,
        parameters: &[],
        derive: no_derived,
        check: no_check,
        expand: theme,
    },
];

/// The recipe named `id`.
#[must_use]
pub fn recipe(id: &str) -> Option<&'static Recipe> {
    RECIPES.iter().find(|recipe| recipe.id == id)
}

/// The recipes `scene.recipe.apply` applies, in documentation order.
pub fn scene_recipes() -> impl Iterator<Item = &'static Recipe> {
    RECIPES
        .iter()
        .filter(|recipe| recipe.target == RecipeTarget::Scene)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(value: Value) -> Map<String, Value> {
        value.as_object().cloned().expect("an object")
    }

    fn expand(id: &str, parameters: Value) -> Vec<Value> {
        recipe(id)
            .expect("a recipe")
            .expand(&object(parameters), None)
            .expect("the request is accepted")
            .operations
    }

    fn refusal(id: &str, parameters: Value) -> String {
        recipe(id)
            .expect("a recipe")
            .expand(&object(parameters), None)
            .expect_err("the request is refused")
            .message()
            .to_owned()
    }

    fn required_parameters(recipe: &Recipe) -> Map<String, Value> {
        recipe
            .parameters
            .iter()
            .filter(|parameter| parameter.required)
            .map(|parameter| {
                let value = match parameter.kind {
                    ParameterKind::ResourcePath { extension, .. } => {
                        json!(format!("res://example{}", extension.unwrap_or(".res")))
                    }
                    _ => unreachable!("only paths are required"),
                };
                (parameter.name.to_owned(), value)
            })
            .collect()
    }

    #[test]
    fn every_recipe_expands_with_its_defaults_into_operations() {
        for recipe in RECIPES {
            let expansion = recipe
                .expand(&required_parameters(recipe), None)
                .unwrap_or_else(|error| panic!("{}: {error}", recipe.id));
            assert!(!expansion.operations.is_empty(), "{}", recipe.id);
            for operation in &expansion.operations {
                assert!(operation["op"].is_string(), "{}: {operation}", recipe.id);
            }
            assert_eq!(
                expansion.placement.name().is_some(),
                recipe.takes_name(),
                "{}",
                recipe.id
            );
        }
    }

    #[test]
    fn a_body_takes_its_name_its_parent_and_its_shape() {
        let expansion = recipe("body-2d")
            .unwrap()
            .expand(
                &object(json!({
                    "name": "Ball", "body": "rigid", "shape": "circle", "radius": 9, "mass": 3
                })),
                Some("Level"),
            )
            .unwrap();
        assert_eq!(
            expansion.operations,
            vec![
                add_node("Level", "Ball", "RigidBody2D"),
                set("Level/Ball", "mass", float(3.0)),
                add_node("Level/Ball", "Shape", "CollisionShape2D"),
                subresource(
                    "Level/Ball/Shape",
                    "shape",
                    "CircleShape2D",
                    properties([("radius", float(9.0))])
                ),
            ]
        );
        assert_eq!(
            expansion.describe(),
            json!({
                "id": "body-2d",
                "parent": "Level",
                "parameters": {
                    "name": "Ball", "body": "rigid", "shape": "circle", "radius": 9.0,
                    "mass": 3.0, "ground_ray": false
                }
            })
        );
    }

    #[test]
    fn an_omitted_property_parameter_writes_nothing() {
        assert_eq!(
            expand("body-3d", json!({})),
            vec![
                add_node(".", "RigidBody", "RigidBody3D"),
                add_node("RigidBody", "Shape", "CollisionShape3D"),
                subresource("RigidBody/Shape", "shape", "BoxShape3D", Map::new()),
                add_node("RigidBody", "Mesh", "MeshInstance3D"),
                subresource("RigidBody/Mesh", "mesh", "BoxMesh", Map::new()),
            ]
        );
    }

    #[test]
    fn a_sphere_mesh_is_as_tall_as_it_is_wide() {
        let operations = expand("body-3d", json!({ "shape": "sphere", "radius": 0.75 }));
        assert_eq!(
            operations[4],
            subresource(
                "RigidBody/Mesh",
                "mesh",
                "SphereMesh",
                properties([("radius", float(0.75)), ("height", float(1.5))])
            )
        );
    }

    #[test]
    fn a_parameter_of_another_variant_is_refused_not_ignored() {
        assert!(
            refusal("body-2d", json!({ "shape": "circle", "height": 5 }))
                .contains("`height` applies only when `shape` is capsule"),
        );
        assert!(
            refusal("body-3d", json!({ "body": "area", "mass": 2 }))
                .contains("`mass` applies only when `body` is rigid"),
        );
        assert!(
            refusal("particles-2d", json!({ "width": 300 }))
                .contains("`width` applies only when `preset` is rain or snow"),
        );
        assert!(
            refusal("body-2d", json!({ "ray_length": 30 }))
                .contains("`ray_length` applies only when `ground_ray` is true"),
        );
    }

    #[test]
    fn values_out_of_shape_or_range_are_refused() {
        assert!(refusal("body-2d", json!({ "radius": 0 })).contains("must be above 0"));
        assert!(refusal("body-2d", json!({ "collision_layer": -1 })).contains("from 0 to"));
        assert!(refusal("body-2d", json!({ "collision_layer": 2.5 })).contains("whole number"));
        assert!(refusal("body-2d", json!({ "body": "kinematic" })).contains("must be one of"));
        assert!(refusal("body-2d", json!({ "colour": 1 })).contains("has no parameter `colour`"));
        assert!(
            refusal(
                "body-3d",
                json!({ "color": { "red": 1, "green": 1, "blue": 1 } })
            )
            .contains("needs a number `alpha`")
        );
        assert!(
            refusal(
                "gridmap-3d",
                json!({ "cells": [{ "x": 0, "y": 0, "z": 40000 }] })
            )
            .contains("from -32768 to 32767")
        );
        assert!(refusal("audio-2d", json!({})).contains("`stream` is required"));
        assert!(
            refusal("shader-2d", json!({ "shader": "res://a.txt" }))
                .contains("must end in .gdshader")
        );
        assert!(
            refusal("audio-2d", json!({ "stream": "res://a//b.wav" }))
                .contains("not a normalized res:// project path")
        );
        assert!(
            refusal(
                "light-3d",
                json!({ "sun": false, "lamp": false, "sky": false, "camera": false })
            )
            .contains("at least one of")
        );
        assert!(
            refusal(
                "shader-2d",
                json!({
                    "shader": "res://a.gdshader",
                    "uniforms": { "1x": { "type": "float", "value": 1 } }
                })
            )
            .contains("not a shader uniform name")
        );
    }

    #[test]
    fn names_and_parents_are_checked_and_fixed_name_recipes_take_none() {
        assert!(refusal("light-3d", json!({ "name": "Stage" })).contains("no parameter `name`"));
        assert!(refusal("body-2d", json!({ "name": "a/b" })).contains("cannot hold"));
        let body = recipe("body-2d").unwrap();
        for parent in ["nh1:abc", "A//B", "/A", "A/..", "A:b"] {
            assert!(
                body.expand(&Map::new(), Some(parent))
                    .unwrap_err()
                    .message()
                    .contains("not a node path"),
                "{parent}"
            );
        }
        assert!(body.expand(&Map::new(), Some("Level/..x")).is_ok());
    }

    #[test]
    fn every_particle_preset_names_its_emitter_and_sets_its_material() {
        for preset in PARTICLE_PRESETS {
            let operations = expand("particles-2d", json!({ "preset": preset }));
            let name = operations[0]["name"].as_str().unwrap().to_owned();
            assert_eq!(name.to_ascii_lowercase(), *preset);
            assert!(
                operations
                    .iter()
                    .any(|operation| operation["class"] == "ParticleProcessMaterial"),
                "{preset}"
            );
        }
        let explosion = expand(
            "particles-2d",
            json!({ "preset": "explosion", "amount": 10 }),
        );
        assert_eq!(explosion[1], set("Explosion", "amount", integer(10)));
        assert_eq!(explosion[3], set("Explosion", "one_shot", boolean(true)));
        let rain = expand("particles-2d", json!({ "preset": "rain", "width": 1000 }));
        assert_eq!(
            rain[3]["properties"]["emission_box_extents"],
            vector3(500.0, 1.0, 1.0)
        );
    }

    #[test]
    fn derived_defaults_are_filled_in_and_reported() {
        let rain = recipe("particles-2d")
            .unwrap()
            .expand(&object(json!({ "preset": "rain" })), None)
            .unwrap();
        assert_eq!(
            rain.describe()["parameters"],
            json!({ "preset": "rain", "name": "Rain", "amount": 200, "lifetime": 1.2, "width": 640.0 })
        );
        let fire = recipe("particles-2d")
            .unwrap()
            .expand(&object(json!({ "amount": 5 })), None)
            .unwrap();
        assert_eq!(
            fire.describe()["parameters"],
            json!({ "preset": "fire", "name": "Fire", "amount": 5, "lifetime": 0.8 })
        );
        let schema = recipe("particles-2d").unwrap().parameters_schema();
        assert!(schema["properties"]["amount"].get("default").is_none());
        assert!(
            schema["properties"]["amount"]["description"]
                .as_str()
                .unwrap()
                .contains("Default: the preset's: fire 64")
        );
        assert_eq!(
            recipe("audio-2d").unwrap().parameters_schema()["properties"]["name"]["default"],
            json!("Sound")
        );
    }

    #[test]
    fn a_hud_below_the_root_uses_anchors_and_at_the_root_is_uncontrolled() {
        let hud = recipe("hud").unwrap();
        let at_root = hud.expand(&Map::new(), None).unwrap().operations;
        assert_eq!(at_root[0], set(".", "layout_mode", integer(3)));
        let below = hud.expand(&Map::new(), Some("UI")).unwrap().operations;
        assert_eq!(below[0], set("UI", "layout_mode", integer(1)));
        assert_eq!(below[3], add_node("UI", "Score", "Label"));
    }

    #[test]
    fn every_parameter_schema_has_a_type_and_a_description() {
        for recipe in RECIPES {
            let schema = recipe.parameters_schema();
            assert_eq!(
                schema["additionalProperties"],
                json!(false),
                "{}",
                recipe.id
            );
            for parameter in recipe.parameters {
                let property = &schema["properties"][parameter.name];
                assert!(
                    property["type"].is_string(),
                    "{}.{}",
                    recipe.id,
                    parameter.name
                );
                assert!(
                    property["description"].is_string(),
                    "{}.{}",
                    recipe.id,
                    parameter.name
                );
            }
        }
        let keyed = recipe_schema();
        assert_eq!(
            keyed["properties"]
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            {
                let mut ids: Vec<&str> = scene_recipes().map(|recipe| recipe.id).collect();
                ids.sort_unstable();
                ids
            }
        );
        assert!(keyed["properties"].get("theme").is_none());
    }
}
