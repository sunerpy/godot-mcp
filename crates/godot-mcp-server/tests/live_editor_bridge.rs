//! Live-editor acceptance: the shipped `godot_mcp` add-on in real Godot
//! editors, driven through the production MCP server.
//!
//! The scripted plugin in `editor_settings.rs` and its neighbours pins what
//! the server owns; this suite checks that the add-on a user installs does
//! what those cases assume, in the exact Godot builds the lock names.
//!
//! It needs real editors, so every case is ignored on a plain `cargo test`:
//!
//! * `GODOT_MCP_TEST_EDITORS` lists editor binaries, comma-separated, or
//! * `GODOT_MCP_LOCKED_ARTIFACT` names one verified editor archive, which is
//!   unpacked with `unzip` (what `make godot-test` passes).
//!
//! A run that names neither fails rather than passing without having run.
//! Every editor gets its own `HOME` and XDG directories, so no user setting is
//! read or written.

#![cfg(target_os = "linux")]

mod support;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::{ProjectTree, Spawn, StdioSession, error_code, sha256_hex};

const EDITORS: &str = "GODOT_MCP_TEST_EDITORS";
const ARTIFACT: &str = "GODOT_MCP_LOCKED_ARTIFACT";

/// The editor binaries this run checks, with the scratch directory an
/// unpacked archive lives in.
fn editors() -> (Vec<PathBuf>, Option<tempfile::TempDir>) {
    if let Ok(listed) = std::env::var(EDITORS) {
        let binaries: Vec<PathBuf> = listed
            .split(',')
            .filter(|binary| !binary.is_empty())
            .map(PathBuf::from)
            .collect();
        assert!(!binaries.is_empty(), "{EDITORS} lists no editor");
        return (binaries, None);
    }
    let archive = PathBuf::from(
        std::env::var_os(ARTIFACT)
            .unwrap_or_else(|| panic!("set {EDITORS} or {ARTIFACT} to run the live-editor suite")),
    );
    let name = archive
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(".zip"))
        .unwrap_or_else(|| panic!("{} is not a zip archive", archive.display()))
        .to_owned();
    let work = tempfile::tempdir().expect("scratch directory for the editor");
    let status = Command::new("unzip")
        .arg("-q")
        .arg("-o")
        .arg(&archive)
        .arg("-d")
        .arg(work.path())
        .status()
        .expect("run unzip");
    assert!(
        status.success(),
        "unzip {} failed: {status}",
        archive.display()
    );
    let binary = work.path().join(&name);
    assert!(
        binary.is_file(),
        "{} does not unpack to {name}",
        archive.display()
    );
    (vec![binary], Some(work))
}

/// One running editor, killed when dropped.
struct Editor {
    child: Child,
    log: PathBuf,
    binary: String,
}

impl Editor {
    /// Launch `binary` as a headless editor on `project`, with private user
    /// directories under `home`.
    fn launch(binary: &Path, project: &Path, home: &Path) -> Self {
        Self::launch_with(binary, project, home, &["--headless", "--editor", "--path"])
    }

    /// Launch `binary` as an editor with a window on the display the test
    /// runs under, drawn by the OpenGL renderer.
    fn launch_windowed(binary: &Path, project: &Path, home: &Path) -> Self {
        Self::launch_with(
            binary,
            project,
            home,
            &["--editor", "--rendering-driver", "opengl3", "--path"],
        )
    }

    /// Launch `binary` with `arguments` on `project`, with private user
    /// directories under `home`. The editor finds no user session either: its
    /// runtime directory is a private one, so it reaches no audio server, and
    /// its session bus address names a socket nothing listens on. An editor
    /// that finds no address autolaunches a bus, whose document portal mounts
    /// a file system under `home` until the X server exits; one that finds the
    /// user's bus talks to the user's desktop session. Every host then runs
    /// the editor as one without a desktop session does.
    fn launch_with(binary: &Path, project: &Path, home: &Path, arguments: &[&str]) -> Self {
        use std::os::unix::fs::PermissionsExt;

        for directory in ["config", "data", "cache", "runtime"] {
            std::fs::create_dir_all(home.join(directory)).expect("private user directory");
        }
        let runtime = home.join("runtime");
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700))
            .expect("a private runtime directory");
        let log = home.join("editor.log");
        let output = std::fs::File::create(&log).expect("editor log");
        let child = Command::new(binary)
            .args(arguments)
            .arg(project)
            .env("HOME", home)
            .env("XDG_CONFIG_HOME", home.join("config"))
            .env("XDG_DATA_HOME", home.join("data"))
            .env("XDG_CACHE_HOME", home.join("cache"))
            .env("XDG_RUNTIME_DIR", &runtime)
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={}", runtime.join("no-session-bus").display()),
            )
            .stdin(Stdio::null())
            .stdout(output.try_clone().expect("editor log"))
            .stderr(output)
            .spawn()
            .unwrap_or_else(|error| panic!("launch {}: {error}", binary.display()));
        Self {
            child,
            log,
            binary: binary.display().to_string(),
        }
    }

    fn output(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// Where the editor's output ends now.
    fn mark(&self) -> usize {
        self.output().len()
    }

    /// Fail when the editor printed an engine or script error after `mark`:
    /// serving a request must not raise one, and every error also lands in
    /// the user's Output panel. An error the headless dummy renderer raises
    /// is not counted: Godot 4.4 asks it for a texture while saving a scene's
    /// thumbnail, which a windowed editor's renderer has. Nor are two errors
    /// of a game the editor runs, which shares the editor's output: an X
    /// `BadWindow` error, which a game whose window is embedded in the
    /// editor's (Godot 4.4.1 starts it with `--wid`) reports under Xvfb, and
    /// the ALSA driver's failure to open a sound device on a host without one,
    /// after which the game falls back to the dummy audio driver.
    fn assert_no_errors_since(&self, mark: usize, during: &str) {
        let output = self.output();
        let lines: Vec<&str> = output.get(mark..).unwrap_or_default().lines().collect();
        let errors: Vec<&str> = lines
            .iter()
            .enumerate()
            .filter(|(index, line)| {
                line.contains("ERROR:")
                    && !line.contains("Unhandled XServer error: BadWindow")
                    && !lines.get(index + 1).is_some_and(|at| {
                        at.contains("servers/rendering/dummy/") || at.contains("drivers/alsa/")
                    })
            })
            .map(|(_, line)| *line)
            .collect();
        assert!(
            errors.is_empty(),
            "{}: {during} printed {} engine errors, the first: {:?}\n{}",
            self.binary,
            errors.len(),
            errors.iter().take(3).collect::<Vec<_>>(),
            output.get(mark..).unwrap_or_default()
        );
    }

    /// What a failure names: the build and everything the editor printed.
    fn context(&self) -> String {
        format!("{}\n{}", self.binary, self.output())
    }
}

impl Drop for Editor {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn open(tree: &ProjectTree) -> StdioSession {
    StdioSession::initialized(&Spawn {
        project: &tree.root,
        current_dir: None,
        profile: "full",
        godot: None,
    })
}

fn ok(response: &Value, context: &str) -> Value {
    assert!(
        response.get("error").is_none() && response["result"]["isError"] != json!(true),
        "the call must succeed: {response}\n{context}"
    );
    response["result"]["structuredContent"].clone()
}

/// The next request id of one session.
struct Ids(u64);

impl Ids {
    fn next(&mut self) -> u64 {
        self.0 += 1;
        self.0
    }
}

/// Wait until the add-on in `editor` has connected, at most 90 seconds.
fn wait_connected(session: &mut StdioSession, ids: &mut Ids, editor: &Editor) -> Value {
    let started = Instant::now();
    loop {
        let status = ok(
            &session.call_tool(ids.next(), "editor.status.get", json!({})),
            &editor.context(),
        );
        if status["connected"] == json!(true) {
            return status;
        }
        assert!(
            started.elapsed() < Duration::from_secs(90),
            "{}: the add-on never connected: {status}\n{}",
            editor.binary,
            editor.output()
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Wait until `path` exists, at most 30 seconds.
fn wait_for_file(path: &Path, editor: &Editor) {
    let started = Instant::now();
    while !path.exists() {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "{}: {} never appeared\n{}",
            editor.binary,
            path.display(),
            editor.output()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A test-only editor plugin that acts on marker files: `probe_save` makes
/// the editor save its in-memory project settings, as it does whenever a
/// setting is changed in its Project Settings dialog; `probe_hold` changes
/// one setting in memory without saving it.
const SAVE_PROBE: &str = r#"@tool
extends EditorPlugin

func _process(_delta: float) -> void:
	if FileAccess.file_exists("res://probe_save"):
		DirAccess.remove_absolute(ProjectSettings.globalize_path("res://probe_save"))
		ProjectSettings.save()
		FileAccess.open("res://probe_saved", FileAccess.WRITE).store_string("saved")
	if FileAccess.file_exists("res://probe_hold"):
		DirAccess.remove_absolute(ProjectSettings.globalize_path("res://probe_hold"))
		ProjectSettings.set_setting("application/config/description", "held in memory")
		FileAccess.open("res://probe_held", FileAccess.WRITE).store_string("held")
"#;

/// Have the editor save its in-memory project settings, and wait until it has.
fn editor_saves_settings(tree: &ProjectTree, editor: &Editor) {
    let _ = std::fs::remove_file(tree.root.join("probe_saved"));
    std::fs::write(tree.root.join("probe_save"), b"save").expect("save marker");
    wait_for_file(&tree.root.join("probe_saved"), editor);
}

fn manifest(tree: &ProjectTree) -> String {
    std::fs::read_to_string(tree.root.join("project.godot")).expect("project.godot")
}

fn settings_sha(session: &mut StdioSession, ids: &mut Ids, context: &str) -> String {
    ok(
        &session.call_tool(ids.next(), "project.settings.get", json!({})),
        context,
    )["sha256"]
        .as_str()
        .expect("settings sha256")
        .to_owned()
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn a_connected_editor_owns_project_godot() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        a_connected_editor_owns_project_godot_in(binary);
    }
}

fn a_connected_editor_owns_project_godot_in(binary: &Path) {
    let tree = ProjectTree::new("live-settings");
    tree.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"Live\"\n",
    );
    tree.write(
        "main.tscn",
        "[gd_scene format=3]\n\n[node name=\"Main\" type=\"Node2D\"]\n",
    );
    tree.write("autoload/game.gd", "extends Node\n");
    tree.write(
        "addons/save_probe/plugin.cfg",
        "[plugin]\n\nname=\"save_probe\"\ndescription=\"\"\nauthor=\"\"\nversion=\"1\"\nscript=\"plugin.gd\"\n",
    );
    tree.write("addons/save_probe/plugin.gd", SAVE_PROBE);
    tree.write(
        "addons/other/plugin.cfg",
        "[plugin]\n\nname=\"other\"\ndescription=\"\"\nauthor=\"\"\nversion=\"1\"\nscript=\"plugin.gd\"\n",
    );
    tree.write("addons/other/plugin.gd", "@tool\nextends EditorPlugin\n");
    let home = tree.base.join("home");
    let mut session = open(&tree);
    let mut ids = Ids(1);

    // Before any editor runs, the add-on and the probe are installed through
    // the file path.
    let installed = session.call_tool(
        ids.next(),
        "editor.addon.install",
        json!({ "enable": true, "idempotencyId": "live-install" }),
    );
    assert!(installed.get("error").is_none(), "{installed}");
    let before_launch = binary.display().to_string();
    let sha = settings_sha(&mut session, &mut ids, &before_launch);
    let enabled = session.call_tool(
        ids.next(),
        "project.settings.patch",
        json!({
            "expectedSha256": sha,
            "operations": [{ "op": "enable_plugin", "plugin": "res://addons/save_probe/plugin.cfg" }],
            "idempotencyId": "live-enable-probe"
        }),
    );
    assert_eq!(ok(&enabled, &before_launch)["backend"], "file", "{enabled}");

    let editor = Editor::launch(binary, &tree.root, &home);
    let status = wait_connected(&mut session, &mut ids, &editor);
    let mark = editor.mark();
    assert_eq!(
        status["plugin_version"],
        json!(godot_mcp_bridge::addon::ADDON_VERSION),
        "{status}"
    );

    // Control: a write made underneath the running editor is lost at its next
    // save. This is the defect the editor path exists for, so the check below
    // can tell it apart from a fix.
    let before_control = manifest(&tree);
    std::fs::write(
        tree.root.join("project.godot"),
        format!("{before_control}\n[probe_external]\n\nvalue=42\n"),
    )
    .expect("write around the editor");
    editor_saves_settings(&tree, &editor);
    assert!(
        !manifest(&tree).contains("probe_external"),
        "{}: the editor's save keeps a write made underneath it, so this check proves nothing:\n{}",
        editor.binary,
        manifest(&tree)
    );

    // A settings patch is applied by the editor and survives its next save.
    let sha = settings_sha(&mut session, &mut ids, &editor.context());
    let renamed = ok(
        &session.call_tool(
            ids.next(),
            "project.settings.patch",
            json!({
                "expectedSha256": sha,
                "operations": [
                    { "op": "set_value", "setting": "application/config/name", "value": "Live Renamed" }
                ],
                "idempotencyId": "live-rename"
            }),
        ),
        &editor.context(),
    );
    assert_eq!(renamed["backend"], "editor", "{renamed}");
    assert_eq!(
        renamed["after_sha256"],
        json!(sha256_hex(manifest(&tree).as_bytes())),
        "{renamed}"
    );
    assert!(
        manifest(&tree).contains("config/name=\"Live Renamed\""),
        "{}",
        manifest(&tree)
    );
    editor_saves_settings(&tree, &editor);
    assert!(
        manifest(&tree).contains("config/name=\"Live Renamed\""),
        "{}: the editor's next save lost the change:\n{}",
        editor.binary,
        manifest(&tree)
    );

    // A value the editor holds unsaved is never overwritten.
    let _ = std::fs::remove_file(tree.root.join("probe_held"));
    std::fs::write(tree.root.join("probe_hold"), b"hold").expect("hold marker");
    wait_for_file(&tree.root.join("probe_held"), &editor);
    let before_held = manifest(&tree);
    let sha = settings_sha(&mut session, &mut ids, &editor.context());
    let held = session.call_tool(
        ids.next(),
        "project.settings.patch",
        json!({
            "expectedSha256": sha,
            "operations": [
                { "op": "set_value", "setting": "application/config/description", "value": "from MCP" }
            ],
            "idempotencyId": "live-held"
        }),
    );
    assert_eq!(error_code(&held), json!("conflict"), "{held}");
    assert_eq!(manifest(&tree), before_held, "nothing was written");

    // Input actions, autoloads, the main scene and plugin switches.
    let sha = settings_sha(&mut session, &mut ids, &editor.context());
    let applied = ok(
        &session.call_tool(
            ids.next(),
            "project.settings.patch",
            json!({
                "expectedSha256": sha,
                "operations": [
                    { "op": "set_input_action", "name": "jump", "events": [
                        { "type": "key", "key": "Space" }
                    ] },
                    { "op": "set_autoload", "name": "Game", "path": "res://autoload/game.gd" },
                    { "op": "set_main_scene", "scene": "res://main.tscn" },
                    { "op": "enable_plugin", "plugin": "res://addons/other/plugin.cfg" }
                ],
                "idempotencyId": "live-four"
            }),
        ),
        &editor.context(),
    );
    assert_eq!(applied["backend"], "editor", "{applied}");
    let settings = ok(
        &session.call_tool(ids.next(), "project.settings.get", json!({})),
        &editor.context(),
    );
    assert_eq!(settings["main_scene"], "res://main.tscn", "{settings}");
    assert_eq!(
        settings["autoloads"],
        json!([{ "name": "Game", "path": "res://autoload/game.gd", "singleton": true }]),
        "{settings}"
    );
    assert_eq!(settings["input_actions"][0]["name"], "jump", "{settings}");
    assert_eq!(
        settings["input_actions"][0]["events"][0]["key"], "Space",
        "{settings}"
    );
    assert!(
        settings["enabled_plugins"]
            .to_string()
            .contains("res://addons/other/plugin.cfg"),
        "{settings}"
    );
    editor_saves_settings(&tree, &editor);
    let after = manifest(&tree);
    for expected in [
        "jump=",
        "Game=\"*res://autoload/game.gd\"",
        "res://main.tscn",
        "res://addons/other/plugin.cfg",
    ] {
        assert!(
            after.contains(expected),
            "{}: the editor's next save dropped {expected}:\n{after}",
            editor.binary
        );
    }
    // Removing the autoload goes through the editor as well, and stays removed.
    let sha = settings_sha(&mut session, &mut ids, &editor.context());
    let removed = ok(
        &session.call_tool(
            ids.next(),
            "project.settings.patch",
            json!({
                "expectedSha256": sha,
                "operations": [{ "op": "remove_autoload", "name": "Game" }],
                "idempotencyId": "live-remove-autoload"
            }),
        ),
        &editor.context(),
    );
    assert_eq!(removed["backend"], "editor", "{removed}");
    let settings = ok(
        &session.call_tool(ids.next(), "project.settings.get", json!({})),
        &editor.context(),
    );
    assert_eq!(settings["autoloads"], json!([]), "{settings}");
    editor_saves_settings(&tree, &editor);
    assert!(
        !manifest(&tree).contains("Game="),
        "{}: the removed autoload came back:\n{}",
        editor.binary,
        manifest(&tree)
    );
    editor.assert_no_errors_since(mark, "changing project settings");
    let _ = session.finish();
    drop(editor);
}

/// A project whose only editor plugin is godot_mcp, installed through the
/// file path before any editor runs.
fn project_with_addon(label: &str, files: &[(&str, &str)]) -> (ProjectTree, StdioSession, Ids) {
    let tree = ProjectTree::new(label);
    tree.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"Live\"\n",
    );
    for (path, contents) in files {
        tree.write(path, contents);
    }
    let mut session = open(&tree);
    let mut ids = Ids(1);
    let installed = session.call_tool(
        ids.next(),
        "editor.addon.install",
        json!({ "enable": true, "idempotencyId": "live-install" }),
    );
    assert!(installed.get("error").is_none(), "{installed}");
    (tree, session, ids)
}

const MOVER: &str = "class_name LiveMover\nextends CharacterBody2D\n\n@export var speed := 10.0\n";

const READ_SCENE: &str = "[gd_scene load_steps=2 format=3]\n\n\
    [ext_resource type=\"Script\" path=\"res://scripts/mover.gd\" id=\"1\"]\n\n\
    [node name=\"Main\" type=\"Node2D\"]\n\n\
    [node name=\"Player\" type=\"CharacterBody2D\" parent=\".\" groups=[\"players\"]]\n\
    position = Vector2(3, 4)\nscript = ExtResource(\"1\")\nspeed = 25.0\n\
    editor_description = \"esc\\u001b[1m vt\\u000b end\"\n\n\
    [node name=\"Sprite\" type=\"Sprite2D\" parent=\"Player\"]\n";

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn the_editor_reads_nodes_and_classes() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        the_editor_reads_nodes_and_classes_in(binary);
    }
}

fn the_editor_reads_nodes_and_classes_in(binary: &Path) {
    let (tree, mut session, mut ids) = project_with_addon(
        "live-reads",
        &[("scripts/mover.gd", MOVER), ("main.tscn", READ_SCENE)],
    );
    let editor = Editor::launch(binary, &tree.root, &tree.base.join("home"));
    let connected = wait_connected(&mut session, &mut ids, &editor);
    let mark = editor.mark();
    let context = editor.context();
    ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.open",
            json!({ "path": "main.tscn", "idempotencyId": "live-open" }),
        ),
        &context,
    );
    let status = ok(
        &session.call_tool(ids.next(), "editor.status.get", json!({})),
        &context,
    );
    let history_before = status["editor"]["history"].clone();
    let state = &status["editor"];
    assert!(state["filesystem"]["scanning"].is_boolean(), "{status}");
    let importing_api = !["4.4.", "4.5.", "4.6."].iter().any(|old| {
        connected["godot_version"]
            .as_str()
            .is_some_and(|version| version.starts_with(old))
    });
    assert_eq!(
        state["filesystem"]["importing"].is_boolean(),
        importing_api,
        "importing is known from Godot 4.7: {status}"
    );
    assert!(
        ["2D", "3D", "Script", "other"].contains(&state["main_screen"].as_str().unwrap_or("")),
        "{status}"
    );
    assert_eq!(state["playing_scene"], Value::Null, "{status}");
    assert_eq!(
        state["log_capture"],
        json!(has_logger(&connected)),
        "{status}"
    );
    assert_eq!(state["addon"]["update_available"], json!(false), "{status}");

    let scene = ok(
        &session.call_tool(ids.next(), "editor.scene.tree.get", json!({})),
        &context,
    );
    let player = ok(
        &session.call_tool(ids.next(), "editor.node.get", json!({ "node": "Player" })),
        &context,
    );
    assert_eq!(player["digest"], scene["digest"], "{player}");
    assert_eq!(player["node"]["class"], "CharacterBody2D", "{player}");
    assert_eq!(
        player["node"]["script"], "res://scripts/mover.gd",
        "{player}"
    );
    assert_eq!(player["node"]["groups"], json!(["players"]), "{player}");
    assert_eq!(player["node"]["owned"], json!(true), "{player}");
    assert_eq!(
        player["node"]["children"],
        json!([{ "name": "Sprite", "class": "Sprite2D" }]),
        "{player}"
    );
    let property = |name: &str| -> Value {
        player["properties"]
            .as_array()
            .expect("properties")
            .iter()
            .find(|property| property["name"] == name)
            .cloned()
            .unwrap_or_else(|| panic!("{name} is listed: {player}"))
    };
    let stored = scene["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .find(|node| node["path"] == "Player")
        .expect("Player in the tree")["properties"]
        .clone();
    assert_eq!(
        property("position")["value"],
        stored["position"],
        "{player}"
    );
    assert_eq!(property("position")["changed"], json!(true), "{player}");
    assert_eq!(property("position")["type"], "Vector2", "{player}");
    assert_eq!(property("speed")["changed"], json!(true), "{player}");
    // Control characters cross the bridge intact: JSON.stringify leaves them
    // unescaped, which the server would refuse with the whole session.
    assert_eq!(
        property("editor_description")["value"],
        json!({ "type": "string", "value": "esc\u{1b}[1m vt\u{b} end" }),
        "{player}"
    );
    assert_eq!(property("velocity")["changed"], json!(false), "{player}");

    let some = ok(
        &session.call_tool(
            ids.next(),
            "editor.node.get",
            json!({ "node": "Player", "properties": ["position", "no_such_property"] }),
        ),
        &context,
    );
    assert_eq!(
        some["properties"].as_array().map(Vec::len),
        Some(1),
        "{some}"
    );
    assert_eq!(
        some["missing_properties"],
        json!(["no_such_property"]),
        "{some}"
    );
    let missing = session.call_tool(ids.next(), "editor.node.get", json!({ "node": "Nobody" }));
    assert_eq!(error_code(&missing), json!("not_found"), "{missing}");

    let bodies = ok(
        &session.call_tool(
            ids.next(),
            "editor.types.search",
            json!({ "query": "body", "base": "Node2D", "instantiable": true }),
        ),
        &context,
    );
    let names: Vec<&str> = bodies["classes"]
        .as_array()
        .expect("classes")
        .iter()
        .filter_map(|class| class["name"].as_str())
        .collect();
    assert!(names.contains(&"CharacterBody2D"), "{bodies}");
    assert!(
        !names.contains(&"PhysicsBody2D"),
        "an abstract class is not instantiable: {bodies}"
    );
    let scripts = ok(
        &session.call_tool(
            ids.next(),
            "editor.types.search",
            json!({ "query": "mover", "base": "CharacterBody2D" }),
        ),
        &context,
    );
    assert_eq!(
        scripts["classes"],
        json!([{ "name": "LiveMover", "kind": "script", "parent": "CharacterBody2D",
                 "instantiable": true, "path": "res://scripts/mover.gd" }]),
        "{scripts}"
    );

    let history_after = ok(
        &session.call_tool(ids.next(), "editor.status.get", json!({})),
        &context,
    )["editor"]["history"]
        .clone();
    assert_eq!(history_after, history_before, "reads add no undo history");

    // The selection is set, read back from the editor's status and cleared.
    let sorted = |value: &Value| -> Vec<String> {
        let mut paths: Vec<String> = value
            .as_array()
            .unwrap_or_else(|| panic!("a selection list: {value}"))
            .iter()
            .filter_map(|path| path.as_str().map(str::to_owned))
            .collect();
        paths.sort();
        paths
    };
    let selected = ok(
        &session.call_tool(
            ids.next(),
            "editor.selection.set",
            json!({ "nodes": ["Player/Sprite", "Player"], "idempotencyId": "live-select" }),
        ),
        &context,
    );
    assert_eq!(
        sorted(&selected["selection"]),
        ["Player", "Player/Sprite"],
        "{selected}"
    );
    let status = ok(
        &session.call_tool(ids.next(), "editor.status.get", json!({})),
        &context,
    );
    assert_eq!(
        sorted(&status["editor"]["selection"]),
        ["Player", "Player/Sprite"],
        "{status}"
    );
    let cleared = ok(
        &session.call_tool(
            ids.next(),
            "editor.selection.set",
            json!({ "nodes": [], "idempotencyId": "live-clear" }),
        ),
        &context,
    );
    assert_eq!(cleared["selection"], json!([]), "{cleared}");
    let status = ok(
        &session.call_tool(ids.next(), "editor.status.get", json!({})),
        &context,
    );
    assert_eq!(status["editor"]["selection"], json!([]), "{status}");
    let nobody = session.call_tool(
        ids.next(),
        "editor.selection.set",
        json!({ "nodes": ["Nobody"], "idempotencyId": "live-select-nobody" }),
    );
    assert_eq!(error_code(&nobody), json!("not_found"), "{nobody}");
    editor.assert_no_errors_since(mark, "reading nodes and classes");
    let _ = session.finish();
    drop(editor);
}

