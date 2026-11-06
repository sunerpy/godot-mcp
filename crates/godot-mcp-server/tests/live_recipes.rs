//! Live acceptance of the recipes page and of `scene.recipe.apply`: every
//! call of `docs/operator/recipes.md`, read from the page itself, is applied
//! headless in a real Godot build to a scene `scene.create` made, and the theme
//! is made by `resource.create`. Each is read back, then changed by its tuning
//! batch in a live editor, undone, redone, saved, reloaded from its file and
//! read again. Further variants of the body and particle recipes are applied
//! headless, and one recipe is applied to a scene open in the editor, where it
//! is one undoable action. Everything goes through the production MCP server.
//!
//! Like the other live suites it needs real editors, named by
//! `GODOT_MCP_TEST_EDITORS` or `GODOT_MCP_LOCKED_ARTIFACT`, so every case is
//! ignored on a plain `cargo test`; the page parser is checked without one.

#![cfg(target_os = "linux")]

#[path = "support/live.rs"]
mod live;
mod support;

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

use godot_mcp_domain::recipes;
use live::{Editor, Ids, digest_of, editors, ok, wait_connected};
use serde_json::{Value, json};
use support::{ProjectTree, Spawn, StdioSession, sha256_hex};

/// What one recipe makes.
#[derive(Debug, Clone)]
enum Made {
    /// A `scene.recipe.apply` call on a scene whose root has this class.
    Scene { root: String, call: Value },
    /// A resource of this class, made by `resource.create`.
    Resource { class: String },
}

/// One recipe of the page, with its tuning batch.
#[derive(Debug, Clone)]
struct Recipe {
    id: String,
    path: String,
    made: Made,
    /// The oldest locked build that has every class the recipe uses.
    since: Option<String>,
    /// The operations the page shows: what the call becomes, or what
    /// `resource.create` takes.
    operations: Value,
    tune: Value,
}

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
    let open = index + 1;
    assert_eq!(
        lines.get(open).map(|line| line.trim()),
        Some("```json"),
        "the annotation on line {} is followed by a json block",
        index + 1
    );
    let body: Vec<&str> = lines[open + 1..]
        .iter()
        .take_while(|line| line.trim() != "```")
        .copied()
        .collect();
    serde_json::from_str(&body.join("\n"))
        .unwrap_or_else(|error| panic!("the block after line {} is JSON: {error}", index + 1))
}

/// The files the page tells a reader to write before its recipes run, from
/// annotations such as `<!-- recipe-file path=shaders/tint.gdshader -->`,
/// each followed by a fenced block of the file's text.
fn recipe_files() -> Vec<(String, String)> {
    let text = page();
    let lines: Vec<&str> = text.lines().collect();
    let mut files = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if let Some(found) = attributes(line, "recipe-file") {
            let open = lines
                .get(index + 1)
                .map(|line| line.trim())
                .unwrap_or_default();
            assert!(
                open.starts_with("```") && open.len() > 3,
                "the annotation on line {} is followed by a fenced block",
                index + 1
            );
            let body: Vec<&str> = lines[index + 2..]
                .iter()
                .take_while(|line| line.trim() != "```")
                .copied()
                .collect();
            files.push((found["path"].clone(), body.join("\n") + "\n"));
        }
    }
    files
}

/// Every recipe of the page, in page order: a scene recipe from its call and
/// the operations below it, a resource recipe from its operations alone.
fn recipes() -> Vec<Recipe> {
    let text = page();
    let lines: Vec<&str> = text.lines().collect();
    let mut calls: BTreeMap<String, (BTreeMap<String, String>, Value)> = BTreeMap::new();
    let mut recipes: Vec<Recipe> = Vec::new();
    let mut tunes: BTreeMap<String, Value> = BTreeMap::new();
    for (index, line) in lines.iter().enumerate() {
        if let Some(found) = attributes(line, "recipe-call") {
            let id = found["id"].clone();
            let previous = calls.insert(id, (found, block_after(&lines, index)));
            assert!(
                previous.is_none(),
                "line {}: one call per recipe",
                index + 1
            );
        } else if let Some(found) = attributes(line, "recipe") {
            let id = found["id"].clone();
            let operations = block_after(&lines, index);
            let recipe = if let Some(class) = found.get("class") {
                Recipe {
                    path: found["path"].clone(),
                    made: Made::Resource {
                        class: class.clone(),
                    },
                    since: None,
                    id,
                    operations,
                    tune: Value::Null,
                }
            } else {
                let (call_attributes, call) = calls.remove(&id).unwrap_or_else(|| {
                    panic!(
                        "line {}: {id} shows its call before its operations",
                        index + 1
                    )
                });
                Recipe {
                    path: call_attributes["path"].clone(),
                    made: Made::Scene {
                        root: call_attributes["root"].clone(),
                        call,
                    },
                    since: call_attributes.get("since").cloned(),
                    id,
                    operations,
                    tune: Value::Null,
                }
            };
            recipes.push(recipe);
        } else if let Some(found) = attributes(line, "recipe-tune") {
            tunes.insert(found["id"].clone(), block_after(&lines, index));
        }
    }
    assert!(calls.is_empty(), "every call has its operations: {calls:?}");
    for recipe in &mut recipes {
        recipe.tune = tunes
            .remove(&recipe.id)
            .unwrap_or_else(|| panic!("{} has a tuning batch", recipe.id));
    }
    assert!(
        tunes.is_empty(),
        "every tuning batch has its recipe: {tunes:?}"
    );
    recipes
}

