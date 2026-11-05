//! `script.run` (ADR 0012). Without an engine: the operator's switch decides
//! whether the tool runs anything, and a refusal leaves the project untouched.
//! With real Godot builds (`#[ignore]`d; `GODOT_MCP_TEST_EDITORS` or
//! `GODOT_MCP_LOCKED_ARTIFACT` names them): what a run sees, what it changes,
//! what it answers, and what is left after it.

#[path = "support/live.rs"]
mod live;
mod support;

use std::path::Path;
use std::process::Command;

use serde_json::{Value, json};
use support::{ProjectTree, Spawn, StdioSession, error_code};

const SOURCE: &str = "extends RefCounted\n\nfunc run():\n\treturn 1\n";

#[test]
fn script_run_is_listed_but_refused_unless_the_server_allows_scripts() {
    let tree = ProjectTree::new("script-run-off");
    tree.write("project.godot", "config_version=5\n");
    let mut session = StdioSession::initialized(&Spawn {
        project: &tree.root,
        current_dir: None,
        profile: "full",
        godot: None,
    });
    let listed = session.request(2, "tools/list", json!({}));
    assert!(
        listed["result"]["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .any(|tool| tool["name"] == "script.run"),
        "{listed}"
    );
    let refused = session.call_tool(
        3,
        "script.run",
        json!({ "source": SOURCE, "idempotencyId": "off" }),
    );
    assert_eq!(
        error_code(&refused),
        json!("permission_denied"),
        "{refused}"
    );
    assert!(
        refused.to_string().contains("--allow-scripts"),
        "the refusal names the switch: {refused}"
    );
    assert!(
        !tree.root.join("godot_mcp_helpers").exists(),
        "a refused run writes nothing"
    );
    let _ = session.finish();
}

/// A server on `tree` that runs scripts through `binary`.
fn scripting_session(tree: &ProjectTree, binary: &Path) -> StdioSession {
    let mut command = Command::new(env!("CARGO_BIN_EXE_qa-journal-foundation"));
    command
        .arg("--project")
        .arg(&tree.root)
        .arg("--profile")
        .arg("full")
        .arg("--godot-binary")
        .arg(binary)
        .arg("--allow-scripts")
        .env_remove("GODOT_MCP_TEST_LAUNCH_CRASH_STAGE");
    StdioSession::handshake(StdioSession::start_command(command)).0
}

/// A project with an autoload, a script class, plain files and a file a copy
/// leaves out.
fn game(label: &str) -> ProjectTree {
    let tree = ProjectTree::new(label);
    tree.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"Scripted\"\n\n\
         [autoload]\n\nGame=\"*res://game.gd\"\n",
    );
    tree.write("game.gd", "extends Node\n\nvar score := 7\n");
    tree.write(
        "enemy.gd",
        "class_name ProbeEnemy\nextends RefCounted\n\nfunc hp() -> int:\n\treturn 3\n",
    );
    tree.write("data.txt", "one");
    tree.write("old.txt", "old");
    tree.write("secrets.json", "{\"token\": \"kept out\"}");
    tree
}

fn run(session: &mut StdioSession, ids: &mut live::Ids, arguments: Value, context: &str) -> Value {
    live::ok(
        &session.call_tool(ids.next(), "script.run", arguments),
        context,
    )
}

fn import(session: &mut StdioSession, ids: &mut live::Ids, context: &str) {
    let imported = live::ok(
        &session.call_tool(
            ids.next(),
            "project.import.run",
            json!({ "idempotencyId": "import", "deadline_ms": 180_000 }),
        ),
        context,
    );
    // An import nominating no artifact is inconclusive by design; its exit
    // status is what this setup needs.
    assert_eq!(imported["exit_code"], json!(0), "{imported}\n{context}");
}