const EDIT_SCENE: &str = "[gd_scene format=3]\n\n[node name=\"Main\" type=\"Node2D\"]\n\n\
    [node name=\"Player\" type=\"Node2D\" parent=\".\"]\n";

fn digest_of(session: &mut StdioSession, ids: &mut Ids, context: &str) -> String {
    ok(
        &session.call_tool(ids.next(), "editor.scene.tree.get", json!({})),
        context,
    )["digest"]
        .as_str()
        .expect("digest")
        .to_owned()
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn the_editor_edits_the_scene_it_holds_and_saves_it() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        the_editor_edits_the_scene_it_holds_and_saves_it_in(binary);
    }
}

fn the_editor_edits_the_scene_it_holds_and_saves_it_in(binary: &Path) {
    let (tree, mut session, mut ids) =
        project_with_addon("live-edit", &[("main.tscn", EDIT_SCENE)]);
    let editor = Editor::launch(binary, &tree.root, &tree.base.join("home"));
    wait_connected(&mut session, &mut ids, &editor);
    let mark = editor.mark();
    let context = editor.context();
    ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.open",
            json!({ "path": "main.tscn", "idempotencyId": "edit-open" }),
        ),
        &context,
    );
    let original = digest_of(&mut session, &mut ids, &context);
    let file_sha = sha256_hex(EDIT_SCENE.as_bytes());

    let edited = ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.edit",
            json!({
                "expectedDigest": original,
                "operations": [
                    { "op": "add_node", "parent": ".", "name": "Live", "type": "Node2D" },
                    { "op": "set_property", "node": "Live", "property": "position",
                      "value": { "type": "vector2", "value": { "x": 5.0, "y": 6.0 } } }
                ],
                "label": "add Live",
                "idempotencyId": "edit-1"
            }),
        ),
        &context,
    );
    let changed = edited["digest"].as_str().expect("digest").to_owned();
    assert_ne!(changed, original, "{edited}");
    assert_eq!(edited["action"], "godot-mcp: add Live", "{edited}");
    assert_eq!(digest_of(&mut session, &mut ids, &context), changed);
    assert_eq!(
        sha256_hex(&std::fs::read(tree.root.join("main.tscn")).expect("scene")),
        file_sha,
        "{}: a live edit saves nothing",
        editor.binary
    );

    // An edit against a digest the editor no longer holds changes nothing.
    let stale = session.call_tool(
        ids.next(),
        "editor.scene.edit",
        json!({
            "expectedDigest": original,
            "operations": [{ "op": "remove_node", "node": "Player" }],
            "idempotencyId": "edit-stale"
        }),
    );
    assert_eq!(error_code(&stale), json!("conflict"), "{stale}");
    assert_eq!(digest_of(&mut session, &mut ids, &context), changed);

    // A headless batch refuses the scene the editor holds changes to.
    let batch = session.call_tool(
        ids.next(),
        "scene.batch.apply",
        json!({
            "path": "main.tscn",
            "expectedSha256": file_sha,
            "operations": [{ "op": "add_node", "parent": ".", "name": "Other", "type": "Node" }],
            "idempotencyId": "edit-batch"
        }),
    );
    assert_eq!(error_code(&batch), json!("conflict"), "{batch}");
    assert!(batch.to_string().contains("editor.scene.edit"), "{batch}");

    // The edit is one undoable action.
    let undone = ok(
        &session.call_tool(
            ids.next(),
            "editor.undo",
            json!({ "idempotencyId": "edit-undo" }),
        ),
        &context,
    );
    assert_eq!(undone["digest"], json!(original), "{undone}");
    let redone = ok(
        &session.call_tool(
            ids.next(),
            "editor.redo",
            json!({ "idempotencyId": "edit-redo" }),
        ),
        &context,
    );
    assert_eq!(redone["digest"], json!(changed), "{redone}");

    let saved = ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.save",
            json!({ "idempotencyId": "edit-save" }),
        ),
        &context,
    );
    let bytes = std::fs::read(tree.root.join("main.tscn")).expect("scene");
    assert_eq!(saved["saved"][0]["path"], "res://main.tscn", "{saved}");
    assert_eq!(
        saved["saved"][0]["sha256"],
        json!(sha256_hex(&bytes)),
        "{saved}"
    );
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("name=\"Live\"") && text.contains("Vector2(5, 6)"),
        "{}: {text}",
        editor.binary
    );
    let tree_after = ok(
        &session.call_tool(ids.next(), "editor.scene.tree.get", json!({})),
        &context,
    );
    assert_eq!(tree_after["digest"], json!(changed), "{tree_after}");
    assert_eq!(tree_after["unsaved"], json!(false), "{tree_after}");
    editor.assert_no_errors_since(mark, "editing and saving the scene");
    let _ = session.finish();
    drop(editor);
}

/// One digest case: its name, the scene before and after the change, and the
/// other project files both scenes need, by path.
type DigestPair = (&'static str, String, String, Vec<(&'static str, String)>);

/// Make `scene` the scene being edited and add a node `name` to it, unsaved.
fn edit_open_scene(
    session: &mut StdioSession,
    ids: &mut Ids,
    editor: &Editor,
    scene: &str,
    name: &str,
) {
    ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.open",
            json!({ "path": scene, "idempotencyId": format!("open-{scene}-{name}") }),
        ),
        &editor.context(),
    );
    let digest = digest_of(session, ids, &editor.context());
    ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.edit",
            json!({
                "expectedDigest": digest,
                "operations": [{ "op": "add_node", "parent": ".", "name": name, "type": "Node2D" }],
                "idempotencyId": format!("edit-{scene}-{name}")
            }),
        ),
        &editor.context(),
    );
}

/// A scene file and its directory made read-only while the guard lives, and
/// made writable again when it is dropped, so a failing case still cleans up.
struct ReadOnly {
    file: PathBuf,
}

impl ReadOnly {
    /// Panics when this user can still write there: a user who bypasses file
    /// permissions makes a case that relies on them prove nothing.
    fn new(file: PathBuf) -> Self {
        use std::os::unix::fs::PermissionsExt;

        let directory = file.parent().expect("a directory").to_owned();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o444)).expect("chmod");
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o555))
            .expect("chmod");
        let guard = Self { file };
        assert!(
            std::fs::write(directory.join("probe"), b"x").is_err(),
            "this user can still write a read-only directory; run as a user file permissions apply to"
        );
        guard
    }
}

impl Drop for ReadOnly {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;

        if let Some(directory) = self.file.parent() {
            let _ = std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o755));
        }
        let _ = std::fs::set_permissions(&self.file, std::fs::Permissions::from_mode(0o644));
    }
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn saving_every_scene_proves_each_save() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        saving_every_scene_proves_each_save_in(binary);
    }
}

/// `editor.scene.save` with `all` succeeds only when every open scene was
/// written or the editor reports it saved; a scene the editor could not
/// write makes the answer uncertain.
fn saving_every_scene_proves_each_save_in(binary: &Path) {
    let (tree, mut session, mut ids) = project_with_addon(
        "live-save-all",
        &[
            ("clean.tscn", EDIT_SCENE),
            ("one/one.tscn", EDIT_SCENE),
            ("two/two.tscn", EDIT_SCENE),
        ],
    );
    let editor = Editor::launch(binary, &tree.root, &tree.base.join("home"));
    wait_connected(&mut session, &mut ids, &editor);
    ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.open",
            json!({ "path": "clean.tscn", "idempotencyId": "save-all-open-clean" }),
        ),
        &editor.context(),
    );
    edit_open_scene(&mut session, &mut ids, &editor, "one/one.tscn", "First");
    edit_open_scene(&mut session, &mut ids, &editor, "two/two.tscn", "First");
    let saved = ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.save",
            json!({ "all": true, "idempotencyId": "save-all" }),
        ),
        &editor.context(),
    );
    for scene in ["one/one.tscn", "two/two.tscn"] {
        let bytes = std::fs::read(tree.root.join(scene)).expect("scene");
        assert!(
            saved["saved"]
                .as_array()
                .is_some_and(|entries| entries.contains(
                    &json!({ "path": format!("res://{scene}"), "sha256": sha256_hex(&bytes) })
                )),
            "{}: {scene} is listed with its saved bytes: {saved}",
            editor.binary
        );
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            text.contains("name=\"First\""),
            "{}: {scene}: {text}",
            editor.binary
        );
    }

    // A scene whose file and directory cannot be written is not saved.
    edit_open_scene(&mut session, &mut ids, &editor, "one/one.tscn", "Second");
    edit_open_scene(&mut session, &mut ids, &editor, "two/two.tscn", "Second");
    let refused = {
        let _locked = ReadOnly::new(tree.root.join("two/two.tscn"));
        session.call_tool(
            ids.next(),
            "editor.scene.save",
            json!({ "all": true, "idempotencyId": "save-all-locked" }),
        )
    };
    assert_eq!(error_code(&refused), json!("uncertain"), "{refused}");
    assert!(
        !std::fs::read_to_string(tree.root.join("two/two.tscn"))
            .expect("scene")
            .contains("name=\"Second\""),
        "{}: the read-only scene was not written",
        editor.binary
    );
    assert!(
        std::fs::read_to_string(tree.root.join("one/one.tscn"))
            .expect("scene")
            .contains("name=\"Second\""),
        "{}: the scene that could be written was saved",
        editor.binary
    );
    // What the read-only scene's file holds is in doubt, so writes stop.
    let blocked = session.call_tool(
        ids.next(),
        "file.create",
        json!({ "path": "notes/after.txt", "content": "x\n", "idempotencyId": "save-all-after" }),
    );
    assert!(
        blocked.get("error").is_some() || blocked["result"]["isError"] == json!(true),
        "{}: a write after an uncertain save is refused: {blocked}",
        editor.binary
    );
    let _ = session.finish();
    drop(editor);
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn a_batch_the_editor_cannot_save_is_undone() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        a_batch_the_editor_cannot_save_is_undone_in(binary);
    }
}

/// A scene batch the editor applies but cannot save is undone, and the
/// answer says nothing changed only once the file is proven as it was.
fn a_batch_the_editor_cannot_save_is_undone_in(binary: &Path) {
    let (tree, mut session, mut ids) =
        project_with_addon("live-batch-locked", &[("locked/scene.tscn", EDIT_SCENE)]);
    let editor = Editor::launch(binary, &tree.root, &tree.base.join("home"));
    wait_connected(&mut session, &mut ids, &editor);
    ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.open",
            json!({ "path": "locked/scene.tscn", "idempotencyId": "batch-locked-open" }),
        ),
        &editor.context(),
    );
    let original = digest_of(&mut session, &mut ids, &editor.context());
    let file = tree.root.join("locked/scene.tscn");
    let bytes = std::fs::read(&file).expect("scene");
    let refused = {
        let _locked = ReadOnly::new(file.clone());
        session.call_tool(
            ids.next(),
            "scene.batch.apply",
            json!({
                "path": "locked/scene.tscn",
                "expectedSha256": sha256_hex(&bytes),
                "operations": [{ "op": "add_node", "parent": ".", "name": "Batched", "type": "Node2D" }],
                "idempotencyId": "batch-locked"
            }),
        )
    };
    assert_eq!(error_code(&refused), json!("unavailable"), "{refused}");
    assert!(
        refused.to_string().contains("was undone"),
        "{}: {refused}",
        editor.binary
    );
    assert_eq!(
        digest_of(&mut session, &mut ids, &editor.context()),
        original,
        "{}: the editor holds the scene as before",
        editor.binary
    );
    assert_eq!(std::fs::read(&file).expect("scene"), bytes);
    ok(
        &session.call_tool(
            ids.next(),
            "file.create",
            json!({ "path": "notes/after.txt", "content": "x\n", "idempotencyId": "batch-locked-after" }),
        ),
        &editor.context(),
    );
    let _ = session.finish();
    drop(editor);
}

/// Two scene files that differ in one place the old bounded digest did not
/// reach: past the first 2000 nodes, past a node's first 256 properties, and
/// three sub-resources deep.
fn digest_pairs() -> Vec<DigestPair> {
    let mut many = String::from("[gd_scene format=3]\n\n[node name=\"Main\" type=\"Node2D\"]\n");
    let mut many_changed = many.clone();
    for index in 0..2100 {
        let node = format!("\n[node name=\"N{index}\" type=\"Node2D\" parent=\".\"]\n");
        many.push_str(&node);
        many_changed.push_str(&node);
        if index == 2049 {
            many_changed.push_str("position = Vector2(1, 2)\n");
        }
    }
    let mut wide_script = String::from("extends Node\n\n");
    for index in 0..300 {
        wide_script.push_str(&format!("@export var p{index} := 0\n"));
    }
    let wide = |value: i64| {
        format!(
            "[gd_scene load_steps=2 format=3]\n\n\
             [ext_resource type=\"Script\" path=\"res://wide.gd\" id=\"1\"]\n\n\
             [node name=\"Main\" type=\"Node\"]\nscript = ExtResource(\"1\")\np279 = {value}\n"
        )
    };
    let deep = |value: i64| {
        format!(
            "[gd_scene load_steps=6 format=3]\n\n\
             [ext_resource type=\"Script\" path=\"res://deep.gd\" id=\"1\"]\n\
             [ext_resource type=\"Script\" path=\"res://holder.gd\" id=\"2\"]\n\n\
             [sub_resource type=\"Resource\" id=\"L3\"]\nscript = ExtResource(\"1\")\nvalue = {value}\n\n\
             [sub_resource type=\"Resource\" id=\"L2\"]\nscript = ExtResource(\"1\")\nchild = SubResource(\"L3\")\n\n\
             [sub_resource type=\"Resource\" id=\"L1\"]\nscript = ExtResource(\"1\")\nchild = SubResource(\"L2\")\n\n\
             [node name=\"Main\" type=\"Node\"]\nscript = ExtResource(\"2\")\ndata = SubResource(\"L1\")\n"
        )
    };
    vec![
        ("many", many, many_changed, Vec::new()),
        ("wide", wide(1), wide(7), vec![("wide.gd", wide_script)]),
        (
            "deep",
            deep(1),
            deep(2),
            vec![
                (
                    "deep.gd",
                    "extends Resource\n\n@export var child: Resource\n@export var value := 0\n"
                        .to_owned(),
                ),
                (
                    "holder.gd",
                    "extends Node\n\n@export var data: Resource\n".to_owned(),
                ),
            ],
        ),
    ]
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn the_scene_digest_covers_the_whole_scene() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        the_scene_digest_covers_the_whole_scene_in(binary);
    }
}