/// Numbers compared as numbers: the registry writes `12.0` where a page may
/// write `12`, and the server answers what the registry wrote.
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

/// What the saved file of each recipe holds once created.
fn created_lines(id: &str) -> &'static [&'static str] {
    match id {
        "body-2d" => &[
            "[node name=\"Player\" type=\"CharacterBody2D\" parent=\".\"",
            "collision_layer = 2",
            "collision_mask = 5",
            "[sub_resource type=\"CapsuleShape2D\"",
            "radius = 12.0",
            "height = 48.0",
            "target_position = Vector2(0, 32)",
            "collision_mask = 4",
        ],
        "body-3d" => &[
            "[node name=\"Crate\" type=\"RigidBody3D\" parent=\".\"",
            "mass = 2.5",
            "[sub_resource type=\"PhysicsMaterial\"",
            "friction = 0.4",
            "bounce = 0.2",
            "[sub_resource type=\"BoxShape3D\"",
            "size = Vector3(1.2, 1.2, 1.2)",
            "[sub_resource type=\"BoxMesh\"",
            "albedo_color = Color(0.8, 0.5, 0.2, 1)",
            "roughness = 0.7",
        ],
        "light-3d" => &[
            "[node name=\"Sun\" type=\"DirectionalLight3D\" parent=\".\"",
            "light_energy = 1.2",
            "shadow_enabled = true",
            "omni_range = 6.0",
            "light_color = Color(1, 0.85, 0.6, 1)",
            "background_mode = 2",
            "[sub_resource type=\"Sky\"",
            "[sub_resource type=\"ProceduralSkyMaterial\"",
            "sky_top_color = Color(0.3, 0.5, 0.9, 1)",
            "fov = 60.0",
            "current = true",
        ],
        "particles-2d" => &[
            "[node name=\"Fire\" type=\"GPUParticles2D\" parent=\".\"",
            "amount = 64",
            "lifetime = 0.8",
            "[sub_resource type=\"ParticleProcessMaterial\"",
            "spread = 25.0",
            "initial_velocity_min = 80.0",
            "initial_velocity_max = 140.0",
            "gravity = Vector3(0, -40, 0)",
            "[sub_resource type=\"GradientTexture1D\"",
            "[sub_resource type=\"Gradient\"",
            "offsets = PackedFloat32Array(0, 0.6, 1)",
        ],
        "navigation-2d" => &[
            "[node name=\"Floor\" type=\"NavigationRegion2D\" parent=\".\"",
            "navigation_layers = 3",
            "[sub_resource type=\"NavigationPolygon\"",
            "agent_radius = 8.0",
            "vertices = PackedVector2Array(",
            "[node name=\"Agent\" type=\"NavigationAgent2D\" parent=\"Walker\"",
            "path_desired_distance = 4.0",
            "target_desired_distance = 8.0",
            "radius = 12.0",
            "avoidance_enabled = true",
        ],
        "audio-2d" => &[
            "[node name=\"Steps\" type=\"AudioStreamPlayer2D\" parent=\".\"",
            "path=\"res://audio/step.wav\"",
            "stream = ExtResource(",
            "volume_db = -6.0",
            "max_distance = 600.0",
            "bus = &\"SFX\"",
        ],
        "shader-2d" => &[
            "[node name=\"Tint\" type=\"ColorRect\" parent=\".\"",
            "path=\"res://shaders/tint.gdshader\"",
            "[sub_resource type=\"ShaderMaterial\"",
            "shader = ExtResource(",
            "shader_parameter/strength = 0.5",
            "shader_parameter/tint = Color(1, 0.5, 0.2, 1)",
            "material = SubResource(",
            "offset_right = 64.0",
            "offset_bottom = 64.0",
        ],
        "theme" => &[
            "[gd_resource type=\"Theme\"",
            "Label/colors/font_color = Color(0.95, 0.95, 0.95, 1)",
            "Label/font_sizes/font_size = 20",
            "Label/constants/outline_size = 2",
            "[sub_resource type=\"StyleBoxFlat\"",
            "corner_radius_top_left = 8",
            "Panel/styles/panel = SubResource(",
        ],
        "hud" => &[
            "path=\"res://ui/theme.tres\"",
            "theme = ExtResource(",
            "anchors_preset = 15",
            "anchor_right = 1.0",
            "anchor_bottom = 1.0",
            "[node name=\"Score\" type=\"Label\" parent=\".\"",
            "anchors_preset = 1",
            "anchors_preset = 3",
            "text = \"Score: 0\"",
            "horizontal_alignment = 2",
            "theme_override_colors/font_color = Color(1, 0.9, 0.3, 1)",
            "theme_override_font_sizes/font_size = 28",
            "[node name=\"Pause\" type=\"Button\" parent=\".\"",
            "text = \"Pause\"",
        ],
        "gridmap-3d" => &[
            "[node name=\"Grid\" type=\"GridMap\" parent=\".\"",
            "[sub_resource type=\"MeshLibrary\"",
            "item/0/name = \"Block\"",
            "[sub_resource type=\"BoxMesh\"",
            "cell_size = Vector3(1, 1, 1)",
            "\"cells\": PackedInt32Array(",
        ],
        "joystick" => &[
            "[node name=\"Stick\" type=\"VirtualJoystick\" parent=\".\"",
            "joystick_mode = 1",
            "deadzone_ratio = 0.2",
            "action_left = &\"move_left\"",
            "action_down = &\"move_down\"",
            "anchors_preset = 2",
        ],
        other => panic!("the page has a recipe this suite does not check: {other}"),
    }
}

