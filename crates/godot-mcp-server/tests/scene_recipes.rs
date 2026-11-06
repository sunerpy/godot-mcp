//! Contract 0.5 acceptance: `scene.recipe.apply` through the production stdio
//! assembly. A recipe is the scene batch its domain registry expands it to, so
//! this suite pins what the recipe tool adds to `scene.batch.apply`: the batch
//! the engine or the editor receives, the answer naming the recipe and its
//! operations, the journal and replay under the recipe tool's own name, and
//! refusals before any engine runs.
//!
//! What the expanded operations do in a real editor is exercised against the
//! locked Godot builds by `live_recipes.rs`.

#![cfg(target_os = "linux")]

mod support;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use godot_mcp_domain::recipes;
use godot_mcp_server::test_support::fake_editor_plugin::{
    FakeAnswer, FakeEditorPlugin, FakePluginScript,
};
use serde_json::{Value, json};
use support::{ProjectTree, Spawn, StdioSession, error_code, fake_godot, sha256_hex};

const SCENE: &str =
    "[gd_scene format=3 uid=\"uid://b7rq2x1yb4kfl\"]\n\n[node name=\"Main\" type=\"Node2D\"]\n";

fn canonical_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../godot-mcp-engine/tests/fixtures/canonical/binaries/unix/godot-canonical.sh")
        .canonicalize()
        .expect("the engine crate's canonical fixture")
}

fn ok(response: &Value) -> Value {
    assert!(
        response.get("error").is_none() && response["result"]["isError"] != json!(true),
        "the call must succeed: {response}"
    );
    response["result"]["structuredContent"].clone()
}

fn open(tree: &ProjectTree, godot: Option<&Path>) -> StdioSession {
    StdioSession::initialized(&Spawn {
        project: &tree.root,
        current_dir: None,
        profile: "full",
        godot,
    })
}

/// The operations the domain registry expands `parameters` of `recipe` to.
fn expansion(recipe: &str, parameters: &Value, parent: Option<&str>) -> recipes::Expansion {
    recipes::recipe(recipe)
        .expect("a known recipe")
        .expand(parameters.as_object().expect("parameters"), parent)
        .expect("the parameters are accepted")
}

fn ball(expected: &str, idempotency: &str) -> Value {
    json!({
        "path": "scenes/main.tscn",
        "expectedSha256": expected,
        "recipe": { "body-2d": { "name": "Ball", "shape": "circle", "radius": 6 } },
        "idempotencyId": idempotency
    })
}

#[test]
fn a_recipe_is_applied_as_one_journaled_scene_batch() {
    let tree = ProjectTree::new("recipe-apply");
    tree.write("scenes/main.tscn", SCENE);
    let recorded = tree.base.join("helper-request.txt");
    tree.write(
        "fake-engine.conf",
        &format!("record={}\n", recorded.display()),
    );
    let (godot, _) = fake_godot(
        &tree.base,
        &format!("exec /bin/sh '{}' \"$@\"", canonical_fixture().display()),
    );
    let mut session = open(&tree, Some(&godot));

    let arguments = ball(&sha256_hex(SCENE.as_bytes()), "recipe-1");
    let applied = ok(&session.call_tool(2, "scene.recipe.apply", arguments.clone()));
    assert_eq!(applied["conclusion"], "proven_post_state", "{applied}");
    assert_eq!(applied["backend"], "headless", "{applied}");
    let written = std::fs::read_to_string(tree.root.join("scenes/main.tscn")).expect("scene");
    assert_eq!(applied["after_sha256"], sha256_hex(written.as_bytes()));

    // The answer names the recipe with its resolved parameters and carries
    // the operations it became, exactly as the registry expands them.
    let expected = expansion(
        "body-2d",
        &json!({ "name": "Ball", "shape": "circle", "radius": 6 }),
        None,
    );
    assert_eq!(applied["recipe"], expected.describe(), "{applied}");
    assert_eq!(applied["recipe"]["parameters"]["body"], "character");
    assert_eq!(
        applied["operations"],
        Value::Array(expected.operations.clone())
    );

    // The engine received those operations as the batch.
    let request = std::fs::read_to_string(&recorded).expect("the helper request was recorded");
    assert!(request.contains("CharacterBody2D"), "{request}");
    assert!(request.contains("CircleShape2D"), "{request}");
    assert!(request.contains("\"name\":\"Ball\""), "{request}");

    // An exact retry answers from the journal under the recipe tool's name.
    let replay = ok(&session.call_tool(3, "scene.recipe.apply", arguments));
    assert_eq!(replay["replayed"], json!(true), "{replay}");
    assert_eq!(
        std::fs::read_to_string(tree.root.join("scenes/main.tscn")).expect("scene"),
        written
    );
    // The same identifier with other arguments is not a retry.
    let mut other = ball(&sha256_hex(SCENE.as_bytes()), "recipe-1");
    other["recipe"]["body-2d"]["radius"] = json!(7);
    let reused = session.call_tool(4, "scene.recipe.apply", other);
    assert_eq!(error_code(&reused), json!("conflict"), "{reused}");
    let _ = session.finish();
}

