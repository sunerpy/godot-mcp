//! Live authoring acceptance: property paths, graph-resource calls and
//! removals, repeated calls, structural operations (copies, moves, scripts,
//! signals, groups and instances), global script classes and imported models,
//! applied by the bundled headless helper and by the shipped add-on in real
//! Godot editors, through the production MCP server.
//!
//! Every case runs the same operations both ways and checks what the saved
//! scene holds; in the editor it also undoes and redoes the edit and checks
//! the scene digest each time. Like the live-editor suite it needs real
//! editors, named by `GODOT_MCP_TEST_EDITORS` or `GODOT_MCP_LOCKED_ARTIFACT`,
//! so every case is ignored on a plain `cargo test`.

#![cfg(target_os = "linux")]

#[path = "support/live.rs"]
mod live;
mod support;

use std::path::Path;

use live::{Editor, digest_of, editors, ok, project_with_addon, wait_connected};
use serde_json::{Value, json};
use support::{ProjectTree, Spawn, StdioSession, sha256_hex};

fn tagged(kind: &str, value: Value) -> Value {
    json!({ "type": kind, "value": value })
}

fn integer(value: i64) -> Value {
    tagged("integer", json!(value))
}

fn number(value: f64) -> Value {
    tagged("float", json!(value))
}

fn name(value: &str) -> Value {
    tagged("string_name", json!(value))
}

fn vector2(x: f64, y: f64) -> Value {
    tagged("vector2", json!({ "x": x, "y": y }))
}

fn vector2i(x: i64, y: i64) -> Value {
    tagged("vector2i", json!({ "x": x, "y": y }))
}

fn red() -> Value {
    tagged(
        "color",
        json!({ "red": 1.0, "green": 0.0, "blue": 0.0, "alpha": 1.0 }),
    )
}

fn new_resource(class: &str, properties: Value) -> Value {
    json!({ "type": "new_resource", "value": { "class": class, "properties": properties } })
}

/// A headless MCP session on `tree` whose engine is `binary`.
fn headless(tree: &ProjectTree, binary: &Path) -> StdioSession {
    StdioSession::initialized(&Spawn {
        project: &tree.root,
        current_dir: None,
        profile: "full",
        godot: Some(binary),
    })
}

/// Why a refused call was refused: the reason that opens its message.
fn refusal(response: &Value) -> String {
    let message = response["result"]["structuredContent"]["error"]["message"]
        .as_str()
        .or_else(|| response["error"]["message"].as_str())
        .or_else(|| {
            response["result"]["content"][0]["text"]
                .as_str()
                .filter(|_| response["result"]["isError"] == json!(true))
        })
        .unwrap_or_default()
        .to_owned();
    assert!(
        !message.is_empty(),
        "the call must be refused with a message: {response}"
    );
    message
}

const PATHS_SCENE: &str = r#"[gd_scene format=3]

[sub_resource type="StandardMaterial3D" id="StandardMaterial3D_m"]

[sub_resource type="AnimationNodeAnimation" id="AnimationNodeAnimation_i"]
animation = &"idle"

[sub_resource type="AnimationNodeStateMachine" id="AnimationNodeStateMachine_s"]
states/Idle/node = SubResource("AnimationNodeAnimation_i")
states/Idle/position = Vector2(100, 100)

[sub_resource type="PlaceholderTexture2D" id="PlaceholderTexture2D_t"]
size = Vector2(32, 16)

[sub_resource type="TileSetAtlasSource" id="TileSetAtlasSource_a"]
texture = SubResource("PlaceholderTexture2D_t")
0:0/0 = 0

[sub_resource type="TileSet" id="TileSet_t"]
sources/0 = SubResource("TileSetAtlasSource_a")

[node name="Main" type="Node3D"]

[node name="Mesh" type="MeshInstance3D" parent="."]
material_override = SubResource("StandardMaterial3D_m")

[node name="Sprite" type="Node2D" parent="."]

[node name="Tree" type="AnimationTree" parent="."]
tree_root = SubResource("AnimationNodeStateMachine_s")

[node name="Ground" type="TileMapLayer" parent="."]
tile_set = SubResource("TileSet_t")
"#;

/// Writes through property paths, a state the state machine does not list
/// yet, calls on graph resources and a repeated call, with what each call
/// returns.
fn path_operations() -> (Vec<Value>, Value) {
    let cell = |x: i64| json!([vector2i(x, 0), integer(0), vector2i(0, 0)]);
    let operations = vec![
        json!({ "op": "set_property", "node": "Sprite", "property": "position:x", "value": number(7.0) }),
        json!({ "op": "set_property", "node": "Mesh", "property": "material_override:albedo_color", "value": red() }),
        json!({ "op": "set_property", "property": "metadata/level", "value": integer(3) }),
        json!({ "op": "set_subresource", "node": "Tree", "property": "tree_root:states/Run/node",
                "class": "AnimationNodeAnimation", "properties": { "animation": name("run") } }),
        json!({ "op": "set_property", "node": "Tree", "property": "tree_root:states/Idle/position",
                "value": vector2(5.0, 6.0) }),
        json!({ "op": "call_method", "node": "Tree", "resource": { "property": "tree_root" },
                "method": "add_transition",
                "arguments": [name("Idle"), name("Run"),
                    new_resource("AnimationNodeStateMachineTransition", json!({ "xfade_time": number(0.25) }))] }),
        json!({ "op": "call_method", "node": "Ground", "resource": { "property": "tile_set" },
                "method": "add_physics_layer", "arguments": [] }),
        json!({ "op": "call_method", "node": "Ground", "method": "set_cell",
                "repeat": [cell(0), cell(1), cell(2)] }),
    ];
    let results = json!([
        { "index": 5, "method": "add_transition", "value": null },
        { "index": 6, "method": "add_physics_layer", "value": null },
        { "index": 7, "method": "set_cell", "values": [null, null, null] },
    ]);
    (operations, results)
}

/// What [`path_operations`] leave in a saved scene.
fn assert_paths_landed(scene: &str, context: &str) {
    for marker in [
        "position = Vector2(7, 0)",
        "albedo_color = Color(1, 0, 0, 1)",
        "metadata/level = 3",
        "animation = &\"run\"",
        "states/Idle/position = Vector2(5, 6)",
        "xfade_time = 0.25",
        "physics_layer_0/collision_layer = 1",
    ] {
        assert!(
            scene.contains(marker),
            "the saved scene holds {marker:?}: {context}\n{scene}"
        );
    }
    assert!(
        scene
            .lines()
            .any(|line| line.starts_with("states/Run/node = SubResource(")),
        "the state machine holds the state Run: {context}\n{scene}"
    );
    assert!(
        scene
            .lines()
            .any(|line| line.starts_with("transitions = [\"Idle\", \"Run\", SubResource(")),
        "the state machine holds a transition from Idle to Run: {context}\n{scene}"
    );
    let cells = scene
        .lines()
        .find(|line| line.starts_with("tile_map_data = PackedByteArray("))
        .unwrap_or_else(|| panic!("the layer holds cells: {context}\n{scene}"));
    // Two bytes of format, then twelve bytes per cell.
    let bytes = live::packed_bytes(cells).expect("tile map data");
    assert_eq!((bytes.len() - 2) / 12, 3, "three cells: {context}\n{scene}");
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn property_paths_and_graph_calls_land_in_the_editor_and_headless() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        property_paths_and_graph_calls_land_in(binary);
    }
}

fn property_paths_and_graph_calls_land_in(binary: &Path) {
    let (operations, results) = path_operations();
    let context = binary.display().to_string();

    // Headless: the helper changes the resources themselves.
    let tree = ProjectTree::new("live-paths-headless");
    tree.write("main.tscn", PATHS_SCENE);
    let mut session = headless(&tree, binary);
    let applied = ok(
        &session.call_tool(
            2,
            "scene.batch.apply",
            json!({
                "path": "main.tscn",
                "expectedSha256": sha256_hex(PATHS_SCENE.as_bytes()),
                "operations": operations,
                "idempotencyId": "paths-headless"
            }),
        ),
        &context,
    );
    assert_eq!(applied["backend"], "headless", "{applied}");
    assert_eq!(applied["results"], results, "{context}: {applied}");
    let saved = std::fs::read_to_string(tree.root.join("main.tscn")).expect("scene");
    assert_paths_landed(&saved, &format!("{context}, headless"));
    let _ = session.finish();

    // In the editor: one undoable action, graph resources replaced by copies.
    let (tree, mut session, mut ids) =
        project_with_addon("live-paths", &[("main.tscn", PATHS_SCENE)]);
    let editor = Editor::launch(binary, &tree.root, &tree.base.join("home"));
    wait_connected(&mut session, &mut ids, &editor);
    let mark = editor.mark();
    ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.open",
            json!({ "path": "main.tscn", "idempotencyId": "paths-open" }),
        ),
        &editor.context(),
    );
    let original = digest_of(&mut session, &mut ids, &editor.context());
    let edited = ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.edit",
            json!({
                "expectedDigest": original,
                "operations": operations,
                "label": "paths",
                "idempotencyId": "paths-edit"
            }),
        ),
        &editor.context(),
    );
    assert_eq!(edited["results"], results, "{}: {edited}", editor.binary);
    let changed = edited["digest"].as_str().expect("digest").to_owned();
    assert_ne!(changed, original, "{edited}");
    for (tool, expected, why) in [
        ("editor.undo", &original, "undoing puts every resource back"),
        ("editor.redo", &changed, "redoing makes every change again"),
        (
            "editor.undo",
            &original,
            "undoing a redo puts them back again",
        ),
        (
            "editor.redo",
            &changed,
            "and redoing it again changes them again",
        ),
    ] {
        let stepped = ok(
            &session.call_tool(
                ids.next(),
                tool,
                json!({ "idempotencyId": format!("paths-{tool}-{why}") }),
            ),
            &editor.context(),
        );
        assert_eq!(
            stepped["digest"],
            json!(expected),
            "{}: {why}",
            editor.binary
        );
    }
    ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.save",
            json!({ "idempotencyId": "paths-save" }),
        ),
        &editor.context(),
    );
    let saved = std::fs::read_to_string(tree.root.join("main.tscn")).expect("scene");
    assert_paths_landed(&saved, &format!("{}, editor", editor.binary));
    editor.assert_no_errors_since(
        mark,
        "writing through paths, calling graph resources, undoing, redoing and saving",
    );
    let _ = session.finish();
    drop(editor);
}

const SHARED_SCENE: &str = r#"[gd_scene load_steps=2 format=3]

[ext_resource type="Material" path="res://red.tres" id="1"]

[sub_resource type="StandardMaterial3D" id="StandardMaterial3D_m"]

[sub_resource type="AnimationNodeStateMachine" id="AnimationNodeStateMachine_s"]

[node name="Main" type="Node3D"]

[node name="Saved" type="MeshInstance3D" parent="."]
material_override = ExtResource("1")

[node name="Embedded" type="MeshInstance3D" parent="."]
material_override = SubResource("StandardMaterial3D_m")

[node name="Sprite" type="Node2D" parent="."]

[node name="One" type="AnimationTree" parent="."]
tree_root = SubResource("AnimationNodeStateMachine_s")

[node name="Two" type="AnimationTree" parent="."]
tree_root = SubResource("AnimationNodeStateMachine_s")
"#;
const RED: &str = "[gd_resource type=\"StandardMaterial3D\" format=3]\n\n[resource]\nalbedo_color = Color(1, 0, 0, 1)\n";

