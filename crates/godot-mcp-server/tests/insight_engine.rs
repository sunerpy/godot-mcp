//! Real-engine check: `project.uid.get` agrees with Godot's own UID answers.
//!
//! For every editor named in `GODOT_MCP_TEST_EDITORS` (comma-separated), or
//! the one verified archive `GODOT_MCP_LOCKED_ARTIFACT` names, a fresh project
//! is imported by that editor (`--headless --editor --import`), which writes
//! the `.uid` sidecars and `.import` files a real project carries. The
//! production server then answers `project.uid.get` for every file and
//! every UID it records, and a probe script asks the same Godot build
//! (`ResourceLoader.get_resource_uid`, `ResourceUID.has_id` and
//! `ResourceUID.get_id_path`) for its answers. They must agree in both
//! directions, and a file Godot assigns no UID must be `unknown`, never a
//! guess.
//!
//! It needs real editors, so the case is ignored on a plain `cargo test`; a run
//! that names none fails rather than passing without having run. Every editor
//! gets its own `HOME` and XDG directories, so no user setting is read or
//! written.

#![cfg(target_os = "linux")]

#[path = "support/live.rs"]
mod live;
mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::{ProjectTree, Spawn, StdioSession, structured};

/// A UID no file of the project records.
const UNRECORDED: &str = "uid://bnotrecorded1";

/// The UID the menu scene's header records; canonical, so Godot prints it back
/// unchanged.
const MENU_UID: &str = "uid://bmenuscene01";

const PROBE: &str = r#"extends SceneTree

func _walk(directory: String, out: Array) -> void:
	var access := DirAccess.open(directory)
	if access == null:
		return
	for name in access.get_files():
		if name.ends_with(".uid") or name.ends_with(".import") or name.begins_with("uid_probe"):
			continue
		out.append(directory.path_join(name))
	for name in access.get_directories():
		if not name.begins_with("."):
			_walk(directory.path_join(name), out)

func _initialize() -> void:
	var paths: Array = []
	_walk("res://", paths)
	paths.sort()
	for path in paths:
		var id := ResourceLoader.get_resource_uid(path)
		var text := "none"
		if id != ResourceUID.INVALID_ID:
			text = ResourceUID.id_to_text(id)
		print("PROBE\tpath\t", path, "\t", text)
	for text in OS.get_cmdline_user_args():
		var id := ResourceUID.text_to_id(text)
		var path := "none"
		if id != ResourceUID.INVALID_ID and ResourceUID.has_id(id):
			path = ResourceUID.get_id_path(id)
		print("PROBE\tuid\t", text, "\t", path)
	quit()
"#;

/// Run `binary` with `arguments` on `project` under private user directories
/// below `home`, and return its combined output once it exits.
fn run_godot(binary: &Path, home: &Path, arguments: &[&str]) -> String {
    for directory in ["config", "data", "cache", "runtime"] {
        std::fs::create_dir_all(home.join(directory)).expect("private user directory");
    }
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(home.join("runtime"), std::fs::Permissions::from_mode(0o700))
            .expect("a private runtime directory");
    }
    let log = home.join("godot.log");
    let output = std::fs::File::create(&log).expect("godot log");
    let mut child = Command::new(binary)
        .args(arguments)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env("XDG_RUNTIME_DIR", home.join("runtime"))
        .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent")
        .stdin(Stdio::null())
        .stdout(output.try_clone().expect("log handle"))
        .stderr(output)
        .spawn()
        .unwrap_or_else(|error| panic!("start {}: {error}", binary.display()));
    let deadline = Instant::now() + Duration::from_secs(180);
    let status = loop {
        if let Some(status) = child.try_wait().expect("query godot") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "{} {arguments:?} did not exit within 180 s: {}",
                binary.display(),
                std::fs::read_to_string(&log).unwrap_or_default()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let text = std::fs::read_to_string(&log).expect("read godot log");
    assert!(
        status.success(),
        "{} {arguments:?} failed with {status}: {text}",
        binary.display()
    );
    text
}