/// What the saved file holds once the tuning batch is applied.
fn tuned_line(id: &str) -> &'static str {
    match id {
        "body-2d" => "radius = 14.0",
        "body-3d" => "bounce = 0.5",
        "light-3d" => "light_energy = 0.8",
        "particles-2d" => "spread = 40.0",
        "navigation-2d" => "target_desired_distance = 12.0",
        "audio-2d" => "volume_db = -3.0",
        "shader-2d" => "shader_parameter/strength = 0.8",
        "theme" => "Label/font_sizes/font_size = 22",
        "hud" => "theme_override_font_sizes/font_size = 32",
        "joystick" => "deadzone_ratio = 0.3",
        // The cells of a grid map are one packed list; the tuned one is counted.
        "gridmap-3d" => "\"cells\": PackedInt32Array(",
        other => panic!("no tuned line for {other}"),
    }
}

/// The number of painted cells of a saved grid map.
fn grid_cells(scene: &str) -> usize {
    let listed = scene
        .split_once("\"cells\": PackedInt32Array(")
        .and_then(|(_, rest)| rest.split_once(')'))
        .map(|(cells, _)| cells)
        .unwrap_or_default();
    let numbers = listed
        .split(',')
        .filter(|value| !value.trim().is_empty())
        .count();
    numbers / 3
}

/// A 0.1 s mono 16-bit tone, as a PCM WAV file.
fn wav() -> Vec<u8> {
    let rate: u32 = 22_050;
    let samples: Vec<i16> = (0..2_205_u32)
        .map(|n| {
            let phase = f64::from(n) * 440.0 * std::f64::consts::TAU / f64::from(rate);
            // A quiet tone well inside the 16-bit range.
            (phase.sin() * 8_000.0) as i16
        })
        .collect();
    let data = u32::try_from(samples.len() * 2).expect("a short tone");
    let mut bytes = Vec::with_capacity(44 + samples.len() * 2);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&rate.to_le_bytes());
    bytes.extend_from_slice(&(rate * 2).to_le_bytes());
    bytes.extend_from_slice(&2_u16.to_le_bytes());
    bytes.extend_from_slice(&16_u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data.to_le_bytes());
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    bytes
}

/// A bus layout with the `SFX` bus the sound recipe plays on.
const BUS_LAYOUT: &str = "[gd_resource type=\"AudioBusLayout\" format=3]\n\n[resource]\n\
                          bus/1/name = &\"SFX\"\nbus/1/send = &\"Master\"\n";