/// Paths no backend may take, each with the reason both give.
fn refused_paths() -> Vec<(Value, &'static str)> {
    vec![
        (
            json!({ "op": "set_property", "node": "Saved", "property": "material_override:albedo_color", "value": red() }),
            "external_resource",
        ),
        (
            json!({ "op": "set_property", "node": "Sprite", "property": "position:q", "value": number(1.0) }),
            "unknown_property",
        ),
        (
            json!({ "op": "set_property", "node": "Sprite", "property": "speed", "value": number(1.0) }),
            "unknown_property",
        ),
        (
            json!({ "op": "clear_property", "node": "Embedded", "property": "material_override:albedo_color:r" }),
            "invalid_property_path",
        ),
        (
            json!({ "op": "set_property", "node": "Embedded", "property": "material_override:albedo_color", "value": number(1.0) }),
            "value_type_mismatch",
        ),
        (
            json!({ "op": "call_method", "node": "Saved", "resource": { "property": "material_override" },
                    "method": "add_point", "arguments": [] }),
            "external_resource",
        ),
        (
            json!({ "op": "set_subresource", "node": "Sprite", "property": "material", "class": "Node" }),
            "unknown_resource_class",
        ),
        (
            json!({ "op": "call_method", "node": "One", "resource": { "property": "tree_root" },
                    "method": "add_node", "arguments": [name("Run"), new_resource("Node", json!({}))] }),
            "invalid_value",
        ),
    ]
}

/// A call on the state machine two AnimationTrees share.
fn shared_call() -> Value {
    json!({ "op": "call_method", "node": "One", "resource": { "property": "tree_root" },
            "method": "add_node",
            "arguments": [name("Run"), new_resource("AnimationNodeAnimation", json!({ "animation": name("run") }))] })
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn a_path_refuses_what_the_scene_does_not_own() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        a_path_refuses_what_the_scene_does_not_own_in(binary);
    }
}

fn a_path_refuses_what_the_scene_does_not_own_in(binary: &Path) {
    let context = binary.display().to_string();
    let tree = ProjectTree::new("live-paths-refused");
    tree.write("main.tscn", SHARED_SCENE);
    tree.write("red.tres", RED);
    let mut session = headless(&tree, binary);
    let mut id = 1;
    for (operation, reason) in refused_paths() {
        id += 1;
        let response = session.call_tool(
            id,
            "scene.batch.apply",
            json!({
                "path": "main.tscn",
                "expectedSha256": sha256_hex(SHARED_SCENE.as_bytes()),
                "operations": [operation],
                "idempotencyId": format!("refused-{id}")
            }),
        );
        let message = refusal(&response);
        assert!(
            message.contains(reason),
            "{context}, headless: {operation} is refused as {reason}: {message}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(tree.root.join("main.tscn")).expect("scene"),
        SHARED_SCENE,
        "{context}: a refused batch changes nothing"
    );
    // The headless helper changes a shared resource itself, for every holder.
    id += 1;
    let shared = ok(
        &session.call_tool(
            id,
            "scene.batch.apply",
            json!({
                "path": "main.tscn",
                "expectedSha256": sha256_hex(SHARED_SCENE.as_bytes()),
                "operations": [shared_call()],
                "dryRun": true,
                "idempotencyId": "shared-headless"
            }),
        ),
        &context,
    );
    assert_eq!(shared["backend"], "headless", "{shared}");
    let _ = session.finish();

    let (tree, mut session, mut ids) = project_with_addon(
        "live-paths-refused-editor",
        &[("main.tscn", SHARED_SCENE), ("red.tres", RED)],
    );
    let editor = Editor::launch(binary, &tree.root, &tree.base.join("home"));
    wait_connected(&mut session, &mut ids, &editor);
    ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.open",
            json!({ "path": "main.tscn", "idempotencyId": "refused-open" }),
        ),
        &editor.context(),
    );
    let original = digest_of(&mut session, &mut ids, &editor.context());
    let mark = editor.mark();
    let mut refused = refused_paths();
    // A copy given to one AnimationTree would part it from the other.
    refused.push((shared_call(), "shared_resource"));
    for (operation, reason) in refused {
        let response = session.call_tool(
            ids.next(),
            "editor.scene.edit",
            json!({
                "expectedDigest": original,
                "operations": [operation],
                "idempotencyId": format!("refused-edit-{}", ids.0)
            }),
        );
        let message = refusal(&response);
        assert!(
            message.contains(reason),
            "{}, editor: {operation} is refused as {reason}: {message}",
            editor.binary
        );
    }
    assert_eq!(
        digest_of(&mut session, &mut ids, &editor.context()),
        original,
        "{}: a refused edit changes nothing",
        editor.binary
    );
    editor.assert_no_errors_since(mark, "refusing paths the scene does not own");
    let _ = session.finish();
    drop(editor);
}

const ENEMY: &str = "class_name LiveEnemy\nextends Node2D\n\n@export var hp := 9\n";
const ITEM: &str = "class_name LiveItem\nextends Resource\n\n@export var price := 3\n";
const LIBRARY_SCENE: &str = "[gd_scene format=3]\n\n[node name=\"Main\" type=\"Node3D\"]\n";
/// A glTF model of two meshes, Hull and Fin, sharing one triangle.
const SHIP: &str = r#"{"asset":{"version":"2.0"},"scene":0,"scenes":[{"nodes":[0,1]}],"nodes":[{"mesh":0,"name":"Hull"},{"mesh":0,"name":"Fin","translation":[2,0,0]}],"meshes":[{"name":"HullMesh","primitives":[{"attributes":{"POSITION":0}}]}],"buffers":[{"byteLength":36,"uri":"data:application/octet-stream;base64,AAAAAAAAAAAAAAAAAACAPwAAAAAAAAAAAAAAAAAAgD8AAAAA"}],"bufferViews":[{"buffer":0,"byteOffset":0,"byteLength":36}],"accessors":[{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0,0,0],"max":[1,1,0]}]}"#;

/// A node and a resource of global script classes, and an imported model
/// instanced whole.
fn class_operations() -> Vec<Value> {
    vec![
        json!({ "op": "add_node", "name": "Enemy", "type": "LiveEnemy" }),
        json!({ "op": "set_property", "node": "Enemy", "property": "hp", "value": integer(4) }),
        json!({ "op": "add_node", "name": "Holder", "type": "Node2D" }),
        json!({ "op": "set_subresource", "node": "Holder", "property": "metadata/item", "class": "LiveItem",
                "properties": { "price": integer(12) } }),
        json!({ "op": "instance_scene", "name": "Ship", "scene": "res://models/ship.gltf" }),
    ]
}

fn assert_classes_landed(scene: &str, context: &str) {
    // Godot 4.7 adds a unique_id to every node header.
    for marker in [
        "[node name=\"Enemy\" type=\"Node2D\" parent=\".\"",
        "hp = 4",
        "price = 12",
        "path=\"res://models/ship.gltf\"",
        "[node name=\"Ship\" parent=\".\"",
    ] {
        assert!(
            scene.contains(marker),
            "the saved scene holds {marker:?}: {context}\n{scene}"
        );
    }
    assert!(
        scene.contains("res://scripts/enemy.gd") && scene.contains("res://scripts/item.gd"),
        "the node and the resource carry their scripts: {context}\n{scene}"
    );
    assert!(
        scene
            .lines()
            .any(|line| line.starts_with("[node name=\"Ship\"")
                && line.contains(" instance=ExtResource(")),
        "the model is one instance of the imported scene: {context}\n{scene}"
    );
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn global_classes_and_imported_models_are_authored() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        global_classes_and_imported_models_are_authored_in(binary);
    }
}

fn global_classes_and_imported_models_are_authored_in(binary: &Path) {
    let files = [
        ("main.tscn", LIBRARY_SCENE),
        ("scripts/enemy.gd", ENEMY),
        ("scripts/item.gd", ITEM),
        ("models/ship.gltf", SHIP),
    ];
    let context = binary.display().to_string();

    // Headless: the project is imported first, so its classes and the model
    // are known; the helper then creates them.
    let tree = ProjectTree::new("live-classes-headless");
    for (path, contents) in files {
        tree.write(path, contents);
    }
    let mut session = headless(&tree, binary);
    ok(
        &session.call_tool(
            2,
            "project.import.run",
            json!({ "idempotencyId": "classes-import", "deadline_ms": 120_000 }),
        ),
        &context,
    );
    let applied = ok(
        &session.call_tool(
            3,
            "scene.batch.apply",
            json!({
                "path": "main.tscn",
                "expectedSha256": sha256_hex(LIBRARY_SCENE.as_bytes()),
                "operations": class_operations(),
                "idempotencyId": "classes-headless"
            }),
        ),
        &context,
    );
    assert_eq!(applied["backend"], "headless", "{applied}");
    let saved = std::fs::read_to_string(tree.root.join("main.tscn")).expect("scene");
    assert_classes_landed(&saved, &format!("{context}, headless"));
    let _ = session.finish();

    // In the editor, which imported the project when it started.
    let (tree, mut session, mut ids) = project_with_addon("live-classes", &files);
    let editor = Editor::launch(binary, &tree.root, &tree.base.join("home"));
    wait_connected(&mut session, &mut ids, &editor);
    ok(
        &session.call_tool(
            ids.next(),
            "editor.filesystem.scan",
            json!({ "idempotencyId": "classes-scan" }),
        ),
        &editor.context(),
    );
    ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.open",
            json!({ "path": "main.tscn", "idempotencyId": "classes-open" }),
        ),
        &editor.context(),
    );
    let mark = editor.mark();
    let original = digest_of(&mut session, &mut ids, &editor.context());
    let edited = ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.edit",
            json!({
                "expectedDigest": original,
                "operations": class_operations(),
                "idempotencyId": "classes-edit"
            }),
        ),
        &editor.context(),
    );
    let changed = edited["digest"].as_str().expect("digest").to_owned();
    let undone = ok(
        &session.call_tool(
            ids.next(),
            "editor.undo",
            json!({ "idempotencyId": "classes-undo" }),
        ),
        &editor.context(),
    );
    assert_eq!(undone["digest"], json!(original), "{}: undo", editor.binary);
    let redone = ok(
        &session.call_tool(
            ids.next(),
            "editor.redo",
            json!({ "idempotencyId": "classes-redo" }),
        ),
        &editor.context(),
    );
    assert_eq!(redone["digest"], json!(changed), "{}: redo", editor.binary);
    ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.save",
            json!({ "idempotencyId": "classes-save" }),
        ),
        &editor.context(),
    );
    let saved = std::fs::read_to_string(tree.root.join("main.tscn")).expect("scene");
    assert_classes_landed(&saved, &format!("{}, editor", editor.binary));
    editor.assert_no_errors_since(mark, "authoring global classes and an imported model");
    let _ = session.finish();
    drop(editor);
}

/// Settings `set_setting` writes, each with the tagged value it must read back
/// as. They cross Godot's own spelling: fractions and whole numbers in one
/// constructor, a string with quotes and a newline, which Godot writes as it
/// is, and nested containers.
fn generic_settings() -> Vec<(&'static str, Value)> {
    vec![
        (
            "live/values/text",
            tagged("string", json!("say \"hi\"\nsecond line")),
        ),
        (
            "live/values/tint",
            tagged(
                "color",
                json!({ "red": 0.25, "green": 0.5, "blue": 0.75, "alpha": 1.0 }),
            ),
        ),
        (
            "live/values/table",
            tagged(
                "dictionary",
                json!({
                    "speed": integer(3),
                    "offset": vector2(1.5, -2.0),
                    "tags": tagged("packed_string_array", json!(["a", "b"]))
                }),
            ),
        ),
        ("live/values/count", integer(120)),
        ("live/values/ratio", number(0.5)),
        (
            "layer_names/2d_physics/layer_1",
            tagged("string", json!("player")),
        ),
    ]
}

/// The tagged value `project.settings.get` answers for each named setting,
/// with the literal the file holds.
fn selected(session: &mut StdioSession, id: u64, names: &[&str], context: &str) -> Value {
    let answer = ok(
        &session.call_tool(id, "project.settings.get", json!({ "settings": names })),
        context,
    );
    assert_eq!(
        answer["manifest_diagnostics"],
        json!(0),
        "{context}: project.godot reads cleanly: {answer}"
    );
    answer
}