fn write_project(tree: &ProjectTree) {
    tree.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"uid-check\"\n\
         run/main_scene=\"res://scenes/level.tscn\"\n",
    );
    tree.write(
        "scripts/hero.gd",
        "class_name Hero\nextends Node2D\n\n@export var speed := 10\n",
    );
    tree.write(
        "shaders/fx.gdshader",
        "shader_type canvas_item;\nvoid fragment() { COLOR = vec4(1.0); }\n",
    );
    tree.write(
        "icon.svg",
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"16\" height=\"16\">\
         <rect width=\"16\" height=\"16\" fill=\"#478cbf\"/></svg>\n",
    );
    tree.write(
        "scenes/menu.tscn",
        &format!(
            "[gd_scene format=3 uid=\"{MENU_UID}\"]\n\n[node name=\"Menu\" type=\"Control\"]\n"
        ),
    );
    tree.write(
        "scenes/level.tscn",
        &format!(
            "[gd_scene load_steps=3 format=3]\n\n\
             [ext_resource type=\"Script\" path=\"res://scripts/hero.gd\" id=\"1_hero\"]\n\
             [ext_resource type=\"PackedScene\" uid=\"{MENU_UID}\" path=\"res://scenes/menu.tscn\" id=\"2_menu\"]\n\n\
             [node name=\"Level\" type=\"Node2D\"]\nscript = ExtResource(\"1_hero\")\n\n\
             [node name=\"Menu\" parent=\".\" instance=ExtResource(\"2_menu\")]\n"
        ),
    );
    tree.write(
        "data/stats.tres",
        "[gd_resource type=\"Resource\" format=3]\n\n[resource]\n",
    );
}

/// Godot's answers: path -> UID text (or none), and UID text -> path (or none).
fn godot_answers(
    output: &str,
) -> (
    BTreeMap<String, Option<String>>,
    BTreeMap<String, Option<String>>,
) {
    let mut paths = BTreeMap::new();
    let mut uids = BTreeMap::new();
    for line in output.lines() {
        let fields: Vec<&str> = line.trim_end().split('\t').collect();
        let [marker, kind, subject, answer] = fields.as_slice() else {
            continue;
        };
        if *marker != "PROBE" {
            continue;
        }
        let answer = (*answer != "none").then(|| (*answer).to_owned());
        match *kind {
            "path" => {
                let relative = subject.strip_prefix("res://").expect("res:// path");
                paths.insert(relative.to_owned(), answer);
            }
            "uid" => {
                uids.insert(
                    (*subject).to_owned(),
                    answer.map(|path| path.trim_start_matches("res://").to_owned()),
                );
            }
            other => panic!("unknown probe line kind {other}: {line}"),
        }
    }
    (paths, uids)
}

