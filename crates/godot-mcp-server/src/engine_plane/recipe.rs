//! `scene.recipe.apply`: a named recipe, expanded by the domain registry into
//! the typed operations of one scene batch, then validated and applied exactly
//! as `scene.batch.apply` applies a batch — headless and journaled, or in the
//! connected editor as one undoable action when the scene is open there.
//! `resource.create` takes a resource recipe the same way in its `recipe`
//! field: the recipe names the resource's class and makes its operations.
//!
//! The request names its recipe as an object with one key, the recipe's name,
//! whose value is that recipe's parameters. The schemas and these parsers are
//! both generated from [`godot_mcp_domain::recipes`], so they agree on every
//! value; the registry lists the few rules across values only the parser
//! states. A `resource.create` request with a `recipe` takes neither `class`
//! nor `operations`: no tool schema combines shapes at its top level, so the
//! fields' descriptions state that rule and this parser enforces it.

use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::contracts::OutcomeError;
use godot_mcp_domain::recipes::{self, RECIPE_TOOL, RecipeTarget};
use serde_json::{Map, Value, json};

use super::authored::{self, AuthorKind, AuthorRequest, RecipeApplied};
use super::outcome_error;

/// Every top-level field `scene.recipe.apply` takes beside `idempotencyId`.
const ACCEPTED: &[&str] = &[
    "path",
    "expectedSha256",
    "recipe",
    "parent",
    "expectedRevision",
    "dryRun",
];

/// The fields copied unchanged into the scene batch the recipe becomes.
const BATCH_FIELDS: &[&str] = &["path", "expectedSha256", "expectedRevision", "dryRun"];

/// The tool that makes a resource, from a recipe or from its own operations.
const RESOURCE_TOOL: &str = "resource.create";

/// Every top-level field `resource.create` takes with a `recipe`, beside
/// `idempotencyId`.
const RESOURCE_ACCEPTED: &[&str] = &["path", "recipe", "expectedRevision", "dryRun"];

/// The fields copied unchanged into the resource request the recipe becomes.
const RESOURCE_FIELDS: &[&str] = &["path", "expectedRevision", "dryRun"];

fn invalid(message: impl Into<String>) -> OutcomeError {
    outcome_error(ErrorKind::InvalidInput, message)
}

/// The strict input schema of `scene.recipe.apply`.
pub(super) fn input_schema() -> Map<String, Value> {
    let batch = authored::input_schema(AuthorKind::SceneBatch);
    let batch_properties = batch["properties"]
        .as_object()
        .expect("a scene batch schema has properties");
    let mut properties = Map::new();
    for field in ["path", "expectedSha256"] {
        properties.insert(field.to_owned(), batch_properties[field].clone());
    }
    properties.insert("recipe".to_owned(), recipes::recipe_schema());
    properties.insert("parent".to_owned(), recipes::parent_schema());
    for field in ["expectedRevision", "dryRun"] {
        properties.insert(field.to_owned(), batch_properties[field].clone());
    }
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": properties,
        "required": ["path", "idempotencyId", "expectedSha256", "recipe"],
    })
    .as_object()
    .cloned()
    .expect("the recipe schema is a JSON object")
}

/// Validate a `scene.recipe.apply` request, expand its recipe and validate
/// the scene batch it becomes.
///
/// # Errors
/// Returns `invalid_input` for an unknown or null field, a missing field, a
/// recipe that is not one scene recipe, parameters the recipe refuses, and any
/// refusal the scene batch itself gives.
pub(super) fn parse(arguments: Map<String, Value>) -> Result<AuthorRequest, OutcomeError> {
    if let Some(key) = arguments
        .iter()
        .find_map(|(key, value)| value.is_null().then_some(key))
    {
        return Err(invalid(format!(
            "invalid engine request: `{key}` must not be null"
        )));
    }
    if let Some(unknown) = arguments
        .keys()
        .find(|key| !ACCEPTED.contains(&key.as_str()))
    {
        return Err(invalid(format!(
            "invalid engine request: unknown field `{unknown}`, expected one of {}",
            ACCEPTED.join(", ")
        )));
    }
    let (id, recipe, parameters) = named_recipe(&arguments, RECIPE_TOOL, Kind::Scene)?;
    if let RecipeTarget::Resource { .. } = recipe.target {
        return Err(invalid(format!(
            "{RECIPE_TOOL}: {id} makes a resource, not nodes of a scene; create it with \
             {RESOURCE_TOOL} and `recipe`: {{\"{id}\": {{...}}}}"
        )));
    }
    let parent = match arguments.get("parent") {
        None => None,
        Some(Value::String(parent)) => Some(parent.as_str()),
        Some(_) => {
            return Err(invalid(format!(
                "{RECIPE_TOOL}: `parent` is a node path relative to the scene root"
            )));
        }
    };
    let expansion = recipe
        .expand(parameters, parent)
        .map_err(|error| invalid(format!("{RECIPE_TOOL}: {error}")))?;
    let mut batch: Map<String, Value> = BATCH_FIELDS
        .iter()
        .filter_map(|field| {
            arguments
                .get(*field)
                .map(|value| ((*field).to_owned(), value.clone()))
        })
        .collect();
    let operations = Value::Array(expansion.operations.clone());
    batch.insert("operations".to_owned(), operations.clone());
    let mut request = authored::parse_as(AuthorKind::SceneBatch, batch, RECIPE_TOOL)?;
    request.recipe = Some(RecipeApplied {
        described: expansion.describe(),
        operations,
    });
    Ok(request)
}

/// Which recipes a tool applies.
#[derive(Clone, Copy)]
enum Kind {
    Scene,
    Resource,
}

