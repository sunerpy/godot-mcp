//! The recipe prompts: `godot-mcp.recipe.<id>` for every recipe of the
//! domain registry. A prompt is listed while the active profile authorizes
//! the tool its call template names, `scene.recipe.apply` for a scene recipe
//! and `resource.create` for the theme. Prompts take no tool slot, so an
//! agent can find every recipe without the tool list growing.
//!
//! A prompt's text is generated from the registry each time it is read: the
//! parameter table, a call template and the operations the recipe becomes, so
//! it cannot drift from what the tool does.

use godot_mcp_domain::contracts::OutcomeError;
use godot_mcp_domain::manifests::canonical_manifest;
use godot_mcp_domain::recipes::{self, Expansion, RECIPE_TOOL, Recipe, RecipeTarget};
use godot_mcp_domain::{CapabilityProfile, ErrorKind};
use serde_json::{Map, Value, json};

use crate::kernel::{KernelPrompt, KernelPromptArgument, KernelPromptResult};

/// Every recipe prompt's name starts with this.
pub(crate) const PREFIX: &str = "godot-mcp.recipe.";

const PATH_PLACEHOLDER_SCENE: &str = "<the scene's res path, .tscn>";
const PATH_PLACEHOLDER_THEME: &str = "<the theme's res path, .tres>";
const SHA_PLACEHOLDER: &str = "<the scene's current SHA-256: scene.tree.get or project.files.list>";
const ID_PLACEHOLDER: &str = "<a new unique id>";

fn invalid(message: impl Into<String>) -> OutcomeError {
    crate::core::outcome_error(ErrorKind::InvalidInput, message)
}

/// The tool a recipe's call template names.
fn tool(recipe: &Recipe) -> &'static str {
    match recipe.target {
        RecipeTarget::Scene => RECIPE_TOOL,
        RecipeTarget::Resource { .. } => "resource.create",
    }
}

/// The arguments a recipe's prompt takes, each an optional string.
fn arguments(recipe: &Recipe) -> Vec<KernelPromptArgument> {
    let argument = |name: &str, description: String| KernelPromptArgument {
        name: name.to_owned(),
        description,
        required: false,
    };
    let mut arguments = Vec::new();
    match recipe.target {
        RecipeTarget::Scene => {
            arguments.push(argument(
                "path",
                "The scene the recipe goes into, a res:// or project-relative .tscn path."
                    .to_owned(),
            ));
            arguments.push(argument(
                "parent",
                "The node the recipe's nodes go below, as a path relative to the scene root \
                 (default `.`, the root)."
                    .to_owned(),
            ));
            if let Some(name) = recipe.parameter("name") {
                let default = name.default.map_or_else(String::new, |default| {
                    format!(" (default {})", default.display())
                });
                arguments.push(argument(
                    "name",
                    format!("The name of the recipe's main node{default}."),
                ));
            }
            if let Some(preset) = recipe.parameter("preset") {
                let default = preset.default.map_or_else(String::new, |default| {
                    format!(" (default {})", default.display())
                });
                arguments.push(argument(
                    "preset",
                    format!(
                        "The preset to show: {}{default}.",
                        recipes::PARTICLE_PRESETS.join(", ")
                    ),
                ));
            }
        }
        RecipeTarget::Resource { .. } => arguments.push(argument(
            "path",
            "The new theme's res:// or project-relative .tres path.".to_owned(),
        )),
    }
    arguments
}

/// The recipe prompts the profile lists, in registry order.
pub(crate) fn list(profile: CapabilityProfile) -> Vec<KernelPrompt> {
    let tools = canonical_manifest(profile).tools;
    recipes::RECIPES
        .iter()
        .filter(|recipe| tools.contains(tool(recipe)))
        .map(|recipe| KernelPrompt {
            name: format!("{PREFIX}{}", recipe.id),
            description: format!(
                "{}: its parameters, a {} call template and the operations it becomes.",
                recipe.title,
                tool(recipe)
            ),
            arguments: arguments(recipe),
        })
        .collect()
}

/// Resolve a recipe prompt, or `None` when `name` is not one the profile
/// lists.
pub(crate) fn get(
    profile: CapabilityProfile,
    name: &str,
    given: Option<&Map<String, Value>>,
) -> Option<Result<KernelPromptResult, OutcomeError>> {
    let id = name.strip_prefix(PREFIX)?;
    let recipe = recipes::recipe(id)?;
    if !canonical_manifest(profile).tools.contains(tool(recipe)) {
        return None;
    }
    Some(render(recipe, given.unwrap_or(&Map::new())))
}

/// The arguments of one `prompts/get`, checked against the recipe's own. An
/// empty string counts as left out, as clients send for unfilled arguments.
fn read_arguments(
    recipe: &Recipe,
    given: &Map<String, Value>,
) -> Result<Map<String, Value>, OutcomeError> {
    let declared = arguments(recipe);
    let mut read = Map::new();
    for (key, value) in given {
        if !declared.iter().any(|argument| argument.name == *key) {
            let names: Vec<&str> = declared
                .iter()
                .map(|argument| argument.name.as_str())
                .collect();
            return Err(invalid(format!(
                "prompt {PREFIX}{} takes the arguments {}, not `{key}`",
                recipe.id,
                names.join(", ")
            )));
        }
        let Value::String(text) = value else {
            return Err(invalid(format!(
                "prompt {PREFIX}{}: `{key}` must be a string",
                recipe.id
            )));
        };
        if !text.is_empty() {
            read.insert(key.clone(), Value::String(text.clone()));
        }
    }
    Ok(read)
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).expect("JSON values serialize")
}