/// What every project of this suite holds before a recipe is applied.
fn prepare(tree: &ProjectTree) {
    for (path, text) in recipe_files() {
        tree.write(&path, &text);
    }
    tree.write("default_bus_layout.tres", BUS_LAYOUT);
    let wav_path = tree.root.join("audio/step.wav");
    std::fs::create_dir_all(wav_path.parent().expect("audio directory")).expect("audio directory");
    std::fs::write(&wav_path, wav()).expect("write the sound");
}

/// The `major.minor.patch` a binary of the locked set is named for.
fn version_of(binary: &Path) -> String {
    let name = binary
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    name.split('v')
        .nth(1)
        .and_then(|rest| rest.split('-').next())
        .unwrap_or_default()
        .to_owned()
}

fn version_key(version: &str) -> Vec<u32> {
    version
        .split('.')
        .map(|part| part.parse().unwrap_or(0))
        .collect()
}

fn session(tree: &ProjectTree, binary: &Path) -> StdioSession {
    StdioSession::initialized(&Spawn {
        project: &tree.root,
        current_dir: None,
        profile: "full",
        godot: Some(binary),
    })
}

/// The message of a refused call.
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
    assert!(!message.is_empty(), "the call is refused: {response}");
    message
}

/// The nodes each `add_node` of a recipe makes: path and class.
fn added_nodes(operations: &Value) -> Vec<(String, String)> {
    operations
        .as_array()
        .expect("operations")
        .iter()
        .filter(|operation| operation["op"] == "add_node")
        .map(|operation| {
            let parent = operation["parent"].as_str().unwrap_or(".");
            let name = operation["name"].as_str().expect("name");
            let path = if parent == "." {
                name.to_owned()
            } else {
                format!("{parent}/{name}")
            };
            (path, operation["type"].as_str().expect("type").to_owned())
        })
        .collect()
}

/// The classes of the sub-resources a recipe's `set_subresource` operations
/// embed.
fn subresource_classes(operations: &Value) -> Vec<String> {
    operations
        .as_array()
        .expect("operations")
        .iter()
        .filter(|operation| operation["op"] == "set_subresource")
        .map(|operation| operation["class"].as_str().expect("class").to_owned())
        .collect()
}

/// The operations the registry expands `parameters` of `id` to.
fn expansion(id: &str, parameters: &Value) -> Value {
    let expanded = recipes::recipe(id)
        .unwrap_or_else(|| panic!("a registered recipe: {id}"))
        .expand(parameters.as_object().expect("parameters"), None)
        .unwrap_or_else(|error| panic!("{id} takes {parameters}: {}", error.message()));
    Value::Array(expanded.operations)
}

/// The variants applied besides the page's calls: every body and shape the
/// physics recipes take beyond the page's, and every particle preset.
fn variants() -> Vec<(&'static str, Value)> {
    let mut variants = vec![
        ("body-2d", json!({ "body": "rigid", "shape": "circle" })),
        ("body-2d", json!({ "body": "static", "shape": "rectangle" })),
        ("body-2d", json!({ "body": "area", "shape": "capsule" })),
        ("body-3d", json!({ "body": "static", "shape": "sphere" })),
        (
            "body-3d",
            json!({ "body": "character", "shape": "capsule" }),
        ),
        ("body-3d", json!({ "body": "area", "shape": "cylinder" })),
    ];
    for preset in recipes::PARTICLE_PRESETS {
        variants.push(("particles-2d", json!({ "preset": preset })));
    }
    variants
}

/// Make an empty scene with a `root` root at `path`, then apply `call` to it
/// through `scene.recipe.apply`. Answers the tool's answer, or `None` when the
/// build lacks a class the recipe uses: the call is then refused, naming the
/// class, and the scene keeps its bytes.
#[allow(clippy::too_many_arguments)]
fn apply_to_new_scene(
    session: &mut StdioSession,
    ids: &mut Ids,
    tree: &ProjectTree,
    label: &str,
    path: &str,
    root: &str,
    mut call: Value,
    expected: &Value,
    available: bool,
    context: &str,
) -> Option<Value> {
    ok(
        &session.call_tool(
            ids.next(),
            "scene.create",
            json!({ "path": path, "rootType": root, "idempotencyId": format!("empty-{label}") }),
        ),
        context,
    );
    let file = tree.root.join(path);
    let empty = std::fs::read(&file).expect("the empty scene");
    call["path"] = json!(path);
    call["expectedSha256"] = json!(sha256_hex(&empty));
    call["idempotencyId"] = json!(format!("apply-{label}"));
    let response = session.call_tool(ids.next(), "scene.recipe.apply", call);
    if !available {
        let message = refusal(&response);
        let class = added_nodes(expected)
            .into_iter()
            .map(|(_, class)| class)
            .find(|class| message.contains(class.as_str()));
        assert!(
            class.is_some(),
            "{context}: the refusal names the missing class: {message}"
        );
        assert_eq!(
            std::fs::read(&file).expect("the scene"),
            empty,
            "{context}: a refused recipe writes nothing"
        );
        return None;
    }
    let applied = ok(&response, context);
    assert_eq!(
        applied["conclusion"], "proven_post_state",
        "{context}: {applied}"
    );
    assert_eq!(applied["backend"], "headless", "{context}: {applied}");
    assert_eq!(
        normalized(&applied["operations"]),
        normalized(expected),
        "{context}: the answer carries the operations the call became: {applied}"
    );
    let saved = std::fs::read(&file).expect("the scene");
    assert_eq!(
        applied["after_sha256"],
        json!(sha256_hex(&saved)),
        "{context}"
    );
    Some(applied)
}