impl Kind {
    fn names(self) -> String {
        let names: Vec<&str> = match self {
            Self::Scene => recipes::scene_recipes().map(|recipe| recipe.id).collect(),
            Self::Resource => recipes::resource_recipes()
                .map(|recipe| recipe.id)
                .collect(),
        };
        names.join(", ")
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Scene => "scene",
            Self::Resource => "resource",
        }
    }

    const fn example(self) -> &'static str {
        match self {
            Self::Scene => r#"{"particles-2d": {"preset": "smoke"}}"#,
            Self::Resource => r#"{"material-3d": {"roughness": 0.7}}"#,
        }
    }
}

/// The recipe a request's `recipe` field names and the parameters it gives.
fn named_recipe<'a>(
    arguments: &'a Map<String, Value>,
    tool: &str,
    kind: Kind,
) -> Result<(&'a str, &'static recipes::Recipe, &'a Map<String, Value>), OutcomeError> {
    let keyed = match arguments.get("recipe") {
        None => return Err(invalid(format!("{tool}: `recipe` is required"))),
        Some(Value::Object(keyed)) if keyed.len() == 1 => keyed,
        Some(_) => {
            return Err(invalid(format!(
                "{tool}: `recipe` is an object with one key, the recipe's name, whose value is \
                 its parameters, such as {}; the {} recipes are {}",
                kind.example(),
                kind.label(),
                kind.names()
            )));
        }
    };
    let (id, parameters) = keyed.iter().next().expect("one key");
    let recipe = recipes::recipe(id).ok_or_else(|| {
        invalid(format!(
            "{tool}: there is no recipe {id:?}; the {} recipes are {}",
            kind.label(),
            kind.names()
        ))
    })?;
    let parameters = parameters.as_object().ok_or_else(|| {
        invalid(format!(
            "{tool}: the value of `recipe.{id}` is an object of its parameters"
        ))
    })?;
    Ok((id.as_str(), recipe, parameters))
}

/// The strict input schema of `resource.create`: a resource of `class` made
/// by `operations`, or the resource a `recipe` makes.
pub(super) fn resource_create_schema() -> Map<String, Value> {
    let mut schema = authored::input_schema(AuthorKind::ResourceCreate);
    let properties = schema["properties"]
        .as_object_mut()
        .expect("a resource schema has properties");
    for field in ["class", "operations"] {
        let property = properties
            .get_mut(field)
            .unwrap_or_else(|| panic!("resource.create takes `{field}`"));
        let described = property["description"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        property["description"] = Value::String(format!("{described} Not with `recipe`."));
    }
    properties.insert("recipe".to_owned(), recipes::resource_recipe_schema());
    schema
}

/// Validate a `resource.create` request: a resource recipe, or a class and
/// its operations.
///
/// # Errors
/// The refusals of [`parse_resource`] for a request with a `recipe`, and of
/// the authoring parser for one without.
pub(super) fn parse_resource_create(
    arguments: Map<String, Value>,
) -> Result<AuthorRequest, OutcomeError> {
    if arguments.contains_key("recipe") {
        parse_resource(arguments)
    } else {
        authored::parse(AuthorKind::ResourceCreate, arguments)
    }
}

/// Validate a `resource.create` request with a `recipe`, expand the recipe
/// and validate the resource request it becomes.
///
/// # Errors
/// Returns `invalid_input` for an unknown or null field, `class` or
/// `operations` beside the recipe, a recipe that is not one resource recipe,
/// parameters the recipe refuses, and any refusal the resource request itself
/// gives.
fn parse_resource(arguments: Map<String, Value>) -> Result<AuthorRequest, OutcomeError> {
    if let Some(key) = arguments
        .iter()
        .find_map(|(key, value)| value.is_null().then_some(key))
    {
        return Err(invalid(format!(
            "invalid engine request: `{key}` must not be null"
        )));
    }
    if let Some(field) = ["class", "operations"]
        .into_iter()
        .find(|field| arguments.contains_key(*field))
    {
        return Err(invalid(format!(
            "{RESOURCE_TOOL}: `recipe` names the resource's class and makes its operations, so a \
             request with it takes no `{field}`; send the recipe alone, or `class` and \
             `operations` without it"
        )));
    }
    if let Some(unknown) = arguments
        .keys()
        .find(|key| !RESOURCE_ACCEPTED.contains(&key.as_str()))
    {
        return Err(invalid(format!(
            "invalid engine request: unknown field `{unknown}`, expected one of {}",
            RESOURCE_ACCEPTED.join(", ")
        )));
    }
    let (id, recipe, parameters) = named_recipe(&arguments, RESOURCE_TOOL, Kind::Resource)?;
    let RecipeTarget::Resource { class } = recipe.target else {
        return Err(invalid(format!(
            "{RESOURCE_TOOL}: {id} adds nodes to a scene, not a resource; apply it with \
             {RECIPE_TOOL}"
        )));
    };
    let expansion = recipe
        .expand(parameters, None)
        .map_err(|error| invalid(format!("{RESOURCE_TOOL}: {error}")))?;
    let mut create: Map<String, Value> = RESOURCE_FIELDS
        .iter()
        .filter_map(|field| {
            arguments
                .get(*field)
                .map(|value| ((*field).to_owned(), value.clone()))
        })
        .collect();
    let operations = Value::Array(expansion.operations.clone());
    create.insert("class".to_owned(), Value::String(class.to_owned()));
    create.insert("operations".to_owned(), operations.clone());
    let mut request = authored::parse_as(AuthorKind::ResourceCreate, create, RESOURCE_TOOL)?;
    request.recipe = Some(RecipeApplied {
        described: json!({ "id": expansion.id, "parameters": expansion.values.to_map() }),
        operations,
    });
    Ok(request)
}