fn the_scene_digest_covers_the_whole_scene_in(binary: &Path) {
    let pairs = digest_pairs();
    let mut files: Vec<(String, String)> = Vec::new();
    for (name, before, after, extra) in &pairs {
        files.push((format!("{name}_a.tscn"), before.clone()));
        files.push((format!("{name}_b.tscn"), after.clone()));
        for (path, contents) in extra {
            files.push(((*path).to_owned(), contents.clone()));
        }
    }
    let borrowed: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, contents)| (path.as_str(), contents.as_str()))
        .collect();
    let (tree, mut session, mut ids) = project_with_addon("live-digest", &borrowed);
    let editor = Editor::launch(binary, &tree.root, &tree.base.join("home"));
    wait_connected(&mut session, &mut ids, &editor);
    let mark = editor.mark();
    let context = editor.context();
    let mut digest = |scene: &str| -> String {
        ok(
            &session.call_tool(
                ids.next(),
                "editor.scene.open",
                json!({ "path": scene, "idempotencyId": format!("digest-open-{scene}-{}", ids.0) }),
            ),
            &context,
        );
        let digest = digest_of(&mut session, &mut ids, &context);
        editor.assert_no_errors_since(mark, &format!("opening and digesting {scene}"));
        digest
    };
    for (name, _, _, _) in &pairs {
        let first = digest(&format!("{name}_a.tscn"));
        let changed = digest(&format!("{name}_b.tscn"));
        let again = digest(&format!("{name}_a.tscn"));
        assert_eq!(
            first, again,
            "{}: {name}: the digest is deterministic",
            editor.binary
        );
        assert_ne!(
            first, changed,
            "{}: {name}: the change is in the digest",
            editor.binary
        );
    }
    // A node with more children than one answer lists says it is truncated.
    ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.open",
            json!({ "path": "many_a.tscn", "idempotencyId": "digest-open-many-node" }),
        ),
        &context,
    );
    let main = ok(
        &session.call_tool(ids.next(), "editor.node.get", json!({ "node": "." })),
        &context,
    );
    assert_eq!(
        main["node"]["child_count"],
        json!(2100),
        "{}",
        editor.binary
    );
    assert_eq!(
        main["node"]["children"].as_array().map(Vec::len),
        Some(256),
        "{}",
        editor.binary
    );
    assert_eq!(
        main["truncated"],
        json!(true),
        "{}: a node whose children are not all listed is truncated",
        editor.binary
    );
    editor.assert_no_errors_since(mark, "computing scene digests");
    let _ = session.finish();
    drop(editor);
}

/// A test-only editor plugin that prints a line, then pushes a warning and an
/// error, when `probe_log` appears.
const LOG_PROBE: &str = r#"@tool
extends EditorPlugin

func _process(_delta: float) -> void:
	if FileAccess.file_exists("res://probe_log"):
		DirAccess.remove_absolute(ProjectSettings.globalize_path("res://probe_log"))
		print("probe-print")
		push_warning("probe-warning")
		push_error("probe-error")
		FileAccess.open("res://probe_logged", FileAccess.WRITE).store_string("logged")
"#;

/// Whether the connected editor's Godot, as `editor.status.get` reports it,
/// has the `Logger` class, which Godot 4.5 added.
fn has_logger(status: &Value) -> bool {
    !status["godot_version"]
        .as_str()
        .expect("the connected editor's Godot version")
        .starts_with("4.4.")
}

fn log_read(session: &mut StdioSession, ids: &mut Ids, arguments: Value, context: &str) -> Value {
    ok(
        &session.call_tool(ids.next(), "editor.log.get", arguments),
        context,
    )
}

fn entry_mentioning<'a>(page: &'a Value, text: &str) -> Option<&'a Value> {
    page["entries"].as_array()?.iter().find(|entry| {
        entry["text"]
            .as_str()
            .is_some_and(|logged| logged.contains(text))
            || entry["code"]
                .as_str()
                .is_some_and(|logged| logged.contains(text))
    })
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn the_editor_log_is_read_from_a_cursor() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        the_editor_log_is_read_from_a_cursor_in(binary);
    }
}

fn the_editor_log_is_read_from_a_cursor_in(binary: &Path) {
    let (tree, mut session, mut ids) = project_with_addon(
        "live-log",
        &[
            (
                "addons/log_probe/plugin.cfg",
                "[plugin]\n\nname=\"log_probe\"\ndescription=\"\"\nauthor=\"\"\nversion=\"1\"\nscript=\"plugin.gd\"\n",
            ),
            ("addons/log_probe/plugin.gd", LOG_PROBE),
        ],
    );
    let before_launch = binary.display().to_string();
    let sha = settings_sha(&mut session, &mut ids, &before_launch);
    ok(
        &session.call_tool(
            ids.next(),
            "project.settings.patch",
            json!({
                "expectedSha256": sha,
                "operations": [{ "op": "enable_plugin", "plugin": "res://addons/log_probe/plugin.cfg" }],
                "idempotencyId": "live-enable-log-probe"
            }),
        ),
        &before_launch,
    );
    let editor = Editor::launch(binary, &tree.root, &tree.base.join("home"));
    let status = wait_connected(&mut session, &mut ids, &editor);

    if !has_logger(&status) {
        let refused = session.call_tool(ids.next(), "editor.log.get", json!({}));
        assert_eq!(error_code(&refused), json!("unsupported"), "{refused}");
        assert!(refused.to_string().contains("4.5"), "{refused}");
        let output = editor.output();
        assert!(
            !output.lines().any(|line| line.contains("Parse Error")
                || line.contains("addons/godot_mcp") && line.contains("ERROR")),
            "{}: the add-on must load on a Godot without the Logger class:\n{output}",
            editor.binary
        );
        let _ = session.finish();
        drop(editor);
        return;
    }

    let start = log_read(&mut session, &mut ids, json!({}), &editor.context());
    let epoch = start["epoch"].as_str().expect("an epoch").to_owned();
    let cursor = start["next"].as_u64().expect("a cursor");
    assert_eq!(start["capacity"], json!(2000), "{start}");

    let _ = std::fs::remove_file(tree.root.join("probe_logged"));
    std::fs::write(tree.root.join("probe_log"), b"log").expect("log marker");
    wait_for_file(&tree.root.join("probe_logged"), &editor);

    let page = log_read(
        &mut session,
        &mut ids,
        json!({ "after": cursor, "epoch": epoch }),
        &editor.context(),
    );
    assert_eq!(page["reset"], json!(false), "{page}");
    let printed = entry_mentioning(&page, "probe-print").unwrap_or_else(|| panic!("{page}"));
    assert_eq!(printed["kind"], "message", "{printed}");
    let warned = entry_mentioning(&page, "probe-warning").unwrap_or_else(|| panic!("{page}"));
    assert_eq!(warned["kind"], "warning", "{warned}");
    let failed = entry_mentioning(&page, "probe-error").unwrap_or_else(|| panic!("{page}"));
    assert_eq!(failed["kind"], "error", "{failed}");
    assert!(
        failed["backtrace"]
            .as_array()
            .is_some_and(|frames| frames.iter().any(|frame| {
                frame["file"] == "res://addons/log_probe/plugin.gd"
                    && frame["function"] == "_process"
            })),
        "{}: the error carries the script frame that pushed it: {failed}",
        editor.binary
    );
    let sequences: Vec<u64> = page["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .map(|entry| entry["sequence"].as_u64().expect("sequence"))
        .collect();
    assert!(
        sequences.windows(2).all(|pair| pair[0] < pair[1]) && sequences[0] > cursor,
        "entries come after the cursor, oldest first: {sequences:?}"
    );

    // The next read starts where this one ended: nothing repeats.
    let after = log_read(
        &mut session,
        &mut ids,
        json!({ "after": page["next"], "epoch": epoch }),
        &editor.context(),
    );
    assert!(entry_mentioning(&after, "probe-").is_none(), "{after}");

    // A kind filter keeps only those kinds.
    let errors = log_read(
        &mut session,
        &mut ids,
        json!({ "after": cursor, "epoch": epoch, "kinds": ["error"] }),
        &editor.context(),
    );
    assert!(
        entry_mentioning(&errors, "probe-error").is_some(),
        "{errors}"
    );
    assert!(
        errors["entries"]
            .as_array()
            .expect("entries")
            .iter()
            .all(|entry| entry["kind"] == "error"),
        "{errors}"
    );

    // A restarted plugin keeps a new log: the old cursor starts it over.
    ok(
        &session.call_tool(
            ids.next(),
            "editor.addon.reload",
            json!({ "idempotencyId": "live-log-reload" }),
        ),
        &editor.context(),
    );
    let started = Instant::now();
    let restarted = loop {
        let response = session.call_tool(
            ids.next(),
            "editor.log.get",
            json!({ "after": after["next"], "epoch": epoch }),
        );
        if response.get("error").is_none() && response["result"]["isError"] != json!(true) {
            let page = response["result"]["structuredContent"].clone();
            if page["epoch"] != json!(epoch) {
                break page;
            }
        }
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "{}: the reloaded plugin never answered with a new epoch: {response}\n{}",
            editor.binary,
            editor.output()
        );
        std::thread::sleep(Duration::from_millis(250));
    };
    assert_eq!(restarted["reset"], json!(true), "{restarted}");
    let _ = session.finish();
    drop(editor);
}

const SHOWN_SCENE: &str = "[gd_scene format=3]\n\n[node name=\"Main\" type=\"Node2D\"]\n\n\
    [node name=\"Block\" type=\"Polygon2D\" parent=\".\"]\n\
    color = Color(1, 0, 0, 1)\n\
    polygon = PackedVector2Array(0, 0, 400, 0, 400, 300, 0, 300)\n";

fn main_screen(session: &mut StdioSession, ids: &mut Ids, context: &str) -> Value {
    ok(
        &session.call_tool(ids.next(), "editor.status.get", json!({})),
        context,
    )["editor"]["main_screen"]
        .clone()
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn the_main_screen_switches_and_a_headless_editor_captures_nothing() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        the_main_screen_switches_and_a_headless_editor_captures_nothing_in(binary);
    }
}

fn the_main_screen_switches_and_a_headless_editor_captures_nothing_in(binary: &Path) {
    let (tree, mut session, mut ids) =
        project_with_addon("live-screens", &[("main.tscn", SHOWN_SCENE)]);
    let editor = Editor::launch(binary, &tree.root, &tree.base.join("home"));
    wait_connected(&mut session, &mut ids, &editor);
    let mark = editor.mark();
    for screen in ["3D", "Script", "2D"] {
        let switched = ok(
            &session.call_tool(
                ids.next(),
                "editor.main_screen.set",
                json!({ "screen": screen, "idempotencyId": format!("live-screen-{screen}") }),
            ),
            &editor.context(),
        );
        assert_eq!(switched["main_screen"], screen, "{switched}");
        assert_eq!(
            main_screen(&mut session, &mut ids, &editor.context()),
            json!(screen),
            "{}: the status shows the screen switched to",
            editor.binary
        );
    }
    let refused = session.call_tool(
        ids.next(),
        "editor.screenshot.capture",
        json!({ "idempotencyId": "live-headless-shot" }),
    );
    assert_eq!(error_code(&refused), json!("unavailable"), "{refused}");
    assert!(refused.to_string().contains("headless"), "{refused}");
    editor.assert_no_errors_since(mark, "switching main screens");
    let _ = session.finish();
    drop(editor);
}

/// The width and height a PNG's IHDR chunk records.
fn png_size(bytes: &[u8]) -> (u32, u32) {
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "a PNG signature");
    assert_eq!(&bytes[12..16], b"IHDR", "IHDR comes first");
    let word = |at: usize| u32::from_be_bytes(bytes[at..at + 4].try_into().expect("four bytes"));
    (word(16), word(20))
}

fn decode_base64(text: &str) -> Vec<u8> {
    let value = |byte: u8| -> u32 {
        u32::from(match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => panic!("not base64: {byte}"),
        })
    };
    let mut out = Vec::new();
    for quad in text.as_bytes().chunks(4) {
        let padding = quad.iter().rev().take_while(|byte| **byte == b'=').count();
        let mut word = 0_u32;
        for (position, byte) in quad.iter().enumerate() {
            word = (word << 6)
                | if position >= 4 - padding {
                    0
                } else {
                    value(*byte)
                };
        }
        out.extend_from_slice(&[(word >> 16) as u8, (word >> 8) as u8, word as u8][..3 - padding]);
    }
    out
}

/// One capture, read back through its artifact resource.
fn capture(
    session: &mut StdioSession,
    ids: &mut Ids,
    arguments: Value,
    editor: &Editor,
) -> (Value, Vec<u8>) {
    let shot = ok(
        &session.call_tool(ids.next(), "editor.screenshot.capture", arguments),
        &editor.context(),
    );
    let artifact = shot["artifact"].clone();
    let read = session.request(
        ids.next(),
        "resources/read",
        json!({ "uri": artifact["uri"] }),
    );
    let bytes = decode_base64(
        read["result"]["contents"][0]["blob"]
            .as_str()
            .unwrap_or_else(|| panic!("the artifact reads back: {read}")),
    );
    assert_eq!(
        bytes.len() as u64,
        artifact["byte_length"].as_u64().expect("length")
    );
    assert_eq!(
        sha256_hex(&bytes),
        artifact["sha256"].as_str().expect("digest")
    );
    let (width, height) = png_size(&bytes);
    assert_eq!(
        (json!(width), json!(height)),
        (artifact["width"].clone(), artifact["height"].clone()),
        "{}: the PNG is as large as the answer says",
        editor.binary
    );
    (artifact, bytes)
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn a_windowed_editor_captures_what_it_shows() {
    if std::env::var("GODOT_MCP_DISPLAY_REQUIRED").as_deref() != Ok("1") {
        eprintln!(
            "GODOT_MCP_DISPLAY_REQUIRED is not 1; the windowed-editor captures were not checked"
        );
        return;
    }
    assert!(
        std::env::var_os("DISPLAY").is_some(),
        "GODOT_MCP_DISPLAY_REQUIRED=1 needs a display: run under xvfb-run"
    );
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        a_windowed_editor_captures_what_it_shows_in(binary);
    }
}

fn a_windowed_editor_captures_what_it_shows_in(binary: &Path) {
    let (tree, mut session, mut ids) =
        project_with_addon("live-capture", &[("main.tscn", SHOWN_SCENE)]);
    let editor = Editor::launch_windowed(binary, &tree.root, &tree.base.join("home"));
    wait_connected(&mut session, &mut ids, &editor);
    ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.open",
            json!({ "path": "main.tscn", "idempotencyId": "capture-open" }),
        ),
        &editor.context(),
    );
    ok(
        &session.call_tool(
            ids.next(),
            "editor.main_screen.set",
            json!({ "screen": "2D", "idempotencyId": "capture-2d" }),
        ),
        &editor.context(),
    );

    let (window, _) = capture(
        &mut session,
        &mut ids,
        json!({ "idempotencyId": "capture-window" }),
        &editor,
    );
    assert!(
        window["width"].as_u64().is_some_and(|width| width > 0),
        "{window}"
    );
    let (_, red) = capture(
        &mut session,
        &mut ids,
        json!({ "view": "2d", "idempotencyId": "capture-red" }),
        &editor,
    );
    let (scaled, _) = capture(
        &mut session,
        &mut ids,
        json!({ "view": "2d", "maxWidth": 200, "idempotencyId": "capture-scaled" }),
        &editor,
    );
    assert_eq!(scaled["width"], json!(200), "{scaled}");

    // The 2D capture shows the scene: recolouring the block changes it.
    let digest = ok(
        &session.call_tool(ids.next(), "editor.scene.tree.get", json!({})),
        &editor.context(),
    )["digest"]
        .clone();
    ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.edit",
            json!({
                "expectedDigest": digest,
                "operations": [{
                    "op": "set_property", "node": "Block", "property": "color",
                    "value": { "type": "color", "value": { "red": 0.0, "green": 0.0, "blue": 1.0, "alpha": 1.0 } }
                }],
                "idempotencyId": "capture-recolour"
            }),
        ),
        &editor.context(),
    );
    let (_, blue) = capture(
        &mut session,
        &mut ids,
        json!({ "view": "2d", "idempotencyId": "capture-blue" }),
        &editor,
    );
    assert_ne!(
        sha256_hex(&red),
        sha256_hex(&blue),
        "{}: the 2D capture shows the scene's content",
        editor.binary
    );

    // A 3D viewport is captured only while the 3D screen is shown.
    let hidden = session.call_tool(
        ids.next(),
        "editor.screenshot.capture",
        json!({ "view": "3d", "idempotencyId": "capture-3d-hidden" }),
    );
    assert_eq!(error_code(&hidden), json!("unavailable"), "{hidden}");
    ok(
        &session.call_tool(
            ids.next(),
            "editor.main_screen.set",
            json!({ "screen": "3D", "idempotencyId": "capture-3d" }),
        ),
        &editor.context(),
    );
    capture(
        &mut session,
        &mut ids,
        json!({ "view": "3d", "idempotencyId": "capture-3d-shown" }),
        &editor,
    );
    let _ = session.finish();
    drop(editor);
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn a_windowed_editor_saves_without_engine_errors() {
    if std::env::var("GODOT_MCP_DISPLAY_REQUIRED").as_deref() != Ok("1") {
        eprintln!(
            "GODOT_MCP_DISPLAY_REQUIRED is not 1; saving in a windowed editor was not checked"
        );
        return;
    }
    assert!(
        std::env::var_os("DISPLAY").is_some(),
        "GODOT_MCP_DISPLAY_REQUIRED=1 needs a display: run under xvfb-run"
    );
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        a_windowed_editor_saves_without_engine_errors_in(binary);
    }
}

