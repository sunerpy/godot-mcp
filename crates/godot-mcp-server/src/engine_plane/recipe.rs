//! `scene.recipe.apply`: a named recipe, expanded by the domain registry into
//! the typed operations of one scene batch, then validated and applied exactly
//! as `scene.batch.apply` applies a batch — headless and journaled, or in the
//! connected editor as one undoable action when the scene is open there.
//!
//! The request names its recipe as an object with one key, the recipe's name,
//! whose value is that recipe's parameters. The schema and this parser are
//! both generated from [`godot_mcp_domain::recipes`], so they agree on every
//! request.

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
    let scene_names = || {
        recipes::scene_recipes()
            .map(|recipe| recipe.id)
            .collect::<Vec<_>>()
            .join(", ")
    };
    let keyed = match arguments.get("recipe") {
        None => return Err(invalid(format!("{RECIPE_TOOL}: `recipe` is required"))),
        Some(Value::Object(keyed)) if keyed.len() == 1 => keyed,
        Some(_) => {
            return Err(invalid(format!(
                "{RECIPE_TOOL}: `recipe` is an object with one key, the recipe's name, whose \
                 value is its parameters, such as {{\"particles-2d\": {{\"preset\": \"smoke\"}}}}; \
                 the scene recipes are {}",
                scene_names()
            )));
        }
    };
    let (id, parameters) = keyed.iter().next().expect("one key");
    let recipe = recipes::recipe(id).ok_or_else(|| {
        invalid(format!(
            "{RECIPE_TOOL}: there is no recipe {id:?}; the scene recipes are {}",
            scene_names()
        ))
    })?;
    if let RecipeTarget::Resource { class } = recipe.target {
        return Err(invalid(format!(
            "{RECIPE_TOOL}: {id} makes a {class} resource, not nodes of a scene; create it with \
             resource.create (class {class}) and the operations its prompt \
             godot-mcp.recipe.{id} lists"
        )));
    }
    let parameters = parameters.as_object().ok_or_else(|| {
        invalid(format!(
            "{RECIPE_TOOL}: the value of `recipe.{id}` is an object of its parameters"
        ))
    })?;
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