fn entry<'a>(answer: &'a Value, setting: &str) -> &'a Value {
    answer["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .find(|entry| entry["setting"] == json!(setting))
        .expect("the setting is answered")
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn generic_settings_reach_godot_and_read_back() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        generic_settings_reach_godot_and_read_back_in(binary);
    }
}

fn generic_settings_reach_godot_and_read_back_in(binary: &Path) {
    let settings = generic_settings();
    let names: Vec<&str> = settings.iter().map(|(name, _)| *name).collect();
    let (tree, mut session, mut ids) = project_with_addon("live-settings-generic", &[]);
    let context = binary.display().to_string();

    // Before an editor runs, the file path writes the server's spelling.
    let sha = ok(
        &session.call_tool(ids.next(), "project.settings.get", json!({})),
        &context,
    )["sha256"]
        .as_str()
        .expect("sha256")
        .to_owned();
    let operations: Vec<Value> = settings
        .iter()
        .map(|(setting, value)| json!({ "op": "set_setting", "setting": setting, "value": value }))
        .collect();
    let written = ok(
        &session.call_tool(
            ids.next(),
            "project.settings.patch",
            json!({ "expectedSha256": sha, "operations": operations, "idempotencyId": "generic-file" }),
        ),
        &context,
    );
    assert_eq!(written["backend"], "file", "{written}");
    let answer = selected(&mut session, ids.next(), &names, &context);
    for (setting, value) in &settings {
        assert_eq!(
            &entry(&answer, setting)["value"],
            value,
            "{context}: {setting}"
        );
    }

    // The editor loads that spelling; a change it applies makes it save every
    // setting it holds with Godot's own writer, so what reads back after it
    // is what Godot read.
    let editor = Editor::launch(binary, &tree.root, &tree.base.join("home"));
    wait_connected(&mut session, &mut ids, &editor);
    let mark = editor.mark();
    let sha = answer["sha256"].as_str().expect("sha256").to_owned();
    let saved = ok(
        &session.call_tool(
            ids.next(),
            "project.settings.patch",
            json!({
                "expectedSha256": sha,
                "operations": [{
                    "op": "set_setting",
                    "setting": "live/values/marker",
                    "value": tagged("string", json!("saved by the editor"))
                }],
                "idempotencyId": "generic-editor"
            }),
        ),
        &editor.context(),
    );
    assert_eq!(saved["backend"], "editor", "{saved}");
    let manifest = std::fs::read_to_string(tree.root.join("project.godot")).expect("manifest");
    assert!(
        manifest.contains("say \\\"hi\\\"\nsecond line\""),
        "{}: Godot writes a string's newline as it is:\n{manifest}",
        editor.binary
    );
    let mut with_marker = names.clone();
    with_marker.push("live/values/marker");
    let answer = selected(&mut session, ids.next(), &with_marker, &editor.context());
    for (setting, value) in &settings {
        assert_eq!(
            &entry(&answer, setting)["value"],
            value,
            "{}: {setting} after Godot saved it as {}",
            editor.binary,
            entry(&answer, setting)["literal"]
        );
    }
    assert_eq!(
        entry(&answer, "live/values/marker")["value"],
        tagged("string", json!("saved by the editor"))
    );

    // Clearing goes through the editor too: a project setting is erased, a
    // built-in one goes back to its default, which Godot leaves out.
    let sha = answer["sha256"].as_str().expect("sha256").to_owned();
    let cleared = ok(
        &session.call_tool(
            ids.next(),
            "project.settings.patch",
            json!({
                "expectedSha256": sha,
                "operations": [
                    { "op": "clear_setting", "setting": "live/values/table" },
                    { "op": "clear_setting", "setting": "layer_names/2d_physics/layer_1" }
                ],
                "idempotencyId": "generic-clear"
            }),
        ),
        &editor.context(),
    );
    assert_eq!(cleared["backend"], "editor", "{cleared}");
    let answer = selected(
        &mut session,
        ids.next(),
        &[
            "live/values/table",
            "layer_names/2d_physics/layer_1",
            "live/values/tint",
        ],
        &editor.context(),
    );
    assert_eq!(entry(&answer, "live/values/table")["literal"], Value::Null);
    assert_eq!(
        entry(&answer, "layer_names/2d_physics/layer_1")["literal"],
        Value::Null
    );
    assert_eq!(
        entry(&answer, "live/values/tint")["value"],
        settings[1].1,
        "{}: an unrelated setting stays",
        editor.binary
    );
    editor.assert_no_errors_since(mark, "generic settings");
    let _ = session.finish();
}

const BAKE_SCENE: &str = r#"[gd_scene load_steps=8 format=3]

[ext_resource type="Script" path="res://level.gd" id="1"]

[sub_resource type="NavigationMesh" id="NavigationMesh_n"]
geometry_parsed_geometry_type = 1

[sub_resource type="BoxShape3D" id="BoxShape3D_b"]
size = Vector3(20, 1, 20)

[sub_resource type="NavigationPolygon" id="NavigationPolygon_p"]
outlines = Array[PackedVector2Array]([PackedVector2Array(0, 0, 400, 0, 400, 400, 0, 400)])

[sub_resource type="RectangleShape2D" id="RectangleShape2D_r"]
size = Vector2(50, 50)

[sub_resource type="NavigationMesh" id="NavigationMesh_empty"]

[node name="Level" type="Node3D"]
script = ExtResource("1")

[node name="Region" type="NavigationRegion3D" parent="."]
navigation_mesh = SubResource("NavigationMesh_n")

[node name="Ground" type="StaticBody3D" parent="Region"]

[node name="Shape" type="CollisionShape3D" parent="Region/Ground"]
shape = SubResource("BoxShape3D_b")

[node name="Region2D" type="NavigationRegion2D" parent="."]
navigation_polygon = SubResource("NavigationPolygon_p")

[node name="Rock" type="StaticBody2D" parent="Region2D"]

[node name="Shape" type="CollisionShape2D" parent="Region2D/Rock"]
position = Vector2(200, 200)
shape = SubResource("RectangleShape2D_r")

[node name="Bare" type="NavigationRegion3D" parent="."]

[node name="Linked" type="NavigationRegion3D" parent="."]
"#;

/// The level's script writes `ran.txt` when it runs; nothing a bake does may
/// run it.
const LEVEL_SCRIPT: &str = "extends Node3D\n\n\nfunc _ready() -> void:\n\tvar file := FileAccess.open(\"res://ran.txt\", FileAccess.WRITE)\n\tfile.store_string(\"ran\")\n";

const LINKED_MESH: &str = "[gd_resource type=\"NavigationMesh\" format=3]\n\n[resource]\n";

fn bake_operations() -> Vec<Value> {
    vec![
        json!({ "op": "call_method", "node": "Region", "method": "bake_navigation_mesh" }),
        json!({ "op": "call_method", "node": "Region2D", "method": "bake_navigation_polygon",
                "arguments": [tagged("bool", json!(false))] }),
    ]
}

/// The `baked` reports of a bake batch's two calls, checked to hold polygons.
fn baked_reports(answer: &Value, context: &str) -> (Value, Value) {
    let results = answer["results"].as_array().expect("results");
    assert_eq!(results.len(), 2, "{context}: {answer}");
    for (result, class) in results.iter().zip(["NavigationMesh", "NavigationPolygon"]) {
        assert_eq!(result["value"], Value::Null, "{context}: {result}");
        assert_eq!(result["baked"]["class"], class, "{context}: {result}");
        assert!(
            result["baked"]["polygons"]
                .as_u64()
                .is_some_and(|polygons| polygons > 0),
            "{context}: the bake left polygons: {result}"
        );
    }
    assert_eq!(results[1]["baked"]["outlines"], 1, "{context}: {answer}");
    (results[0]["baked"].clone(), results[1]["baked"].clone())
}

/// The saved scene holds baked polygons in the navigation resources the two
/// regions hold. The editor saves its rebuilt copies under new ids, so each is
/// found through the node that holds it.
fn assert_bakes_saved(scene: &str, context: &str) {
    for (node, property) in [
        ("[node name=\"Region\" ", "navigation_mesh = SubResource(\""),
        (
            "[node name=\"Region2D\" ",
            "navigation_polygon = SubResource(\"",
        ),
    ] {
        let id = scene
            .split("\n\n")
            .find(|block| block.starts_with(node))
            .and_then(|block| block.lines().find_map(|line| line.strip_prefix(property)))
            .and_then(|rest| rest.split('"').next())
            .unwrap_or_else(|| panic!("{context}: {node} holds a sub-resource:\n{scene}"));
        let block: String = scene
            .split("[sub_resource")
            .find(|block| block.contains(&format!("id=\"{id}\"")))
            .unwrap_or_else(|| panic!("{context}: the scene keeps {id}:\n{scene}"))
            .to_owned();
        let polygons = block
            .lines()
            .find_map(|line| line.strip_prefix("polygons = "))
            .unwrap_or_default();
        assert!(
            polygons.contains("PackedInt32Array("),
            "{context}: {id} holds the baked polygons:\n{block}"
        );
    }
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn navigation_bakes_finish_before_the_batch_goes_on() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        navigation_bakes_finish_before_the_batch_goes_on_in(binary);
    }
}