/// A windowed editor shows a progress dialog while it saves a scene, and
/// refuses to show one while deferred calls are flushed: a save made from
/// there prints engine errors into the user's Output panel.
fn a_windowed_editor_saves_without_engine_errors_in(binary: &Path) {
    let (tree, mut session, mut ids) =
        project_with_addon("live-save-windowed", &[("main.tscn", EDIT_SCENE)]);
    let editor = Editor::launch_windowed(binary, &tree.root, &tree.base.join("home"));
    wait_connected(&mut session, &mut ids, &editor);
    let mark = editor.mark();
    ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.open",
            json!({ "path": "main.tscn", "idempotencyId": "windowed-open" }),
        ),
        &editor.context(),
    );
    let digest = digest_of(&mut session, &mut ids, &editor.context());
    ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.edit",
            json!({
                "expectedDigest": digest,
                "operations": [{ "op": "add_node", "parent": ".", "name": "Edited", "type": "Node2D" }],
                "idempotencyId": "windowed-edit"
            }),
        ),
        &editor.context(),
    );
    let saved = ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.save",
            json!({ "idempotencyId": "windowed-save" }),
        ),
        &editor.context(),
    );
    let bytes = std::fs::read(tree.root.join("main.tscn")).expect("scene");
    assert_eq!(
        saved["saved"][0]["sha256"],
        json!(sha256_hex(&bytes)),
        "{saved}"
    );
    let batch = ok(
        &session.call_tool(
            ids.next(),
            "scene.batch.apply",
            json!({
                "path": "main.tscn",
                "expectedSha256": sha256_hex(&bytes),
                "operations": [{ "op": "add_node", "parent": ".", "name": "Batched", "type": "Node2D" }],
                "idempotencyId": "windowed-batch"
            }),
        ),
        &editor.context(),
    );
    assert_eq!(batch["backend"], "editor", "{batch}");
    let text = std::fs::read_to_string(tree.root.join("main.tscn")).expect("scene");
    assert!(
        text.contains("name=\"Edited\"") && text.contains("name=\"Batched\""),
        "{}: {text}",
        editor.binary
    );
    editor.assert_no_errors_since(mark, "saving scenes in a windowed editor");
    let _ = session.finish();
    drop(editor);
}

/// A test-only editor plugin that, when `probe_dirty` appears, makes the open
/// script `scripts/b.gd` the current tab and types into it without saving.
const DIRTY_PROBE: &str = r##"@tool
extends EditorPlugin

var _wait := -1

func _process(_delta: float) -> void:
	if FileAccess.file_exists("res://probe_dirty"):
		DirAccess.remove_absolute(ProjectSettings.globalize_path("res://probe_dirty"))
		EditorInterface.edit_script(load("res://scripts/b.gd"), -1, 0, false)
		_wait = 5
	if _wait > 0:
		_wait -= 1
	elif _wait == 0:
		_wait = -1
		var editor := EditorInterface.get_script_editor()
		var code: CodeEdit = editor.get_current_editor().get_base_editor()
		code.insert_text_at_caret("# unsaved edit\n")
		FileAccess.open("res://probe_dirtied", FileAccess.WRITE).store_string(editor.get_current_script().resource_path)
"##;

const SCRIPT_A: &str = "extends Node\n\nfunc a():\n\tpass\n";
const SCRIPT_B: &str = "extends Node\n\nfunc b():\n\tpass\n";

/// A project with godot_mcp and the test plugin `name` enabled, installed and
/// switched on through the file path before any editor runs.
fn project_with_probe(
    label: &str,
    name: &str,
    source: &str,
    files: &[(&str, &str)],
) -> (ProjectTree, StdioSession, Ids) {
    let cfg = format!(
        "[plugin]\n\nname=\"{name}\"\ndescription=\"\"\nauthor=\"\"\nversion=\"1\"\nscript=\"plugin.gd\"\n"
    );
    let cfg_path = format!("addons/{name}/plugin.cfg");
    let gd_path = format!("addons/{name}/plugin.gd");
    let mut all: Vec<(&str, &str)> = files.to_vec();
    all.push((&cfg_path, &cfg));
    all.push((&gd_path, source));
    let (tree, mut session, mut ids) = project_with_addon(label, &all);
    let sha = settings_sha(&mut session, &mut ids, label);
    ok(
        &session.call_tool(
            ids.next(),
            "project.settings.patch",
            json!({
                "expectedSha256": sha,
                "operations": [{ "op": "enable_plugin", "plugin": format!("res://{cfg_path}") }],
                "idempotencyId": format!("enable-{name}")
            }),
        ),
        label,
    );
    (tree, session, ids)
}

fn open_script_entry(status: &Value, path: &str) -> Value {
    status["editor"]["open_scripts"]
        .as_array()
        .and_then(|scripts| scripts.iter().find(|script| script["path"] == path))
        .cloned()
        .unwrap_or(Value::Null)
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn the_editor_rescans_and_reloads_what_changed() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        the_editor_rescans_and_reloads_what_changed_in(binary);
    }
}

fn the_editor_rescans_and_reloads_what_changed_in(binary: &Path) {
    let (tree, mut session, mut ids) = project_with_probe(
        "live-scan",
        "dirty_probe",
        DIRTY_PROBE,
        &[("scripts/a.gd", SCRIPT_A), ("scripts/b.gd", SCRIPT_B)],
    );
    let editor = Editor::launch(binary, &tree.root, &tree.base.join("home"));
    let connected = wait_connected(&mut session, &mut ids, &editor);

    let opened = ok(
        &session.call_tool(
            ids.next(),
            "editor.script.open",
            json!({ "path": "scripts/a.gd", "line": 3, "column": 2, "idempotencyId": "scan-open-a" }),
        ),
        &editor.context(),
    );
    assert_eq!(
        opened["caret"],
        json!({ "line": 3, "column": 2 }),
        "{}: the caret is where it was asked to be: {opened}",
        editor.binary
    );
    ok(
        &session.call_tool(
            ids.next(),
            "editor.script.open",
            json!({ "path": "scripts/b.gd", "idempotencyId": "scan-open-b" }),
        ),
        &editor.context(),
    );
    std::fs::write(tree.root.join("probe_dirty"), b"dirty").expect("dirty marker");
    wait_for_file(&tree.root.join("probe_dirtied"), &editor);
    assert_eq!(
        std::fs::read_to_string(tree.root.join("probe_dirtied")).expect("dirtied"),
        "res://scripts/b.gd"
    );

    // Both files change outside the editor, and a new script class appears.
    std::fs::write(
        tree.root.join("scripts/a.gd"),
        "extends Node\n\nfunc a():\n\tprint(\"outside\")\n",
    )
    .expect("rewrite a");
    std::fs::write(
        tree.root.join("scripts/b.gd"),
        "extends Node\n\nfunc b():\n\tprint(\"outside\")\n",
    )
    .expect("rewrite b");
    ok(
        &session.call_tool(
            ids.next(),
            "file.create",
            json!({
                "path": "scripts/late.gd",
                "content": "class_name LateClass\nextends Node\n",
                "idempotencyId": "scan-late-class"
            }),
        ),
        &editor.context(),
    );
    let screen = main_screen(&mut session, &mut ids, &editor.context());
    let before = ok(
        &session.call_tool(ids.next(), "project.info.get", json!({})),
        &editor.context(),
    )["workspace"]["revision"]
        .as_u64()
        .expect("revision");

    let scanned = ok(
        &session.call_tool(
            ids.next(),
            "editor.filesystem.scan",
            json!({ "idempotencyId": "scan-1" }),
        ),
        &editor.context(),
    );
    assert_eq!(scanned["script_reload"], "done", "{scanned}");
    assert_eq!(
        scanned["reloaded_scripts"],
        json!(["res://scripts/a.gd"]),
        "{scanned}"
    );
    assert_eq!(
        scanned["stale_scripts"],
        json!([{ "path": "res://scripts/b.gd", "reason": "unsaved" }]),
        "{}: an unsaved tab keeps its edit: {scanned}",
        editor.binary
    );
    assert_eq!(
        scanned["diagnostics"].is_array(),
        has_logger(&connected),
        "{scanned}"
    );
    assert!(
        scanned["workspace"]["revision"].as_u64().expect("revision") > before,
        "the scan's answer has a newer revision: {scanned}"
    );
    assert!(
        tree.root.join("scripts/late.gd.uid").is_file(),
        "{}: the editor gave the new script a UID",
        editor.binary
    );
    assert_eq!(
        main_screen(&mut session, &mut ids, &editor.context()),
        screen,
        "the scan leaves the main screen as it was"
    );
    let late = ok(
        &session.call_tool(
            ids.next(),
            "editor.types.search",
            json!({ "query": "LateClass" }),
        ),
        &editor.context(),
    );
    assert!(
        late["classes"]
            .as_array()
            .is_some_and(|classes| classes.iter().any(|class| class["name"] == "LateClass")),
        "{}: the new script class is registered: {late}",
        editor.binary
    );

    let status = ok(
        &session.call_tool(ids.next(), "editor.status.get", json!({})),
        &editor.context(),
    );
    // Before Godot 4.7 the editor cannot say whether a tab is unsaved; the
    // plugin knows for a tab it proved is the script's when it reloaded it.
    assert_eq!(
        open_script_entry(&status, "res://scripts/a.gd")["unsaved"],
        json!(false),
        "{status}"
    );
    assert_eq!(
        open_script_entry(&status, "res://scripts/b.gd")["unsaved"],
        json!(true),
        "{status}"
    );
    // The reloaded script is no longer held: a headless change goes ahead.
    let a_now = std::fs::read(tree.root.join("scripts/a.gd")).expect("a");
    ok(
        &session.call_tool(
            ids.next(),
            "file.patch",
            json!({
                "path": "scripts/a.gd",
                "expectedSha256": sha256_hex(&a_now),
                "change": { "content": "extends Node\n\nfunc a():\n\tprint(\"headless\")\n" },
                "idempotencyId": "scan-after-a"
            }),
        ),
        &editor.context(),
    );
    let _ = session.finish();
    drop(editor);
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn a_scene_save_writes_the_scripts_the_editor_holds_unsaved() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        a_scene_save_writes_the_scripts_the_editor_holds_unsaved_in(binary);
    }
}

/// A script tab with unsaved edits holds its file against headless writes.
/// Saving a scene saves it too, and lists it, after which the file can be
/// changed headless and the scan reloads the tab: no one has to save it by
/// hand first.
fn a_scene_save_writes_the_scripts_the_editor_holds_unsaved_in(binary: &Path) {
    let (tree, mut session, mut ids) = project_with_probe(
        "live-save-scripts",
        "dirty_probe",
        DIRTY_PROBE,
        &[
            ("main.tscn", EDIT_SCENE),
            ("scripts/a.gd", SCRIPT_A),
            ("scripts/b.gd", SCRIPT_B),
        ],
    );
    let editor = Editor::launch(binary, &tree.root, &tree.base.join("home"));
    wait_connected(&mut session, &mut ids, &editor);
    ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.open",
            json!({ "path": "main.tscn", "idempotencyId": "scripts-open-scene" }),
        ),
        &editor.context(),
    );
    ok(
        &session.call_tool(
            ids.next(),
            "editor.script.open",
            json!({ "path": "scripts/b.gd", "idempotencyId": "scripts-open-b" }),
        ),
        &editor.context(),
    );
    std::fs::write(tree.root.join("probe_dirty"), b"dirty").expect("dirty marker");
    wait_for_file(&tree.root.join("probe_dirtied"), &editor);
    let held = session.call_tool(
        ids.next(),
        "file.patch",
        json!({
            "path": "scripts/b.gd",
            "expectedSha256": sha256_hex(SCRIPT_B.as_bytes()),
            "change": { "content": "extends Node\n\nfunc b():\n\tprint(\"fixed\")\n" },
            "idempotencyId": "scripts-held"
        }),
    );
    assert_eq!(error_code(&held), json!("conflict"), "{held}");

    let saved = ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.save",
            json!({ "idempotencyId": "scripts-save" }),
        ),
        &editor.context(),
    );
    assert!(
        saved["also_saved"]
            .as_array()
            .is_some_and(|also| also.contains(&json!({ "path": "res://scripts/b.gd" }))),
        "{}: the save lists the script it wrote: {saved}",
        editor.binary
    );
    let written = std::fs::read(tree.root.join("scripts/b.gd")).expect("b");
    assert!(
        String::from_utf8_lossy(&written).starts_with("# unsaved edit\n"),
        "{}: the script's unsaved edit is on disk",
        editor.binary
    );
    let status = ok(
        &session.call_tool(ids.next(), "editor.status.get", json!({})),
        &editor.context(),
    );
    assert_eq!(
        open_script_entry(&status, "res://scripts/b.gd")["unsaved"],
        json!(false),
        "{status}"
    );

    ok(
        &session.call_tool(
            ids.next(),
            "file.patch",
            json!({
                "path": "scripts/b.gd",
                "expectedSha256": sha256_hex(&written),
                "change": { "content": "extends Node\n\nfunc b():\n\tprint(\"fixed\")\n" },
                "idempotencyId": "scripts-fix"
            }),
        ),
        &editor.context(),
    );
    let scanned = ok(
        &session.call_tool(
            ids.next(),
            "editor.filesystem.scan",
            json!({ "idempotencyId": "scripts-scan" }),
        ),
        &editor.context(),
    );
    assert_eq!(
        scanned["reloaded_scripts"],
        json!(["res://scripts/b.gd"]),
        "{}: the fixed script is reloaded into its tab: {scanned}",
        editor.binary
    );
    let _ = session.finish();
    drop(editor);
}

/// The `major.minor` of the editor at `binary`, as `--version` prints it.
fn editor_series(binary: &Path) -> String {
    let output = Command::new(binary)
        .arg("--version")
        .output()
        .unwrap_or_else(|error| panic!("{} --version: {error}", binary.display()));
    let version = String::from_utf8_lossy(&output.stdout);
    let mut parts = version.trim().split('.');
    format!(
        "{}.{}",
        parts.next().expect("major"),
        parts.next().expect("minor")
    )
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn the_editor_tools_never_start_an_external_program() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        the_editor_tools_never_start_an_external_program_in(binary);
    }
}

fn the_editor_tools_never_start_an_external_program_in(binary: &Path) {
    let (tree, mut session, mut ids) =
        project_with_addon("live-external", &[("scripts/a.gd", SCRIPT_A)]);
    let home = tree.base.join("home");
    let ran = tree.base.join("external-ran");
    let program = tree.base.join("external-program.sh");
    std::fs::write(
        &program,
        format!("#!/bin/sh\necho \"$@\" >> '{}'\n", ran.display()),
    )
    .expect("external program");
    std::fs::set_permissions(
        &program,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .expect("executable");
    let settings = home.join("config").join("godot");
    std::fs::create_dir_all(&settings).expect("editor settings directory");
    std::fs::write(
        settings.join(format!("editor_settings-{}.tres", editor_series(binary))),
        format!(
            "[gd_resource type=\"EditorSettings\" format=3]\n\n[resource]\n\
             text_editor/external/use_external_editor = true\n\
             text_editor/external/exec_path = \"{0}\"\n\
             text_editor/external/exec_flags = \"{{file}}\"\n\
             filesystem/import/blender/blender_path = \"{0}\"\n",
            program.display()
        ),
    )
    .expect("editor settings");
    let editor = Editor::launch(binary, &tree.root, &home);
    wait_connected(&mut session, &mut ids, &editor);
    let ran_before = std::fs::read_to_string(&ran).unwrap_or_default();

    let refused = session.call_tool(
        ids.next(),
        "editor.script.open",
        json!({ "path": "scripts/a.gd", "idempotencyId": "external-open" }),
    );
    assert_eq!(error_code(&refused), json!("unavailable"), "{refused}");
    assert!(refused.to_string().contains("external"), "{refused}");
    let scanned = ok(
        &session.call_tool(
            ids.next(),
            "editor.filesystem.scan",
            json!({ "idempotencyId": "external-scan" }),
        ),
        &editor.context(),
    );
    assert_eq!(
        scanned["script_reload"], "skipped_external_editor",
        "{scanned}"
    );

    // A .blend file Blender would import makes the scan refuse to run.
    std::fs::write(
        tree.root.join("model.blend"),
        b"BLENDER-v300 not a real file",
    )
    .expect("blend");
    let blocked = session.call_tool(
        ids.next(),
        "editor.filesystem.scan",
        json!({ "idempotencyId": "external-blend" }),
    );
    assert_eq!(error_code(&blocked), json!("unavailable"), "{blocked}");
    assert!(blocked.to_string().contains("Blender"), "{blocked}");
    assert_eq!(
        std::fs::read_to_string(&ran).unwrap_or_default(),
        ran_before,
        "{}: no tool started the external program",
        editor.binary
    );
    let _ = session.finish();
    drop(editor);
}

/// The text the editor's settings file holds for `name`, or `None` when it
/// leaves the setting out, as it does a setting at its default.
fn settings_file_line(file: &Path, name: &str) -> Option<String> {
    let text = std::fs::read_to_string(file).unwrap_or_default();
    let prefix = format!("{name} = ");
    text.lines()
        .find_map(|line| line.strip_prefix(&prefix).map(str::to_owned))
}

/// The entry for `name` of an `editor.preferences.get` answer.
fn preference<'a>(page: &'a Value, name: &str) -> &'a Value {
    page["settings"]
        .as_array()
        .and_then(|entries| entries.iter().find(|entry| entry["name"] == name))
        .unwrap_or_else(|| panic!("{name} is listed: {page}"))
}

fn preferences(
    session: &mut StdioSession,
    ids: &mut Ids,
    arguments: Value,
    context: &str,
) -> Value {
    ok(
        &session.call_tool(ids.next(), "editor.preferences.get", arguments),
        context,
    )
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn the_editor_settings_are_read_changed_and_saved() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        the_editor_settings_are_read_changed_and_saved_in(binary);
    }
}