fn ok(response: &Value) -> &Value {
    assert_ne!(
        response["result"]["isError"],
        json!(true),
        "the call must succeed: {response}"
    );
    &structured(response)["data"]
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn uid_lookups_agree_with_godot() {
    let (editors, _unpacked) = live::editors();
    for editor in editors {
        let tree = ProjectTree::new("insight-uid");
        write_project(&tree);
        let home = tree.base.join("home");
        let root = tree.root.to_string_lossy().into_owned();
        run_godot(
            &editor,
            &home,
            &["--headless", "--editor", "--path", &root, "--import"],
        );
        for sidecar in [
            "scripts/hero.gd.uid",
            "shaders/fx.gdshader.uid",
            "icon.svg.import",
        ] {
            assert!(
                tree.root.join(sidecar).is_file(),
                "{} wrote {sidecar}",
                editor.display()
            );
        }

        // The table the server records, read before the probe script joins
        // the tree; the probe then asks Godot about every UID in it.
        let mut session = StdioSession::initialized(&Spawn::core(&tree.root));
        let table = session.call_tool(2, "project.uid.get", json!({ "limit": 1000 }));
        let table = ok(&table).clone();
        let exited = session.finish();
        assert_eq!(
            exited.code,
            Some(0),
            "the server exits cleanly: {}",
            exited.stderr
        );
        let recorded: BTreeMap<String, String> = table["records"]
            .as_array()
            .expect("records")
            .iter()
            .filter(|record| record["exists"] == json!(true))
            .map(|record| {
                (
                    record["path"].as_str().expect("path").to_owned(),
                    record["uid"].as_str().expect("uid").to_owned(),
                )
            })
            .collect();
        let mut asked: Vec<String> = recorded.values().cloned().collect();
        asked.push(UNRECORDED.to_owned());

        tree.write("uid_probe.gd", PROBE);
        let mut arguments = vec![
            "--headless",
            "--path",
            &root,
            "--script",
            "res://uid_probe.gd",
            "--",
        ];
        arguments.extend(asked.iter().map(String::as_str));
        let output = run_godot(&editor, &home, &arguments);
        for leftover in ["uid_probe.gd", "uid_probe.gd.uid"] {
            let _ = std::fs::remove_file(tree.root.join(leftover));
        }
        let (by_path, by_uid) = godot_answers(&output);
        assert!(
            by_path.contains_key("scripts/hero.gd") && by_uid.contains_key(UNRECORDED),
            "{} answered the probe: {output}",
            editor.display()
        );

        let mut session = StdioSession::initialized(&Spawn::core(&tree.root));
        let mut id = 10;
        let mut call = |arguments: Value| {
            id += 1;
            session.call_tool(id, "project.uid.get", arguments)
        };

        // Path -> UID, for every file Godot enumerates.
        let godot_known: BTreeMap<String, String> = by_path
            .iter()
            .filter_map(|(path, uid)| uid.clone().map(|uid| (path.clone(), uid)))
            .collect();
        assert_eq!(
            recorded,
            godot_known,
            "{}: the recorded UID table equals Godot's own",
            editor.display()
        );
        for (path, uid) in &by_path {
            let answer = call(json!({ "path": path }));
            let answer = ok(&answer);
            match uid {
                Some(uid) => {
                    assert_eq!(answer["status"], json!("resolved"), "{path}: {answer}");
                    assert_eq!(answer["value"], json!(uid), "{path}: {answer}");
                }
                None => {
                    assert_eq!(answer["status"], json!("unknown"), "{path}: {answer}");
                    assert_eq!(answer["value"], Value::Null, "{path}: {answer}");
                }
            }
        }

        // UID -> path, for every recorded UID and one nobody records.
        for (uid, path) in &by_uid {
            let answer = call(json!({ "uid": uid }));
            let answer = ok(&answer);
            match path {
                Some(path) => {
                    assert_eq!(answer["status"], json!("resolved"), "{uid}: {answer}");
                    assert_eq!(answer["value"], json!(path), "{uid}: {answer}");
                }
                None => {
                    assert_eq!(answer["status"], json!("unknown"), "{uid}: {answer}");
                    assert_eq!(answer["confidence"], json!("unresolved"), "{uid}: {answer}");
                }
            }
        }
        let menu = call(json!({ "uid": MENU_UID }));
        let referrers: BTreeSet<&str> = ok(&menu)["claims"]
            .as_array()
            .expect("claims")
            .iter()
            .filter_map(|claim| claim["referrer"].as_str())
            .collect();
        assert!(referrers.contains("scenes/level.tscn"), "{menu}");

        let exited = session.finish();
        assert_eq!(
            exited.code,
            Some(0),
            "the server exits cleanly: {}",
            exited.stderr
        );
        eprintln!(
            "{}: {} files, {} recorded UIDs agree with Godot",
            editor.display(),
            by_path.len(),
            recorded.len()
        );
    }
}

const CYCLE_PROBE: &str = r#"extends SceneTree

func _initialize() -> void:
	for path in OS.get_cmdline_user_args():
		var resource = ResourceLoader.load(path)
		var usable: bool = resource != null
		if usable and (resource is Script or resource is PackedScene):
			usable = resource.can_instantiate()
		print("CYCLE\t", path, "\t", "usable" if usable else "broken")
	quit()
"#;

/// Five rings, one per way Godot treats a cycle.
fn write_rings(tree: &ProjectTree) {
    tree.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"cycle-check\"\n",
    );
    for (this, other) in [("a", "b"), ("b", "a")] {
        tree.write(
            &format!("scenes/loop_{this}.tscn"),
            &format!(
                "[gd_scene load_steps=2 format=3]\n\n\
                 [ext_resource type=\"PackedScene\" path=\"res://scenes/loop_{other}.tscn\" id=\"1_{other}\"]\n\n\
                 [node name=\"Loop{this}\" type=\"Node\"]\n\n\
                 [node name=\"Inner\" parent=\".\" instance=ExtResource(\"1_{other}\")]\n"
            ),
        );
        tree.write(
            &format!("scripts/pre_{this}.gd"),
            &format!(
                "extends RefCounted\nconst OTHER = preload(\"res://scripts/pre_{other}.gd\")\n"
            ),
        );
        tree.write(
            &format!("scripts/load_{this}.gd"),
            &format!(
                "extends RefCounted\nfunc other() -> Resource:\n\treturn load(\"res://scripts/load_{other}.gd\")\n"
            ),
        );
        let upper = |name: &str| name.to_uppercase();
        tree.write(
            &format!("scripts/name_{this}.gd"),
            &format!(
                "class_name Name{}\nextends RefCounted\nfunc other() -> Name{}:\n\treturn Name{}.new()\n",
                upper(this),
                upper(other),
                upper(other)
            ),
        );
    }
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn cycle_classes_match_what_godot_does() {
    let (editors, _unpacked) = live::editors();
    for editor in editors {
        let tree = ProjectTree::new("insight-cycles");
        write_rings(&tree);
        let home = tree.base.join("home");
        let root = tree.root.to_string_lossy().into_owned();
        // The import builds the class-name cache the `class_name` ring needs.
        // The inheritance ring joins only afterwards: the 4.4.1 and 4.5.1
        // editors crash (SIGSEGV) while scanning a project that holds one.
        run_godot(
            &editor,
            &home,
            &["--headless", "--editor", "--path", &root, "--import"],
        );
        tree.write(
            "scripts/cycle_a.gd",
            "extends \"res://scripts/cycle_b.gd\"\n",
        );
        tree.write(
            "scripts/cycle_b.gd",
            "extends \"res://scripts/cycle_a.gd\"\n",
        );

        let mut session = StdioSession::initialized(&Spawn::core(&tree.root));
        let cycles = session.call_tool(
            2,
            "project.dependencies.audit",
            json!({ "view": "cycles", "limit": 100 }),
        );
        let cycles = ok(&cycles).clone();
        let exited = session.finish();
        assert_eq!(
            exited.code,
            Some(0),
            "the server exits cleanly: {}",
            exited.stderr
        );
        let classified: BTreeMap<String, String> = cycles["cycles"]
            .as_array()
            .expect("cycles")
            .iter()
            .map(|cycle| {
                let members: Vec<String> = cycle["members"]
                    .as_array()
                    .expect("members")
                    .iter()
                    .map(|member| member.as_str().expect("member").to_owned())
                    .collect();
                (
                    members.join(" "),
                    cycle["classification"].as_str().expect("class").to_owned(),
                )
            })
            .collect();
        assert_eq!(
            classified,
            BTreeMap::from([
                (
                    "scenes/loop_a.tscn scenes/loop_b.tscn".to_owned(),
                    "resource_load".to_owned()
                ),
                (
                    "scripts/cycle_a.gd scripts/cycle_b.gd".to_owned(),
                    "inheritance".to_owned()
                ),
                (
                    "scripts/load_a.gd scripts/load_b.gd".to_owned(),
                    "tolerated".to_owned()
                ),
                (
                    "scripts/name_a.gd scripts/name_b.gd".to_owned(),
                    "tolerated".to_owned()
                ),
                (
                    "scripts/pre_a.gd scripts/pre_b.gd".to_owned(),
                    "tolerated".to_owned()
                ),
            ]),
            "{cycles}"
        );

        tree.write("cycle_probe.gd", CYCLE_PROBE);
        let members: Vec<String> = classified
            .keys()
            .flat_map(|members| members.split(' ').map(|member| format!("res://{member}")))
            .collect();
        let mut arguments = vec![
            "--headless",
            "--path",
            &root,
            "--script",
            "res://cycle_probe.gd",
            "--",
        ];
        arguments.extend(members.iter().map(String::as_str));
        let output = run_godot(&editor, &home, &arguments);
        let usable: BTreeMap<&str, bool> = output
            .lines()
            .filter_map(|line| {
                let fields: Vec<&str> = line.trim_end().split('\t').collect();
                match fields.as_slice() {
                    ["CYCLE", path, state] => {
                        Some((path.trim_start_matches("res://"), *state == "usable"))
                    }
                    _ => None,
                }
            })
            .collect();
        let errors: Vec<&str> = output
            .lines()
            .filter(|line| line.contains("ERROR"))
            .collect();
        for cycle in cycles["cycles"].as_array().expect("cycles") {
            let members: Vec<&str> = cycle["members"]
                .as_array()
                .expect("members")
                .iter()
                .map(|member| member.as_str().expect("member"))
                .collect();
            let mentioned = |member: &str| {
                errors
                    .iter()
                    .any(|line| line.contains(&format!("res://{member}")))
            };
            match cycle["classification"].as_str().expect("class") {
                "tolerated" => {
                    assert_eq!(cycle["breaks_loading"], json!(false), "{cycle}");
                    for member in &members {
                        assert_eq!(
                            usable.get(member),
                            Some(&true),
                            "{}: {member} loads: {output}",
                            editor.display()
                        );
                        assert!(
                            !mentioned(member),
                            "{}: no error names {member}: {output}",
                            editor.display()
                        );
                    }
                }
                "inheritance" => {
                    assert_eq!(cycle["breaks_loading"], json!(true), "{cycle}");
                    for member in &members {
                        assert_eq!(
                            usable.get(member),
                            Some(&false),
                            "{}: {member} fails: {output}",
                            editor.display()
                        );
                    }
                    assert!(
                        output.contains("Could not resolve super class inheritance"),
                        "{}: {output}",
                        editor.display()
                    );
                }
                "resource_load" => {
                    assert_eq!(cycle["breaks_loading"], json!(true), "{cycle}");
                    assert!(
                        errors.iter().any(|line| {
                            line.contains("referenced non-existent resource")
                                && members
                                    .iter()
                                    .any(|member| line.contains(&format!("res://{member}")))
                        }),
                        "{}: the loader rejects the ring: {output}",
                        editor.display()
                    );
                }
                other => panic!("unknown classification {other}"),
            }
        }
        eprintln!(
            "{}: {} cycles behave as classified",
            editor.display(),
            cycles["cycles"].as_array().map_or(0, Vec::len)
        );
    }
}

const SIGNAL_PROBE: &str = r#"extends SceneTree

func _initialize() -> void:
	var scene: PackedScene = load("res://main.tscn")
	var node := scene.instantiate()
	print("SIGNAL\tconnections\t", node.get_node("Emitter").get_signal_connection_list("fired").size())
	root.add_child(node)
	node.get_node("Emitter").fired.emit()
	print("SIGNAL\temitted")
	quit()
"#;

/// A scene connection to a method the receiving script lacks is reported by
/// `project.signals.get`; Godot itself instantiates the scene and skips the
/// connection when the signal fires, printing nothing.
#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn missing_handlers_are_silent_in_godot() {
    let (editors, _unpacked) = live::editors();
    for editor in editors {
        let tree = ProjectTree::new("insight-signals");
        tree.write(
            "project.godot",
            "config_version=5\n\n[application]\n\nconfig/name=\"signal-check\"\n",
        );
        tree.write("emitter.gd", "extends Node\nsignal fired\n");
        tree.write(
            "receiver.gd",
            "extends Node\nfunc _on_fired() -> void:\n\tprint(\"SIGNAL\\thandled\")\n",
        );
        tree.write(
            "main.tscn",
            "[gd_scene load_steps=3 format=3]\n\n\
             [ext_resource type=\"Script\" path=\"res://emitter.gd\" id=\"1_e\"]\n\
             [ext_resource type=\"Script\" path=\"res://receiver.gd\" id=\"2_r\"]\n\n\
             [node name=\"Main\" type=\"Node\"]\n\n\
             [node name=\"Emitter\" type=\"Node\" parent=\".\"]\nscript = ExtResource(\"1_e\")\n\n\
             [node name=\"Receiver\" type=\"Node\" parent=\".\"]\nscript = ExtResource(\"2_r\")\n\n\
             [connection signal=\"fired\" from=\"Emitter\" to=\"Receiver\" method=\"_on_fired\"]\n\
             [connection signal=\"fired\" from=\"Emitter\" to=\"Receiver\" method=\"_on_gone\"]\n",
        );
        let home = tree.base.join("home");
        let root = tree.root.to_string_lossy().into_owned();

        let mut session = StdioSession::initialized(&Spawn::core(&tree.root));
        let problems = session.call_tool(2, "project.signals.get", json!({ "view": "problems" }));
        let problems = ok(&problems).clone();
        let exited = session.finish();
        assert_eq!(
            exited.code,
            Some(0),
            "the server exits cleanly: {}",
            exited.stderr
        );
        let missing: Vec<(&str, &str)> = problems["problems"]
            .as_array()
            .expect("problems")
            .iter()
            .filter(|problem| problem["kind"] == json!("missing_handler"))
            .map(|problem| {
                (
                    problem["signal"].as_str().expect("signal"),
                    problem["detail"].as_str().expect("detail"),
                )
            })
            .collect();
        assert_eq!(missing.len(), 1, "{problems}");
        assert_eq!(missing[0].0, "fired", "{problems}");
        assert!(missing[0].1.contains("_on_gone"), "{problems}");

        tree.write("signal_probe.gd", SIGNAL_PROBE);
        let output = run_godot(
            &editor,
            &home,
            &[
                "--headless",
                "--path",
                &root,
                "--script",
                "res://signal_probe.gd",
            ],
        );
        assert!(
            output.contains("SIGNAL\tconnections\t2"),
            "{}: {output}",
            editor.display()
        );
        assert!(
            output.contains("SIGNAL\thandled"),
            "{}: {output}",
            editor.display()
        );
        assert!(
            output.contains("SIGNAL\temitted"),
            "{}: {output}",
            editor.display()
        );
        assert!(
            !output.contains("ERROR"),
            "{}: Godot reports nothing for the missing handler: {output}",
            editor.display()
        );
        eprintln!(
            "{}: the missing handler is silent at run time",
            editor.display()
        );
    }
}