/// The files a run left in the helper area: none, once it has answered.
fn helper_files(tree: &ProjectTree) -> Vec<String> {
    let directory = tree.root.join("godot_mcp_helpers").join("script_run");
    std::fs::read_dir(directory)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn paths(changes: &Value, kind: &str) -> Vec<String> {
    changes[kind]
        .as_array()
        .unwrap_or_else(|| panic!("{kind} in {changes}"))
        .iter()
        .map(|file| file["path"].as_str().expect("path").to_owned())
        .collect()
}

fn read(tree: &ProjectTree, path: &str) -> Option<String> {
    std::fs::read_to_string(tree.root.join(path)).ok()
}

const PROJECT_RUN: &str = "extends Node

func run():
\tvar game = get_tree().root.get_node(\"Game\")
\tvar made = FileAccess.open(\"res://made.txt\", FileAccess.WRITE)
\tmade.store_string(\"made\")
\tmade.close()
\tvar data = FileAccess.open(\"res://data.txt\", FileAccess.WRITE)
\tdata.store_string(\"two\")
\tdata.close()
\tDirAccess.remove_absolute(ProjectSettings.globalize_path(\"res://old.txt\"))
\tvar count := \"\"
\tif FileAccess.file_exists(\"res://runs.txt\"):
\t\tcount = FileAccess.get_file_as_string(\"res://runs.txt\")
\tvar runs = FileAccess.open(\"res://runs.txt\", FileAccess.WRITE)
\truns.store_string(count + \"x\")
\truns.close()
\tprint(\"hello from run\")
\treturn {\"score\": game.score, \"hp\": ProbeEnemy.new().hp(), \"list\": [1, 2.5, \"x\"], \"where\": Vector2(1, 2)}
";

#[test]
#[ignore = "needs a Godot build: set GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn a_project_run_reaches_the_project_and_what_it_changes_stays() {
    let (binaries, _unpacked) = live::editors();
    for binary in &binaries {
        let context = binary.display().to_string();
        let tree = game("script-run-project");
        let mut session = scripting_session(&tree, binary);
        let mut ids = live::Ids(1);
        import(&mut session, &mut ids, &context);
        let arguments = json!({ "source": PROJECT_RUN, "idempotencyId": "project-run" });
        let answer = run(&mut session, &mut ids, arguments.clone(), &context);
        let report = format!("{answer}\n{context}");
        assert_eq!(answer["classification"], json!("passed"), "{report}");
        assert_eq!(answer["context"], json!("project"), "{report}");
        assert_eq!(
            answer["result"],
            json!({ "type": "dictionary", "value": {
                "hp": { "type": "integer", "value": 3 },
                "list": { "type": "array", "value": [
                    { "type": "integer", "value": 1 },
                    { "type": "float", "value": 2.5 },
                    { "type": "string", "value": "x" },
                ] },
                "score": { "type": "integer", "value": 7 },
                "where": { "type": "vector2", "value": { "x": 1.0, "y": 2.0 } },
            } }),
            "{report}"
        );
        let changes = &answer["changes"];
        assert_eq!(
            paths(changes, "added"),
            ["made.txt", "runs.txt"],
            "{report}"
        );
        assert_eq!(paths(changes, "modified"), ["data.txt"], "{report}");
        assert_eq!(paths(changes, "removed"), ["old.txt"], "{report}");
        assert_eq!(changes["total"], json!(4), "{report}");
        assert_eq!(answer["applied"], json!(true), "{report}");
        assert_eq!(answer["rolled_back"], json!(false), "{report}");
        assert_eq!(answer["cleanup_error"], Value::Null, "{report}");
        assert!(
            answer["stdout"].to_string().contains("hello from run"),
            "{report}"
        );
        assert_eq!(read(&tree, "made.txt").as_deref(), Some("made"), "{report}");
        assert_eq!(read(&tree, "data.txt").as_deref(), Some("two"), "{report}");
        assert_eq!(read(&tree, "old.txt"), None, "{report}");
        assert!(
            helper_files(&tree).is_empty(),
            "{:?}\n{report}",
            helper_files(&tree)
        );

        // The same request again answers from the journal: the code is not
        // run a second time.
        let replayed = live::ok(
            &session.call_tool(ids.next(), "script.run", arguments),
            &context,
        );
        assert_eq!(
            read(&tree, "runs.txt").as_deref(),
            Some("x"),
            "{replayed}\n{report}"
        );
        assert_eq!(replayed["replayed"], json!(true), "{replayed}");
        assert_eq!(replayed["status"], json!("committed"), "{replayed}");
        assert!(
            replayed
                .to_string()
                .contains(answer["transaction_id"].as_str().expect("id")),
            "the replay names the run it answers for: {replayed}\n{report}"
        );
        let _ = session.finish();
    }
}

const COPY_RUN: &str = "extends RefCounted

func run():
\tvar note = FileAccess.open(\"res://copy_only.txt\", FileAccess.WRITE)
\tnote.store_string(\"copy\")
\tnote.close()
\tvar game = Engine.get_main_loop().root.get_node(\"Game\")
\treturn [ProbeEnemy.new().hp(), FileAccess.file_exists(\"res://secrets.json\"), game.score]
";

#[test]
#[ignore = "needs a Godot build: set GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn a_copy_run_sees_a_prepared_copy_and_leaves_the_project_alone() {
    let (binaries, _unpacked) = live::editors();
    for binary in &binaries {
        let context = binary.display().to_string();
        let tree = game("script-run-copy");
        let mut session = scripting_session(&tree, binary);
        let mut ids = live::Ids(1);
        let answer = run(
            &mut session,
            &mut ids,
            json!({ "source": COPY_RUN, "context": "copy", "idempotencyId": "copy-run" }),
            &context,
        );
        let report = format!("{answer}\n{context}");
        assert_eq!(answer["classification"], json!("passed"), "{report}");
        assert_eq!(answer["context"], json!("copy"), "{report}");
        assert_eq!(
            answer["result"],
            json!({ "type": "array", "value": [
                { "type": "integer", "value": 3 },
                { "type": "bool", "value": false },
                { "type": "integer", "value": 7 },
            ] }),
            "{report}"
        );
        assert_eq!(
            paths(&answer["changes"], "added"),
            ["copy_only.txt"],
            "{report}"
        );
        assert_eq!(answer["changes"]["total"], json!(1), "{report}");
        assert_eq!(answer["applied"], json!(false), "{report}");
        assert!(
            answer["copy_excluded"]["paths"]
                .as_array()
                .expect("excluded")
                .contains(&json!({ "path": "secrets.json", "reason": "secret_material" })),
            "{report}"
        );
        assert_eq!(answer["copy_discarded"], Value::Null, "{report}");
        assert_eq!(read(&tree, "copy_only.txt"), None, "{report}");
        assert!(!tree.root.join("godot_mcp_helpers").exists(), "{report}");
        let copies: Vec<_> = std::fs::read_dir(tree.root.join(".godot-mcp").join("script-copies"))
            .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
            .unwrap_or_default();
        assert!(
            copies.is_empty(),
            "the copy is discarded: {copies:?}\n{report}"
        );
        let _ = session.finish();
    }
}

/// The line, within the caller's source, every diagnostic of `answer` names.
fn source_lines(answer: &Value) -> Vec<u64> {
    answer["diagnostics"]
        .as_array()
        .expect("diagnostics")
        .iter()
        .filter_map(|diagnostic| diagnostic["source_line"].as_u64())
        .collect()
}

#[test]
#[ignore = "needs a Godot build: set GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn a_failed_run_says_what_went_wrong_and_where_in_the_source() {
    let (binaries, _unpacked) = live::editors();
    for binary in &binaries {
        let context = binary.display().to_string();
        let tree = ProjectTree::new("script-run-failures");
        tree.write(
            "project.godot",
            "config_version=5\n\n[application]\n\nconfig/name=\"Failures\"\n",
        );
        let mut session = scripting_session(&tree, binary);
        let mut ids = live::Ids(1);

        let compile = run(
            &mut session,
            &mut ids,
            json!({
                "source": "extends RefCounted\n\nfunc run():\n\treturn undefined_name\n",
                "idempotencyId": "compile",
            }),
            &context,
        );
        let report = format!("{compile}\n{context}");
        assert_eq!(
            compile["classification"],
            json!("compile_error"),
            "{report}"
        );
        assert_eq!(compile["result"], Value::Null, "{report}");
        assert_eq!(compile["changes"]["total"], json!(0), "{report}");

        let runtime = run(
            &mut session,
            &mut ids,
            json!({
                "source": "extends RefCounted\n\nfunc run():\n\tvar items := []\n\treturn items[3]\n",
                "idempotencyId": "runtime",
            }),
            &context,
        );
        let report = format!("{runtime}\n{context}");
        assert_eq!(runtime["classification"], json!("failed"), "{report}");
        assert!(source_lines(&runtime).contains(&5), "{report}");

        let object = run(
            &mut session,
            &mut ids,
            json!({
                "source": "extends RefCounted\n\nfunc run():\n\treturn Gradient.new()\n",
                "idempotencyId": "object",
            }),
            &context,
        );
        let report = format!("{object}\n{context}");
        assert_eq!(object["classification"], json!("passed"), "{report}");
        assert_eq!(object["result"], Value::Null, "{report}");
        assert!(
            object["result_literal"]
                .as_str()
                .is_some_and(|literal| literal.starts_with("Object(Gradient")),
            "{report}"
        );
        assert!(object["result_unavailable"].is_string(), "{report}");
        assert!(
            helper_files(&tree).is_empty(),
            "{:?}\n{report}",
            helper_files(&tree)
        );
        let _ = session.finish();
    }
}

#[test]
#[ignore = "needs a Godot build: set GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn the_deadline_ends_a_run_that_never_returns() {
    let (binaries, _unpacked) = live::editors();
    for binary in &binaries {
        let context = binary.display().to_string();
        let tree = ProjectTree::new("script-run-deadline");
        tree.write(
            "project.godot",
            "config_version=5\n\n[application]\n\nconfig/name=\"Deadline\"\n",
        );
        let mut session = scripting_session(&tree, binary);
        let mut ids = live::Ids(1);
        let started = std::time::Instant::now();
        let refused = session.call_tool(
            ids.next(),
            "script.run",
            json!({
                "source": "extends RefCounted\n\nfunc run():\n\tvar note = FileAccess.open(\"res://before_loop.txt\", FileAccess.WRITE)\n\tnote.store_string(\"written\")\n\tnote.close()\n\twhile true:\n\t\tpass\n",
                "deadline_ms": 3_000,
                "idempotencyId": "forever",
            }),
        );
        let report = format!("{refused}\n{context}");
        assert_eq!(error_code(&refused), json!("timeout"), "{report}");
        let details = &refused["result"]["structuredContent"]["details"];
        assert_eq!(
            details["changed_paths"],
            json!({ "type": "array", "value": [{ "type": "string", "value": "before_loop.txt" }] }),
            "{report}"
        );
        assert_eq!(
            details["rolled_back"],
            json!({ "type": "bool", "value": false }),
            "{report}"
        );
        assert_eq!(
            read(&tree, "before_loop.txt").as_deref(),
            Some("written"),
            "{report}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(60),
            "{report}"
        );
        assert!(
            helper_files(&tree).is_empty(),
            "{:?}\n{report}",
            helper_files(&tree)
        );

        // The next call reads the project as the ended run left it.
        let listed = live::ok(
            &session.call_tool(ids.next(), "project.files.list", json!({})),
            &context,
        );
        assert!(listed.to_string().contains("before_loop.txt"), "{listed}");
        let _ = session.finish();
    }
}

/// One route's calls during a comparison task: how many, how long they took,
/// and how many bytes went each way.
struct Meter<'a> {
    session: &'a mut StdioSession,
    ids: &'a mut live::Ids,
    calls: u32,
    elapsed: std::time::Duration,
    sent: usize,
    received: usize,
}

impl<'a> Meter<'a> {
    fn new(session: &'a mut StdioSession, ids: &'a mut live::Ids) -> Self {
        Self {
            session,
            ids,
            calls: 0,
            elapsed: std::time::Duration::ZERO,
            sent: 0,
            received: 0,
        }
    }

    fn call(&mut self, name: &str, arguments: Value) -> Value {
        let started = std::time::Instant::now();
        self.sent += arguments.to_string().len();
        let response = self.session.call_tool(self.ids.next(), name, arguments);
        self.elapsed += started.elapsed();
        self.calls += 1;
        self.received += response.to_string().len();
        response
    }

    fn row(&self, task: &str, route: &str, completed: bool, notes: &str) -> String {
        format!(
            "| {task} | {route} | {} | {} | {} | {} | {} | {notes} |",
            self.calls,
            self.elapsed.as_millis(),
            self.sent,
            self.received,
            if completed { "yes" } else { "no" },
        )
    }
}

const ROOM: &str = "[gd_scene format=3]\n\n[node name=\"Room\" type=\"Node2D\"]\n\n\
                    [node name=\"Player\" type=\"Node2D\" parent=\".\"]\n";

/// A project with an autoload that computes a value and five scenes that each
/// hold a `Player`.
fn comparison_project(label: &str) -> ProjectTree {
    let tree = ProjectTree::new(label);
    tree.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"Compared\"\n\n\
         [autoload]\n\nGame=\"*res://game.gd\"\n",
    );
    tree.write(
        "game.gd",
        "extends Node\n\nvar score := 7\n\nfunc level_count() -> int:\n\treturn 3 * 4\n",
    );
    for index in 0..5 {
        tree.write(&format!("rooms/room_{index}.tscn"), ROOM);
    }
    tree
}

/// The node paths of one scene, as the server reads it.
fn node_paths(session: &mut StdioSession, ids: &mut live::Ids, scene: &str) -> Vec<String> {
    let tree = live::ok(
        &session.call_tool(ids.next(), "scene.tree.get", json!({ "scene": scene })),
        scene,
    );
    tree["data"]["nodes"]
        .as_array()
        .unwrap_or_else(|| panic!("nodes in {tree}"))
        .iter()
        .filter_map(|node| node["path"].as_str().map(str::to_owned))
        .collect()
}

fn sha_of(listing: &Value, path: &str) -> String {
    listing["files"]
        .as_array()
        .expect("files")
        .iter()
        .find(|file| file["path"] == json!(path))
        .and_then(|file| file["sha256"].as_str())
        .unwrap_or_else(|| panic!("{path} in {listing}"))
        .to_owned()
}

fn has_jump(session: &mut StdioSession, ids: &mut live::Ids) -> bool {
    let settings = live::ok(
        &session.call_tool(ids.next(), "project.settings.get", json!({})),
        "settings",
    );
    settings["input_actions"]
        .as_array()
        .is_some_and(|actions| actions.iter().any(|action| action["name"] == json!("jump")))
}

const GRID_SCRIPT: &str = "extends RefCounted

func run():
\tvar root := Node2D.new()
\troot.name = \"Grid\"
\tfor i in 10:
\t\tvar tile := Marker2D.new()
\t\ttile.name = \"Tile_%d\" % i
\t\ttile.position = Vector2(i * 32, 0)
\t\troot.add_child(tile)
\t\ttile.owner = root
\tvar packed := PackedScene.new()
\tpacked.pack(root)
\tDirAccess.make_dir_recursive_absolute(\"res://levels\")
\tvar error := ResourceSaver.save(packed, \"res://levels/grid.tscn\")
\troot.free()
\treturn error
";

const RENAME_SCRIPT: &str = "extends RefCounted

func run():
\tvar renamed := []
\tfor i in 5:
\t\tvar path := \"res://rooms/room_%d.tscn\" % i
\t\tvar scene: Node = load(path).instantiate()
\t\tscene.get_node(\"Player\").name = \"Hero\"
\t\tvar packed := PackedScene.new()
\t\tpacked.pack(scene)
\t\tResourceSaver.save(packed, path)
\t\tscene.free()
\t\trenamed.append(path)
\treturn renamed
";

const INPUT_SCRIPT: &str = "extends RefCounted

func run():
\tvar event := InputEventKey.new()
\tevent.physical_keycode = KEY_SPACE
\tProjectSettings.set_setting(\"input/jump\", {\"deadzone\": 0.5, \"events\": [event]})
\treturn ProjectSettings.save()
";

/// The rename again, broken on the fourth scene: the first three are saved
/// before the error.
const BROKEN_RENAME_SCRIPT: &str = "extends RefCounted

func run():
\tfor i in 5:
\t\tvar path := \"res://rooms/room_%d.tscn\" % i
\t\tvar scene: Node = load(path).instantiate()
\t\tvar target := \"Player\" if i < 3 else \"Missing\"
\t\tscene.get_node(target).name = \"Hero\"
\t\tvar packed := PackedScene.new()
\t\tpacked.pack(scene)
\t\tResourceSaver.save(packed, path)
\t\tscene.free()
";

/// The same tasks done once with the typed tools and once with `script.run`,
/// each on a fresh copy of one project: both routes must finish what they can,
/// and the measured table goes to `GODOT_MCP_COMPARISON_REPORT` when it is set.
#[test]
#[ignore = "needs a Godot build: set GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn typed_tools_and_scripts_do_the_same_tasks() {
    let (binaries, _unpacked) = live::editors();
    let mut report = vec![
        "| Task | Route | Calls | ms | Bytes sent | Bytes received | Done | Feedback and recovery |"
            .to_owned(),
        "| --- | --- | --- | --- | --- | --- | --- | --- |".to_owned(),
    ];
    for binary in &binaries {
        let build = binary
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        report.push(format!("| **{build}** | | | | | | | |"));

        // A. Build a scene of ten placed nodes.
        let tree = comparison_project("compare-grid-typed");
        let mut session = scripting_session(&tree, binary);
        let mut ids = live::Ids(1);
        let mut operations = Vec::new();
        for index in 0..10 {
            let name = format!("Tile_{index}");
            operations
                .push(json!({ "op": "add_node", "parent": ".", "name": name, "type": "Marker2D" }));
            operations.push(json!({ "op": "set_property", "node": name, "property": "position",
                "value": { "type": "vector2", "value": { "x": f64::from(index * 32), "y": 0.0 } } }));
        }
        let mut meter = Meter::new(&mut session, &mut ids);
        let created = meter.call(
            "scene.create",
            json!({ "path": "levels/grid.tscn", "rootType": "Node2D", "rootName": "Grid",
                    "operations": operations, "idempotencyId": "grid" }),
        );
        let row_typed = meter.row(
            "A. scene of 10 placed nodes",
            "typed: scene.create",
            false,
            "journaled; before/after digests and the node tree in the answer",
        );
        live::ok(&created, &build);
        let typed_nodes = node_paths(&mut session, &mut ids, "levels/grid.tscn");
        report.push(row_typed.replace(
            "| no |",
            if typed_nodes.len() == 11 {
                "| yes |"
            } else {
                "| no |"
            },
        ));
        let _ = session.finish();

        let tree = comparison_project("compare-grid-script");
        let mut session = scripting_session(&tree, binary);
        let mut ids = live::Ids(1);
        let mut meter = Meter::new(&mut session, &mut ids);
        let ran = meter.call(
            "script.run",
            json!({ "source": GRID_SCRIPT, "idempotencyId": "grid" }),
        );
        let row_script = meter.row(
            "A. scene of 10 placed nodes",
            "script.run",
            false,
            "changed files listed by content; nothing undone",
        );
        let ran = live::ok(&ran, &build);
        assert_eq!(ran["classification"], json!("passed"), "{ran}");
        let script_nodes = node_paths(&mut session, &mut ids, "levels/grid.tscn");
        assert_eq!(script_nodes, typed_nodes, "both routes build the same tree");
        report.push(row_script.replace("| no |", "| yes |"));
        let _ = session.finish();

        // B. Read a value only the running project computes.
        let tree = comparison_project("compare-value");
        let mut session = scripting_session(&tree, binary);
        let mut ids = live::Ids(1);
        let mut meter = Meter::new(&mut session, &mut ids);
        let symbols = meter.call("gdscript.symbols.get", json!({ "script": "game.gd" }));
        assert!(symbols.to_string().contains("level_count"), "{symbols}");
        report.push(meter.row(
            "B. value of Game.level_count()",
            "typed: gdscript.symbols.get",
            false,
            "finds the function, not what it returns: no typed tool runs it",
        ));
        let mut meter = Meter::new(&mut session, &mut ids);
        let value = meter.call(
            "script.run",
            json!({ "source": "extends Node\n\nfunc run():\n\treturn get_tree().root.get_node(\"Game\").level_count()\n",
                    "idempotencyId": "value" }),
        );
        let value = live::ok(&value, &build);
        assert_eq!(
            value["result"],
            json!({ "type": "integer", "value": 12 }),
            "{value}"
        );
        report.push(meter.row(
            "B. value of Game.level_count()",
            "script.run",
            true,
            "the value as a tagged integer",
        ));
        let _ = session.finish();

        // C. Add an input action bound to Space.
        let tree = comparison_project("compare-input-typed");
        let mut session = scripting_session(&tree, binary);
        let mut ids = live::Ids(1);
        let mut meter = Meter::new(&mut session, &mut ids);
        let settings = live::ok(&meter.call("project.settings.get", json!({})), &build);
        let patched = meter.call(
            "project.settings.patch",
            json!({ "expectedSha256": settings["sha256"], "idempotencyId": "jump",
                    "operations": [{ "op": "set_input_action", "name": "jump",
                                     "events": [{ "type": "key", "key": "space" }] }] }),
        );
        live::ok(&patched, &build);
        let row = meter.row(
            "C. input action `jump` = Space",
            "typed: settings.get + settings.patch",
            false,
            "journaled; other keys and comments kept; diff in the answer",
        );
        let done = has_jump(&mut session, &mut ids);
        report.push(if done {
            row.replace("| no |", "| yes |")
        } else {
            row
        });
        let _ = session.finish();

        let tree = comparison_project("compare-input-script");
        let mut session = scripting_session(&tree, binary);
        let mut ids = live::Ids(1);
        let mut meter = Meter::new(&mut session, &mut ids);
        let ran = live::ok(
            &meter.call(
                "script.run",
                json!({ "source": INPUT_SCRIPT, "idempotencyId": "jump" }),
            ),
            &build,
        );
        let row = meter.row(
            "C. input action `jump` = Space",
            "script.run",
            false,
            "Godot rewrites project.godot whole; the answer names the file, not the keys",
        );
        assert_eq!(
            paths(&ran["changes"], "modified"),
            ["project.godot"],
            "{ran}"
        );
        let done = has_jump(&mut session, &mut ids);
        report.push(if done {
            row.replace("| no |", "| yes |")
        } else {
            row
        });
        let _ = session.finish();

        // D. Rename a node in five scenes.
        let tree = comparison_project("compare-rename-typed");
        let mut session = scripting_session(&tree, binary);
        let mut ids = live::Ids(1);
        let mut meter = Meter::new(&mut session, &mut ids);
        let listing = live::ok(
            &meter.call("project.files.list", json!({ "prefix": "rooms" })),
            &build,
        );
        for index in 0..5 {
            let path = format!("rooms/room_{index}.tscn");
            let applied = meter.call(
                "scene.batch.apply",
                json!({ "path": path, "expectedSha256": sha_of(&listing, &path),
                        "operations": [{ "op": "rename_node", "node": "Player", "name": "Hero" }],
                        "idempotencyId": format!("rename-{index}") }),
            );
            live::ok(&applied, &build);
        }
        let row = meter.row(
            "D. rename `Player` in 5 scenes",
            "typed: files.list + 5 x scene.batch.apply",
            false,
            "one journaled transaction per scene, each refused if the file changed meanwhile",
        );
        let renamed = (0..5).all(|index| {
            node_paths(&mut session, &mut ids, &format!("rooms/room_{index}.tscn")) == [".", "Hero"]
        });
        report.push(if renamed {
            row.replace("| no |", "| yes |")
        } else {
            row
        });
        let _ = session.finish();

        // The same rename as one multi-file batch: one transaction for all five.
        let tree = comparison_project("compare-rename-batch");
        let mut session = scripting_session(&tree, binary);
        let mut ids = live::Ids(1);
        let mut meter = Meter::new(&mut session, &mut ids);
        let listing = live::ok(
            &meter.call("project.files.list", json!({ "prefix": "rooms" })),
            &build,
        );
        let files: Vec<Value> = (0..5)
            .map(|index| {
                let path = format!("rooms/room_{index}.tscn");
                json!({ "kind": "scene", "path": path, "expectedSha256": sha_of(&listing, &path),
                        "operations": [{ "op": "rename_node", "node": "Player", "name": "Hero" }] })
            })
            .collect();
        live::ok(
            &meter.call(
                "project.batch.apply",
                json!({ "files": files, "idempotencyId": "rename" }),
            ),
            &build,
        );
        let row = meter.row(
            "D. rename `Player` in 5 scenes",
            "typed: files.list + project.batch.apply",
            false,
            "one journaled transaction for the 5 scenes, refused whole if any of them changed \
             meanwhile",
        );
        let renamed = (0..5).all(|index| {
            node_paths(&mut session, &mut ids, &format!("rooms/room_{index}.tscn")) == [".", "Hero"]
        });
        report.push(if renamed {
            row.replace("| no |", "| yes |")
        } else {
            row
        });
        let _ = session.finish();

        let tree = comparison_project("compare-rename-script");
        let mut session = scripting_session(&tree, binary);
        let mut ids = live::Ids(1);
        let mut meter = Meter::new(&mut session, &mut ids);
        let ran = live::ok(
            &meter.call(
                "script.run",
                json!({ "source": RENAME_SCRIPT, "idempotencyId": "rename" }),
            ),
            &build,
        );
        assert_eq!(ran["changes"]["total"], json!(5), "{ran}");
        let row = meter.row(
            "D. rename `Player` in 5 scenes",
            "script.run",
            false,
            "Godot re-saves each scene; no precondition on the files it reads",
        );
        let renamed = (0..5).all(|index| {
            node_paths(&mut session, &mut ids, &format!("rooms/room_{index}.tscn")) == [".", "Hero"]
        });
        report.push(if renamed {
            row.replace("| no |", "| yes |")
        } else {
            row
        });
        let _ = session.finish();

        // E. A mistake: a class that does not exist, then a rename that breaks
        // part way.
        let tree = comparison_project("compare-mistake-typed");
        let mut session = scripting_session(&tree, binary);
        let mut ids = live::Ids(1);
        let mut meter = Meter::new(&mut session, &mut ids);
        let refused = meter.call(
            "scene.batch.apply",
            json!({ "path": "rooms/room_0.tscn", "expectedSha256": support::sha256_hex(ROOM.as_bytes()),
                    "operations": [{ "op": "add_node", "parent": ".", "name": "Spawn", "type": "Markr2D" }],
                    "idempotencyId": "typo" }),
        );
        assert_eq!(error_code(&refused), json!("invalid_input"), "{refused}");
        assert!(refused.to_string().contains("Markr2D"), "{refused}");
        assert_eq!(read(&tree, "rooms/room_0.tscn").as_deref(), Some(ROOM));
        report.push(meter.row(
            "E. mistake: unknown class",
            "typed: scene.batch.apply",
            true,
            "refused before any write, naming `Markr2D`; nothing to recover",
        ));
        let _ = session.finish();

        // The broken rename as one multi-file batch: refused whole.
        let tree = comparison_project("compare-mistake-batch");
        let mut session = scripting_session(&tree, binary);
        let mut ids = live::Ids(1);
        let mut meter = Meter::new(&mut session, &mut ids);
        let listing = live::ok(
            &meter.call("project.files.list", json!({ "prefix": "rooms" })),
            &build,
        );
        let files: Vec<Value> = (0..5)
            .map(|index| {
                let path = format!("rooms/room_{index}.tscn");
                let node = if index < 3 { "Player" } else { "Missing" };
                json!({ "kind": "scene", "path": path, "expectedSha256": sha_of(&listing, &path),
                        "operations": [{ "op": "rename_node", "node": node, "name": "Hero" }] })
            })
            .collect();
        let refused = meter.call(
            "project.batch.apply",
            json!({ "files": files, "idempotencyId": "broken" }),
        );
        assert_eq!(error_code(&refused), json!("invalid_input"), "{refused}");
        assert!(
            refused.to_string().contains("rooms/room_3.tscn"),
            "{refused}"
        );
        let untouched = (0..5)
            .all(|index| read(&tree, &format!("rooms/room_{index}.tscn")).as_deref() == Some(ROOM));
        assert!(untouched, "the refused batch changed no scene");
        report.push(meter.row(
            "E. mistake: rename breaks at scene 4",
            "typed: files.list + project.batch.apply",
            true,
            "refused whole, naming `rooms/room_3.tscn`; no scene changed, nothing to recover",
        ));
        let _ = session.finish();

        let tree = comparison_project("compare-mistake-script");
        let mut session = scripting_session(&tree, binary);
        let mut ids = live::Ids(1);
        let mut meter = Meter::new(&mut session, &mut ids);
        let typo = live::ok(
            &meter.call(
                "script.run",
                json!({ "source": "extends RefCounted\n\nfunc run():\n\treturn Markr2D.new()\n",
                        "idempotencyId": "typo" }),
            ),
            &build,
        );
        assert_eq!(typo["classification"], json!("compile_error"), "{typo}");
        report.push(meter.row(
            "E. mistake: unknown class",
            "script.run",
            true,
            "compile_error before run(); nothing changed",
        ));
        let mut meter = Meter::new(&mut session, &mut ids);
        let broken = live::ok(
            &meter.call(
                "script.run",
                json!({ "source": BROKEN_RENAME_SCRIPT, "idempotencyId": "broken" }),
            ),
            &build,
        );
        assert_eq!(broken["classification"], json!("failed"), "{broken}");
        assert_eq!(broken["changes"]["total"], json!(3), "{broken}");
        assert_eq!(broken["rolled_back"], json!(false), "{broken}");
        report.push(meter.row(
            "E. mistake: rename breaks at scene 4",
            "script.run",
            false,
            &format!(
                "failed at source line {:?}; 3 scenes stay changed, listed; nothing undone",
                source_lines(&broken)
            ),
        ));
        let _ = session.finish();
    }
    let table = report.join("\n");
    eprintln!("{table}");
    if let Some(path) = std::env::var_os("GODOT_MCP_COMPARISON_REPORT") {
        std::fs::write(path, format!("{table}\n")).expect("write the comparison report");
    }
}