/// `editor.preferences.get` reads the settings the Editor Settings dialog
/// shows, withholding paths and credentials; `editor.preferences.set` applies
/// a change and writes it to the editor's settings file, refuses one it may
/// not make, and puts a change back when the file cannot be written.
fn the_editor_settings_are_read_changed_and_saved_in(binary: &Path) {
    const INDENT: &str = "text_editor/behavior/indent/size";
    const HIDDEN: &str = "filesystem/file_dialog/show_hidden_files";
    const PASS: &str = "export/android/debug_keystore_pass";
    const SERVER_PASSWORD: &str = "filesystem/file_server/password";
    let (tree, mut session, mut ids) = project_with_addon("live-prefs", &[]);
    let home = tree.base.join("home");
    let editor = Editor::launch(binary, &tree.root, &home);
    wait_connected(&mut session, &mut ids, &editor);
    let context = editor.context();
    let file = home
        .join("config")
        .join("godot")
        .join(format!("editor_settings-{}.tres", editor_series(binary)));
    let mark = editor.mark();

    let named = preferences(
        &mut session,
        &mut ids,
        json!({ "settings": [INDENT, HIDDEN, PASS, SERVER_PASSWORD, "no/such_setting"] }),
        &context,
    );
    let indent = preference(&named, INDENT);
    assert_eq!(indent["type"], "integer", "{indent}");
    assert_eq!(indent["value"], integer(4), "{indent}");
    assert_eq!(indent["default"], integer(4), "{indent}");
    assert_eq!(indent["changed"], json!(false), "{indent}");
    assert_eq!(indent["hint"], "range", "{indent}");
    assert_eq!(indent["hint_string"], "1,64,1", "{indent}");
    assert_eq!(indent["changeable"], json!(true), "{indent}");
    assert_eq!(
        preference(&named, HIDDEN)["value"],
        tagged("bool", json!(false))
    );
    // A credential with a plain hint is withheld by its name, as one with a
    // password hint is.
    for name in [PASS, SERVER_PASSWORD] {
        let credential = preference(&named, name);
        assert_eq!(credential["withheld"], json!(true), "{credential}");
        assert_eq!(credential["changeable"], json!(false), "{credential}");
        for withheld in ["value", "default", "hint_string"] {
            assert!(
                credential.get(withheld).is_none(),
                "{withheld} is withheld: {credential}"
            );
        }
    }
    assert_eq!(named["unknown"], json!(["no/such_setting"]), "{named}");

    // Every setting, page by page, and none shows a path of this user's.
    let home_text = home.display().to_string();
    let (mut offset, mut seen, mut total) = (0_u64, 0_u64, None);
    loop {
        let page = preferences(
            &mut session,
            &mut ids,
            json!({ "offset": offset, "limit": 200 }),
            &context,
        );
        total.get_or_insert(page["total"].as_u64().expect("total"));
        seen += page["settings"].as_array().expect("settings").len() as u64;
        assert!(
            !page.to_string().contains(&home_text),
            "{}: no setting read shows a path under {home_text}",
            editor.binary
        );
        match page["next_offset"].as_u64() {
            Some(next) => offset = next,
            None => break,
        }
    }
    assert_eq!(Some(seen), total, "{}", editor.binary);
    assert!(seen > 400, "{}: {seen} settings", editor.binary);
    let prefixed = preferences(
        &mut session,
        &mut ids,
        json!({ "prefix": "text_editor/behavior/indent/" }),
        &context,
    );
    let names: Vec<&str> = prefixed["settings"]
        .as_array()
        .expect("settings")
        .iter()
        .filter_map(|entry| entry["name"].as_str())
        .collect();
    assert!(
        names.contains(&INDENT)
            && names
                .iter()
                .all(|name| name.starts_with("text_editor/behavior/indent/")),
        "{prefixed}"
    );

    // A change is applied, saved and read back.
    let changed = ok(
        &session.call_tool(
            ids.next(),
            "editor.preferences.set",
            json!({
                "settings": { INDENT: integer(2), HIDDEN: tagged("bool", json!(true)) },
                "idempotencyId": "prefs-change"
            }),
        ),
        &context,
    );
    assert_eq!(changed["mutation_status"], "committed", "{changed}");
    assert_eq!(changed["saved"], json!(true), "{changed}");
    let entry = changed["settings"]
        .as_array()
        .and_then(|entries| entries.iter().find(|entry| entry["name"] == INDENT))
        .unwrap_or_else(|| panic!("the change lists {INDENT}: {changed}"));
    assert_eq!(entry["before"], integer(4), "{changed}");
    assert_eq!(entry["after"], integer(2), "{changed}");
    assert_eq!(settings_file_line(&file, INDENT).as_deref(), Some("2"));
    assert_eq!(settings_file_line(&file, HIDDEN).as_deref(), Some("true"));
    let reread = preferences(
        &mut session,
        &mut ids,
        json!({ "settings": [INDENT] }),
        &context,
    );
    assert_eq!(preference(&reread, INDENT)["value"], integer(2), "{reread}");
    assert_eq!(
        preference(&reread, INDENT)["changed"],
        json!(true),
        "{reread}"
    );
    editor.assert_no_errors_since(mark, "reading and changing editor settings");

    // A change it may not make, or makes no sense of, changes nothing.
    let held = std::fs::read(&file).expect("the settings file");
    for (settings, code, what) in [
        (
            json!({ "interface/theme/custom_theme": text("res://theme.tres") }),
            "permission_denied",
            "a file path, refused by its hint",
        ),
        (
            json!({ "filesystem/directories/default_project_path": text("/tmp") }),
            "permission_denied",
            "a directory path, refused by its hint",
        ),
        (
            json!({ SERVER_PASSWORD: text("secret") }),
            "permission_denied",
            "the file server's password",
        ),
        (
            json!({ INDENT: integer(0) }),
            "invalid_input",
            "below its range",
        ),
        (
            json!({ INDENT: integer(65) }),
            "invalid_input",
            "above its range",
        ),
        (
            json!({ INDENT: text("2") }),
            "invalid_input",
            "of another type",
        ),
        (
            json!({ "text_editor/behavior/indent/type": integer(7) }),
            "invalid_input",
            "none of its choices",
        ),
        (
            json!({ "no/such_setting": integer(1) }),
            "not_found",
            "a setting this editor lacks",
        ),
        (
            json!({ HIDDEN: tagged("bool", json!(false)), "no/such_setting": integer(1) }),
            "not_found",
            "a setting beside one this editor lacks",
        ),
    ] {
        let refused = session.call_tool(
            ids.next(),
            "editor.preferences.set",
            json!({ "settings": settings, "idempotencyId": format!("prefs-refused-{}", ids.0) }),
        );
        assert_eq!(
            error_code(&refused),
            json!(code),
            "{}: {what}: {refused}",
            editor.binary
        );
    }
    assert_eq!(
        std::fs::read(&file).expect("the settings file"),
        held,
        "{}: a refused change writes nothing",
        editor.binary
    );
    let still = preferences(
        &mut session,
        &mut ids,
        json!({ "settings": [INDENT, HIDDEN] }),
        &context,
    );
    assert_eq!(preference(&still, INDENT)["value"], integer(2), "{still}");
    assert_eq!(
        preference(&still, HIDDEN)["value"],
        tagged("bool", json!(true)),
        "{still}"
    );

    // A settings file the editor cannot write: the change is put back.
    {
        let _locked = ReadOnly::new(file.clone());
        let failed = session.call_tool(
            ids.next(),
            "editor.preferences.set",
            json!({ "settings": { INDENT: integer(3) }, "idempotencyId": "prefs-locked" }),
        );
        assert_eq!(error_code(&failed), json!("unavailable"), "{failed}");
        assert!(failed.to_string().contains("put back"), "{failed}");
        let after = preferences(
            &mut session,
            &mut ids,
            json!({ "settings": [INDENT] }),
            &context,
        );
        assert_eq!(preference(&after, INDENT)["value"], integer(2), "{after}");
    }
    assert_eq!(settings_file_line(&file, INDENT).as_deref(), Some("2"));
    let _ = session.finish();
    drop(editor);
}

/// Whether the Godot an editor runs, as `editor.status.get` reports it,
/// switches a theme preset to Custom when a color the preset sets changes, as
/// Godot 4.7's Editor Settings dialog does for a plugin's change too.
fn links_presets(status: &Value) -> bool {
    let version = status["godot_version"]
        .as_str()
        .expect("the connected editor's Godot version");
    !["4.4.", "4.5.", "4.6."]
        .iter()
        .any(|series| version.starts_with(series))
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn a_color_a_preset_sets_changes_only_with_the_preset_custom() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        a_color_a_preset_sets_changes_only_with_the_preset_custom_in(binary);
    }
}

/// A theme color the editor's color preset sets holds a value of its own only
/// while the preset is Custom. Before Godot 4.7 the editor puts a color
/// changed alone back, so the change is refused and nothing is changed; 4.7
/// switches the preset itself, which the answer lists. With the preset set to
/// Custom in the same change, every release keeps the color.
fn a_color_a_preset_sets_changes_only_with_the_preset_custom_in(binary: &Path) {
    const ACCENT: &str = "interface/theme/accent_color";
    let (tree, mut session, mut ids) = project_with_addon("live-presets", &[]);
    let home = tree.base.join("home");
    let editor = Editor::launch(binary, &tree.root, &home);
    let status = wait_connected(&mut session, &mut ids, &editor);
    let context = editor.context();
    let file = home
        .join("config")
        .join("godot")
        .join(format!("editor_settings-{}.tres", editor_series(binary)));
    let named = preferences(
        &mut session,
        &mut ids,
        json!({ "settings": [ACCENT, "interface/theme/preset", "interface/theme/color_preset"] }),
        &context,
    );
    // Godot 4.6 renamed the preset.
    let preset = if named["unknown"] == json!(["interface/theme/color_preset"]) {
        "interface/theme/preset"
    } else {
        "interface/theme/color_preset"
    };
    assert_ne!(
        preference(&named, preset)["value"],
        text("Custom"),
        "{}: {named}",
        editor.binary
    );
    let accent_before = preference(&named, ACCENT)["value"].clone();
    let orange = tagged(
        "color",
        json!({ "red": 1.0, "green": 0.5, "blue": 0.0, "alpha": 1.0 }),
    );
    let mark = editor.mark();
    let held = std::fs::read(&file).unwrap_or_default();

    let alone = session.call_tool(
        ids.next(),
        "editor.preferences.set",
        json!({ "settings": { ACCENT: orange.clone() }, "idempotencyId": "presets-alone" }),
    );
    if links_presets(&status) {
        let alone = ok(&alone, &context);
        assert_eq!(alone["mutation_status"], "committed", "{alone}");
        let linked = alone["also_changed"]["settings"]
            .as_array()
            .and_then(|entries| entries.iter().find(|entry| entry["name"] == preset))
            .unwrap_or_else(|| panic!("{}: the preset switch is listed: {alone}", editor.binary));
        assert_eq!(linked["after"], text("Custom"), "{alone}");
        assert_eq!(
            settings_file_line(&file, preset).as_deref(),
            Some("\"Custom\""),
            "{}",
            editor.binary
        );
    } else {
        assert_eq!(
            error_code(&alone),
            json!("conflict"),
            "{}: {alone}",
            editor.binary
        );
        let message = alone.to_string();
        assert!(
            message.contains("put back") && message.contains(preset) && message.contains("Custom"),
            "{}: the refusal says what to change with the color: {alone}",
            editor.binary
        );
        assert_eq!(
            std::fs::read(&file).unwrap_or_default(),
            held,
            "{}: a refused change writes nothing",
            editor.binary
        );
        let still = preferences(
            &mut session,
            &mut ids,
            json!({ "settings": [ACCENT] }),
            &context,
        );
        assert_eq!(
            preference(&still, ACCENT)["value"],
            accent_before,
            "{still}"
        );
    }

    // The preset set to Custom with the color keeps it on every release.
    let both = ok(
        &session.call_tool(
            ids.next(),
            "editor.preferences.set",
            json!({
                "settings": { ACCENT: orange.clone(), preset: text("Custom") },
                "idempotencyId": "presets-both"
            }),
        ),
        &context,
    );
    assert_eq!(both["mutation_status"], "committed", "{both}");
    assert_eq!(
        settings_file_line(&file, ACCENT).as_deref(),
        Some("Color(1, 0.5, 0, 1)"),
        "{}",
        editor.binary
    );
    assert_eq!(
        settings_file_line(&file, preset).as_deref(),
        Some("\"Custom\""),
        "{}",
        editor.binary
    );
    let kept = preferences(
        &mut session,
        &mut ids,
        json!({ "settings": [ACCENT, preset] }),
        &context,
    );
    assert_eq!(preference(&kept, ACCENT)["value"], orange, "{kept}");

    // Choosing a 3D navigation scheme: Godot 4.7.1 sets the scheme's mouse buttons and modifier
    // shortcuts with it, and the answer lists them; earlier releases change the scheme alone.
    let maya = ok(
        &session.call_tool(
            ids.next(),
            "editor.preferences.set",
            json!({
                "settings": { "editors/3d/navigation/navigation_scheme": integer(1) },
                "idempotencyId": "presets-scheme"
            }),
        ),
        &context,
    );
    assert_eq!(maya["mutation_status"], "committed", "{maya}");
    let also = &maya["also_changed"];
    if links_presets(&status) {
        assert!(
            also["settings"].as_array().is_some_and(|entries| entries
                .iter()
                .any(|entry| entry["name"] == "editors/3d/navigation/orbit_mouse_button")),
            "{}: the scheme's mouse buttons are listed: {maya}",
            editor.binary
        );
        let modifier = also["shortcuts"]
            .as_array()
            .and_then(|entries| {
                entries
                    .iter()
                    .find(|entry| entry["name"] == "spatial_editor/viewport_orbit_modifier_1")
            })
            .unwrap_or_else(|| {
                panic!("{}: the scheme's modifier is listed: {maya}", editor.binary)
            });
        assert_eq!(modifier["after"][0]["key"], "Alt", "{maya}");
        assert!(
            settings_file_lists(&file, "spatial_editor/viewport_orbit_modifier_1"),
            "{}: the settings file holds the modifier",
            editor.binary
        );
    } else {
        assert_eq!(
            also,
            &json!({ "settings": [], "shortcuts": [] }),
            "{}: {maya}",
            editor.binary
        );
    }
    editor.assert_no_errors_since(mark, "changing a color a preset sets");
    let _ = session.finish();
    drop(editor);
}

/// Whether the editor's settings file lists `name` among its changed
/// shortcuts or built-in actions.
fn settings_file_lists(file: &Path, name: &str) -> bool {
    std::fs::read_to_string(file)
        .unwrap_or_default()
        .contains(&format!("\"name\": \"{name}\""))
}

/// The entry for `name` of an `editor.shortcuts.get` answer.
fn shortcut<'a>(page: &'a Value, name: &str) -> &'a Value {
    page["shortcuts"]
        .as_array()
        .and_then(|entries| entries.iter().find(|entry| entry["name"] == name))
        .unwrap_or_else(|| panic!("{name} is listed: {page}"))
}

fn shortcuts(session: &mut StdioSession, ids: &mut Ids, arguments: Value, context: &str) -> Value {
    ok(
        &session.call_tool(ids.next(), "editor.shortcuts.get", arguments),
        context,
    )
}

fn shortcut_names(page: &Value) -> Vec<String> {
    page["shortcuts"]
        .as_array()
        .expect("shortcuts")
        .iter()
        .filter_map(|entry| entry["name"].as_str().map(str::to_owned))
        .collect()
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn the_editor_shortcuts_are_read_changed_and_saved() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        the_editor_shortcuts_are_read_changed_and_saved_in(binary);
    }
}

/// `editor.shortcuts.get` lists every editor shortcut and built-in action the
/// Shortcuts tab does, with its key events and defaults;
/// `editor.shortcuts.set` binds them, writes the bindings to the editor's
/// settings file, refuses a change it cannot make, puts a change back when
/// the file cannot be written, and restores defaults. Godot before 4.6 names
/// no shortcut to a plugin.
fn the_editor_shortcuts_are_read_changed_and_saved_in(binary: &Path) {
    const SAVE: &str = "script_editor/save";
    const CANCEL: &str = "ui_cancel";
    const CLOSE: &str = "editor/close_scene";
    const FREELOOK: &str = "spatial_editor/freelook_left";
    let (tree, mut session, mut ids) = project_with_addon("live-keys", &[]);
    let home = tree.base.join("home");
    let editor = Editor::launch(binary, &tree.root, &home);
    let status = wait_connected(&mut session, &mut ids, &editor);
    let context = editor.context();
    if status["editor"]["shortcuts"] == json!(false) {
        for (tool, arguments) in [
            ("editor.shortcuts.get", json!({})),
            (
                "editor.shortcuts.set",
                json!({ "shortcuts": { SAVE: [] }, "idempotencyId": "keys-old" }),
            ),
        ] {
            let refused = session.call_tool(ids.next(), tool, arguments);
            assert_eq!(
                error_code(&refused),
                json!("unsupported"),
                "{}: {tool}: {refused}",
                editor.binary
            );
            assert!(refused.to_string().contains("4.6"), "{refused}");
        }
        let _ = session.finish();
        return;
    }
    assert_eq!(
        status["editor"]["shortcuts"],
        json!(true),
        "{}: {status}",
        editor.binary
    );
    let file = home
        .join("config")
        .join("godot")
        .join(format!("editor_settings-{}.tres", editor_series(binary)));
    let mark = editor.mark();

    let named = shortcuts(
        &mut session,
        &mut ids,
        json!({ "shortcuts": [SAVE, CANCEL, FREELOOK, "no/such_shortcut"] }),
        &context,
    );
    let save = shortcut(&named, SAVE);
    assert_eq!(save["kind"], "shortcut", "{save}");
    assert_eq!(save["label"], "Save", "{save}");
    assert_eq!(save["changed"], json!(false), "{save}");
    assert_eq!(save["events"], save["default"], "{save}");
    let event = &save["events"][0];
    assert_eq!(event["key"], "S", "{save}");
    assert_eq!(event["alt"], json!(true), "{save}");
    assert_eq!(event["command_or_control"], json!(true), "{save}");
    assert_eq!(event["ctrl"], json!(false), "{save}");
    assert_eq!(event["text"], "Ctrl+Alt+S", "{save}");
    let cancel = shortcut(&named, CANCEL);
    assert_eq!(cancel["kind"], "action", "{cancel}");
    assert_eq!(cancel["events"][0]["key"], "Escape", "{cancel}");
    let freelook = shortcut(&named, FREELOOK);
    assert_eq!(freelook["events"][0]["key"], "A", "{freelook}");
    assert_eq!(freelook["events"][0]["match"], "physical", "{freelook}");
    assert_eq!(named["unknown"], json!(["no/such_shortcut"]), "{named}");

    // Every shortcut, page by page, each named once.
    let (mut offset, mut names, mut total) = (0_u64, Vec::new(), None);
    loop {
        let page = shortcuts(
            &mut session,
            &mut ids,
            json!({ "offset": offset, "limit": 200 }),
            &context,
        );
        total.get_or_insert(page["total"].as_u64().expect("total"));
        for entry in page["shortcuts"].as_array().expect("shortcuts") {
            assert!(
                matches!(entry["kind"].as_str(), Some("shortcut" | "action")),
                "{entry}"
            );
        }
        names.extend(shortcut_names(&page));
        match page["next_offset"].as_u64() {
            Some(next) => offset = next,
            None => break,
        }
    }
    let count = names.len();
    names.sort();
    names.dedup();
    assert_eq!(
        names.len(),
        count,
        "{}: every name is listed once",
        editor.binary
    );
    assert_eq!(Some(count as u64), total, "{}", editor.binary);
    assert!(count > 500, "{}: {count} shortcuts", editor.binary);
    let actions = shortcuts(
        &mut session,
        &mut ids,
        json!({ "prefix": "ui_", "limit": 500 }),
        &context,
    );
    let listed = actions["shortcuts"].as_array().expect("shortcuts");
    assert!(
        listed.len() > 60
            && listed.iter().all(|entry| entry["kind"] == "action"
                && entry["name"]
                    .as_str()
                    .is_some_and(|name| name.starts_with("ui_"))),
        "{}: {actions}",
        editor.binary
    );
    assert_eq!(
        shortcuts(
            &mut session,
            &mut ids,
            json!({ "changedOnly": true }),
            &context
        )["total"],
        json!(0)
    );

    // A change is applied, saved and read back.
    let changed = ok(
        &session.call_tool(
            ids.next(),
            "editor.shortcuts.set",
            json!({
                "shortcuts": {
                    SAVE: [
                        { "key": "F9", "command_or_control": true },
                        { "key": "J", "match": "physical", "shift": true }
                    ],
                    CANCEL: [{ "key": "F10", "alt": true }],
                    CLOSE: []
                },
                "idempotencyId": "keys-change"
            }),
        ),
        &context,
    );
    assert_eq!(changed["mutation_status"], "committed", "{changed}");
    assert_eq!(changed["saved"], json!(true), "{changed}");
    assert_eq!(
        changed["also_changed"],
        json!({ "settings": [], "shortcuts": [] }),
        "{changed}"
    );
    let entry = changed["shortcuts"]
        .as_array()
        .and_then(|entries| entries.iter().find(|entry| entry["name"] == SAVE))
        .unwrap_or_else(|| panic!("the change lists {SAVE}: {changed}"));
    assert_eq!(entry["before"][0]["text"], "Ctrl+Alt+S", "{changed}");
    assert_eq!(entry["after"][0]["text"], "Ctrl+F9", "{changed}");
    assert_eq!(entry["after"][1]["key"], "J", "{changed}");
    assert_eq!(entry["after"][1]["match"], "physical", "{changed}");
    for name in [SAVE, CANCEL, CLOSE] {
        assert!(
            settings_file_lists(&file, name),
            "{}: the settings file lists {name}",
            editor.binary
        );
    }
    let reread = shortcuts(
        &mut session,
        &mut ids,
        json!({ "changedOnly": true }),
        &context,
    );
    assert_eq!(
        shortcut_names(&reread),
        vec![CLOSE, SAVE, CANCEL],
        "{}: {reread}",
        editor.binary
    );
    assert_eq!(shortcut(&reread, CLOSE)["events"], json!([]), "{reread}");
    assert_eq!(
        shortcut(&reread, CANCEL)["events"][0]["text"],
        "Alt+F10",
        "{reread}"
    );

    // A binding that is not the default comes back from an answer's `before`, as given.
    let custom = entry["after"].clone();
    let moved = ok(
        &session.call_tool(
            ids.next(),
            "editor.shortcuts.set",
            json!({ "shortcuts": { SAVE: [{ "key": "F7" }] }, "idempotencyId": "keys-move" }),
        ),
        &context,
    );
    let before = moved["shortcuts"][0]["before"].clone();
    assert_eq!(before, custom, "{}: {moved}", editor.binary);
    let restored = ok(
        &session.call_tool(
            ids.next(),
            "editor.shortcuts.set",
            json!({ "shortcuts": { SAVE: before }, "idempotencyId": "keys-restore" }),
        ),
        &context,
    );
    assert_eq!(restored["mutation_status"], "committed", "{restored}");
    assert_eq!(restored["shortcuts"][0]["after"], custom, "{restored}");
    let back = shortcuts(
        &mut session,
        &mut ids,
        json!({ "shortcuts": [SAVE] }),
        &context,
    );
    assert_eq!(shortcut(&back, SAVE)["events"], custom, "{back}");
    editor.assert_no_errors_since(mark, "reading and changing shortcuts");

    // A change it cannot make changes nothing.
    let held = std::fs::read(&file).expect("the settings file");
    for (bindings, code, what) in [
        (
            json!({ SAVE: "default", "no/such_shortcut": [] }),
            "not_found",
            "a shortcut this editor lacks, beside one it has",
        ),
        (
            json!({ SAVE: [{ "key": "NoSuchKey" }] }),
            "invalid_input",
            "a key Godot does not know",
        ),
    ] {
        let refused = session.call_tool(
            ids.next(),
            "editor.shortcuts.set",
            json!({ "shortcuts": bindings, "idempotencyId": format!("keys-refused-{}", ids.0) }),
        );
        assert_eq!(
            error_code(&refused),
            json!(code),
            "{}: {what}: {refused}",
            editor.binary
        );
    }
    assert_eq!(
        std::fs::read(&file).expect("the settings file"),
        held,
        "{}: a refused change writes nothing",
        editor.binary
    );

    // A settings file the editor cannot write: the change is put back.
    {
        let _locked = ReadOnly::new(file.clone());
        let failed = session.call_tool(
            ids.next(),
            "editor.shortcuts.set",
            json!({ "shortcuts": { SAVE: "default" }, "idempotencyId": "keys-locked" }),
        );
        assert_eq!(error_code(&failed), json!("unavailable"), "{failed}");
        assert!(failed.to_string().contains("put back"), "{failed}");
        let after = shortcuts(
            &mut session,
            &mut ids,
            json!({ "shortcuts": [SAVE] }),
            &context,
        );
        assert_eq!(
            shortcut(&after, SAVE)["events"][0]["text"],
            "Ctrl+F9",
            "{after}"
        );
    }

    // Back to the defaults: the file lists none of them.
    let reset = ok(
        &session.call_tool(
            ids.next(),
            "editor.shortcuts.set",
            json!({
                "shortcuts": { SAVE: "default", CANCEL: "default", CLOSE: "default" },
                "idempotencyId": "keys-reset"
            }),
        ),
        &context,
    );
    assert_eq!(reset["mutation_status"], "committed", "{reset}");
    for name in [SAVE, CANCEL, CLOSE] {
        assert!(
            !settings_file_lists(&file, name),
            "{}: the settings file no longer lists {name}",
            editor.binary
        );
    }
    assert_eq!(
        shortcuts(
            &mut session,
            &mut ids,
            json!({ "changedOnly": true }),
            &context
        )["total"],
        json!(0)
    );
    let _ = session.finish();
    drop(editor);
}

