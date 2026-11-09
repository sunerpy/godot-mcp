//! The requests the operator pages show are requests the server takes: every
//! call and operation block of `docs/operator/recipes.md` and every example of
//! `docs/operator/editor-bridge.md` conforms to the schema its tool
//! advertises and passes the tool's own check. A client that checks its
//! arguments against `tools/list` can send each one as it is written.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{Map, Value, json};

use super::authored::{self, AuthorKind};
use super::read::ReadKind;
use crate::schema::conformance::conforms;

/// A SHA-256 standing in for the placeholder a call block shows.
const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn page(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/operator")
        .join(name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

/// One fenced `json` block of a page: the 1-based line its fence opens on,
/// the HTML comment on the line before it, if any, and its JSON.
struct Block {
    line: usize,
    annotation: Option<String>,
    json: Value,
}

fn blocks(text: &str) -> Vec<Block> {
    let lines: Vec<&str> = text.lines().collect();
    let mut found = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if line.trim() != "```json" {
            continue;
        }
        let body: Vec<&str> = lines[index + 1..]
            .iter()
            .take_while(|line| line.trim() != "```")
            .copied()
            .collect();
        let json = serde_json::from_str(&body.join("\n"))
            .unwrap_or_else(|error| panic!("the block on line {} is JSON: {error}", index + 1));
        let annotation = index
            .checked_sub(1)
            .map(|before| lines[before].trim())
            .and_then(|before| before.strip_prefix("<!--"))
            .and_then(|before| before.strip_suffix("-->"))
            .map(|inner| inner.trim().to_owned());
        found.push(Block {
            line: index + 1,
            annotation,
            json,
        });
    }
    found
}

/// The kind and the `key=value` attributes of an annotation such as
/// `recipe-call id=body-2d path=scenes/player.tscn root=Node2D`.
fn attributes(annotation: &str) -> (&str, BTreeMap<&str, &str>) {
    let mut words = annotation.split_whitespace();
    let kind = words.next().expect("an annotation names its kind");
    (
        kind,
        words.filter_map(|pair| pair.split_once('=')).collect(),
    )
}

fn advertised(tool: &str) -> Value {
    Value::Object(crate::kernel::tool_schema(
        tool,
        crate::core::CoreLimits::default(),
    ))
}

/// Hold `tool`'s advertised schema and its own `check` to taking
/// `arguments`, the request the block `at` shows.
fn taken(tool: &str, arguments: &Value, check: impl FnOnce(Map<String, Value>) -> bool, at: &str) {
    assert!(
        conforms(&advertised(tool), arguments),
        "{at}: the {tool} schema refuses {arguments}"
    );
    let mut handed = arguments.as_object().cloned().expect("an object");
    // The kernel takes the idempotency id out before a handler runs.
    handed.remove("idempotencyId");
    assert!(check(handed), "{at}: {tool} refuses {arguments}");
}

/// The operations of a scene batch as every tool that takes them takes them:
/// in a batch, in a new scene with the recipe's root, in several files at
/// once and in the editor's copy of the scene.
fn scene_operations_taken(operations: &Value, path: &str, root: Option<&str>, at: &str) {
    let batch = json!({ "path": path, "expectedSha256": SHA, "operations": operations,
                        "idempotencyId": "docs-batch" });
    taken(
        "scene.batch.apply",
        &batch,
        |arguments| authored::parse(AuthorKind::SceneBatch, arguments).is_ok(),
        at,
    );
    if let Some(root) = root {
        let create = json!({ "path": path, "rootType": root, "operations": operations,
                             "idempotencyId": "docs-create" });
        taken(
            "scene.create",
            &create,
            |arguments| authored::parse(AuthorKind::SceneCreate, arguments).is_ok(),
            at,
        );
    }
    let files = json!({ "files": [{ "kind": "scene", "path": path, "expectedSha256": SHA,
                                    "operations": operations }],
                        "idempotencyId": "docs-files" });
    taken(
        "project.batch.apply",
        &files,
        |arguments| super::batches::parse(arguments).is_ok(),
        at,
    );
    let edit = json!({ "expectedDigest": SHA, "operations": operations,
                       "idempotencyId": "docs-edit" });
    taken(
        "editor.scene.edit",
        &edit,
        |_| {
            operations
                .as_array()
                .expect("a list of operations")
                .iter()
                .enumerate()
                .all(|(index, operation)| {
                    authored::validate_live_operation(operation, index).is_ok()
                })
        },
        at,
    );
}

#[test]
fn every_request_the_recipes_page_shows_is_one_its_tools_take() {
    let text = page("recipes.md");
    // The scene each scene recipe's call names, with its root's class; the
    // resource each resource recipe makes, with its class.
    let mut scenes: BTreeMap<String, (String, String)> = BTreeMap::new();
    let mut resources: BTreeMap<String, (String, String)> = BTreeMap::new();
    let mut checked = 0;
    for block in blocks(&text) {
        let at = format!("recipes.md:{}", block.line);
        let annotation = block
            .annotation
            .as_deref()
            .unwrap_or_else(|| panic!("{at}: every block names what it is"));
        let (kind, attributes) = attributes(annotation);
        let id = attributes["id"].to_owned();
        match kind {
            "recipe-call" if attributes.contains_key("class") => {
                let mut call = block.json.clone();
                call["idempotencyId"] = json!("docs-recipe");
                taken(
                    "resource.create",
                    &call,
                    |arguments| super::recipe::parse_resource_create(arguments).is_ok(),
                    &at,
                );
                resources.insert(
                    id,
                    (
                        attributes["path"].to_owned(),
                        attributes["class"].to_owned(),
                    ),
                );
            }
            "recipe-call" => {
                let mut call = block.json.clone();
                call["expectedSha256"] = json!(SHA);
                call["idempotencyId"] = json!("docs-recipe");
                taken(
                    "scene.recipe.apply",
                    &call,
                    |arguments| super::recipe::parse(arguments).is_ok(),
                    &at,
                );
                scenes.insert(
                    id,
                    (attributes["path"].to_owned(), attributes["root"].to_owned()),
                );
            }
            "recipe" if resources.contains_key(&id) => {
                // A resource recipe's operations also make the resource
                // without the recipe, as `class` and `operations`.
                let (path, class) = &resources[&id];
                let create = json!({ "path": path, "class": class, "operations": block.json,
                                     "idempotencyId": "docs-resource" });
                taken(
                    "resource.create",
                    &create,
                    |arguments| super::recipe::parse_resource_create(arguments).is_ok(),
                    &at,
                );
            }
            "recipe" => {
                let (path, root) = scenes
                    .get(&id)
                    .unwrap_or_else(|| panic!("{at}: {id} shows its call first"));
                scene_operations_taken(&block.json, path, Some(root), &at);
            }
            "recipe-tune" => {
                if let Some((path, _)) = resources.get(&id) {
                    let batch = json!({ "path": path, "expectedSha256": SHA,
                                        "operations": block.json,
                                        "idempotencyId": "docs-tune" });
                    taken(
                        "resource.batch.apply",
                        &batch,
                        |arguments| authored::parse(AuthorKind::ResourceBatch, arguments).is_ok(),
                        &at,
                    );
                } else {
                    let (path, _) = scenes
                        .get(&id)
                        .unwrap_or_else(|| panic!("{at}: {id} shows its call first"));
                    scene_operations_taken(&block.json, path, None, &at);
                }
            }
            other => panic!("{at}: an unknown annotation `{other}`"),
        }
        checked += 1;
    }
    assert!(checked > 0, "the recipes page shows requests");
}

#[test]
fn every_example_the_editor_bridge_page_shows_is_one_its_tool_takes() {
    let text = page("editor-bridge.md");
    let examples = blocks(&text);
    assert!(
        !examples.is_empty(),
        "the editor bridge page shows examples"
    );
    for block in examples {
        let at = format!("editor-bridge.md:{}", block.line);
        let example = &block.json;
        if example.get("op").is_some() {
            scene_operations_taken(&json!([example]), "levels/one.tscn", None, &at);
        } else if example.get("path").is_some() {
            taken(
                "scene.read",
                example,
                |arguments| super::read::parse(ReadKind::Scene, arguments).is_ok(),
                &at,
            );
        } else if example.get("calls").is_some() {
            taken(
                "editor.scene.read",
                example,
                |arguments| crate::editor::scene_read::request(&arguments, None).is_ok(),
                &at,
            );
        } else {
            panic!("{at}: name the tool this example is for: {example}");
        }
    }
}