fn render(recipe: &Recipe, given: &Map<String, Value>) -> Result<KernelPromptResult, OutcomeError> {
    let arguments = read_arguments(recipe, given)?;
    let text_argument = |key: &str| arguments.get(key).and_then(Value::as_str);
    let mut parameters = Map::new();
    for key in ["name", "preset"] {
        if let Some(value) = text_argument(key) {
            parameters.insert(key.to_owned(), Value::String(value.to_owned()));
        }
    }
    // A required parameter has no default to show, so the template and the
    // operations carry a placeholder path the caller replaces.
    for parameter in recipe
        .parameters
        .iter()
        .filter(|parameter| parameter.required)
    {
        if let recipes::ParameterKind::ResourcePath {
            resource_type,
            extension,
        } = parameter.kind
        {
            parameters.insert(
                parameter.name.to_owned(),
                json!(format!(
                    "res://<the {resource_type}'s path>{}",
                    extension.unwrap_or_default()
                )),
            );
        }
    }
    let parent = text_argument("parent");
    let expansion = recipe
        .expand(&parameters, parent)
        .map_err(|error| invalid(format!("prompt {PREFIX}{}: {error}", recipe.id)))?;

    let mut text = format!(
        "# Recipe `{}`: {}\n\n{}\n\n",
        recipe.id, recipe.title, recipe.summary
    );
    if let Some(since) = recipe.since {
        text.push_str(&format!("It needs Godot {since} or later.\n\n"));
    }
    text.push_str("## Parameters\n\n");
    text.push_str(&recipe.parameter_table());
    text.push('\n');
    match recipe.target {
        RecipeTarget::Scene => {
            scene_sections(&mut text, recipe, &expansion, &parameters, &arguments)
        }
        RecipeTarget::Resource { class } => {
            resource_sections(&mut text, class, &expansion, text_argument("path"));
        }
    }
    Ok(KernelPromptResult {
        description: format!("Recipe {}: {}", recipe.id, recipe.title),
        text,
    })
}

fn scene_sections(
    text: &mut String,
    recipe: &Recipe,
    expansion: &Expansion,
    parameters: &Map<String, Value>,
    arguments: &Map<String, Value>,
) {
    let mut call = Map::new();
    call.insert(
        "path".to_owned(),
        arguments
            .get("path")
            .cloned()
            .unwrap_or_else(|| json!(PATH_PLACEHOLDER_SCENE)),
    );
    call.insert("expectedSha256".to_owned(), json!(SHA_PLACEHOLDER));
    call.insert("recipe".to_owned(), json!({ recipe.id: parameters }));
    if let Some(parent) = arguments.get("parent") {
        call.insert("parent".to_owned(), parent.clone());
    }
    call.insert("idempotencyId".to_owned(), json!(ID_PLACEHOLDER));
    text.push_str(&format!(
        "## Call\n\nA call template for `{RECIPE_TOOL}`, not a call to send as it is:\n\n\
         ```json\n{}\n```\n\n\
         Replace each `<...>` placeholder first: `path` is the scene the nodes go into, \
         `expectedSha256` is the SHA-256 that `scene.tree.get` or `project.files.list` reports \
         for the scene's current bytes, and `idempotencyId` is a new identifier of your own; \
         a retry with the same identifier and arguments answers from the journal. Any \
         parameter from the table goes into the `{}` object; one left out keeps its default. \
         The answer names the recipe with every parameter it used and the operations it \
         applied, and the editor's Undo reverts it as one action when the scene is open \
         there.\n\n",
        pretty(&Value::Object(call)),
        recipe.id
    ));
    text.push_str(&format!(
        "## Operations\n\nWith these parameters the recipe becomes these operations:\n\n\
         ```json\n{}\n```\n\n\
         The same operations work as they are in `scene.batch.apply` (a scene on disk), \
         `editor.scene.edit` (the scene the editor is editing), `scene.create` (a new scene) \
         and `project.batch.apply` (several files in one transaction).\n",
        pretty(&Value::Array(expansion.operations.clone()))
    ));
}