const CLOSE_A: &str = "[gd_scene format=3]\n\n[node name=\"A\" type=\"Node2D\"]\n";
const CLOSE_B: &str = "[gd_scene format=3]\n\n[node name=\"B\" type=\"Node2D\"]\n";

fn open_scene_paths(status: &Value) -> Vec<String> {
    status["editor"]["open_scenes"]
        .as_array()
        .expect("open scenes")
        .iter()
        .filter_map(|entry| entry["path"].as_str())
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect()
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn the_editor_closes_only_a_scene_it_holds_saved() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        the_editor_closes_only_a_scene_it_holds_saved_in(binary);
    }
}

/// `editor.scene.close` closes an open scene, edited or not, and refuses one
/// holding a change its file does not, which the editor would discard. Godot
/// 4.4 cannot close a scene from a plugin.
fn the_editor_closes_only_a_scene_it_holds_saved_in(binary: &Path) {
    let (tree, mut session, mut ids) =
        project_with_addon("live-close", &[("a.tscn", CLOSE_A), ("b.tscn", CLOSE_B)]);
    let home = tree.base.join("home");
    let editor = Editor::launch(binary, &tree.root, &home);
    let status = wait_connected(&mut session, &mut ids, &editor);
    let context = editor.context();
    for path in ["a.tscn", "b.tscn"] {
        ok(
            &session.call_tool(
                ids.next(),
                "editor.scene.open",
                json!({ "path": path, "idempotencyId": format!("close-open-{path}") }),
            ),
            &context,
        );
    }
    let close = |session: &mut StdioSession, ids: &mut Ids, scene: Option<&str>| {
        let mut arguments = json!({ "idempotencyId": format!("close-{}", ids.0 + 1) });
        if let Some(scene) = scene {
            arguments["scene"] = json!(scene);
        }
        session.call_tool(ids.next(), "editor.scene.close", arguments)
    };
    if status["editor"]["scene_close"] == json!(false) {
        let refused = close(&mut session, &mut ids, None);
        assert_eq!(error_code(&refused), json!("unsupported"), "{refused}");
        assert!(refused.to_string().contains("4.5"), "{refused}");
        let _ = session.finish();
        return;
    }
    assert_eq!(
        status["editor"]["scene_close"],
        json!(true),
        "{}: {status}",
        editor.binary
    );
    let mark = editor.mark();

    // A change its file does not hold keeps the edited scene open.
    let digest = digest_of(&mut session, &mut ids, &context);
    ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.edit",
            json!({
                "expectedDigest": digest,
                "operations": [{ "op": "add_node", "parent": ".", "name": "Extra", "type": "Node" }],
                "idempotencyId": "close-edit"
            }),
        ),
        &context,
    );
    let refused = close(&mut session, &mut ids, None);
    assert_eq!(error_code(&refused), json!("conflict"), "{refused}");
    assert!(
        refused.to_string().contains("editor.scene.save"),
        "the refusal says how to keep the change: {refused}"
    );
    let state = ok(
        &session.call_tool(ids.next(), "editor.status.get", json!({})),
        &context,
    );
    assert_eq!(open_scene_paths(&state), ["res://a.tscn", "res://b.tscn"]);

    // Saved, it closes, and the editor shows the other scene.
    ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.save",
            json!({ "idempotencyId": "close-save" }),
        ),
        &context,
    );
    let closed = ok(&close(&mut session, &mut ids, None), &context);
    assert_eq!(closed["mutation_status"], "committed", "{closed}");
    assert_eq!(closed["closed"], "res://b.tscn", "{closed}");
    assert_eq!(closed["open_scenes"], json!(["res://a.tscn"]), "{closed}");
    assert_eq!(closed["edited_scene"], "res://a.tscn", "{closed}");
    assert!(
        std::fs::read_to_string(tree.root.join("b.tscn"))
            .expect("b.tscn")
            .contains("Extra"),
        "the saved change stays in the file"
    );

    // A scene that is open but not edited, holding a change its file does
    // not, is refused before the editor shows anything else.
    let open = |session: &mut StdioSession, ids: &mut Ids, path: &str| {
        let arguments =
            json!({ "path": path, "idempotencyId": format!("close-open-{}", ids.0 + 1) });
        ok(
            &session.call_tool(ids.next(), "editor.scene.open", arguments),
            &context,
        );
    };
    let held = digest_of(&mut session, &mut ids, &context);
    ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.edit",
            json!({
                "expectedDigest": held,
                "operations": [{ "op": "add_node", "parent": ".", "name": "Held", "type": "Node" }],
                "idempotencyId": "close-edit-a"
            }),
        ),
        &context,
    );
    open(&mut session, &mut ids, "b.tscn");
    let unsaved_other = close(&mut session, &mut ids, Some("a.tscn"));
    assert_eq!(
        error_code(&unsaved_other),
        json!("conflict"),
        "{unsaved_other}"
    );
    let state = ok(
        &session.call_tool(ids.next(), "editor.status.get", json!({})),
        &context,
    );
    assert_eq!(
        state["editor"]["edited_scene"], "res://b.tscn",
        "a refused close shows nothing else: {state}"
    );
    assert_eq!(open_scene_paths(&state), ["res://a.tscn", "res://b.tscn"]);

    // A scene the editor holds open, saved or not, is not deleted under it.
    for path in ["a.tscn", "b.tscn"] {
        let bytes = std::fs::read(tree.root.join(path)).expect("scene bytes");
        let deleted = session.call_tool(
            ids.next(),
            "file.delete",
            json!({
                "path": path,
                "expectedSha256": sha256_hex(&bytes),
                "idempotencyId": format!("close-delete-open-{path}")
            }),
        );
        assert_eq!(error_code(&deleted), json!("conflict"), "{path}: {deleted}");
        assert!(
            tree.root.join(path).is_file(),
            "{path} is still there after the refusal"
        );
    }

    // Undone, it closes, and the edited scene stays.
    open(&mut session, &mut ids, "a.tscn");
    ok(
        &session.call_tool(
            ids.next(),
            "editor.undo",
            json!({ "idempotencyId": "close-undo-a" }),
        ),
        &context,
    );
    open(&mut session, &mut ids, "b.tscn");
    let other = ok(&close(&mut session, &mut ids, Some("a.tscn")), &context);
    assert_eq!(other["closed"], "res://a.tscn", "{other}");
    assert_eq!(other["open_scenes"], json!(["res://b.tscn"]), "{other}");
    assert_eq!(other["edited_scene"], "res://b.tscn", "{other}");
    let last = ok(&close(&mut session, &mut ids, None), &context);
    assert_eq!(last["open_scenes"], json!([]), "{last}");
    assert_eq!(last["edited_scene"], Value::Null, "{last}");
    let none = close(&mut session, &mut ids, None);
    assert_eq!(error_code(&none), json!("not_found"), "{none}");

    // Closed, a scene is deleted like any other file.
    let bytes = std::fs::read(tree.root.join("b.tscn")).expect("scene bytes");
    let deleted = ok(
        &session.call_tool(
            ids.next(),
            "file.delete",
            json!({
                "path": "b.tscn",
                "expectedSha256": sha256_hex(&bytes),
                "idempotencyId": "close-delete-closed"
            }),
        ),
        &context,
    );
    assert_eq!(deleted["operation"], "delete", "{deleted}");
    assert!(!tree.root.join("b.tscn").exists(), "b.tscn was deleted");
    editor.assert_no_errors_since(mark, "closing scenes");
    let _ = session.finish();
    drop(editor);
}

/// A scene with one receiver of every class the editing-methods table lists:
/// nodes the calls go to directly, and resources the scene embeds that a
/// `resource` selector picks.
const CALL_SCENE: &str = r#"[gd_scene format=3]

[sub_resource type="PlaceholderTexture2D" id="PlaceholderTexture2D_t"]
size = Vector2(32, 16)

[sub_resource type="TileSetAtlasSource" id="TileSetAtlasSource_s"]
texture = SubResource("PlaceholderTexture2D_t")
0:0/0 = 0
1:0/0 = 0

[sub_resource type="TileSet" id="TileSet_s"]
sources/0 = SubResource("TileSetAtlasSource_s")

[sub_resource type="Curve" id="Curve_w"]

[sub_resource type="Gradient" id="Gradient_g"]

[sub_resource type="Curve2D" id="Curve2D_p"]

[sub_resource type="Curve3D" id="Curve3D_p"]

[sub_resource type="SpriteFrames" id="SpriteFrames_f"]

[sub_resource type="Theme" id="Theme_t"]

[node name="Main" type="Node2D"]

[node name="Layer" type="TileMapLayer" parent="."]
tile_set = SubResource("TileSet_s")

[node name="Line" type="Line2D" parent="."]
width_curve = SubResource("Curve_w")
gradient = SubResource("Gradient_g")

[node name="Sprite" type="Node2D" parent="."]

[node name="Player" type="AnimationPlayer" parent="."]

[node name="Path" type="Path2D" parent="."]
curve = SubResource("Curve2D_p")

[node name="Path3" type="Path3D" parent="."]
curve = SubResource("Curve3D_p")

[node name="Grid" type="GridMap" parent="."]

[node name="Frames" type="AnimatedSprite2D" parent="."]
sprite_frames = SubResource("SpriteFrames_f")

[node name="Panel" type="Control" parent="."]
theme = SubResource("Theme_t")
"#;

fn tagged(kind: &str, value: Value) -> Value {
    json!({ "type": kind, "value": value })
}

fn integer(value: i64) -> Value {
    tagged("integer", json!(value))
}

fn number(value: f64) -> Value {
    tagged("float", json!(value))
}

fn text(value: &str) -> Value {
    tagged("string", json!(value))
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

/// One `call_method` operation on `node`, or on the resource `resource`
/// picks.
fn call(node: &str, resource: Option<Value>, method: &str, arguments: Vec<Value>) -> Value {
    let mut operation = json!({
        "op": "call_method", "node": node, "method": method, "arguments": arguments
    });
    if let Some(resource) = resource {
        operation["resource"] = resource;
    }
    operation
}

/// A call on every class of the table, with what each call returns.
fn every_class_calls() -> (Vec<Value>, Vec<Value>) {
    let walk = || Some(json!({ "animation": "walk" }));
    let calls = vec![
        (
            call(
                "Layer",
                None,
                "set_cell",
                vec![vector2i(1, 2), integer(0), vector2i(0, 0), integer(0)],
            ),
            Value::Null,
        ),
        (
            call(
                "Layer",
                None,
                "set_cell",
                vec![vector2i(3, 4), integer(0), vector2i(1, 0)],
            ),
            Value::Null,
        ),
        (
            call("Layer", None, "erase_cell", vec![vector2i(1, 2)]),
            Value::Null,
        ),
        (
            call("Line", None, "add_point", vec![vector2(1.0, 2.0)]),
            Value::Null,
        ),
        (
            call("Line", None, "add_point", vec![vector2(3.0, 4.0)]),
            Value::Null,
        ),
        (
            call(
                "Line",
                Some(json!({ "property": "width_curve" })),
                "add_point",
                vec![vector2(0.0, 1.0)],
            ),
            integer(0),
        ),
        (
            call(
                "Line",
                Some(json!({ "property": "gradient" })),
                "add_point",
                vec![number(0.5), red()],
            ),
            Value::Null,
        ),
        (
            call(
                "Player",
                None,
                "add_animation_library",
                vec![
                    text(""),
                    tagged("new_resource", json!({ "class": "AnimationLibrary" })),
                ],
            ),
            integer(0),
        ),
        (
            call(
                "Player",
                Some(json!({ "library": "" })),
                "add_animation",
                vec![
                    text("walk"),
                    tagged("new_resource", json!({ "class": "Animation" })),
                ],
            ),
            integer(0),
        ),
        (
            call("Player", walk(), "add_track", vec![integer(0)]),
            integer(0),
        ),
        (
            call(
                "Player",
                walk(),
                "track_set_path",
                vec![integer(0), tagged("node_path", json!("Sprite:position"))],
            ),
            Value::Null,
        ),
        (
            call(
                "Player",
                walk(),
                "track_insert_key",
                vec![integer(0), number(0.0), vector2(1.0, 1.0)],
            ),
            integer(0),
        ),
        (
            call("Player", walk(), "set_length", vec![number(2.0)]),
            Value::Null,
        ),
        (
            call(
                "Path",
                Some(json!({ "property": "curve" })),
                "add_point",
                vec![vector2(1.0, 1.0)],
            ),
            Value::Null,
        ),
        (
            call(
                "Path3",
                Some(json!({ "property": "curve" })),
                "add_point",
                vec![tagged("vector3", json!({ "x": 1.0, "y": 2.0, "z": 3.0 }))],
            ),
            Value::Null,
        ),
        (
            call(
                "Path3",
                Some(json!({ "property": "curve" })),
                "set_point_tilt",
                vec![integer(0), number(0.5)],
            ),
            Value::Null,
        ),
        (
            call(
                "Grid",
                None,
                "set_cell_item",
                vec![
                    tagged("vector3i", json!({ "x": 1, "y": 2, "z": 3 })),
                    integer(0),
                ],
            ),
            Value::Null,
        ),
        (
            call(
                "Frames",
                Some(json!({ "property": "sprite_frames" })),
                "add_animation",
                vec![text("run")],
            ),
            Value::Null,
        ),
        (
            call(
                "Frames",
                Some(json!({ "property": "sprite_frames" })),
                "set_animation_speed",
                vec![text("run"), number(12.0)],
            ),
            Value::Null,
        ),
        (
            call(
                "Panel",
                Some(json!({ "property": "theme" })),
                "set_color",
                vec![text("font_color"), text("Label"), red()],
            ),
            Value::Null,
        ),
        (
            call(
                "Panel",
                Some(json!({ "property": "theme" })),
                "set_constant",
                vec![text("margin"), text("Box"), integer(4)],
            ),
            Value::Null,
        ),
    ];
    let results = calls
        .iter()
        .enumerate()
        .map(|(index, (operation, value))| {
            json!({ "index": index, "method": operation["method"], "value": value })
        })
        .collect();
    (
        calls.into_iter().map(|(operation, _)| operation).collect(),
        results,
    )
}

/// The bytes a `PackedByteArray(...)` property line holds, in either the
/// base64 or the comma-separated spelling.
fn packed_bytes(scene: &str, property: &str) -> Option<Vec<u8>> {
    let line = scene
        .lines()
        .find(|line| line.starts_with(&format!("{property} = PackedByteArray(")))?;
    let inner = line.split_once("PackedByteArray(")?.1.strip_suffix(')')?;
    if let Some(encoded) = inner
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    {
        return Some(decode_base64(encoded));
    }
    inner
        .split(',')
        .map(|byte| byte.trim().parse().ok())
        .collect()
}

/// What the calls of [`every_class_calls`] leave in a saved scene.
fn assert_calls_landed(scene: &str, context: &str) {
    assert_eq!(
        packed_bytes(scene, "tile_map_data"),
        Some(vec![0, 0, 3, 0, 4, 0, 0, 0, 1, 0, 0, 0, 0, 0]),
        "only the cell at (3, 4) is painted, from atlas tile (1, 0): {context}\n{scene}"
    );
    for marker in [
        "points = PackedVector2Array(1, 2, 3, 4)",
        "[sub_resource type=\"AnimationLibrary\"",
        "tracks/0/path = NodePath(\"Sprite:position\")",
        "length = 2.0",
        "\"tilts\": PackedFloat32Array(0.5)",
        "&\"run\"",
        "Label/colors/font_color = Color(1, 0, 0, 1)",
        "Box/constants/margin = 4",
    ] {
        assert!(
            scene.contains(marker),
            "the saved scene holds {marker:?}: {context}\n{scene}"
        );
    }
    assert!(
        scene
            .lines()
            .any(|line| line.starts_with("offsets = PackedFloat32Array(") && line.contains("0.5")),
        "the gradient holds a point at 0.5: {context}\n{scene}"
    );
    assert!(
        scene.matches("point_count = 1").count() >= 3,
        "the width curve, the 2D and the 3D curve each hold one point: {context}\n{scene}"
    );
    assert!(
        scene.contains("\"cells\": PackedInt32Array(")
            && !scene.contains("\"cells\": PackedInt32Array()"),
        "the grid holds a cell: {context}\n{scene}"
    );
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn editing_calls_reach_every_listed_class_in_the_editor_and_headless() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        editing_calls_reach_every_listed_class_in(binary);
    }
}