/// Read the scene back: its root has class `root`, and every node the
/// operations add has the class they name.
fn assert_tree(
    session: &mut StdioSession,
    ids: &mut Ids,
    path: &str,
    root: &str,
    operations: &Value,
    context: &str,
) {
    let read = ok(
        &session.call_tool(ids.next(), "scene.tree.get", json!({ "scene": path })),
        context,
    );
    let nodes = read["data"]["nodes"].as_array().expect("nodes").clone();
    let class_of = |path: &str| {
        nodes
            .iter()
            .find(|node| node["path"] == path)
            .and_then(|node| node["type"].as_str())
            .map(str::to_owned)
    };
    assert_eq!(class_of(".").as_deref(), Some(root), "{context}: {read}");
    for (path, class) in added_nodes(operations) {
        assert_eq!(
            class_of(&path).as_deref(),
            Some(class.as_str()),
            "{context}: reads back {path}"
        );
    }
}

#[test]
fn the_page_holds_a_call_or_operations_and_a_tuning_batch_for_each_recipe() {
    let recipes = recipes();
    let ids: Vec<&str> = recipes.iter().map(|recipe| recipe.id.as_str()).collect();
    let registry: Vec<&str> = recipes::RECIPES.iter().map(|recipe| recipe.id).collect();
    assert_eq!(ids, registry);
    for recipe in &recipes {
        assert!(!created_lines(&recipe.id).is_empty());
        assert!(!tuned_line(&recipe.id).is_empty());
        assert!(
            recipe
                .operations
                .as_array()
                .is_some_and(|ops| !ops.is_empty()),
            "{}",
            recipe.id
        );
        assert!(recipe.tune.as_array().is_some_and(|ops| !ops.is_empty()));
        let registered = recipes::recipe(&recipe.id).expect("registered");
        match &recipe.made {
            Made::Scene { root, call } => {
                assert_eq!(call["path"], json!(recipe.path), "{}", recipe.id);
                assert!(call["recipe"].get(&recipe.id).is_some(), "{}", recipe.id);
                assert_eq!(root, registered.parent_class, "{}", recipe.id);
                assert_eq!(recipe.since.as_deref(), registered.since, "{}", recipe.id);
            }
            Made::Resource { class } => match registered.target {
                recipes::RecipeTarget::Resource { class: registered } => {
                    assert_eq!(class, registered, "{}", recipe.id);
                }
                recipes::RecipeTarget::Scene => panic!("{} is a scene recipe", recipe.id),
            },
        }
    }
    let files = recipe_files();
    assert_eq!(
        files
            .iter()
            .map(|(path, _)| path.as_str())
            .collect::<Vec<_>>(),
        ["shaders/tint.gdshader"]
    );
    assert!(
        files[0].1.starts_with("shader_type canvas_item;\n"),
        "{files:?}"
    );
    for (id, parameters) in variants() {
        assert!(
            !expansion(id, &parameters)
                .as_array()
                .expect("ops")
                .is_empty()
        );
    }
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn every_recipe_is_applied_tuned_undone_redone_saved_and_reloaded() {
    let (binaries, _unpacked) = editors();
    let recipes = recipes();
    for binary in &binaries {
        let started = Instant::now();
        recipes_in(binary, &recipes);
        eprintln!(
            "live_recipes: {} passed in {:.1} s",
            binary.display(),
            started.elapsed().as_secs_f64()
        );
    }
}

fn recipes_in(binary: &Path, recipes: &[Recipe]) {
    let version = version_of(binary);
    let context = binary.display().to_string();
    let applies = |recipe: &Recipe| {
        recipe
            .since
            .as_deref()
            .is_none_or(|since| version_key(&version) >= version_key(since))
    };

    // Headless: make each recipe as its page says, read it back and check the
    // saved file.
    let tree = ProjectTree::new("live-recipes");
    prepare(&tree);
    let mut headless = session(&tree, binary);
    let mut ids = Ids(1);
    ok(
        &headless.call_tool(
            ids.next(),
            "project.import.run",
            json!({ "idempotencyId": "recipes-import", "assets": ["audio/step.wav"],
                    "deadline_ms": 120_000 }),
        ),
        &context,
    );
    // The resources first: the HUD's call names the theme.
    let mut ordered: Vec<&Recipe> = recipes
        .iter()
        .filter(|recipe| matches!(recipe.made, Made::Resource { .. }))
        .collect();
    ordered.extend(
        recipes
            .iter()
            .filter(|recipe| matches!(recipe.made, Made::Scene { .. })),
    );
    for recipe in ordered {
        let context = format!("{context}: {}", recipe.id);
        match &recipe.made {
            Made::Resource { class } => {
                ok(
                    &headless.call_tool(
                        ids.next(),
                        "resource.create",
                        json!({
                            "path": recipe.path,
                            "class": class,
                            "operations": recipe.operations,
                            "idempotencyId": format!("create-{}", recipe.id),
                        }),
                    ),
                    &context,
                );
            }
            Made::Scene { root, call } => {
                let applied = apply_to_new_scene(
                    &mut headless,
                    &mut ids,
                    &tree,
                    &recipe.id,
                    &recipe.path,
                    root,
                    call.clone(),
                    &recipe.operations,
                    applies(recipe),
                    &context,
                );
                let Some(applied) = applied else {
                    continue;
                };
                assert_eq!(applied["recipe"]["id"], json!(recipe.id), "{context}");
                assert_tree(
                    &mut headless,
                    &mut ids,
                    &recipe.path,
                    root,
                    &recipe.operations,
                    &context,
                );
            }
        }
        let saved = std::fs::read_to_string(tree.root.join(&recipe.path)).expect("made file");
        for line in created_lines(&recipe.id) {
            assert!(saved.contains(line), "{context} holds {line}:\n{saved}");
        }
    }
    // The other bodies, shapes and particle presets, each with the registry's
    // defaults, in their own scenes.
    for (index, (id, parameters)) in variants().into_iter().enumerate() {
        let registered = recipes::recipe(id).expect("registered");
        let label = format!("{id}-{index}");
        let path = format!("variants/{label}.tscn");
        let context = format!("{context}: {id} {parameters}");
        let expected = expansion(id, &parameters);
        apply_to_new_scene(
            &mut headless,
            &mut ids,
            &tree,
            &label,
            &path,
            registered.parent_class,
            json!({ "recipe": { id: parameters } }),
            &expected,
            true,
            &context,
        );
        assert_tree(
            &mut headless,
            &mut ids,
            &path,
            registered.parent_class,
            &expected,
            &context,
        );
        let saved = std::fs::read_to_string(tree.root.join(&path)).expect("variant");
        for class in subresource_classes(&expected) {
            let embedded = format!("[sub_resource type=\"{class}\"");
            assert!(
                saved.contains(&embedded),
                "{context} embeds {class}:\n{saved}"
            );
        }
    }
    // The empty scene the editor applies a recipe to.
    ok(
        &headless.call_tool(
            ids.next(),
            "scene.create",
            json!({ "path": "scenes/smoke.tscn", "rootType": "Node2D",
                    "idempotencyId": "empty-editor-smoke" }),
        ),
        &context,
    );
    // The engine reads back what two recipes made: the grid's painted cells
    // and the polygons the navigation bake left in the region.
    let cell =
        |x: i64, y: i64, z: i64| json!({ "type": "vector3i", "value": { "x": x, "y": y, "z": z } });
    let grid = ok(
        &headless.call_tool(
            ids.next(),
            "scene.read",
            json!({
                "path": "scenes/blocks.tscn",
                "calls": [
                    { "node": "Grid", "method": "get_used_cells" },
                    { "node": "Grid", "method": "get_cell_item", "arguments": [cell(2, 1, 0)] },
                    { "node": "Grid", "method": "get_cell_item", "arguments": [cell(5, 5, 5)] }
                ]
            }),
        ),
        &context,
    );
    let cells = grid["calls"][0]["value"]["value"].as_array().map(Vec::len);
    assert_eq!(cells, Some(4), "{context}: four painted cells: {grid}");
    assert_eq!(
        grid["calls"][1]["value"],
        json!({ "type": "integer", "value": 0 }),
        "{context}: {grid}"
    );
    assert_eq!(
        grid["calls"][2]["value"],
        json!({ "type": "integer", "value": -1 }),
        "{context}: an empty cell: {grid}"
    );
    let floor = ok(
        &headless.call_tool(
            ids.next(),
            "scene.read",
            json!({
                "path": "scenes/arena.tscn",
                "calls": [{ "node": "Floor", "resource": { "property": "navigation_polygon" },
                            "method": "get_polygon_count" }]
            }),
        ),
        &context,
    );
    assert!(
        floor["calls"][0]["value"]["value"].as_i64() > Some(0),
        "{context}: the baked region holds polygons: {floor}"
    );
    let _ = headless.finish();

    // In the editor: apply one recipe to an open scene as one undoable action,
    // then tune each recipe's scene, undo, redo, save, reload from the file and
    // read the same scene again.
    let editing = ProjectTree::new("live-recipes-editor");
    editing.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"Recipes\"\n",
    );
    prepare(&editing);
    let copied = recipes
        .iter()
        .filter(|recipe| applies(recipe))
        .map(|recipe| recipe.path.as_str())
        .chain(["scenes/smoke.tscn"]);
    for path in copied {
        let bytes = std::fs::read(tree.root.join(path)).expect("made file");
        let target = editing.root.join(path);
        std::fs::create_dir_all(target.parent().expect("parent")).expect("parent");
        std::fs::write(target, bytes).expect("copy the made file");
    }
    let mut session = session(&editing, binary);
    let mut ids = Ids(1);
    ok(
        &session.call_tool(
            ids.next(),
            "editor.addon.install",
            json!({ "enable": true, "idempotencyId": "recipes-install" }),
        ),
        &context,
    );
    let editor = Editor::launch(binary, &editing.root, &editing.base.join("home"));
    wait_connected(&mut session, &mut ids, &editor);
    ok(
        &session.call_tool(
            ids.next(),
            "editor.filesystem.scan",
            json!({ "idempotencyId": "recipes-scan" }),
        ),
        &editor.context(),
    );
    let mark = editor.mark();
    apply_in_editor(&mut session, &mut ids, &editing, &editor.context());
    for recipe in recipes.iter().filter(|recipe| applies(recipe)) {
        tune_in_editor(&mut session, &mut ids, &editing, recipe, &editor.context());
    }
    editor.assert_no_errors_since(
        mark,
        "applying a recipe in the editor, then tuning, undoing, redoing, saving and \
         reloading every recipe",
    );
    let _ = session.finish();
    drop(editor);
}