fn navigation_bakes_finish_before_the_batch_goes_on_in(binary: &Path) {
    let context = binary.display().to_string();
    let scene = BAKE_SCENE.replace(
        "[node name=\"Linked\" type=\"NavigationRegion3D\" parent=\".\"]\n",
        "[node name=\"Linked\" type=\"NavigationRegion3D\" parent=\".\"]\nnavigation_mesh = ExtResource(\"2\")\n",
    )
    .replace(
        "[ext_resource type=\"Script\" path=\"res://level.gd\" id=\"1\"]\n",
        "[ext_resource type=\"Script\" path=\"res://level.gd\" id=\"1\"]\n[ext_resource type=\"NavigationMesh\" path=\"res://linked.tres\" id=\"2\"]\n",
    );
    let files = [
        ("main.tscn", scene.as_str()),
        ("level.gd", LEVEL_SCRIPT),
        ("linked.tres", LINKED_MESH),
    ];

    // Headless: the helper bakes in a script-free copy and saves the scene.
    let tree = ProjectTree::new("live-bake-headless");
    for (path, contents) in files {
        tree.write(path, contents);
    }
    let mut session = headless(&tree, binary);
    let applied = ok(
        &session.call_tool(
            2,
            "scene.batch.apply",
            json!({
                "path": "main.tscn",
                "expectedSha256": sha256_hex(scene.as_bytes()),
                "operations": bake_operations(),
                "idempotencyId": "bake-headless"
            }),
        ),
        &context,
    );
    assert_eq!(applied["backend"], "headless", "{applied}");
    let headless_reports = baked_reports(&applied, &format!("{context}, headless"));
    let saved = std::fs::read_to_string(tree.root.join("main.tscn")).expect("scene");
    assert_bakes_saved(&saved, &format!("{context}, headless"));
    assert!(
        !tree.root.join("ran.txt").exists(),
        "{context}: a headless bake ran no project code"
    );
    let saved_sha = sha256_hex(saved.as_bytes());
    for (operation, why) in [
        (
            json!({ "op": "call_method", "node": "Region", "method": "bake_navigation_mesh",
                    "arguments": [tagged("bool", json!(true))] }),
            "on_thread must be false",
        ),
        (
            json!({ "op": "call_method", "node": "Region", "method": "bake_navigation_mesh",
                    "repeat": [[], []] }),
            "`repeat` does not apply",
        ),
        (
            json!({ "op": "call_method", "node": "Bare", "method": "bake_navigation_mesh" }),
            "holds no navigation_mesh",
        ),
        (
            json!({ "op": "call_method", "node": "Linked", "method": "bake_navigation_mesh" }),
            "res://linked.tres",
        ),
    ] {
        let refused = session.call_tool(
            3,
            "scene.batch.apply",
            json!({
                "path": "main.tscn",
                "expectedSha256": saved_sha,
                "operations": [operation],
                "idempotencyId": format!("bake-refused-{why}")
            }),
        );
        let message = refusal(&refused);
        assert!(message.contains(why), "{context}: {why}: {message}");
    }
    assert_eq!(
        std::fs::read_to_string(tree.root.join("main.tscn")).expect("scene"),
        saved,
        "{context}: a refused bake changed nothing"
    );
    let _ = session.finish();

    // In the editor: one undoable action on a rebuilt copy of each resource.
    let (tree, mut session, mut ids) = project_with_addon("live-bake", &files);
    let editor = Editor::launch(binary, &tree.root, &tree.base.join("home"));
    wait_connected(&mut session, &mut ids, &editor);
    let mark = editor.mark();
    ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.open",
            json!({ "path": "main.tscn", "idempotencyId": "bake-open" }),
        ),
        &editor.context(),
    );
    let original = digest_of(&mut session, &mut ids, &editor.context());
    let edited = ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.edit",
            json!({
                "expectedDigest": original,
                "operations": bake_operations(),
                "label": "bake",
                "idempotencyId": "bake-edit"
            }),
        ),
        &editor.context(),
    );
    let editor_reports = baked_reports(&edited, &editor.context());
    assert_eq!(
        editor_reports, headless_reports,
        "{}: the editor and the helper bake the same geometry alike",
        editor.binary
    );
    let changed = edited["digest"].as_str().expect("digest").to_owned();
    assert_ne!(changed, original, "{edited}");
    for (tool, expected, why) in [
        (
            "editor.undo",
            &original,
            "undoing gives each region its original resource",
        ),
        (
            "editor.redo",
            &changed,
            "redoing gives each region its baked copy",
        ),
    ] {
        let stepped = ok(
            &session.call_tool(
                ids.next(),
                tool,
                json!({ "idempotencyId": format!("bake-{tool}") }),
            ),
            &editor.context(),
        );
        assert_eq!(
            stepped["digest"],
            json!(expected),
            "{}: {why}",
            editor.binary
        );
    }
    ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.save",
            json!({ "idempotencyId": "bake-save" }),
        ),
        &editor.context(),
    );
    let saved = std::fs::read_to_string(tree.root.join("main.tscn")).expect("scene");
    assert_bakes_saved(&saved, &format!("{}, editor", editor.binary));
    assert!(
        !tree.root.join("ran.txt").exists(),
        "{}: the editor runs no non-tool script",
        editor.binary
    );
    editor.assert_no_errors_since(mark, "baking, undoing, redoing and saving");
    let _ = session.finish();
    drop(editor);
}

const BUS_LAYOUT: &str =
    "[gd_resource type=\"AudioBusLayout\" format=3]\n\n[resource]\nbus/0/volume_db = 0.0\n";

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn a_bus_layout_gains_buses_and_effects_in_its_file() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        a_bus_layout_gains_buses_and_effects_in_its_file_in(binary);
    }
}

fn a_bus_layout_gains_buses_and_effects_in_its_file_in(binary: &Path) {
    let context = binary.display().to_string();
    let tree = ProjectTree::new("live-bus-layout");
    tree.write("default_bus_layout.tres", BUS_LAYOUT);
    let mut session = headless(&tree, binary);
    let operations = json!([
        { "op": "set_property", "property": "bus/1/name", "value": name("Music") },
        { "op": "set_property", "property": "bus/1/volume_db", "value": number(-6.0) },
        { "op": "set_property", "property": "bus/1/send", "value": name("Master") },
        { "op": "set_subresource", "property": "bus/1/effect/0/effect", "class": "AudioEffectReverb",
          "properties": { "room_size": number(0.6) } },
        { "op": "set_property", "property": "bus/1/effect/0/enabled", "value": tagged("bool", json!(true)) },
    ]);
    ok(
        &session.call_tool(
            2,
            "resource.batch.apply",
            json!({
                "path": "default_bus_layout.tres",
                "expectedSha256": sha256_hex(BUS_LAYOUT.as_bytes()),
                "operations": operations,
                "idempotencyId": "bus-layout"
            }),
        ),
        &context,
    );
    let saved = std::fs::read_to_string(tree.root.join("default_bus_layout.tres")).expect("layout");
    for line in [
        "bus/1/name = &\"Music\"",
        "bus/1/volume_db = -6.0",
        "bus/1/send = &\"Master\"",
        "bus/1/effect/0/enabled = true",
        "[sub_resource type=\"AudioEffectReverb\"",
        "room_size = 0.6",
    ] {
        assert!(
            saved.contains(line),
            "{context}: the layout holds {line}:\n{saved}"
        );
    }
    // A name that is no bus field is refused, and nothing is saved.
    let refused = session.call_tool(
        3,
        "resource.batch.apply",
        json!({
            "path": "default_bus_layout.tres",
            "expectedSha256": sha256_hex(saved.as_bytes()),
            "operations": [{ "op": "set_property", "property": "bus/1/loudness", "value": number(1.0) }],
            "idempotencyId": "bus-layout-refused"
        }),
    );
    let message = refusal(&refused);
    assert!(message.contains("bus/1/loudness"), "{context}: {message}");
    assert_eq!(
        std::fs::read_to_string(tree.root.join("default_bus_layout.tres")).expect("layout"),
        saved,
        "{context}: the refused batch changed nothing"
    );
    let _ = session.finish();
}

/// A scene with a receiver for each read: a tile layer whose cells a batch
/// paints, a line, a navigation region holding its polygon, an animation
/// player a batch gives an animation, and an animation tree whose state
/// machine 4.4.1 cannot list. Its script would write `ran.txt` if the scene
/// ever entered the tree.
const READ_SCENE: &str = r#"[gd_scene load_steps=7 format=3]

[ext_resource type="Script" path="res://level.gd" id="1"]

[sub_resource type="PlaceholderTexture2D" id="PlaceholderTexture2D_t"]
size = Vector2(32, 16)

[sub_resource type="TileSetAtlasSource" id="TileSetAtlasSource_s"]
texture = SubResource("PlaceholderTexture2D_t")
0:0/0 = 0
1:0/0 = 0

[sub_resource type="TileSet" id="TileSet_s"]
sources/0 = SubResource("TileSetAtlasSource_s")

[sub_resource type="NavigationPolygon" id="NavigationPolygon_n"]
vertices = PackedVector2Array(0, 0, 100, 0, 100, 100, 0, 100)
polygons = [PackedInt32Array(0, 1, 2, 3)]
outlines = [PackedVector2Array(0, 0, 100, 0, 100, 100, 0, 100)]

[sub_resource type="AnimationNodeStateMachine" id="AnimationNodeStateMachine_m"]

[node name="Level" type="Node2D"]
script = ExtResource("1")

[node name="Layer" type="TileMapLayer" parent="."]
tile_set = SubResource("TileSet_s")

[node name="Line" type="Line2D" parent="."]
points = PackedVector2Array(0, 0, 10, 0, 10, 10)

[node name="Region" type="NavigationRegion2D" parent="."]
navigation_polygon = SubResource("NavigationPolygon_n")

[node name="Sprite" type="Node2D" parent="."]

[node name="Player" type="AnimationPlayer" parent="."]

[node name="Tree" type="AnimationTree" parent="."]
tree_root = SubResource("AnimationNodeStateMachine_m")
"#;

const READ_SCRIPT: &str = "extends Node2D\n\n\nfunc _ready() -> void:\n\tvar file := FileAccess.open(\"res://ran.txt\", FileAccess.WRITE)\n\tfile.store_string(\"ran\")\n";

const READ_CURVE: &str = "[gd_resource type=\"Curve\" format=3]\n\n[resource]\n_data = [Vector2(0, 0), 0.0, 0.0, 0, 0, Vector2(1, 1), 0.0, 0.0, 0, 0]\npoint_count = 2\n";

/// The batch that paints two cells and gives the player an animation with
/// one keyed track.
fn read_setup() -> Vec<Value> {
    let walk = || json!({ "animation": "walk" });
    vec![
        json!({ "op": "call_method", "node": "Layer", "method": "set_cell",
                "arguments": [vector2i(1, 2), integer(0), vector2i(0, 0)] }),
        json!({ "op": "call_method", "node": "Layer", "method": "set_cell",
                "arguments": [vector2i(3, 4), integer(0), vector2i(1, 0)] }),
        json!({ "op": "call_method", "node": "Player", "method": "add_animation_library",
                "arguments": [tagged("string", json!("")), new_resource("AnimationLibrary", json!({}))] }),
        json!({ "op": "call_method", "node": "Player", "resource": { "library": "" },
                "method": "add_animation",
                "arguments": [tagged("string", json!("walk")), new_resource("Animation", json!({}))] }),
        json!({ "op": "call_method", "node": "Player", "resource": walk(), "method": "add_track",
                "arguments": [integer(0)] }),
        json!({ "op": "call_method", "node": "Player", "resource": walk(),
                "method": "track_set_path",
                "arguments": [integer(0), tagged("node_path", json!("Sprite:position"))] }),
        json!({ "op": "call_method", "node": "Player", "resource": walk(),
                "method": "track_insert_key",
                "arguments": [integer(0), number(0.5), vector2(4.0, 2.0)] }),
    ]
}

/// The version of Godot `binary` is, as `major.minor.patch`.
fn version_of(binary: &Path) -> String {
    let output = std::process::Command::new(binary)
        .args(["--headless", "--version"])
        .output()
        .expect("the editor reports its version");
    let text = String::from_utf8_lossy(&output.stdout);
    let mut parts = text.trim().split('.');
    let numbers: Vec<&str> = parts.by_ref().take(3).collect();
    match numbers.as_slice() {
        [major, minor, patch] if patch.chars().all(|c| c.is_ascii_digit()) => {
            format!("{major}.{minor}.{patch}")
        }
        [major, minor, ..] => format!("{major}.{minor}.0"),
        _ => panic!("unexpected version line {text}"),
    }
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn reads_answer_what_the_engine_holds() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        reads_answer_what_the_engine_holds_in(binary);
    }
}