fn editing_calls_reach_every_listed_class_in(binary: &Path) {
    let (operations, expected) = every_class_calls();

    // Headless: the batch is applied by the bundled helper and saved.
    let headless = ProjectTree::new("live-calls-headless");
    headless.write("main.tscn", CALL_SCENE);
    let mut session = StdioSession::initialized(&Spawn {
        project: &headless.root,
        current_dir: None,
        profile: "full",
        godot: Some(binary),
    });
    let context = binary.display().to_string();
    let applied = ok(
        &session.call_tool(
            2,
            "scene.batch.apply",
            json!({
                "path": "main.tscn",
                "expectedSha256": sha256_hex(CALL_SCENE.as_bytes()),
                "operations": operations,
                "idempotencyId": "calls-headless"
            }),
        ),
        &context,
    );
    assert_eq!(applied["backend"], "headless", "{applied}");
    assert_eq!(applied["results"], json!(expected), "{context}: {applied}");
    let saved = std::fs::read_to_string(headless.root.join("main.tscn")).expect("scene");
    assert_calls_landed(&saved, &format!("{context}, headless"));
    let _ = session.finish();

    // In the editor: one undoable action on the scene it holds.
    let (tree, mut session, mut ids) =
        project_with_addon("live-calls", &[("main.tscn", CALL_SCENE)]);
    let editor = Editor::launch(binary, &tree.root, &tree.base.join("home"));
    wait_connected(&mut session, &mut ids, &editor);
    let mark = editor.mark();
    let context = editor.context();
    ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.open",
            json!({ "path": "main.tscn", "idempotencyId": "calls-open" }),
        ),
        &context,
    );
    let original = digest_of(&mut session, &mut ids, &context);
    let edited = ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.edit",
            json!({
                "expectedDigest": original,
                "operations": operations,
                "label": "every class",
                "idempotencyId": "calls-edit"
            }),
        ),
        &editor.context(),
    );
    assert_eq!(
        edited["results"],
        json!(expected),
        "{}: {edited}",
        editor.binary
    );
    let changed = edited["digest"].as_str().expect("digest").to_owned();
    assert_ne!(changed, original, "{edited}");
    let undone = ok(
        &session.call_tool(
            ids.next(),
            "editor.undo",
            json!({ "idempotencyId": "calls-undo" }),
        ),
        &editor.context(),
    );
    assert_eq!(
        undone["digest"],
        json!(original),
        "{}: undoing the calls puts every receiver back",
        editor.binary
    );
    let redone = ok(
        &session.call_tool(
            ids.next(),
            "editor.redo",
            json!({ "idempotencyId": "calls-redo" }),
        ),
        &editor.context(),
    );
    assert_eq!(
        redone["digest"],
        json!(changed),
        "{}: redoing the calls makes them again",
        editor.binary
    );
    ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.save",
            json!({ "idempotencyId": "calls-save" }),
        ),
        &editor.context(),
    );
    let saved = std::fs::read_to_string(tree.root.join("main.tscn")).expect("scene");
    assert_calls_landed(&saved, &format!("{}, editor", editor.binary));
    editor.assert_no_errors_since(
        mark,
        "calling every listed class, undoing, redoing and saving",
    );
    let _ = session.finish();
    drop(editor);
}

const OWN_SCENE: &str = "[gd_scene format=3]\n\n[node name=\"Main\" type=\"Node2D\"]\n\n\
    [node name=\"Player\" type=\"AnimationPlayer\" parent=\".\"]\n\n\
    [node name=\"Line\" type=\"Line2D\" parent=\".\"]\n";
const LIBRARY: &str = "[gd_resource type=\"AnimationLibrary\" format=3]\n\n[resource]\n";

/// Calls the scene does not own, or that do not fit their method, each with
/// the refusal reason both backends give.
fn refused_calls() -> Vec<(Value, &'static str)> {
    vec![
        (
            call(
                "Player",
                Some(json!({ "library": "shared" })),
                "add_animation",
                vec![
                    text("x"),
                    tagged("new_resource", json!({ "class": "Animation" })),
                ],
            ),
            "external_resource",
        ),
        (
            call("Line", None, "set_cell", vec![vector2i(0, 0), integer(0)]),
            "method_not_listed",
        ),
        (
            call("Line", None, "add_point", vec![text("not a point")]),
            "invalid_arguments",
        ),
        (
            call(
                "Line",
                None,
                "add_point",
                vec![vector2(1.0, 1.0), integer(0), integer(1)],
            ),
            "invalid_arguments",
        ),
        (
            call(
                "Line",
                Some(json!({ "property": "texture" })),
                "add_point",
                vec![vector2(1.0, 1.0)],
            ),
            "not_a_resource",
        ),
        (
            call(
                "Player",
                Some(json!({ "animation": "missing" })),
                "add_track",
                vec![integer(0)],
            ),
            "unknown_animation",
        ),
    ]
}

/// The call that hands the scene a library saved in its own file.
fn shared_library_call() -> Value {
    call(
        "Player",
        None,
        "add_animation_library",
        vec![
            text("shared"),
            tagged(
                "resource",
                json!({ "resource": "res://lib.tres", "resource_type": "AnimationLibrary" }),
            ),
        ],
    )
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn editing_calls_refuse_what_the_scene_does_not_own() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        editing_calls_refuse_what_the_scene_does_not_own_in(binary);
    }
}

fn editing_calls_refuse_what_the_scene_does_not_own_in(binary: &Path) {
    let library_sha = sha256_hex(LIBRARY.as_bytes());

    // Headless.
    let headless = ProjectTree::new("live-own-headless");
    headless.write("main.tscn", OWN_SCENE);
    headless.write("lib.tres", LIBRARY);
    let mut session = StdioSession::initialized(&Spawn {
        project: &headless.root,
        current_dir: None,
        profile: "full",
        godot: Some(binary),
    });
    let context = binary.display().to_string();
    let scene_sha = |tree: &ProjectTree| {
        sha256_hex(&std::fs::read(tree.root.join("main.tscn")).expect("scene"))
    };
    let shared = ok(
        &session.call_tool(
            2,
            "scene.batch.apply",
            json!({
                "path": "main.tscn",
                "expectedSha256": scene_sha(&headless),
                "operations": [shared_library_call()],
                "idempotencyId": "own-shared"
            }),
        ),
        &context,
    );
    assert_eq!(shared["results"][0]["value"], integer(0), "{shared}");
    let scene = std::fs::read_to_string(headless.root.join("main.tscn")).expect("scene");
    assert!(
        scene.contains("path=\"res://lib.tres\""),
        "{context}: the scene refers to the library's file\n{scene}"
    );
    assert_eq!(
        sha256_hex(&std::fs::read(headless.root.join("lib.tres")).expect("library")),
        library_sha,
        "{context}: handing a call a resource file leaves that file as it was"
    );
    let mut id = 2;
    for (operation, reason) in refused_calls() {
        id += 1;
        let before = scene_sha(&headless);
        let refused = session.call_tool(
            id,
            "scene.batch.apply",
            json!({
                "path": "main.tscn",
                "expectedSha256": before,
                "operations": [operation],
                "idempotencyId": format!("own-{id}")
            }),
        );
        assert!(
            refused
                .to_string()
                .contains(&format!("{reason} at operation 0")),
            "{context}, headless: {operation} is refused as {reason}: {refused}"
        );
        assert_eq!(
            scene_sha(&headless),
            before,
            "{context}: a refusal writes nothing"
        );
    }
    let _ = session.finish();

    // In the editor.
    let (tree, mut session, mut ids) = project_with_addon(
        "live-own",
        &[("main.tscn", OWN_SCENE), ("lib.tres", LIBRARY)],
    );
    let editor = Editor::launch(binary, &tree.root, &tree.base.join("home"));
    wait_connected(&mut session, &mut ids, &editor);
    let mark = editor.mark();
    let context = editor.context();
    ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.open",
            json!({ "path": "main.tscn", "idempotencyId": "own-open" }),
        ),
        &context,
    );
    let original = digest_of(&mut session, &mut ids, &context);
    let shared = ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.edit",
            json!({
                "expectedDigest": original,
                "operations": [shared_library_call()],
                "idempotencyId": "own-edit-shared"
            }),
        ),
        &editor.context(),
    );
    assert_eq!(shared["results"][0]["value"], integer(0), "{shared}");
    let mut digest = shared["digest"].as_str().expect("digest").to_owned();
    for (operation, reason) in refused_calls() {
        let refused = session.call_tool(
            ids.next(),
            "editor.scene.edit",
            json!({
                "expectedDigest": digest,
                "operations": [operation],
                "idempotencyId": format!("own-edit-{reason}-{}", ids.0)
            }),
        );
        assert!(
            refused
                .to_string()
                .contains(&format!("{reason} at operation 0")),
            "{}, editor: {operation} is refused as {reason}: {refused}",
            editor.binary
        );
        assert_eq!(
            digest_of(&mut session, &mut ids, &editor.context()),
            digest,
            "{}: a refused call changes nothing",
            editor.binary
        );
        digest = digest_of(&mut session, &mut ids, &editor.context());
    }
    ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.save",
            json!({ "idempotencyId": "own-save" }),
        ),
        &editor.context(),
    );
    assert_eq!(
        sha256_hex(&std::fs::read(tree.root.join("lib.tres")).expect("library")),
        library_sha,
        "{}: saving the scene leaves the library's file as it was",
        editor.binary
    );
    editor.assert_no_errors_since(mark, "refusing calls the scene does not own");
    let _ = session.finish();
    drop(editor);
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn created_and_edited_resources_call_their_own_methods() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        created_and_edited_resources_call_their_own_methods_in(binary);
    }
}

fn created_and_edited_resources_call_their_own_methods_in(binary: &Path) {
    let tree = ProjectTree::new("live-resource-calls");
    tree.write(
        "curve.tres",
        "[gd_resource type=\"Curve\" format=3]\n\n[resource]\n",
    );
    let mut session = StdioSession::initialized(&Spawn {
        project: &tree.root,
        current_dir: None,
        profile: "full",
        godot: Some(binary),
    });
    let context = binary.display().to_string();
    let read = |path: &str| std::fs::read_to_string(tree.root.join(path)).expect("written file");

    let created = ok(
        &session.call_tool(
            2,
            "resource.create",
            json!({
                "path": "grad.tres",
                "class": "Gradient",
                "operations": [{ "op": "call_method", "method": "add_point",
                                 "arguments": [number(0.5), red()] }],
                "idempotencyId": "create-gradient"
            }),
        ),
        &context,
    );
    assert_eq!(
        created["results"],
        json!([{ "index": 0, "method": "add_point", "value": null }]),
        "{context}: {created}"
    );
    assert!(
        read("grad.tres")
            .lines()
            .any(|line| line.starts_with("offsets = PackedFloat32Array(") && line.contains("0.5")),
        "{context}: {}",
        read("grad.tres")
    );

    let edited = ok(
        &session.call_tool(
            3,
            "resource.batch.apply",
            json!({
                "path": "curve.tres",
                "expectedSha256": sha256_hex(read("curve.tres").as_bytes()),
                "operations": [
                    { "op": "call_method", "method": "add_point", "arguments": [vector2(0.0, 1.0)] },
                    { "op": "call_method", "method": "set_point_value", "arguments": [integer(0), number(0.5)] }
                ],
                "idempotencyId": "edit-curve"
            }),
        ),
        &context,
    );
    assert_eq!(
        edited["results"],
        json!([
            { "index": 0, "method": "add_point", "value": integer(0) },
            { "index": 1, "method": "set_point_value", "value": null }
        ]),
        "{context}: {edited}"
    );
    assert!(
        read("curve.tres").contains("point_count = 1")
            && read("curve.tres").contains("Vector2(0, 0.5)"),
        "{context}: {}",
        read("curve.tres")
    );

    let before = sha256_hex(read("curve.tres").as_bytes());
    let unlisted = session.call_tool(
        4,
        "resource.batch.apply",
        json!({
            "path": "curve.tres",
            "expectedSha256": before,
            "operations": [{ "op": "call_method", "method": "add_track", "arguments": [integer(0)] }],
            "idempotencyId": "curve-unlisted"
        }),
    );
    assert!(
        unlisted
            .to_string()
            .contains("method_not_listed at operation 0"),
        "{context}: a Curve has no add_track: {unlisted}"
    );
    let selected = session.call_tool(
        5,
        "resource.batch.apply",
        json!({
            "path": "curve.tres",
            "expectedSha256": before,
            "operations": [{ "op": "call_method", "resource": { "property": "x" },
                             "method": "add_point", "arguments": [vector2(0.0, 0.0)] }],
            "idempotencyId": "curve-selector"
        }),
    );
    assert_eq!(error_code(&selected), json!("invalid_input"), "{selected}");
    assert_eq!(sha256_hex(read("curve.tres").as_bytes()), before);

    let scene = ok(
        &session.call_tool(
            6,
            "scene.create",
            json!({
                "path": "line.tscn",
                "rootType": "Line2D",
                "operations": [{ "op": "call_method", "node": ".", "method": "add_point",
                                 "arguments": [vector2(5.0, 5.0)] }],
                "idempotencyId": "create-line"
            }),
        ),
        &context,
    );
    assert_eq!(scene["results"][0]["method"], "add_point", "{scene}");
    assert!(
        read("line.tscn").contains("points = PackedVector2Array(5, 5)"),
        "{context}: {}",
        read("line.tscn")
    );
    let _ = session.finish();
}

const PLAY_MAIN: &str = "[gd_scene format=3]\n\n[node name=\"Main\" type=\"Node2D\"]\n";
const PLAY_OTHER: &str = "[gd_scene format=3]\n\n[node name=\"Other\" type=\"Node2D\"]\n";

/// The processes whose parent is `pid`: the game a running editor started.
fn children_of(pid: u32) -> Vec<u32> {
    let output = Command::new("pgrep")
        .arg("-P")
        .arg(pid.to_string())
        .output()
        .expect("run pgrep");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

/// Wait until `pid` has no child process, at most 10 seconds.
fn wait_childless(pid: u32, editor: &Editor) {
    let started = Instant::now();
    while !children_of(pid).is_empty() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{}: the game process {:?} outlived its stop\n{}",
            editor.binary,
            children_of(pid),
            editor.output()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn playing_now(session: &mut StdioSession, ids: &mut Ids, editor: &Editor) -> (Value, Value) {
    let status = ok(
        &session.call_tool(ids.next(), "editor.status.get", json!({})),
        &editor.context(),
    );
    (
        status["editor"]["playing"].clone(),
        status["editor"]["playing_scene"].clone(),
    )
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn a_windowed_editor_runs_and_stops_the_project() {
    if std::env::var("GODOT_MCP_DISPLAY_REQUIRED").as_deref() != Ok("1") {
        eprintln!("GODOT_MCP_DISPLAY_REQUIRED is not 1; running the project was not checked");
        return;
    }
    assert!(
        std::env::var_os("DISPLAY").is_some(),
        "GODOT_MCP_DISPLAY_REQUIRED=1 needs a display: run under xvfb-run"
    );
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        a_windowed_editor_runs_and_stops_the_project_in(binary);
    }
}

fn a_windowed_editor_runs_and_stops_the_project_in(binary: &Path) {
    let (tree, mut session, mut ids) = project_with_addon(
        "live-play",
        &[
            ("main.tscn", PLAY_MAIN),
            ("other.tscn", PLAY_OTHER),
            ("levels/level.tscn", PLAY_OTHER),
        ],
    );
    let editor = Editor::launch_windowed(binary, &tree.root, &tree.base.join("home"));
    let pid = editor.child.id();
    wait_connected(&mut session, &mut ids, &editor);
    let mark = editor.mark();
    ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.open",
            json!({ "path": "main.tscn", "idempotencyId": "play-open" }),
        ),
        &editor.context(),
    );

    // No main scene: the editor would ask for one, so nothing runs.
    let unset = session.call_tool(
        ids.next(),
        "editor.play.start",
        json!({ "idempotencyId": "play-no-main" }),
    );
    assert_eq!(error_code(&unset), json!("invalid_input"), "{unset}");
    assert!(unset.to_string().contains("main scene"), "{unset}");
    assert!(
        children_of(pid).is_empty(),
        "{}: nothing ran",
        editor.binary
    );

    // The scene being edited, with an edit the editor saves before it runs.
    let digest = digest_of(&mut session, &mut ids, &editor.context());
    ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.edit",
            json!({
                "expectedDigest": digest,
                "operations": [{ "op": "add_node", "parent": ".", "name": "Added", "type": "Node2D" }],
                "idempotencyId": "play-edit"
            }),
        ),
        &editor.context(),
    );
    let current = ok(
        &session.call_tool(
            ids.next(),
            "editor.play.start",
            json!({ "scene": "current", "idempotencyId": "play-current" }),
        ),
        &editor.context(),
    );
    assert_eq!(current["playing"], json!(true), "{current}");
    assert_eq!(current["scene"], "res://main.tscn", "{current}");
    let bytes = std::fs::read(tree.root.join("main.tscn")).expect("scene");
    assert!(
        String::from_utf8_lossy(&bytes).contains("name=\"Added\""),
        "{}: the editor saved the edit before running",
        editor.binary
    );
    assert_eq!(
        current["saved"],
        json!([{ "path": "res://main.tscn", "sha256": sha256_hex(&bytes) }]),
        "{current}"
    );
    assert_eq!(
        playing_now(&mut session, &mut ids, &editor),
        (json!(true), json!("res://main.tscn"))
    );
    assert!(
        !children_of(pid).is_empty(),
        "{}: the game runs as the editor's child",
        editor.binary
    );
    let again = session.call_tool(
        ids.next(),
        "editor.play.start",
        json!({ "idempotencyId": "play-again" }),
    );
    assert_eq!(error_code(&again), json!("conflict"), "{again}");
    let stopped = ok(
        &session.call_tool(
            ids.next(),
            "editor.play.stop",
            json!({ "idempotencyId": "play-stop-current" }),
        ),
        &editor.context(),
    );
    assert_eq!(stopped["was_playing"], json!(true), "{stopped}");
    assert_eq!(stopped["playing"], json!(false), "{stopped}");
    wait_childless(pid, &editor);
    assert_eq!(playing_now(&mut session, &mut ids, &editor).0, json!(false));

    // A scene named by path, then the main scene once the project has one.
    let other = ok(
        &session.call_tool(
            ids.next(),
            "editor.play.start",
            json!({ "scene": "other.tscn", "idempotencyId": "play-other" }),
        ),
        &editor.context(),
    );
    assert_eq!(other["scene"], "res://other.tscn", "{other}");
    // Godot 4.4.1 saves every open scene before each run, changed or not; the
    // bytes stay the same.
    for saved in other["saved"].as_array().expect("saved") {
        assert_eq!(saved["path"], "res://main.tscn", "{other}");
        assert_eq!(saved["sha256"], json!(sha256_hex(&bytes)), "{other}");
    }
    assert_eq!(
        std::fs::read(tree.root.join("main.tscn")).expect("scene"),
        bytes,
        "{}: an unchanged scene keeps its bytes",
        editor.binary
    );
    ok(
        &session.call_tool(
            ids.next(),
            "editor.play.stop",
            json!({ "idempotencyId": "play-stop-other" }),
        ),
        &editor.context(),
    );
    wait_childless(pid, &editor);
    let sha = settings_sha(&mut session, &mut ids, &editor.context());
    ok(
        &session.call_tool(
            ids.next(),
            "project.settings.patch",
            json!({
                "expectedSha256": sha,
                "operations": [{ "op": "set_main_scene", "scene": "res://main.tscn" }],
                "idempotencyId": "play-set-main"
            }),
        ),
        &editor.context(),
    );
    let main = ok(
        &session.call_tool(
            ids.next(),
            "editor.play.start",
            json!({ "idempotencyId": "play-main" }),
        ),
        &editor.context(),
    );
    assert_eq!(main["scene"], "res://main.tscn", "{main}");
    ok(
        &session.call_tool(
            ids.next(),
            "editor.play.stop",
            json!({ "idempotencyId": "play-stop-main" }),
        ),
        &editor.context(),
    );
    wait_childless(pid, &editor);
    let idle = ok(
        &session.call_tool(
            ids.next(),
            "editor.play.stop",
            json!({ "idempotencyId": "play-stop-idle" }),
        ),
        &editor.context(),
    );
    assert_eq!(idle["was_playing"], json!(false), "{idle}");
    editor.assert_no_errors_since(mark, "running and stopping the project");

    // A scene the editor cannot save before running it stops the game, and
    // whether its file was written is not known.
    edit_open_scene(
        &mut session,
        &mut ids,
        &editor,
        "levels/level.tscn",
        "Unsaved",
    );
    let refused = {
        let _locked = ReadOnly::new(tree.root.join("levels/level.tscn"));
        session.call_tool(
            ids.next(),
            "editor.play.start",
            json!({ "scene": "current", "idempotencyId": "play-locked" }),
        )
    };
    assert_eq!(error_code(&refused), json!("uncertain"), "{refused}");
    assert_eq!(
        std::fs::read_to_string(tree.root.join("levels/level.tscn")).expect("scene"),
        PLAY_OTHER,
        "{}: the read-only scene was not written",
        editor.binary
    );
    wait_childless(pid, &editor);
    let _ = session.finish();
    drop(editor);
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn a_headless_editor_runs_nothing() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        a_headless_editor_runs_nothing_in(binary);
    }
}