#[test]
fn a_recipe_dry_run_reports_its_operations_and_publishes_nothing() {
    let tree = ProjectTree::new("recipe-dry");
    tree.write("scenes/main.tscn", SCENE);
    let (godot, _) = fake_godot(
        &tree.base,
        &format!("exec /bin/sh '{}' \"$@\"", canonical_fixture().display()),
    );
    let mut session = open(&tree, Some(&godot));

    let mut arguments = json!({
        "path": "scenes/main.tscn",
        "expectedSha256": sha256_hex(SCENE.as_bytes()),
        "recipe": { "particles-2d": { "preset": "rain" } },
        "parent": "Main",
        "dryRun": true,
        "idempotencyId": "recipe-dry-1"
    });
    let dry = ok(&session.call_tool(2, "scene.recipe.apply", arguments.clone()));
    assert_eq!(dry["dry_run"], json!(true), "{dry}");
    assert!(dry.get("transaction_id").is_none(), "{dry}");
    let expected = expansion("particles-2d", &json!({ "preset": "rain" }), Some("Main"));
    assert_eq!(dry["recipe"], expected.describe(), "{dry}");
    assert_eq!(dry["recipe"]["parent"], "Main");
    assert_eq!(dry["recipe"]["parameters"]["width"], json!(640.0));
    assert_eq!(dry["operations"], Value::Array(expected.operations));
    assert_eq!(
        std::fs::read_to_string(tree.root.join("scenes/main.tscn")).expect("scene"),
        SCENE
    );

    // The real call publishes.
    arguments["dryRun"] = json!(false);
    arguments["idempotencyId"] = json!("recipe-dry-2");
    let applied = ok(&session.call_tool(3, "scene.recipe.apply", arguments));
    assert_eq!(applied["conclusion"], "proven_post_state", "{applied}");
    let _ = session.finish();
}

#[test]
fn a_refused_recipe_launches_no_engine() {
    let tree = ProjectTree::new("recipe-refused");
    tree.write("scenes/main.tscn", SCENE);
    let (godot, argv_log) = fake_godot(
        &tree.base,
        &format!("exec /bin/sh '{}' \"$@\"", canonical_fixture().display()),
    );
    let mut session = open(&tree, Some(&godot));
    let current = sha256_hex(SCENE.as_bytes());

    let stale = session.call_tool(
        2,
        "scene.recipe.apply",
        ball(&sha256_hex(b"older bytes"), "refused-stale"),
    );
    assert_eq!(error_code(&stale), json!("conflict"), "{stale}");

    let mut id = 2;
    for (recipe, needle) in [
        (json!({ "theme": {} }), "resource.create"),
        (json!({ "fog-2d": {} }), "there is no recipe"),
        (
            json!({ "body-2d": { "shape": "circle", "height": 4 } }),
            "applies only when",
        ),
        (json!({ "light-3d": { "name": "Stage" } }), "name"),
        (json!("body-2d"), "one key"),
    ] {
        id += 1;
        let refused = session.call_tool(
            id,
            "scene.recipe.apply",
            json!({
                "path": "scenes/main.tscn",
                "expectedSha256": current,
                "recipe": recipe,
                "idempotencyId": format!("refused-{id}")
            }),
        );
        assert_eq!(error_code(&refused), json!("invalid_input"), "{refused}");
        assert!(refused.to_string().contains(needle), "{needle}: {refused}");
    }
    // A recipe is a journaled mutation: it needs an idempotency identifier.
    id += 1;
    let anonymous = session.call_tool(
        id,
        "scene.recipe.apply",
        json!({
            "path": "scenes/main.tscn",
            "expectedSha256": current,
            "recipe": { "hud": {} }
        }),
    );
    assert_eq!(
        error_code(&anonymous),
        json!("invalid_input"),
        "{anonymous}"
    );

    let launched = std::fs::read_to_string(&argv_log).unwrap_or_default();
    assert!(
        !launched.contains("--script"),
        "no refused recipe may launch the helper: {launched}"
    );
    assert_eq!(
        std::fs::read_to_string(tree.root.join("scenes/main.tscn")).expect("scene"),
        SCENE
    );
    let _ = session.finish();
}