fn reads_answer_what_the_engine_holds_in(binary: &Path) {
    let context = binary.display().to_string();
    let version = version_of(binary);
    let tree = ProjectTree::new("live-engine-reads");
    tree.write("level.tscn", READ_SCENE);
    tree.write("level.gd", READ_SCRIPT);
    tree.write("curve.tres", READ_CURVE);
    let mut session = headless(&tree, binary);
    ok(
        &session.call_tool(
            2,
            "scene.batch.apply",
            json!({
                "path": "level.tscn",
                "expectedSha256": sha256_hex(READ_SCENE.as_bytes()),
                "operations": read_setup(),
                "idempotencyId": "read-setup"
            }),
        ),
        &context,
    );
    let painted = std::fs::read(tree.root.join("level.tscn")).expect("scene");
    let walk = || json!({ "animation": "walk" });
    let answer = ok(
        &session.call_tool(
            3,
            "scene.read",
            json!({
                "path": "level.tscn",
                "calls": [
                    { "node": "Layer", "method": "get_used_cells" },
                    { "node": "Layer", "method": "get_cell_atlas_coords", "arguments": [vector2i(3, 4)] },
                    { "node": "Layer", "method": "get_cell_source_id", "arguments": [vector2i(9, 9)] },
                    { "node": "Line", "method": "get_point_count" },
                    { "node": "Player", "method": "get_animation_list" },
                    { "node": "Player", "resource": walk(), "method": "track_get_key_count", "arguments": [integer(0)] },
                    { "node": "Player", "resource": walk(), "method": "track_get_key_value", "arguments": [integer(0), integer(0)] },
                    { "node": "Player", "resource": walk(), "method": "track_get_key_count", "arguments": [integer(7)] },
                    { "node": "Player", "resource": { "animation": "run" }, "method": "get_length" },
                    { "node": "Region", "resource": { "property": "navigation_polygon" }, "method": "get_polygon_count" },
                    { "node": "Tree", "resource": { "property": "tree_root" }, "method": "get_node_list" },
                    { "node": "Tree", "resource": { "property": "tree_root" }, "method": "get_transition_count" },
                    { "node": "Layer", "method": "get_point_count" },
                    { "node": "Nope", "method": "get_point_count" }
                ]
            }),
        ),
        &context,
    );
    let calls = answer["calls"].as_array().expect("one answer per call");
    assert_eq!(calls.len(), 14, "{context}: {answer}");
    for (index, call) in calls.iter().enumerate() {
        assert_eq!(call["index"], json!(index), "{context}: {call}");
        assert_eq!(
            call["diagnostics_complete"],
            json!(true),
            "{context}: both markers of call {index} were captured: {call}"
        );
    }
    let mut cells: Vec<Value> = calls[0]["value"]["value"]
        .as_array()
        .unwrap_or_else(|| panic!("{context}: the painted cells: {}", calls[0]))
        .clone();
    cells.sort_by_key(|cell| cell["value"]["x"].as_i64());
    assert_eq!(
        cells,
        vec![vector2i(1, 2), vector2i(3, 4)],
        "{context}: {}",
        calls[0]
    );
    assert_eq!(calls[1]["value"], vector2i(1, 0), "{context}: {}", calls[1]);
    assert_eq!(
        calls[2]["value"],
        integer(-1),
        "{context}: an unpainted cell"
    );
    assert_eq!(calls[3]["value"], integer(3), "{context}: {}", calls[3]);
    let animations = &calls[4]["value"];
    assert!(
        animations.to_string().contains("walk"),
        "{context}: the player lists its animation: {animations}"
    );
    assert_eq!(calls[5]["value"], integer(1), "{context}: {}", calls[5]);
    assert_eq!(
        calls[6]["value"],
        vector2(4.0, 2.0),
        "{context}: {}",
        calls[6]
    );
    // An out-of-range track: the engine answers -1 and says why, and the
    // error is that call's own.
    assert_eq!(calls[7]["value"], integer(-1), "{context}: {}", calls[7]);
    let raised = calls[7]["diagnostics"].as_array().expect("diagnostics");
    assert!(
        raised
            .iter()
            .any(|diagnostic| diagnostic["severity"] == json!("error")),
        "{context}: the out-of-range track raised an engine error: {}",
        calls[7]
    );
    for (index, call) in calls.iter().enumerate().filter(|(index, _)| *index != 7) {
        assert_eq!(
            call["diagnostics"],
            json!([]),
            "{context}: call {index} raised nothing: {call}"
        );
    }
    assert_eq!(
        calls[8]["error"]["kind"],
        json!("not_found"),
        "{context}: {}",
        calls[8]
    );
    assert_eq!(calls[9]["value"], integer(1), "{context}: {}", calls[9]);
    if version == "4.4.1" {
        assert_eq!(
            calls[10]["error"]["kind"],
            json!("unsupported"),
            "{context}: 4.4.1 has no get_node_list: {}",
            calls[10]
        );
    } else {
        assert!(
            calls[10].get("error").is_none() && calls[10]["value"]["type"] == json!("array"),
            "{context}: {}",
            calls[10]
        );
    }
    assert_eq!(calls[11]["value"], integer(0), "{context}: {}", calls[11]);
    assert_eq!(
        calls[12]["error"]["kind"],
        json!("invalid_input"),
        "{context}: a TileMapLayer has no get_point_count: {}",
        calls[12]
    );
    assert_eq!(
        calls[13]["error"]["kind"],
        json!("not_found"),
        "{context}: {}",
        calls[13]
    );
    assert_eq!(answer["backend"], json!("headless"));
    assert_eq!(answer["sha256"], json!(sha256_hex(&painted)));
    assert_eq!(
        std::fs::read(tree.root.join("level.tscn")).expect("scene"),
        painted,
        "{context}: a read changes nothing"
    );
    assert!(
        !tree.root.join("ran.txt").exists(),
        "{context}: the scene never entered the tree, so its _ready never ran"
    );

    let curve = ok(
        &session.call_tool(
            4,
            "resource.read",
            json!({
                "path": "res://curve.tres",
                "calls": [
                    { "method": "get_point_count" },
                    { "method": "get_point_position", "arguments": [integer(1)] },
                    { "method": "sample", "arguments": [number(0.0)] }
                ]
            }),
        ),
        &context,
    );
    let calls = curve["calls"].as_array().expect("one answer per call");
    assert_eq!(calls[0]["value"], integer(2), "{context}: {curve}");
    assert_eq!(calls[1]["value"], vector2(1.0, 1.0), "{context}: {curve}");
    assert_eq!(calls[2]["value"], number(0.0), "{context}: {curve}");

    // A missing file is not found; nothing ran.
    let missing = session.call_tool(
        5,
        "scene.read",
        json!({ "path": "absent.tscn", "calls": [{ "method": "get_used_cells" }] }),
    );
    let message = refusal(&missing);
    assert!(message.contains("absent.tscn"), "{context}: {message}");
    let leftovers: Vec<_> = std::fs::read_dir(tree.root.join("godot_mcp_helpers/4/io"))
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.file_name())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        leftovers.is_empty(),
        "{context}: a read leaves no scratch document behind: {leftovers:?}"
    );
    let _ = session.finish();
}

/// The calls an editor read of the read scene makes, in this order.
fn editor_read_calls() -> Value {
    let walk = || json!({ "animation": "walk" });
    json!([
        { "node": "Layer", "method": "get_used_cells" },
        { "node": "Layer", "method": "get_cell_atlas_coords", "arguments": [vector2i(3, 4)] },
        { "node": "Line", "method": "get_point_count" },
        { "node": "Player", "method": "get_animation_list" },
        { "node": "Player", "resource": walk(), "method": "track_get_key_count", "arguments": [integer(0)] },
        { "node": "Player", "resource": walk(), "method": "track_get_key_value", "arguments": [integer(0), integer(0)] },
        { "node": "Player", "resource": walk(), "method": "track_get_key_count", "arguments": [integer(7)] },
        { "node": "Region", "resource": { "property": "navigation_polygon" }, "method": "get_polygon_count" },
        { "node": "Tree", "resource": { "property": "tree_root" }, "method": "get_node_list" },
        { "node": "Layer", "method": "get_point_count" },
        { "node": "Nope", "method": "get_point_count" }
    ])
}

/// The edited scene's digest, history version and unsaved state.
fn edited_state(
    session: &mut StdioSession,
    ids: &mut live::Ids,
    context: &str,
) -> (Value, Value, Value) {
    let tree = ok(
        &session.call_tool(ids.next(), "editor.scene.tree.get", json!({})),
        context,
    );
    (
        tree["digest"].clone(),
        tree["history"]["version"].clone(),
        tree["unsaved"].clone(),
    )
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn the_editor_reads_engine_values_as_it_holds_them() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        the_editor_reads_engine_values_as_it_holds_them_in(binary);
    }
}

fn the_editor_reads_engine_values_as_it_holds_them_in(binary: &Path) {
    let version = version_of(binary);
    let (tree, mut session, mut ids) = project_with_addon(
        "live-editor-reads",
        &[
            ("level.tscn", READ_SCENE),
            ("level.gd", READ_SCRIPT),
            ("curve.tres", READ_CURVE),
        ],
    );
    let editor = Editor::launch(binary, &tree.root, &tree.base.join("home"));
    wait_connected(&mut session, &mut ids, &editor);
    let context = editor.context();
    ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.open",
            json!({ "path": "level.tscn", "idempotencyId": "reads-open" }),
        ),
        &context,
    );
    let saved = std::fs::read(tree.root.join("level.tscn")).expect("scene");

    // Before any edit: nothing is painted, as in the file.
    let (original, _, _) = edited_state(&mut session, &mut ids, &context);
    let first = ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.read",
            json!({ "calls": [{ "node": "Layer", "method": "get_used_cells" }] }),
        ),
        &context,
    );
    assert_eq!(first["backend"], json!("editor"), "{context}: {first}");
    assert_eq!(
        first["scene"],
        json!("res://level.tscn"),
        "{context}: {first}"
    );
    assert_eq!(first["digest"], original, "{context}: {first}");
    assert_eq!(
        first["calls"][0]["value"],
        json!({ "type": "array", "value": [] }),
        "{context}: no cell is painted yet: {first}"
    );

    // An edit the editor holds and the file does not.
    let edited = ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.edit",
            json!({
                "expectedDigest": original,
                "operations": read_setup(),
                "label": "reads",
                "idempotencyId": "reads-edit"
            }),
        ),
        &context,
    );
    let changed = edited["digest"].clone();
    let (digest, history, unsaved) = edited_state(&mut session, &mut ids, &context);
    assert_eq!(digest, changed, "{context}");
    assert_eq!(unsaved, json!(true), "{context}: the edit is not saved");

    let mark = editor.mark();
    let answer = ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.read",
            json!({ "scene": "res://level.tscn", "calls": editor_read_calls() }),
        ),
        &context,
    );
    assert_eq!(answer["digest"], changed, "{context}: {answer}");
    assert_eq!(answer["unsaved"], json!(true), "{context}: {answer}");
    let calls = answer["calls"].as_array().expect("one answer per call");
    assert_eq!(calls.len(), 11, "{context}: {answer}");
    let logged = version != "4.4.1";
    for (index, call) in calls.iter().enumerate() {
        assert_eq!(call["index"], json!(index), "{context}: {call}");
        assert_eq!(
            call["diagnostics_complete"],
            json!(logged),
            "{context}: Godot {version} {} its log to the plugin: {call}",
            if logged { "hands" } else { "does not hand" }
        );
    }
    let mut cells: Vec<Value> = calls[0]["value"]["value"]
        .as_array()
        .unwrap_or_else(|| panic!("{context}: the painted cells: {}", calls[0]))
        .clone();
    cells.sort_by_key(|cell| cell["value"]["x"].as_i64());
    assert_eq!(
        cells,
        vec![vector2i(1, 2), vector2i(3, 4)],
        "{context}: the unsaved cells are read: {}",
        calls[0]
    );
    assert_eq!(calls[1]["value"], vector2i(1, 0), "{context}: {}", calls[1]);
    assert_eq!(calls[2]["value"], integer(3), "{context}: {}", calls[2]);
    assert!(
        calls[3]["value"].to_string().contains("walk"),
        "{context}: the unsaved animation is listed: {}",
        calls[3]
    );
    assert_eq!(calls[4]["value"], integer(1), "{context}: {}", calls[4]);
    assert_eq!(
        calls[5]["value"],
        vector2(4.0, 2.0),
        "{context}: {}",
        calls[5]
    );
    // An out-of-range track: the engine answers -1, and what it logged is
    // that call's diagnostic where the editor hands its log over.
    assert_eq!(calls[6]["value"], integer(-1), "{context}: {}", calls[6]);
    let raised = calls[6]["diagnostics"].as_array().expect("diagnostics");
    if logged {
        assert!(
            raised
                .iter()
                .any(|diagnostic| diagnostic["severity"] == json!("error")
                    && diagnostic["message"]
                        .as_str()
                        .is_some_and(|message| message.contains("out of bounds"))),
            "{context}: the out-of-range track raised an engine error: {}",
            calls[6]
        );
    } else {
        assert_eq!(raised, &Vec::<Value>::new(), "{context}: {}", calls[6]);
    }
    for (index, call) in calls.iter().enumerate().filter(|(index, _)| *index != 6) {
        assert_eq!(
            call["diagnostics"],
            json!([]),
            "{context}: call {index} raised nothing: {call}"
        );
    }
    assert_eq!(calls[7]["value"], integer(1), "{context}: {}", calls[7]);
    if version == "4.4.1" {
        assert_eq!(
            calls[8]["error"]["kind"],
            json!("unsupported"),
            "{context}: 4.4.1 has no get_node_list: {}",
            calls[8]
        );
    } else {
        assert!(
            calls[8].get("error").is_none() && calls[8]["value"]["type"] == json!("array"),
            "{context}: {}",
            calls[8]
        );
    }
    assert_eq!(
        calls[9]["error"]["kind"],
        json!("invalid_input"),
        "{context}: {}",
        calls[9]
    );
    assert_eq!(
        calls[10]["error"]["kind"],
        json!("not_found"),
        "{context}: {}",
        calls[10]
    );

    // The read changed nothing: the same digest, no undo step, the file as saved.
    let (digest_after, history_after, unsaved_after) =
        edited_state(&mut session, &mut ids, &context);
    assert_eq!(digest_after, changed, "{context}");
    assert_eq!(
        history_after, history,
        "{context}: a read adds no undo history"
    );
    assert_eq!(unsaved_after, json!(true), "{context}");
    assert_eq!(
        std::fs::read(tree.root.join("level.tscn")).expect("scene"),
        saved,
        "{context}: the file still holds no painted cell"
    );
    assert!(
        !tree.root.join("ran.txt").exists(),
        "{context}: the scene's script is not a tool script, so the editor never ran its _ready"
    );
    // The editor printed the one error the read raised, on every build: on
    // 4.4.1 the answer cannot carry it, which is why it says its calls' logs
    // are not complete.
    let printed = editor.output();
    let printed = printed.get(mark..).unwrap_or_default();
    assert_eq!(
        printed.matches("out of bounds").count(),
        1,
        "{}: the read raised exactly the out-of-range track's error:\n{printed}",
        editor.binary
    );

    // Undo puts the editor back, and a read follows it.
    ok(
        &session.call_tool(
            ids.next(),
            "editor.undo",
            json!({ "idempotencyId": "reads-undo" }),
        ),
        &context,
    );
    let undone = ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.read",
            json!({ "calls": [{ "node": "Layer", "method": "get_used_cells" }] }),
        ),
        &context,
    );
    assert_eq!(undone["digest"], original, "{context}: {undone}");
    assert_eq!(
        undone["calls"][0]["value"],
        json!({ "type": "array", "value": [] }),
        "{context}: {undone}"
    );

    // A scene that is not open is not read.
    let closed = session.call_tool(
        ids.next(),
        "editor.scene.read",
        json!({ "scene": "res://absent.tscn", "calls": [{ "method": "get_used_cells" }] }),
    );
    let message = refusal(&closed);
    assert!(message.contains("absent.tscn"), "{context}: {message}");
    drop(editor);
    let _ = session.finish();
}