fn a_headless_editor_runs_nothing_in(binary: &Path) {
    let (tree, mut session, mut ids) =
        project_with_addon("live-play-headless", &[("main.tscn", PLAY_MAIN)]);
    let editor = Editor::launch(binary, &tree.root, &tree.base.join("home"));
    let pid = editor.child.id();
    wait_connected(&mut session, &mut ids, &editor);
    let refused = session.call_tool(
        ids.next(),
        "editor.play.start",
        json!({ "scene": "main.tscn", "idempotencyId": "headless-play" }),
    );
    assert_eq!(error_code(&refused), json!("unavailable"), "{refused}");
    assert!(refused.to_string().contains("headless"), "{refused}");
    assert!(
        children_of(pid).is_empty(),
        "{}: nothing ran",
        editor.binary
    );
    let _ = session.finish();
    drop(editor);
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn a_managed_game_reports_its_performance_monitors() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        a_managed_game_reports_its_performance_monitors_in(binary);
    }
}

fn a_managed_game_reports_its_performance_monitors_in(binary: &Path) {
    let tree = ProjectTree::new("live-performance");
    tree.write("main.tscn", PLAY_MAIN);
    let mut session = StdioSession::initialized(&Spawn {
        project: &tree.root,
        current_dir: None,
        profile: "runtime",
        godot: Some(binary),
    });
    let context = binary.display().to_string();
    let started = ok(
        &session.call_tool(
            2,
            "project.run.start",
            json!({ "scene": "main.tscn", "display": "headless", "idempotencyId": "perf-start" }),
        ),
        &context,
    );
    let id = started["session"]["session"]
        .as_str()
        .expect("session id")
        .to_owned();
    let every = ok(
        &session.call_tool(3, "runtime.performance.get", json!({ "session": id })),
        &context,
    );
    for monitor in [
        "TIME_FPS",
        "TIME_PROCESS",
        "OBJECT_COUNT",
        "OBJECT_NODE_COUNT",
        "MEMORY_STATIC",
    ] {
        assert!(
            every["monitors"][monitor].is_number(),
            "{context}: {monitor} is a number: {every}"
        );
    }
    assert!(
        every["monitors"]["OBJECT_NODE_COUNT"].as_f64() >= Some(1.0),
        "{context}: the game has nodes: {every}"
    );
    assert!(every["frame"].is_u64(), "{every}");
    let named = ok(
        &session.call_tool(
            4,
            "runtime.performance.get",
            json!({ "session": id, "monitors": ["TIME_FPS", "OBJECT_NODE_COUNT"] }),
        ),
        &context,
    );
    let mut names: Vec<&String> = named["monitors"]
        .as_object()
        .expect("monitors")
        .keys()
        .collect();
    names.sort();
    assert_eq!(names, ["OBJECT_NODE_COUNT", "TIME_FPS"], "{named}");
    let unknown = session.call_tool(
        5,
        "runtime.performance.get",
        json!({ "session": id, "monitors": ["NOT_A_MONITOR"] }),
    );
    assert_eq!(error_code(&unknown), json!("invalid_input"), "{unknown}");
    ok(
        &session.call_tool(
            6,
            "project.run.stop",
            json!({ "session": id, "idempotencyId": "perf-stop" }),
        ),
        &context,
    );
    let _ = session.finish();
}

/// A test-only editor plugin that reads and works the godot_mcp panel as a
/// user does: `probe_panel` writes what the bottom panel and the dock show to
/// `probe_panel_dump`; `probe_pause` and `probe_resume` turn the dock's
/// "Pause agent edits" switch on and off; `probe_reconnect` presses the
/// dock's "Reconnect now".
const PANEL_PROBE: &str = r#"@tool
extends EditorPlugin

func _find(name: String) -> Node:
	return EditorInterface.get_base_control().find_child(name, true, false)

func _done(marker: String, text: String) -> void:
	var part := ProjectSettings.globalize_path("res://%s.part" % marker)
	FileAccess.open(part, FileAccess.WRITE).store_string(text)
	DirAccess.rename_absolute(part, ProjectSettings.globalize_path("res://%s" % marker))

func _process(_delta: float) -> void:
	if FileAccess.file_exists("res://probe_panel"):
		DirAccess.remove_absolute(ProjectSettings.globalize_path("res://probe_panel"))
		var panel := _find("GodotMcpPanel")
		var dock := _find("GodotMcp")
		_done("probe_panel_dump", JSON.stringify({
			"panel": panel.snapshot() if panel != null else null,
			"dock": dock.snapshot() if dock != null else null,
		}))
	for switch in [["probe_pause", true], ["probe_resume", false]]:
		if FileAccess.file_exists("res://%s" % switch[0]):
			DirAccess.remove_absolute(ProjectSettings.globalize_path("res://%s" % switch[0]))
			_find("GodotMcp").set_paused(switch[1])
			_done("%s_done" % switch[0], "done")
	if FileAccess.file_exists("res://probe_reconnect"):
		DirAccess.remove_absolute(ProjectSettings.globalize_path("res://probe_reconnect"))
		_find("GodotMcp").find_child("ReconnectNow", true, false).pressed.emit()
		_done("probe_reconnect_done", "done")
"#;

/// What the godot_mcp panel and dock show now, through the panel probe.
fn panel_dump(tree: &ProjectTree, editor: &Editor) -> Value {
    let dump = tree.root.join("probe_panel_dump");
    let _ = std::fs::remove_file(&dump);
    std::fs::write(tree.root.join("probe_panel"), b"dump").expect("panel marker");
    wait_for_file(&dump, editor);
    let text = std::fs::read_to_string(&dump).expect("panel dump");
    serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("{}: the panel dump {text:?}: {error}", editor.binary))
}

/// Turn the dock's "Pause agent edits" switch, as a user clicking it does.
fn switch_pause(tree: &ProjectTree, editor: &Editor, paused: bool) {
    let marker = if paused {
        "probe_pause"
    } else {
        "probe_resume"
    };
    let done = tree.root.join(format!("{marker}_done"));
    let _ = std::fs::remove_file(&done);
    std::fs::write(tree.root.join(marker), b"switch").expect("switch marker");
    wait_for_file(&done, editor);
}

fn add_node_batch(session: &mut StdioSession, ids: &mut Ids, sha: &str, name: &str) -> Value {
    session.call_tool(
        ids.next(),
        "scene.batch.apply",
        json!({
            "path": "main.tscn",
            "expectedSha256": sha,
            "operations": [{ "op": "add_node", "parent": ".", "name": name, "type": "Node2D" }],
            "idempotencyId": format!("panel-batch-{name}")
        }),
    )
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn the_panel_shows_the_server_and_pauses_agent_edits() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        the_panel_shows_the_server_and_pauses_agent_edits_in(binary);
    }
}

fn the_panel_shows_the_server_and_pauses_agent_edits_in(binary: &Path) {
    let (tree, mut session, mut ids) = project_with_probe(
        "live-panel",
        "panel_probe",
        PANEL_PROBE,
        &[("main.tscn", EDIT_SCENE)],
    );
    let launched = Instant::now();
    let editor = Editor::launch(binary, &tree.root, &tree.base.join("home"));
    wait_connected(&mut session, &mut ids, &editor);
    let connected = launched.elapsed();
    let mark = editor.mark();
    let context = editor.context();

    // The Tools tab lists exactly what the server serves, grouped by need.
    let listed = session.request(ids.next(), "tools/list", json!({}));
    let tools = listed["result"]["tools"].as_array().expect("tools").len();
    let mut shown = panel_dump(&tree, &editor);
    while shown["panel"]["tools"].as_u64().unwrap_or(0) == 0 {
        assert!(
            launched.elapsed() < Duration::from_secs(90),
            "{}: the Tools tab was never populated: {shown}\n{}",
            editor.binary,
            editor.output()
        );
        std::thread::sleep(Duration::from_millis(100));
        shown = panel_dump(&tree, &editor);
    }
    let populated = launched.elapsed();
    let panel = &shown["panel"];
    eprintln!(
        "{}: connected {} ms after launch; the Tools tab was seen populated {} ms after launch; \
         the panel took the server status {} ms after the engine started",
        editor.binary,
        connected.as_millis(),
        populated.as_millis(),
        panel["first_status_ticks_msec"]
    );
    assert_eq!(panel["tools_state"], "described", "{shown}");
    assert_eq!(panel["tools"], json!(tools), "{shown}");
    assert!(panel["tool_groups"].as_u64() >= Some(3), "{shown}");
    assert!(
        panel["tools_note"]
            .as_str()
            .is_some_and(|note| note.contains("full profile")),
        "{shown}"
    );
    assert_eq!(
        panel["tabs"],
        json!(["Status", "Activity", "Tools", "Add-on", "Controls"]),
        "{shown}"
    );
    assert_eq!(
        panel["status"]["server"],
        env!("CARGO_PKG_VERSION"),
        "{shown}"
    );
    assert!(panel["addon_methods"].as_u64() > Some(20), "{shown}");
    assert_eq!(shown["dock"]["paused_banner"], json!(false), "{shown}");

    // A mutation is allowed until the user pauses agent edits.
    ok(
        &session.call_tool(
            ids.next(),
            "editor.scene.open",
            json!({ "path": "main.tscn", "idempotencyId": "panel-open" }),
        ),
        &context,
    );
    let digest = digest_of(&mut session, &mut ids, &context);
    let sha = sha256_hex(&std::fs::read(tree.root.join("main.tscn")).expect("scene"));

    switch_pause(&tree, &editor, true);
    let paused = panel_dump(&tree, &editor);
    assert_eq!(paused["dock"]["paused_banner"], json!(true), "{paused}");
    assert_eq!(paused["panel"]["paused"], json!(true), "{paused}");

    // Paused, a scene batch the editor would save is refused before it
    // starts, and nothing reaches the file or the editor's scene.
    let refused = add_node_batch(&mut session, &mut ids, &sha, "WhilePaused");
    assert_eq!(error_code(&refused), json!("unavailable"), "{refused}");
    assert!(
        refused.to_string().contains("paused"),
        "the refusal says edits are paused: {refused}"
    );
    assert!(
        refused.to_string().contains("not_started"),
        "nothing started: {refused}"
    );
    let live = session.call_tool(
        ids.next(),
        "editor.scene.edit",
        json!({
            "expectedDigest": digest,
            "operations": [{ "op": "add_node", "parent": ".", "name": "LivePaused", "type": "Node2D" }],
            "idempotencyId": "panel-edit-paused"
        }),
    );
    assert_eq!(error_code(&live), json!("unavailable"), "{live}");
    assert_eq!(
        sha256_hex(&std::fs::read(tree.root.join("main.tscn")).expect("scene")),
        sha,
        "{}: a paused editor changed the file",
        editor.binary
    );
    // The editor carries out project settings changes while it is connected,
    // so they are paused too.
    let settings_before = manifest(&tree);
    let sha_settings = settings_sha(&mut session, &mut ids, &context);
    let settings = session.call_tool(
        ids.next(),
        "project.settings.patch",
        json!({
            "expectedSha256": sha_settings,
            "operations": [{ "op": "set_value", "setting": "application/config/name", "value": "Paused" }],
            "idempotencyId": "panel-settings-paused"
        }),
    );
    assert_eq!(error_code(&settings), json!("unavailable"), "{settings}");
    assert_eq!(manifest(&tree), settings_before, "{}", editor.binary);
    // Reads still work while paused.
    assert_eq!(digest_of(&mut session, &mut ids, &context), digest);

    // Unpaused, the same batch goes through and the editor saves it.
    switch_pause(&tree, &editor, false);
    let applied = ok(
        &add_node_batch(&mut session, &mut ids, &sha, "AfterResume"),
        &context,
    );
    assert_eq!(applied["backend"], "editor", "{applied}");
    let text = std::fs::read_to_string(tree.root.join("main.tscn")).expect("scene");
    assert!(
        text.contains("name=\"AfterResume\"") && !text.contains("WhilePaused"),
        "{}: {text}",
        editor.binary
    );

    // The Activity tab holds the requests, the refusal among them, and no
    // argument value.
    let after = panel_dump(&tree, &editor);
    let rows = after["panel"]["activity"]
        .as_array()
        .expect("activity rows");
    let batches: Vec<&Value> = rows
        .iter()
        .filter(|row| row[1] == "editor.scene.batch")
        .collect();
    assert_eq!(batches.len(), 2, "{after}");
    // Newest first: the batch that went through, then the refused one.
    assert_eq!(batches[0][3], "res://main.tscn", "{after}");
    assert_eq!(batches[0][4], "ok", "{after}");
    assert_eq!(batches[0][5], "committed", "{after}");
    assert_eq!(batches[1][4], "unavailable", "{after}");
    assert_eq!(batches[1][5], "not_started", "{after}");
    assert!(
        rows.iter()
            .any(|row| row[1] == "editor.scene.tree" && row[2] == "read"),
        "{after}"
    );
    assert!(after["panel"]["refused"].as_u64() >= Some(2), "{after}");
    assert!(after["panel"]["served"].as_u64() >= Some(3), "{after}");
    assert!(
        !after.to_string().contains("WhilePaused") && !after.to_string().contains("AfterResume"),
        "the panel shows no argument value: {after}"
    );

    // "Reconnect now" drops the session and dials again at once: the server
    // describes a second session, and the panel shows why the first ended
    // and the tools again.
    let described = |dump: &Value| {
        dump["panel"]["activity"].as_array().map_or(0, |rows| {
            rows.iter()
                .filter(|row| row[1] == "bridge.describe_session")
                .count()
        })
    };
    assert_eq!(described(&after), 1, "{after}");
    let done = tree.root.join("probe_reconnect_done");
    std::fs::write(tree.root.join("probe_reconnect"), b"reconnect").expect("reconnect marker");
    wait_for_file(&done, &editor);
    let started = Instant::now();
    let mut again = panel_dump(&tree, &editor);
    while described(&again) < 2 || again["panel"]["tools_state"] != "described" {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "{}: the editor never reconnected: {again}\n{}",
            editor.binary,
            editor.output()
        );
        std::thread::sleep(Duration::from_millis(100));
        again = panel_dump(&tree, &editor);
    }
    assert_eq!(
        again["panel"]["status"]["connection"], "connected",
        "{again}"
    );
    assert!(
        again["panel"]["status"]["last_disconnect"]
            .as_str()
            .is_some_and(|reason| reason.contains("reconnect")),
        "{again}"
    );
    wait_connected(&mut session, &mut ids, &editor);
    assert_eq!(digest_of(&mut session, &mut ids, &context).len(), 64);
    editor.assert_no_errors_since(mark, "showing the panel, pausing edits and reconnecting");
    let _ = session.finish();
    drop(editor);
}

/// The add-on panel's own cases, kept beside the add-on: they drive its state,
/// dispatcher, bridge client, bottom panel and dock as the plugin wires them,
/// headless, with no editor and no server.
const PANEL_CASES: &str =
    include_str!("../../godot-mcp-bridge/tests/fixtures/addon/panel_cases.gd");

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn the_panel_cases_pass_against_the_shipped_add_on() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        the_panel_cases_pass_against_the_shipped_add_on_in(binary);
    }
}

fn the_panel_cases_pass_against_the_shipped_add_on_in(binary: &Path) {
    let tree = ProjectTree::new("live-panel-cases");
    tree.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"Panel cases\"\n",
    );
    for file in godot_mcp_bridge::addon::FILES {
        let path = tree
            .root
            .join(godot_mcp_bridge::addon::ADDON_DIRECTORY)
            .join(file.name);
        std::fs::create_dir_all(path.parent().expect("add-on directory")).expect("add-on dir");
        std::fs::write(&path, file.bytes).expect("add-on file");
    }
    tree.write("panel_cases.gd", PANEL_CASES);
    let mut run = Editor::launch_with(
        binary,
        &tree.root,
        &tree.base.join("home"),
        &["--headless", "--script", "res://panel_cases.gd", "--path"],
    );
    let started = Instant::now();
    let status = loop {
        if let Some(status) = run.child.try_wait().expect("wait for Godot") {
            break status;
        }
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "{}: the panel cases never finished\n{}",
            run.binary,
            run.output()
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    let printed = run.output();
    let context = format!("{}\n{printed}", run.binary);
    let declared = PANEL_CASES
        .lines()
        .skip_while(|line| !line.starts_with("const CASES := ["))
        .skip(1)
        .take_while(|line| !line.starts_with(']'))
        .count();
    assert!(declared > 0, "the cases runner declares its cases");
    let failures: Vec<&str> = printed
        .lines()
        .filter(|line| line.starts_with("panel_case FAIL"))
        .collect();
    assert!(failures.is_empty(), "{failures:#?}\n{context}");
    let passed = printed
        .lines()
        .filter(|line| line.starts_with("panel_case ok "))
        .count();
    assert_eq!(passed, declared, "every declared case ran\n{context}");
    assert!(
        printed.contains(&format!("panel_cases: {declared}/{declared} passed")),
        "{context}"
    );
    let noise: Vec<&str> = printed
        .lines()
        .filter(|line| {
            line.contains("ERROR")
                || line.contains("Parse Error")
                || line.contains("WARNING")
                || line.contains("leaked")
        })
        .collect();
    assert!(
        noise.is_empty(),
        "the cases print no engine error or warning: {noise:#?}\n{context}"
    );
    assert!(status.success(), "the cases exit 0: {status}\n{context}");
}