/// The views leave out exactly the scripts Godot's editor scan leaves out:
/// hidden files, files under hidden directories and files under a directory
/// holding a `.gdignore` (one at the project root is not honoured).
#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn ignored_files_match_godot() {
    let (editors, _unpacked) = live::editors();
    for editor in editors {
        let tree = ProjectTree::new("insight-ignored");
        tree.write(
            "project.godot",
            "config_version=5\n\n[application]\n\nconfig/name=\"ignored-check\"\n",
        );
        tree.write(".gdignore", "");
        tree.write("root.gd", "class_name AtRoot\nextends Node\n");
        tree.write("ok/visible.gd", "class_name Visible\nextends Node\n");
        tree.write(
            ".hidden/in_hidden.gd",
            "class_name InHidden\nextends Node\n",
        );
        tree.write("ok/.dot_file.gd", "class_name DotFile\nextends Node\n");
        tree.write("tools/.gdignore", "");
        tree.write(
            "tools/deep/in_ignored.gd",
            "class_name InIgnored\nextends Node\n",
        );
        let home = tree.base.join("home");
        let root = tree.root.to_string_lossy().into_owned();
        run_godot(
            &editor,
            &home,
            &["--headless", "--editor", "--path", &root, "--import"],
        );

        let cache = std::fs::read_to_string(tree.root.join(".godot/global_script_class_cache.cfg"))
            .unwrap_or_else(|error| panic!("{}: the class cache: {error}", editor.display()));
        let registered: BTreeSet<&str> = cache
            .split("\"class\": &\"")
            .skip(1)
            .filter_map(|rest| rest.split('"').next())
            .collect();
        assert_eq!(
            registered,
            BTreeSet::from(["AtRoot", "Visible"]),
            "{}: {cache}",
            editor.display()
        );
        let mut scanned = BTreeSet::new();
        let mut pending = vec![tree.root.clone()];
        while let Some(directory) = pending.pop() {
            for entry in std::fs::read_dir(&directory)
                .expect("read project directory")
                .flatten()
            {
                let path = entry.path();
                if path.is_dir() {
                    pending.push(path);
                } else if let Some(script) = path.to_string_lossy().strip_suffix(".gd.uid") {
                    let relative = Path::new(&format!("{script}.gd"))
                        .strip_prefix(&tree.root)
                        .expect("inside the project")
                        .to_string_lossy()
                        .into_owned();
                    scanned.insert(relative);
                }
            }
        }

        let mut session = StdioSession::initialized(&Spawn::core(&tree.root));
        let stats = session.call_tool(2, "project.metrics.get", json!({}));
        let stats = ok(&stats).clone();
        let found = session.call_tool(
            3,
            "project.search",
            json!({ "query": "class_name", "include": ["**/*.gd"] }),
        );
        let found = ok(&found).clone();
        let exited = session.finish();
        assert_eq!(
            exited.code,
            Some(0),
            "the server exits cleanly: {}",
            exited.stderr
        );

        assert_eq!(
            stats["statistics"]["scripts"]["class_names"],
            json!(registered.len()),
            "{}: {stats}",
            editor.display()
        );
        assert_eq!(
            stats["ignored_files"],
            json!({ "hidden": 4, "gdignore": 1 }),
            "{}: {stats}",
            editor.display()
        );
        let searched: BTreeSet<String> = found["matches"]
            .as_array()
            .expect("matches")
            .iter()
            .map(|hit| hit["path"].as_str().expect("path").to_owned())
            .collect();
        assert_eq!(searched, scanned, "{}: {found}", editor.display());
        eprintln!(
            "{}: {} scripts scanned and registered as Godot does",
            editor.display(),
            scanned.len()
        );
    }
}