const GOOD_SHADER: &str = "shader_type spatial;\nuniform vec4 tint : source_color = vec4(1.0);\nuniform float strength : hint_range(0.0, 2.0) = 1.0;\nuniform sampler2D albedo_tex;\n\nvoid fragment() {\n\tALBEDO = tint.rgb * strength;\n}\n";
const BROKEN_SHADER: &str = "shader_type canvas_item;\n#include \"res://helpers.gdshaderinc\"\nuniform float speed = 1.0;\n\nvoid fragment() {\n\tCOLOR = vec4(undefined_name, helper(speed), 0.0, 1.0);\n}\n";
const BROKEN_INCLUDE: &str = "float helper(float x) {\n\treturn x * missing_factor;\n}\n";

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn shaders_are_checked_by_the_engine() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        shaders_are_checked_by_the_engine_in(binary);
    }
}

fn shaders_are_checked_by_the_engine_in(binary: &Path) {
    let context = binary.display().to_string();
    let tree = ProjectTree::new("live-shader-check");
    tree.write("fx/glow.gdshader", GOOD_SHADER);
    tree.write("fx/broken.gdshader", BROKEN_SHADER);
    tree.write("helpers.gdshaderinc", BROKEN_INCLUDE);
    let mut session = headless(&tree, binary);
    let good = ok(
        &session.call_tool(
            2,
            "shader.check",
            json!({ "path": "res://fx/glow.gdshader" }),
        ),
        &context,
    );
    assert_eq!(good["compiled"], json!(true), "{context}: {good}");
    assert_eq!(good["mode"], json!("spatial"), "{context}: {good}");
    assert_eq!(good["errors"], json!([]), "{context}: {good}");
    let names: Vec<(&str, &str)> = good["uniforms"]
        .as_array()
        .expect("uniforms")
        .iter()
        .map(|uniform| {
            (
                uniform["name"].as_str().unwrap_or_default(),
                uniform["type"].as_str().unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(
        names,
        vec![
            ("tint", "Color"),
            ("strength", "float"),
            ("albedo_tex", "Object")
        ],
        "{context}: {good}"
    );
    assert_eq!(
        good["uniforms"][1]["hint_string"],
        json!("0.0,2.0,0.0010000000475")
    );

    let broken = ok(
        &session.call_tool(3, "shader.check", json!({ "path": "fx/broken.gdshader" })),
        &context,
    );
    assert_eq!(broken["compiled"], json!(false), "{context}: {broken}");
    assert_eq!(broken["mode"], json!("canvas_item"), "{context}: {broken}");
    let errors = broken["errors"].as_array().expect("errors");
    assert!(
        !errors.is_empty()
            && errors
                .iter()
                .all(|error| error["line"].is_u64() && error["file"].is_string()),
        "{context}: every error names its file and line: {broken}"
    );
    // Godot reports the first error it meets, in the include or the shader.
    let first = &errors[0];
    assert!(
        (first["file"] == json!("res://helpers.gdshaderinc") && first["line"] == json!(2))
            || (first["file"] == json!("res://fx/broken.gdshader") && first["line"] == json!(6)),
        "{context}: {first}"
    );
    assert_eq!(broken["uniforms"], json!([]), "{context}: {broken}");

    let refused = session.call_tool(4, "shader.check", json!({ "path": "helpers.gdshaderinc" }));
    let message = refusal(&refused);
    assert!(message.contains(".gdshader"), "{context}: {message}");
    let _ = session.finish();
}

const BATCH_MAIN: &str = "[gd_scene format=3]\n\n[node name=\"Main\" type=\"Node2D\"]\n\n[node name=\"Player\" type=\"Node2D\" parent=\".\"]\n";
const BATCH_LEVEL: &str = "[gd_scene format=3]\n\n[node name=\"Level\" type=\"Node3D\"]\n";
const BATCH_THEME: &str = "[gd_resource type=\"Theme\" format=3]\n\n[resource]\n";

/// The three files of the batch, the theme in a directory of its own so a
/// case can make its publication fail.
const BATCH_FILES: [(&str, &str); 3] = [
    ("scenes/main.tscn", BATCH_MAIN),
    ("scenes/level.tscn", BATCH_LEVEL),
    ("ui/theme.tres", BATCH_THEME),
];

fn batch_files(player: &str, sun_property: &str, theme_sha: &str) -> Value {
    json!([
        {
            "kind": "scene",
            "path": "scenes/main.tscn",
            "expectedSha256": sha256_hex(BATCH_MAIN.as_bytes()),
            "operations": [
                { "op": "rename_node", "node": player, "name": "Hero" },
                { "op": "add_node", "parent": ".", "name": "Camera", "type": "Camera2D" },
            ],
        },
        {
            "kind": "scene",
            "path": "scenes/level.tscn",
            "expectedSha256": sha256_hex(BATCH_LEVEL.as_bytes()),
            "operations": [
                { "op": "add_node", "parent": ".", "name": "Sun", "type": "DirectionalLight3D" },
                { "op": "set_property", "node": "Sun", "property": sun_property, "value": number(2.0) },
            ],
        },
        {
            "kind": "resource",
            "path": "ui/theme.tres",
            "expectedSha256": theme_sha,
            "operations": [
                { "op": "call_method", "method": "set_color",
                  "arguments": [name("font_color"), name("Label"), red()] },
            ],
        },
    ])
}

fn assert_batch_untouched(tree: &ProjectTree, context: &str) {
    for (path, original) in BATCH_FILES {
        assert_eq!(
            std::fs::read_to_string(tree.root.join(path)).expect("batch file"),
            original,
            "{context}: {path} still holds its original bytes"
        );
    }
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn several_files_land_together_or_not_at_all() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        several_files_land_together_or_not_at_all_in(binary);
    }
}

fn several_files_land_together_or_not_at_all_in(binary: &Path) {
    let context = binary.display().to_string();
    let tree = ProjectTree::new("live-batches");
    for (path, contents) in BATCH_FILES {
        tree.write(path, contents);
    }
    let theme_sha = sha256_hex(BATCH_THEME.as_bytes());
    let mut session = headless(&tree, binary);
    let read = ok(
        &session.call_tool(2, "scene.tree.get", json!({ "scene": "scenes/main.tscn" })),
        &context,
    );
    let player = read["data"]["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .find(|node| node["path"] == "Player")
        .and_then(|node| node["handle"].as_str())
        .expect("Player carries a handle")
        .to_owned();

    // A dry run applies every batch in the engine and answers each file's
    // diff, and writes nothing.
    let dry = ok(
        &session.call_tool(
            3,
            "project.batch.apply",
            json!({
                "files": batch_files(&player, "light_energy", &theme_sha),
                "dryRun": true,
                "idempotencyId": "batches-dry"
            }),
        ),
        &context,
    );
    let answered = dry["files"].as_array().expect("files");
    assert_eq!(answered.len(), 3, "{context}: {dry}");
    for (file, (path, _)) in answered.iter().zip(BATCH_FILES) {
        assert_eq!(file["path"], path, "{context}");
        assert_eq!(file["changed"], json!(true), "{context}: {file}");
    }
    assert!(
        answered[0]["diff"].to_string().contains("Hero"),
        "{context}: {}",
        answered[0]["diff"]
    );
    assert_batch_untouched(&tree, &context);

    // An operation the engine refuses in one file refuses every file, and the
    // refusal names that file.
    let refused = session.call_tool(
        4,
        "project.batch.apply",
        json!({
            "files": batch_files(&player, "no_such_property", &theme_sha),
            "idempotencyId": "batches-refused"
        }),
    );
    let message = refusal(&refused);
    assert!(
        message.contains("scenes/level.tscn"),
        "{context}: {message}"
    );
    assert_batch_untouched(&tree, &context);

    // A stale hash for one file refuses before any engine runs.
    let stale = session.call_tool(
        5,
        "project.batch.apply",
        json!({
            "files": batch_files(&player, "light_energy", &"0".repeat(64)),
            "idempotencyId": "batches-stale"
        }),
    );
    let message = refusal(&stale);
    assert!(message.contains("ui/theme.tres"), "{context}: {message}");
    assert_batch_untouched(&tree, &context);

    // When the last file cannot be written, the files already written are put
    // back and the transaction ends compensated, leaving writes open.
    let locked = tree.root.join("ui");
    set_mode(&locked, 0o555);
    let probe = locked.join("probe");
    let enforced = std::fs::write(&probe, b"").is_err();
    let _ = std::fs::remove_file(&probe);
    if enforced {
        let undone = session.call_tool(
            6,
            "project.batch.apply",
            json!({
                "files": batch_files(&player, "light_energy", &theme_sha),
                "idempotencyId": "batches-undone"
            }),
        );
        let message = refusal(&undone);
        set_mode(&locked, 0o755);
        assert!(
            message.contains("every file still holds its prior state")
                && message.contains("undid the changes already made to 2 files"),
            "{context}: {message}"
        );
        assert_batch_untouched(&tree, &context);
    } else {
        // A privileged user writes into a read-only directory anyway.
        set_mode(&locked, 0o755);
        eprintln!("{context}: read-only directories are not enforced here; undo case skipped");
    }

    // Applied for real, every file lands in one committed transaction.
    let arguments = json!({
        "files": batch_files(&player, "light_energy", &theme_sha),
        "idempotencyId": "batches-applied"
    });
    let applied = ok(
        &session.call_tool(7, "project.batch.apply", arguments.clone()),
        &context,
    );
    assert_eq!(
        applied["conclusion"],
        json!("proven_post_state"),
        "{context}: {applied}"
    );
    for file in applied["files"].as_array().expect("files") {
        let path = file["path"].as_str().expect("path");
        let saved = std::fs::read(tree.root.join(path)).expect("saved file");
        assert_eq!(
            file["after_sha256"],
            json!(sha256_hex(&saved)),
            "{context}: {path}"
        );
    }
    let main = std::fs::read_to_string(tree.root.join("scenes/main.tscn")).expect("main");
    assert!(
        main.contains("[node name=\"Hero\" type=\"Node2D\" parent=\".\"")
            && main.contains("[node name=\"Camera\" type=\"Camera2D\" parent=\".\"")
            && !main.contains("Player"),
        "{context}: the handle followed Player through its rename:\n{main}"
    );
    let level = std::fs::read_to_string(tree.root.join("scenes/level.tscn")).expect("level");
    assert!(
        level.contains("[node name=\"Sun\" type=\"DirectionalLight3D\" parent=\".\"")
            && level.contains("light_energy = 2.0"),
        "{context}:\n{level}"
    );
    let theme = std::fs::read_to_string(tree.root.join("ui/theme.tres")).expect("theme");
    assert!(
        theme.contains("Label/colors/font_color = Color(1, 0, 0, 1)"),
        "{context}:\n{theme}"
    );

    // The same call again is answered from the journal and changes nothing.
    let replayed = ok(
        &session.call_tool(8, "project.batch.apply", arguments),
        &context,
    );
    assert_eq!(replayed["replayed"], json!(true), "{context}: {replayed}");
    assert_eq!(
        std::fs::read_to_string(tree.root.join("scenes/main.tscn")).expect("main"),
        main,
        "{context}"
    );
    let _ = session.finish();
}

fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .expect("change directory permissions");
}

const PLAYER_SCENE: &str = "[gd_scene format=3]\n\n[node name=\"Main\" type=\"Node2D\"]\n\n[node name=\"Steps\" type=\"AudioStreamPlayer2D\" parent=\".\"]\n";

/// A write whose value the engine replaces: a player keeps `Master` for a bus
/// the project's layout lacks.
fn unkept_bus() -> Value {
    json!([{ "op": "set_property", "node": "Steps", "property": "bus",
             "value": tagged("string_name", json!("SFX")) }])
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn a_value_the_engine_does_not_keep_is_refused() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        a_value_the_engine_does_not_keep_is_refused_in(binary);
    }
}

fn a_value_the_engine_does_not_keep_is_refused_in(binary: &Path) {
    let context = binary.display().to_string();

    // Headless: the helper reads the value back and refuses the batch.
    let tree = ProjectTree::new("live-unkept");
    tree.write("main.tscn", PLAYER_SCENE);
    let mut session = headless(&tree, binary);
    let refused = session.call_tool(
        2,
        "scene.batch.apply",
        json!({
            "path": "main.tscn",
            "expectedSha256": sha256_hex(PLAYER_SCENE.as_bytes()),
            "operations": unkept_bus(),
            "idempotencyId": "unkept-headless"
        }),
    );
    let message = refusal(&refused);
    assert!(
        message.contains("value_not_kept") && message.contains("Master"),
        "{context}: {message}"
    );
    assert_eq!(
        std::fs::read_to_string(tree.root.join("main.tscn")).expect("scene"),
        PLAYER_SCENE,
        "{context}: nothing is written"
    );
    let _ = session.finish();

    // In the editor: the same refusal, and the scene stays as it was.
    let (tree, mut session, mut ids) =
        project_with_addon("live-unkept", &[("main.tscn", PLAYER_SCENE)]);
    let editor = Editor::launch(binary, &tree.root, &tree.base.join("home"));
    wait_connected(&mut session, &mut ids, &editor);
    ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.open",
            json!({ "path": "main.tscn", "idempotencyId": "unkept-open" }),
        ),
        &editor.context(),
    );
    let original = digest_of(&mut session, &mut ids, &editor.context());
    let refused = session.call_tool(
        ids.next(),
        "editor.scene.edit",
        json!({
            "expectedDigest": original,
            "operations": unkept_bus(),
            "label": "unkept",
            "idempotencyId": "unkept-edit"
        }),
    );
    let message = refusal(&refused);
    assert!(
        message.contains("value_not_kept") && message.contains("Master"),
        "{}: {message}",
        editor.binary
    );
    assert_eq!(
        digest_of(&mut session, &mut ids, &editor.context()),
        original,
        "{}: the editor's scene is unchanged",
        editor.binary
    );
    let _ = session.finish();
    drop(editor);
}