/// `scene.recipe.apply` on a scene open in the editor: the editor applies the
/// operations as one action named after the tool and saves the scene; one
/// undo reverts all of it and one redo restores it.
fn apply_in_editor(session: &mut StdioSession, ids: &mut Ids, editing: &ProjectTree, editor: &str) {
    let path = "scenes/smoke.tscn";
    let context = format!("{editor}: {path}");
    ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.open",
            json!({ "path": path, "idempotencyId": "open-smoke" }),
        ),
        &context,
    );
    let empty = digest_of(session, ids, &context);
    let file = editing.root.join(path);
    let before = std::fs::read(&file).expect("the empty scene");
    let parameters = json!({ "preset": "smoke" });
    let expected = expansion("particles-2d", &parameters);
    let applied = ok(
        &session.call_tool(
            ids.next(),
            "scene.recipe.apply",
            json!({
                "path": path,
                "expectedSha256": sha256_hex(&before),
                "recipe": { "particles-2d": parameters },
                "idempotencyId": "editor-smoke",
            }),
        ),
        &context,
    );
    assert_eq!(applied["backend"], "editor", "{context}: {applied}");
    assert_eq!(
        applied["conclusion"], "proven_post_state",
        "{context}: {applied}"
    );
    assert_eq!(
        normalized(&applied["operations"]),
        normalized(&expected),
        "{context}: {applied}"
    );
    let node = "[node name=\"Smoke\" type=\"GPUParticles2D\" parent=\".\"";
    let saved = std::fs::read(&file).expect("the saved scene");
    assert_eq!(
        applied["after_sha256"],
        json!(sha256_hex(&saved)),
        "{context}"
    );
    assert!(
        String::from_utf8_lossy(&saved).contains(node),
        "{context}: the editor saved the recipe:\n{}",
        String::from_utf8_lossy(&saved)
    );
    let status = ok(
        &session.call_tool(ids.next(), "editor.status.get", json!({})),
        &context,
    );
    assert_eq!(
        status["editor"]["history"]["current_action"],
        json!("godot-mcp: scene.recipe.apply"),
        "{context}: one action named after the tool: {status}"
    );
    let made = digest_of(session, ids, &context);
    assert_ne!(made, empty, "{context}");
    for (step, (tool, expected)) in [("editor.undo", &empty), ("editor.redo", &made)]
        .into_iter()
        .enumerate()
    {
        let stepped = ok(
            &session.call_tool(
                ids.next(),
                tool,
                json!({ "idempotencyId": format!("{tool}-smoke-{step}") }),
            ),
            &context,
        );
        assert_eq!(stepped["digest"], json!(expected), "{context}: {tool}");
    }
    ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.save",
            json!({ "idempotencyId": "save-smoke" }),
        ),
        &context,
    );
    let saved = std::fs::read_to_string(&file).expect("the saved scene");
    assert!(saved.contains(node), "{context}:\n{saved}");
    ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.open",
            json!({ "path": path, "reload": true, "idempotencyId": "reload-smoke" }),
        ),
        &context,
    );
    assert_eq!(
        digest_of(session, ids, &context),
        made,
        "{context}: the reloaded scene is the scene the editor saved"
    );
}