const PACK_PROBE: &str = r#"extends SceneTree

func _initialize() -> void:
	var arguments := OS.get_cmdline_user_args()
	print("PACK\tloaded\t", ProjectSettings.load_resource_pack(arguments[0], false))
	for index in range(1, arguments.size()):
		print("PACK\t", arguments[index], "\t", FileAccess.file_exists("res://" + arguments[index]))
	quit()
"#;

/// The files an export preset's `include_filter` adds are roots exactly when
/// Godot packs them: `*` crosses `/`, `?` matches anything but `.`, and the
/// match ignores case.
#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn export_include_filters_match_godot() {
    let candidates = [
        "Data/upper.json",
        "data/levels.json",
        "data/sub/deep.json",
        "notes/a.b.txt",
        "notes/a.txt",
        "notes/ab.txt",
        "notes/x.y",
        "notes/xzy",
    ];
    let (editors, _unpacked) = live::editors();
    for editor in editors {
        let tree = ProjectTree::new("insight-export");
        tree.write(
            "project.godot",
            "config_version=5\n\n[application]\n\nconfig/name=\"export-check\"\n",
        );
        for candidate in candidates {
            tree.write(candidate, "{}\n");
        }
        tree.write(
            "export_presets.cfg",
            "[preset.0]\n\nname=\"Linux\"\nplatform=\"Linux\"\nrunnable=true\n\
             dedicated_server=false\ncustom_features=\"\"\nexport_filter=\"all_resources\"\n\
             include_filter=\"data/*.json, notes/??.txt, notes/?.?.txt, notes/x?y\"\n\
             exclude_filter=\"\"\nexport_path=\"out/game.x86_64\"\nencrypt_pck=false\n\
             encrypt_directory=false\nscript_export_mode=2\n\n[preset.0.options]\n\n\
             binary_format/embed_pck=false\nbinary_format/architecture=\"x86_64\"\n",
        );
        let home = tree.base.join("home");
        let root = tree.root.to_string_lossy().into_owned();
        let pack = tree.base.join("game.pck").to_string_lossy().into_owned();

        let mut session = StdioSession::initialized(&Spawn::core(&tree.root));
        let audit = session.call_tool(2, "project.dependencies.audit", json!({ "limit": 1 }));
        let audit = ok(&audit).clone();
        let exited = session.finish();
        assert_eq!(
            exited.code,
            Some(0),
            "the server exits cleanly: {}",
            exited.stderr
        );
        let rooted: BTreeSet<String> = audit["roots"]
            .as_array()
            .expect("roots")
            .iter()
            .filter(|root| root["reason"] == json!("export_include_filter"))
            .map(|root| root["path"].as_str().expect("path").to_owned())
            .collect();

        run_godot(
            &editor,
            &home,
            &[
                "--headless",
                "--path",
                &root,
                "--export-pack",
                "Linux",
                &pack,
            ],
        );
        let checker = tree.base.join("checker");
        std::fs::create_dir_all(&checker).expect("checker project");
        std::fs::write(
            checker.join("project.godot"),
            "config_version=5\n\n[application]\n\nconfig/name=\"pack-check\"\n",
        )
        .expect("checker settings");
        std::fs::write(checker.join("pack_probe.gd"), PACK_PROBE).expect("pack probe");
        let checker_root = checker.to_string_lossy().into_owned();
        let mut arguments = vec![
            "--headless",
            "--path",
            &checker_root,
            "--script",
            "res://pack_probe.gd",
            "--",
            &pack,
        ];
        arguments.extend(candidates);
        let output = run_godot(&editor, &home, &arguments);
        assert!(
            output.contains("PACK\tloaded\ttrue"),
            "{}: {output}",
            editor.display()
        );
        let packed: BTreeSet<String> = output
            .lines()
            .filter_map(|line| {
                let fields: Vec<&str> = line.trim_end().split('\t').collect();
                match fields.as_slice() {
                    ["PACK", path, "true"] if *path != "loaded" => Some((*path).to_owned()),
                    _ => None,
                }
            })
            .collect();
        assert_eq!(rooted, packed, "{}: {output}", editor.display());
        assert!(
            !packed.contains("notes/x.y") && packed.contains("notes/xzy"),
            "{output}"
        );
        eprintln!(
            "{}: {} filter-selected files agree with the exported pack",
            editor.display(),
            packed.len()
        );
    }
}