/// A scene for the structural operations: a scripted root, a body with a
/// child, a scripted pickup in a group with a connection to the root, a label
/// under a canvas layer and an instanced marker.
const STRUCTURE_SCENE: &str = r#"[gd_scene load_steps=4 format=3]

[ext_resource type="Script" path="res://main.gd" id="1_main"]
[ext_resource type="Script" path="res://coin.gd" id="2_coin"]
[ext_resource type="PackedScene" path="res://spawn_a.tscn" id="3_a"]

[node name="Main" type="Node2D"]
script = ExtResource("1_main")

[node name="Player" type="CharacterBody2D" parent="."]

[node name="Sprite" type="Sprite2D" parent="Player"]

[node name="Coin" type="Area2D" parent="." groups=["pickups"]]
script = ExtResource("2_coin")

[node name="Hud" type="CanvasLayer" parent="."]

[node name="Score" type="Label" parent="Hud"]
text = "0"

[node name="Spawn" parent="." instance=ExtResource("3_a")]

[connection signal="area_entered" from="Coin" to="." method="_on_coin_area_entered"]
"#;

const STRUCTURE_FILES: [(&str, &str); 5] = [
    (
        "main.gd",
        "extends Node2D\n\n\nfunc _on_coin_area_entered(_area: Area2D) -> void:\n\tpass\n",
    ),
    ("coin.gd", "extends Area2D\n"),
    (
        "player.gd",
        "extends CharacterBody2D\n\n\nfunc _on_coin_body_entered(_body: Node2D) -> void:\n\tpass\n",
    ),
    (
        "spawn_a.tscn",
        "[gd_scene format=3]\n\n[node name=\"SpawnA\" type=\"Marker2D\"]\n",
    ),
    (
        "spawn_b.tscn",
        "[gd_scene format=3]\n\n[node name=\"SpawnB\" type=\"Node2D\"]\n",
    ),
];

/// Every structural operation once: the copy is taken before the original
/// loses its script, group and connection.
fn structure_operations() -> Value {
    json!([
        { "op": "duplicate_node", "node": "Coin", "name": "Coin2" },
        { "op": "move_node", "node": "Hud/Score", "parent": "Player" },
        { "op": "attach_script", "node": "Player", "script": "res://player.gd" },
        { "op": "connect_signal", "from": "Coin", "signal": "body_entered", "to": "Player",
          "method": "_on_coin_body_entered" },
        { "op": "add_group", "node": "Player", "group": "players" },
        { "op": "remove_group", "node": "Coin", "group": "pickups" },
        { "op": "disconnect_signal", "from": "Coin", "signal": "area_entered", "to": ".",
          "method": "_on_coin_area_entered" },
        { "op": "detach_script", "node": "Coin" },
        { "op": "replace_instance", "node": "Spawn", "scene": "res://spawn_b.tscn" }
    ])
}

/// The header line of the node named `name` and the property lines under it.
fn node_section<'a>(saved: &'a str, name: &str, context: &str) -> (&'a str, Vec<&'a str>) {
    let header_prefix = format!("[node name=\"{name}\"");
    let mut lines = saved.lines();
    let header = lines
        .by_ref()
        .find(|line| line.starts_with(&header_prefix))
        .unwrap_or_else(|| panic!("{context}: no node {name}:\n{saved}"));
    let body = lines.take_while(|line| !line.starts_with('[')).collect();
    (header, body)
}

/// What [`structure_operations`] leave in a saved scene.
fn assert_structure_landed(saved: &str, context: &str) {
    let scripted = |body: &[&str]| {
        body.iter()
            .any(|line| line.starts_with("script = ExtResource("))
    };
    let (player, player_body) = node_section(saved, "Player", context);
    assert!(
        player.contains("groups=[\"players\"]"),
        "{context}: {player}"
    );
    assert!(scripted(&player_body), "{context}: Player has its script");
    assert!(saved.contains("path=\"res://player.gd\""), "{context}");
    let (score, _) = node_section(saved, "Score", context);
    assert!(
        score.starts_with("[node name=\"Score\" type=\"Label\" parent=\"Player\""),
        "{context}: {score}"
    );
    let (coin, coin_body) = node_section(saved, "Coin", context);
    assert!(!coin.contains("groups="), "{context}: {coin}");
    assert!(!scripted(&coin_body), "{context}: Coin lost its script");
    let (copy, copy_body) = node_section(saved, "Coin2", context);
    assert!(
        copy.starts_with("[node name=\"Coin2\" type=\"Area2D\" parent=\".\"")
            && copy.contains("groups=[\"pickups\"]"),
        "{context}: {copy}"
    );
    assert!(scripted(&copy_body), "{context}: the copy kept the script");
    for (present, line) in [
        (
            true,
            "[connection signal=\"body_entered\" from=\"Coin\" to=\"Player\" method=\"_on_coin_body_entered\"]",
        ),
        (
            true,
            "[connection signal=\"area_entered\" from=\"Coin2\" to=\".\" method=\"_on_coin_area_entered\"]",
        ),
        (
            false,
            "[connection signal=\"area_entered\" from=\"Coin\" to=\".\" method=\"_on_coin_area_entered\"]",
        ),
    ] {
        assert_eq!(
            saved.lines().any(|held| held == line),
            present,
            "{context}: {line}\n{saved}"
        );
    }
    let (spawn, _) = node_section(saved, "Spawn", context);
    assert!(
        // 4.7.1 writes a `unique_id` between the parent and the instance.
        spawn.starts_with("[node name=\"Spawn\" parent=\".\"")
            && spawn.contains(" instance=ExtResource("),
        "{context}: {spawn}"
    );
    assert!(
        saved.contains("path=\"res://spawn_b.tscn\"") && !saved.contains("spawn_a.tscn"),
        "{context}: the instance was replaced:\n{saved}"
    );
}

/// The connections the editor reports for `signal` of `node`, as
/// `(receiver, method)` pairs.
fn connections_of(
    session: &mut StdioSession,
    ids: &mut live::Ids,
    node: &str,
    signal: &str,
    context: &str,
) -> Vec<(Value, Value)> {
    let read = ok(
        &session.call_tool(ids.next(), "editor.node.get", json!({ "node": node })),
        context,
    );
    read["signals"]
        .as_array()
        .expect("signals")
        .iter()
        .find(|entry| entry["name"] == json!(signal))
        .unwrap_or_else(|| panic!("{context}: {node} has no {signal}: {read}"))["connections"]
        .as_array()
        .expect("connections")
        .iter()
        .map(|connection| (connection["to"].clone(), connection["method"].clone()))
        .collect()
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn structural_operations_land_in_the_editor_and_headless() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        structural_operations_land_in(binary);
    }
}

