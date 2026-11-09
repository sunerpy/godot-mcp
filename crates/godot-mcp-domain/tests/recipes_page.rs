//! The recipes page documents the recipe registry: every `scene.recipe.apply`
//! or `resource.create` call block on `docs/operator/recipes.md` expands,
//! through the registry, to exactly the operations block printed below it, and
//! every parameter table is the registry's own. A recipe whose defaults alone
//! make no request shows the registry's example in its call. The page cannot
//! show a call the tool answers differently, or a parameter it does not take.

use std::collections::BTreeMap;
use std::path::Path;

use godot_mcp_domain::recipes::{self, RecipeTarget};
use serde_json::Value;

fn page() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/operator/recipes.md");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

/// The `key=value` attributes of an annotation such as
/// `<!-- recipe-call id=body-2d path=scenes/player.tscn root=Node2D -->`.
fn attributes(line: &str, kind: &str) -> Option<BTreeMap<String, String>> {
    let inner = line
        .trim()
        .strip_prefix("<!--")?
        .strip_suffix("-->")?
        .trim();
    let rest = inner.strip_prefix(kind)?.strip_prefix(' ')?;
    Some(
        rest.split_whitespace()
            .filter_map(|pair| pair.split_once('='))
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect(),
    )
}

/// The JSON of the fenced block that follows line `index`.
fn block_after(lines: &[&str], index: usize) -> Value {
    assert_eq!(
        lines.get(index + 1).map(|line| line.trim()),
        Some("```json"),
        "the annotation on line {} is followed by a json block",
        index + 1
    );
    let body: Vec<&str> = lines[index + 2..]
        .iter()
        .take_while(|line| line.trim() != "```")
        .copied()
        .collect();
    serde_json::from_str(&body.join("\n"))
        .unwrap_or_else(|error| panic!("the block after line {} is JSON: {error}", index + 1))
}

/// The table lines that follow line `index`, up to the first line that is not
/// a table row.
fn table_after(lines: &[&str], index: usize) -> String {
    lines[index + 1..]
        .iter()
        .take_while(|line| line.starts_with('|'))
        .map(|line| format!("{line}\n"))
        .collect()
}