fn resource_sections(text: &mut String, class: &str, expansion: &Expansion, path: Option<&str>) {
    let call = json!({
        "path": path.unwrap_or(PATH_PLACEHOLDER_THEME),
        "class": class,
        "operations": expansion.operations,
        "idempotencyId": ID_PLACEHOLDER,
    });
    text.push_str(&format!(
        "## Call\n\nThis recipe makes a `{class}` resource, not nodes of a scene, so it goes to \
         `resource.create` rather than `{RECIPE_TOOL}`. A call template, not a call to send as \
         it is:\n\n```json\n{}\n```\n\n\
         Replace each `<...>` placeholder first: `path` is the new resource's file, which must \
         not exist yet, and `idempotencyId` is a new identifier of your own; a retry with the \
         same identifier and arguments answers from the journal.\n\n\
         The same `operations` go to `resource.batch.apply` to change an existing {class}, \
         with the SHA-256 of its current bytes as `expectedSha256`.\n",
        pretty(&call)
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_write_profiles_list_the_recipe_prompts() {
        for profile in [CapabilityProfile::Core, CapabilityProfile::Runtime] {
            assert!(list(profile).is_empty(), "{profile:?}");
            assert!(get(profile, "godot-mcp.recipe.body-2d", None).is_none());
        }
        for profile in [CapabilityProfile::Editor, CapabilityProfile::Full] {
            let names: Vec<String> = list(profile)
                .into_iter()
                .map(|prompt| prompt.name)
                .collect();
            let expected: Vec<String> = recipes::RECIPES
                .iter()
                .map(|recipe| format!("{PREFIX}{}", recipe.id))
                .collect();
            assert_eq!(names, expected, "{profile:?}");
            assert_eq!(names.len(), 11);
        }
        assert!(get(CapabilityProfile::Full, "godot-mcp.recipe.fog", None).is_none());
        assert!(get(CapabilityProfile::Full, "godot-mcp.profile-guide", None).is_none());
    }

    #[test]
    fn a_scene_recipe_prompt_holds_its_table_template_and_operations() {
        let given = Map::from_iter([
            ("path".to_owned(), json!("res://levels/rain.tscn")),
            ("parent".to_owned(), json!("World")),
            ("preset".to_owned(), json!("rain")),
            ("name".to_owned(), json!("")),
        ]);
        let result = get(
            CapabilityProfile::Editor,
            "godot-mcp.recipe.particles-2d",
            Some(&given),
        )
        .expect("listed")
        .expect("the arguments are accepted");
        let text = &result.text;
        assert!(text.starts_with("# Recipe `particles-2d`"), "{text}");
        assert!(text.contains("| `preset` |"), "{text}");
        assert!(
            text.contains("\"path\": \"res://levels/rain.tscn\""),
            "{text}"
        );
        assert!(text.contains(SHA_PLACEHOLDER), "{text}");
        assert!(text.contains(ID_PLACEHOLDER), "{text}");
        assert!(text.contains("\"parent\": \"World\""), "{text}");
        let expansion = recipes::recipe("particles-2d")
            .expect("recipe")
            .expand(
                json!({ "preset": "rain" }).as_object().expect("parameters"),
                Some("World"),
            )
            .expect("accepted");
        assert!(
            text.contains(&pretty(&Value::Array(expansion.operations))),
            "the prompt shows exactly the operations the recipe becomes: {text}"
        );
        for tool in [
            "scene.batch.apply",
            "editor.scene.edit",
            "scene.create",
            "project.batch.apply",
        ] {
            assert!(text.contains(tool), "{tool}: {text}");
        }
    }

    #[test]
    fn the_theme_prompt_is_a_resource_create_template() {
        let result = get(CapabilityProfile::Full, "godot-mcp.recipe.theme", None)
            .expect("listed")
            .expect("no arguments");
        let text = &result.text;
        assert!(text.contains("\"class\": \"Theme\""), "{text}");
        assert!(text.contains(PATH_PLACEHOLDER_THEME), "{text}");
        assert!(text.contains("resource.batch.apply"), "{text}");
        assert!(!text.contains("\"expectedSha256\""), "{text}");
        let refused = get(
            CapabilityProfile::Full,
            "godot-mcp.recipe.theme",
            Some(&Map::from_iter([("parent".to_owned(), json!("UI"))])),
        )
        .expect("listed")
        .expect_err("the theme has no parent");
        assert_eq!(refused.kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn bad_prompt_arguments_are_refused() {
        for (prompt, key, value) in [
            ("particles-2d", "preset", json!("hail")),
            ("body-2d", "parent", json!("/Level")),
            ("body-2d", "parent", json!("nh1:abc")),
            ("body-2d", "name", json!("a/b")),
            ("light-3d", "name", json!("Stage")),
            ("body-2d", "preset", json!("fire")),
            ("body-2d", "colour", json!("red")),
            ("body-2d", "path", json!(3)),
        ] {
            let given = Map::from_iter([(key.to_owned(), value.clone())]);
            let refused = get(
                CapabilityProfile::Full,
                &format!("{PREFIX}{prompt}"),
                Some(&given),
            )
            .expect("listed")
            .expect_err("refused");
            assert_eq!(
                refused.kind(),
                ErrorKind::InvalidInput,
                "{prompt} {key} {value}"
            );
        }
    }

    #[test]
    fn every_recipe_prompt_renders_with_its_defaults() {
        for recipe in recipes::RECIPES {
            let result = get(
                CapabilityProfile::Full,
                &format!("{PREFIX}{}", recipe.id),
                None,
            )
            .expect("listed")
            .unwrap_or_else(|error| panic!("{}: {error}", recipe.id));
            assert!(result.text.contains(recipe.title), "{}", recipe.id);
        }
    }
}