fn structural_operations_land_in(binary: &Path) {
    let context = binary.display().to_string();
    let mut files: Vec<(&str, &str)> = STRUCTURE_FILES.to_vec();
    files.push(("main.tscn", STRUCTURE_SCENE));

    // Headless: the helper moves, copies, connects and replaces nodes.
    let tree = ProjectTree::new("live-structure-headless");
    for (path, text) in &files {
        tree.write(path, text);
    }
    let mut session = headless(&tree, binary);
    let applied = ok(
        &session.call_tool(
            2,
            "scene.batch.apply",
            json!({
                "path": "main.tscn",
                "expectedSha256": sha256_hex(STRUCTURE_SCENE.as_bytes()),
                "operations": structure_operations(),
                "idempotencyId": "structure-headless"
            }),
        ),
        &context,
    );
    assert_eq!(applied["backend"], "headless", "{applied}");
    let headless_results = applied["results"].clone();
    let saved = std::fs::read_to_string(tree.root.join("main.tscn")).expect("scene");
    assert_structure_landed(&saved, &format!("{context}, headless"));
    let _ = session.finish();

    // In the editor: one undoable action, saved and checked the same way.
    let (tree, mut session, mut ids) = project_with_addon("live-structure", &files);
    let editor = Editor::launch(binary, &tree.root, &tree.base.join("home"));
    wait_connected(&mut session, &mut ids, &editor);
    let mark = editor.mark();
    ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.open",
            json!({ "path": "main.tscn", "idempotencyId": "structure-open" }),
        ),
        &editor.context(),
    );
    let original = digest_of(&mut session, &mut ids, &editor.context());
    assert_eq!(
        connections_of(
            &mut session,
            &mut ids,
            "Coin",
            "area_entered",
            &editor.context()
        ),
        [(json!("."), json!("_on_coin_area_entered"))],
        "the editor reports the scene's connection"
    );
    let edited = ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.edit",
            json!({
                "expectedDigest": original,
                "operations": structure_operations(),
                "label": "structure",
                "idempotencyId": "structure-edit"
            }),
        ),
        &editor.context(),
    );
    assert_eq!(
        edited["results"], headless_results,
        "{}: both backends answer the same results",
        editor.binary
    );
    let changed = edited["digest"].as_str().expect("digest").to_owned();
    assert_ne!(changed, original, "{edited}");
    for (signal, expected) in [
        ("area_entered", vec![]),
        (
            "body_entered",
            vec![(json!("Player"), json!("_on_coin_body_entered"))],
        ),
    ] {
        assert_eq!(
            connections_of(&mut session, &mut ids, "Coin", signal, &editor.context()),
            expected,
            "{}: {signal} after the edit",
            editor.binary
        );
    }
    for (tool, expected) in [("editor.undo", &original), ("editor.redo", &changed)] {
        let stepped = ok(
            &session.call_tool(
                ids.next(),
                tool,
                json!({ "idempotencyId": format!("structure-{tool}") }),
            ),
            &editor.context(),
        );
        assert_eq!(
            stepped["digest"],
            json!(expected),
            "{}: {tool}",
            editor.binary
        );
    }
    ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.save",
            json!({ "idempotencyId": "structure-save" }),
        ),
        &editor.context(),
    );
    let saved = std::fs::read_to_string(tree.root.join("main.tscn")).expect("scene");
    assert_structure_landed(&saved, &format!("{}, editor", editor.binary));
    editor.assert_no_errors_since(
        mark,
        "moving, copying, scripting, connecting, grouping and replacing nodes",
    );
    let _ = session.finish();
    drop(editor);
}

/// A scene whose state machine holds two states joined by a transition, with
/// an empty blend tree, an animation player and a painted tile layer.
const GRAPH_SCENE: &str = r#"[gd_scene format=3]

[sub_resource type="AnimationNodeAnimation" id="AnimationNodeAnimation_i"]
animation = &"idle"

[sub_resource type="AnimationNodeAnimation" id="AnimationNodeAnimation_r"]
animation = &"run"

[sub_resource type="AnimationNodeStateMachineTransition" id="AnimationNodeStateMachineTransition_t"]

[sub_resource type="AnimationNodeStateMachine" id="AnimationNodeStateMachine_s"]
states/Idle/node = SubResource("AnimationNodeAnimation_i")
states/Idle/position = Vector2(100, 100)
states/Run/node = SubResource("AnimationNodeAnimation_r")
states/Run/position = Vector2(300, 100)
transitions = ["Idle", "Run", SubResource("AnimationNodeStateMachineTransition_t")]

[sub_resource type="AnimationNodeBlendTree" id="AnimationNodeBlendTree_b"]

[sub_resource type="PlaceholderTexture2D" id="PlaceholderTexture2D_t"]
size = Vector2(32, 16)

[sub_resource type="TileSetAtlasSource" id="TileSetAtlasSource_a"]
texture = SubResource("PlaceholderTexture2D_t")
0:0/0 = 0

[sub_resource type="TileSet" id="TileSet_t"]
sources/0 = SubResource("TileSetAtlasSource_a")

[node name="Main" type="Node2D"]

[node name="Sprite" type="Sprite2D" parent="."]

[node name="Player" type="AnimationPlayer" parent="."]

[node name="Machine" type="AnimationTree" parent="."]
tree_root = SubResource("AnimationNodeStateMachine_s")

[node name="Blend" type="AnimationTree" parent="."]
tree_root = SubResource("AnimationNodeBlendTree_b")

[node name="Ground" type="TileMapLayer" parent="."]
tile_set = SubResource("TileSet_t")
tile_map_data = PackedByteArray(0, 0, 3, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0)
"#;

/// Removals and connections on graph resources, an animation library and a
/// tile layer: a library gains two animations and loses one, a key's value is
/// replaced, the state machine loses a state and its transition, the blend
/// tree gains a connected node, and the layer is cleared.
fn graph_operations() -> Value {
    let call = |node: &str, resource: Option<Value>, method: &str, arguments: Vec<Value>| {
        let mut operation =
            json!({ "op": "call_method", "node": node, "method": method, "arguments": arguments });
        if let Some(resource) = resource {
            operation["resource"] = resource;
        }
        operation
    };
    let library = || Some(json!({ "library": "" }));
    let walk = || Some(json!({ "animation": "walk" }));
    let machine = || Some(json!({ "property": "tree_root" }));
    json!([
        call(
            "Player",
            None,
            "add_animation_library",
            vec![name(""), new_resource("AnimationLibrary", json!({}))]
        ),
        call(
            "Player",
            library(),
            "add_animation",
            vec![
                name("walk"),
                new_resource("Animation", json!({ "length": number(1.0) }))
            ]
        ),
        call(
            "Player",
            library(),
            "add_animation",
            vec![name("run"), new_resource("Animation", json!({}))]
        ),
        call("Player", walk(), "add_track", vec![integer(0)]),
        call(
            "Player",
            walk(),
            "track_set_path",
            vec![integer(0), tagged("node_path", json!("Sprite:position"))]
        ),
        call(
            "Player",
            walk(),
            "track_insert_key",
            vec![integer(0), number(0.0), vector2(1.0, 1.0)]
        ),
        call(
            "Player",
            walk(),
            "track_set_key_value",
            vec![integer(0), integer(0), vector2(2.0, 2.0)]
        ),
        call("Player", library(), "remove_animation", vec![name("run")]),
        call(
            "Machine",
            machine(),
            "remove_transition",
            vec![name("Idle"), name("Run")]
        ),
        call("Machine", machine(), "remove_node", vec![name("Run")]),
        call(
            "Blend",
            machine(),
            "add_node",
            vec![
                name("Anim"),
                new_resource(
                    "AnimationNodeAnimation",
                    json!({ "animation": name("walk") })
                ),
                vector2(200.0, 100.0),
            ]
        ),
        call(
            "Blend",
            machine(),
            "connect_node",
            vec![name("output"), integer(0), name("Anim")]
        ),
        call("Ground", None, "clear", vec![]),
    ])
}

/// What [`graph_operations`] leave in a saved scene.
fn assert_graph_landed(saved: &str, context: &str) {
    for marker in [
        "states/Idle/node = SubResource(",
        "nodes/Anim/node = SubResource(",
        "node_connections = [&\"output\", 0, &\"Anim\"]",
        "&\"walk\"",
        "\"values\": [Vector2(2, 2)]",
    ] {
        assert!(saved.contains(marker), "{context}: {marker}\n{saved}");
    }
    assert!(
        !saved.lines().any(|line| line.starts_with("states/Run/")),
        "{context}: the Run state is gone\n{saved}"
    );
    assert!(
        !saved.contains("&\"run\""),
        "{context}: neither the run animation nor the Run state's node is left\n{saved}"
    );
    assert!(
        !saved
            .lines()
            .any(|line| line.starts_with("transitions = [") && line != "transitions = []"),
        "{context}: the transition is gone\n{saved}"
    );
    let cells = saved
        .lines()
        .find(|line| line.starts_with("tile_map_data = PackedByteArray("))
        .and_then(live::packed_bytes);
    assert!(
        cells.is_none_or(|bytes| bytes.len() <= 2),
        "{context}: the layer holds no cell\n{saved}"
    );
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn graph_and_library_removals_land_in_the_editor_and_headless() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        graph_and_library_removals_land_in(binary);
    }
}

fn graph_and_library_removals_land_in(binary: &Path) {
    let context = binary.display().to_string();
    let tree = ProjectTree::new("live-graph-headless");
    tree.write("main.tscn", GRAPH_SCENE);
    let mut session = headless(&tree, binary);
    let applied = ok(
        &session.call_tool(
            2,
            "scene.batch.apply",
            json!({
                "path": "main.tscn",
                "expectedSha256": sha256_hex(GRAPH_SCENE.as_bytes()),
                "operations": graph_operations(),
                "idempotencyId": "graph-headless"
            }),
        ),
        &context,
    );
    assert_eq!(applied["backend"], "headless", "{applied}");
    let headless_results = applied["results"].clone();
    let saved = std::fs::read_to_string(tree.root.join("main.tscn")).expect("scene");
    assert_graph_landed(&saved, &format!("{context}, headless"));
    let _ = session.finish();

    let (tree, mut session, mut ids) =
        project_with_addon("live-graph", &[("main.tscn", GRAPH_SCENE)]);
    let editor = Editor::launch(binary, &tree.root, &tree.base.join("home"));
    wait_connected(&mut session, &mut ids, &editor);
    let mark = editor.mark();
    ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.open",
            json!({ "path": "main.tscn", "idempotencyId": "graph-open" }),
        ),
        &editor.context(),
    );
    let original = digest_of(&mut session, &mut ids, &editor.context());
    let edited = ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.edit",
            json!({
                "expectedDigest": original,
                "operations": graph_operations(),
                "label": "graph",
                "idempotencyId": "graph-edit"
            }),
        ),
        &editor.context(),
    );
    assert_eq!(
        edited["results"], headless_results,
        "{}: both backends answer the same results",
        editor.binary
    );
    let changed = edited["digest"].as_str().expect("digest").to_owned();
    for (tool, expected) in [("editor.undo", &original), ("editor.redo", &changed)] {
        let stepped = ok(
            &session.call_tool(
                ids.next(),
                tool,
                json!({ "idempotencyId": format!("graph-{tool}") }),
            ),
            &editor.context(),
        );
        assert_eq!(
            stepped["digest"],
            json!(expected),
            "{}: {tool}",
            editor.binary
        );
    }
    ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.save",
            json!({ "idempotencyId": "graph-save" }),
        ),
        &editor.context(),
    );
    let saved = std::fs::read_to_string(tree.root.join("main.tscn")).expect("scene");
    assert_graph_landed(&saved, &format!("{}, editor", editor.binary));
    editor.assert_no_errors_since(mark, "removing graph nodes, animations and cells");
    let _ = session.finish();
    drop(editor);
}