/// Numbers compared as numbers: the registry writes `12.0` where a page may
/// write `12`.
fn normalized(value: &Value) -> Value {
    match value {
        Value::Number(number) => number
            .as_f64()
            .and_then(serde_json::Number::from_f64)
            .map_or_else(|| value.clone(), Value::Number),
        Value::Array(items) => Value::Array(items.iter().map(normalized).collect()),
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, field)| (key.clone(), normalized(field)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// One annotated recipe of the page.
#[derive(Debug, Default)]
struct Documented {
    call: Option<(BTreeMap<String, String>, Value)>,
    operations: Option<(BTreeMap<String, String>, Value)>,
    table: Option<String>,
    tune: bool,
}

/// Every annotated recipe, in page order.
fn documented() -> Vec<(String, Documented)> {
    let text = page();
    let lines: Vec<&str> = text.lines().collect();
    let mut found: Vec<(String, Documented)> = Vec::new();
    fn entry(found: &mut Vec<(String, Documented)>, id: &str) -> usize {
        if let Some(position) = found.iter().position(|(known, _)| known == id) {
            position
        } else {
            found.push((id.to_owned(), Documented::default()));
            found.len() - 1
        }
    }
    for (index, line) in lines.iter().enumerate() {
        if let Some(attributes) = attributes(line, "recipe-call") {
            let at = entry(&mut found, &attributes["id"]);
            assert!(
                found[at].1.call.is_none(),
                "one call block per recipe: line {}",
                index + 1
            );
            found[at].1.call = Some((attributes, block_after(&lines, index)));
        } else if let Some(attributes) = attributes(line, "recipe") {
            let at = entry(&mut found, &attributes["id"]);
            assert!(
                found[at].1.operations.is_none(),
                "one operations block per recipe: line {}",
                index + 1
            );
            found[at].1.operations = Some((attributes, block_after(&lines, index)));
        } else if let Some(attributes) = attributes(line, "recipe-parameters") {
            let at = entry(&mut found, &attributes["id"]);
            found[at].1.table = Some(table_after(&lines, index));
        } else if let Some(attributes) = attributes(line, "recipe-tune") {
            let at = entry(&mut found, &attributes["id"]);
            found[at].1.tune = true;
        }
    }
    found
}

#[test]
fn the_page_documents_every_recipe_of_the_registry_in_its_order() {
    let documented: Vec<String> = documented().into_iter().map(|(id, _)| id).collect();
    let registry: Vec<&str> = recipes::RECIPES.iter().map(|recipe| recipe.id).collect();
    assert_eq!(documented, registry);
}

#[test]
fn every_call_block_becomes_the_operations_block_below_it() {
    for (id, recipe) in documented() {
        let registered = recipes::recipe(&id).expect("a registered recipe");
        let (operations_attributes, operations) = recipe
            .operations
            .unwrap_or_else(|| panic!("{id} shows the operations it becomes"));
        assert!(recipe.tune, "{id} has a tuning batch");
        let (attributes, call) = recipe
            .call
            .unwrap_or_else(|| panic!("{id} shows the call that applies it"));
        assert_eq!(attributes["id"], id);
        assert_eq!(
            call["path"],
            attributes["path"].as_str(),
            "{id}: the call's path"
        );
        assert!(
            call["idempotencyId"].as_str().is_some(),
            "{id}: the call is complete"
        );
        let Some(Value::Object(keyed)) = call.get("recipe") else {
            panic!("{id}: the call names its recipe");
        };
        assert_eq!(keyed.len(), 1, "{id}: one recipe per call");
        let parameters = keyed
            .get(&id)
            .and_then(Value::as_object)
            .unwrap_or_else(|| panic!("{id}: the call's recipe is {id}"));
        let example = registered.example();
        if !example.is_empty() {
            assert_eq!(
                normalized(&Value::Object(parameters.clone())),
                normalized(&Value::Object(example)),
                "{id}: the call's parameters are the registry's example"
            );
        }
        let parent = call.get("parent").and_then(Value::as_str);
        let expansion = registered
            .expand(parameters, parent)
            .unwrap_or_else(|error| panic!("{id}: {}", error.message()));
        let expected = Value::Array(expansion.operations);
        assert_eq!(
            normalized(&operations),
            normalized(&expected),
            "{id}: the page's operations are what the call becomes; expected:\n{}",
            serde_json::to_string(&expected).expect("JSON")
        );
        assert!(
            operations_attributes.keys().all(|key| key == "id"),
            "{id}: the call carries the path and the class or root"
        );
        match registered.target {
            RecipeTarget::Scene => {
                for key in ["path", "root"] {
                    assert!(
                        attributes.contains_key(key),
                        "{id}: the call names its {key}"
                    );
                }
                assert_eq!(
                    attributes.get("since").map(String::as_str),
                    registered.since,
                    "{id}: the oldest build"
                );
                assert_eq!(
                    attributes["root"], registered.parent_class,
                    "{id}: the root class"
                );
                assert!(
                    call["expectedSha256"].as_str().is_some(),
                    "{id}: the call is complete"
                );
            }
            RecipeTarget::Resource { class } => {
                assert_eq!(
                    attributes.get("class").map(String::as_str),
                    Some(class),
                    "{id}: the call names the class the recipe makes"
                );
                assert!(
                    call.get("class").is_none() && call.get("operations").is_none(),
                    "{id}: a recipe call takes neither `class` nor `operations`"
                );
            }
        }
        if registered.parameters.is_empty() {
            assert!(recipe.table.is_none(), "{id} takes no parameters");
        } else {
            let table = recipe
                .table
                .unwrap_or_else(|| panic!("{id} shows its parameter table"));
            assert_eq!(
                table,
                registered.parameter_table(),
                "{id}: the page's table is the registry's"
            );
        }
    }
}