/// A recipe's tuning batch: a scene's in the editor, as one undoable action,
/// undone, redone, saved and reloaded; a resource's headless, since the
/// editor holds no buffer of it.
fn tune_in_editor(
    session: &mut StdioSession,
    ids: &mut Ids,
    editing: &ProjectTree,
    recipe: &Recipe,
    editor: &str,
) {
    let context = format!("{editor}: {}", recipe.id);
    let file = editing.root.join(&recipe.path);
    if let Made::Resource { .. } = recipe.made {
        let before = std::fs::read(&file).expect("resource");
        ok(
            &session.call_tool(
                ids.next(),
                "resource.batch.apply",
                json!({
                    "path": recipe.path,
                    "expectedSha256": sha256_hex(&before),
                    "operations": recipe.tune,
                    "idempotencyId": format!("tune-{}", recipe.id),
                }),
            ),
            &context,
        );
        let saved = std::fs::read_to_string(&file).expect("resource");
        assert!(
            saved.contains(tuned_line(&recipe.id)),
            "{context}:\n{saved}"
        );
        return;
    }
    ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.open",
            json!({ "path": recipe.path, "idempotencyId": format!("open-{}", recipe.id) }),
        ),
        &context,
    );
    let original = digest_of(session, ids, &context);
    let before_cells = grid_cells(&std::fs::read_to_string(&file).expect("scene"));
    let edited = ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.edit",
            json!({
                "expectedDigest": original,
                "operations": recipe.tune,
                "label": format!("tune {}", recipe.id),
                "idempotencyId": format!("tune-{}", recipe.id),
            }),
        ),
        &context,
    );
    let changed = edited["digest"].as_str().expect("digest").to_owned();
    assert_ne!(changed, original, "{context}: {edited}");
    for (step, (tool, expected)) in [("editor.undo", &original), ("editor.redo", &changed)]
        .into_iter()
        .enumerate()
    {
        let stepped = ok(
            &session.call_tool(
                ids.next(),
                tool,
                json!({ "idempotencyId": format!("{tool}-{}-{step}", recipe.id) }),
            ),
            &context,
        );
        assert_eq!(stepped["digest"], json!(expected), "{context}: {tool}");
    }
    ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.save",
            json!({ "idempotencyId": format!("save-{}", recipe.id) }),
        ),
        &context,
    );
    let saved = std::fs::read_to_string(&file).expect("saved scene");
    if recipe.id == "gridmap-3d" {
        assert_eq!(
            grid_cells(&saved),
            before_cells + 1,
            "{context}: one more cell is painted:\n{saved}"
        );
    } else {
        assert!(
            saved.contains(tuned_line(&recipe.id)),
            "{context}:\n{saved}"
        );
    }
    for line in created_lines(&recipe.id)
        .iter()
        .filter(|line| line.starts_with("[node "))
    {
        assert!(saved.contains(line), "{context}: the editor keeps {line}");
    }
    ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.open",
            json!({ "path": recipe.path, "reload": true,
                    "idempotencyId": format!("reload-{}", recipe.id) }),
        ),
        &context,
    );
    assert_eq!(
        digest_of(session, ids, &context),
        changed,
        "{context}: the reloaded scene is the scene the editor saved"
    );
}