fn rendezvous(tree: &ProjectTree) -> PathBuf {
    tree.root
        .join(".godot-mcp")
        .join("editor-bridge.rendezvous")
}

fn wait_connected(session: &mut StdioSession, next_id: &mut u64) {
    let started = Instant::now();
    loop {
        *next_id += 1;
        let status = ok(&session.call_tool(*next_id, "editor.status.get", json!({})));
        if status["connected"] == json!(true) {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the plugin never connected: {status}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_recipe_for_a_scene_open_in_the_editor_is_one_undoable_editor_action() {
    let tree = ProjectTree::new("recipe-editor");
    tree.write("scenes/main.tscn", SCENE);
    let mut session = open(&tree, None);
    let saved = format!("{SCENE}\n[node name=\"Ball\" type=\"CharacterBody2D\" parent=\".\"]\n");
    let plugin = FakeEditorPlugin::spawn(
        &rendezvous(&tree),
        FakePluginScript::implementing(&["editor.state", "editor.scene.batch"])
            .answering(
                "editor.state",
                FakeAnswer::Payload(json!({
                    "open_scenes": [{
                        "path": "res://scenes/main.tscn", "edited": true, "unsaved": false
                    }],
                    "open_scripts": [],
                    "edited_scene": "res://scenes/main.tscn",
                    "selection": [],
                    "history": {
                        "version": 1, "has_undo": false, "has_redo": false, "current_action": ""
                    },
                    "playing": false
                })),
            )
            .answering(
                "editor.scene.batch",
                FakeAnswer::SaveThenAnswer {
                    path: tree.root.join("scenes/main.tscn"),
                    contents: saved.clone().into_bytes(),
                    payload: json!({
                        "path": "res://scenes/main.tscn",
                        "saved": true,
                        "action": "godot-mcp: scene.recipe.apply",
                        "version": 2,
                        "digest": "live-digest"
                    }),
                },
            ),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);

    let arguments = ball(&sha256_hex(SCENE.as_bytes()), "editor-recipe-1");
    id += 1;
    let applied = ok(&session.call_tool(id, "scene.recipe.apply", arguments.clone()));
    assert_eq!(applied["backend"], "editor", "{applied}");
    assert_eq!(applied["conclusion"], "proven_post_state", "{applied}");
    assert_eq!(applied["after_sha256"], sha256_hex(saved.as_bytes()));
    assert_eq!(applied["recipe"]["id"], "body-2d", "{applied}");

    // The editor receives the expanded operations as one action named after
    // the recipe tool, which its own Undo reverts as one step.
    let expected = expansion(
        "body-2d",
        &json!({ "name": "Ball", "shape": "circle", "radius": 6 }),
        None,
    );
    let (_, sent) = plugin
        .received()
        .into_iter()
        .find(|(method, _)| method == "editor.scene.batch")
        .expect("the editor received the batch");
    assert_eq!(sent["label"], "godot-mcp: scene.recipe.apply", "{sent}");
    assert_eq!(
        sent["operations"],
        Value::Array(expected.operations.clone())
    );
    assert_eq!(applied["operations"], Value::Array(expected.operations));

    id += 1;
    let replay = ok(&session.call_tool(id, "scene.recipe.apply", arguments));
    assert_eq!(replay["replayed"], json!(true), "{replay}");
    let batches = plugin
        .received()
        .iter()
        .filter(|(method, _)| method == "editor.scene.batch")
        .count();
    assert_eq!(batches, 1, "a replay never asks the editor again");
    let _ = session.finish();
    plugin.join().expect("the plugin saw the server stop");
}
