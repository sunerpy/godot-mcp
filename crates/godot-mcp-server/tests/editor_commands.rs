//! `editor.commands.list` and `editor.commands.call` (ADR 0016) at the MCP
//! surface, with a scripted editor plugin: the list reads the project's
//! declaration without running anything, a call is checked against the
//! declaration before the editor is asked, an answer says what the command
//! returned, logged and changed, a call the editor does not answer in time is
//! uncertain and blocks writes, and the operator's switch decides whether the
//! method is ever offered. What a real editor runs is checked by
//! `live_editor_commands.rs`.

#![cfg(target_os = "linux")]

mod support;

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use godot_mcp_bridge::protocol::mutation::MutationStatus;
use godot_mcp_domain::ErrorKind;
use godot_mcp_server::test_support::fake_editor_plugin::{
    FakeAnswer, FakeEditorPlugin, FakePluginScript,
};
use serde_json::{Value, json};
use support::{ProjectTree, StdioSession, error_code, sha256_hex};

const METHOD: &str = "editor.commands.call";
const PROVIDER: &str = "@tool\nextends RefCounted\n\nfunc mcp_call(command: String, args: Dictionary):\n\treturn {\"ok\": true, \"value\": args}\n";

fn declaration() -> Value {
    json!({
        "version": 1,
        "providers": [
            {
                "id": "levels",
                "script": "res://tools/level_commands.gd",
                "commands": [
                    {
                        "name": "rebuild",
                        "title": "Rebuild the level",
                        "description": "Regenerates the level from a seed.",
                        "input_schema": {
                            "type": "object",
                            "additionalProperties": false,
                            "required": ["seed"],
                            "properties": { "seed": { "type": "integer", "minimum": 0 } }
                        },
                        "mutates": true
                    },
                    {
                        "name": "count",
                        "title": "Count nodes",
                        "description": "Counts the nodes of the edited scene.",
                        "input_schema": { "type": "object", "additionalProperties": false, "properties": {} },
                        "mutates": false
                    },
                    {
                        "name": "mail",
                        "title": "Mail a report",
                        "description": "Declares an argument with a format keyword.",
                        "input_schema": {
                            "type": "object",
                            "properties": { "to": { "type": "string", "format": "email" } }
                        },
                        "mutates": false
                    }
                ]
            },
            {
                "id": "ghost",
                "script": "res://tools/missing.gd",
                "commands": [{
                    "name": "boo",
                    "title": "Boo",
                    "description": "Its script does not exist.",
                    "input_schema": { "type": "object" },
                    "mutates": false
                }]
            },
            {
                "id": "plain",
                "script": "res://tools/plain.gd",
                "commands": [{
                    "name": "hello",
                    "title": "Hello",
                    "description": "Its script is not a tool script.",
                    "input_schema": { "type": "object" },
                    "mutates": false
                }]
            }
        ]
    })
}

/// A project declaring [`declaration`], with the scripts it names except the
/// missing one.
fn project(label: &str) -> ProjectTree {
    let tree = ProjectTree::new(label);
    tree.write(
        "godot_mcp_commands.json",
        &serde_json::to_string_pretty(&declaration()).expect("serializes"),
    );
    tree.write("tools/level_commands.gd", PROVIDER);
    tree.write(
        "tools/plain.gd",
        "extends RefCounted\n\nfunc mcp_call(command, args):\n\treturn {\"ok\": true}\n",
    );
    tree
}

fn rendezvous(tree: &ProjectTree) -> PathBuf {
    tree.root
        .join(".godot-mcp")
        .join("editor-bridge.rendezvous")
}

/// An editor-profile server on `tree`, loading `config` when given.
fn server(tree: &ProjectTree, config: Option<&str>) -> StdioSession {
    let mut command = Command::new(env!("CARGO_BIN_EXE_qa-journal-foundation"));
    command
        .arg("--project")
        .arg(&tree.root)
        .arg("--profile")
        .arg("editor")
        .env_remove("GODOT_MCP_TEST_LAUNCH_CRASH_STAGE")
        .env_remove("GODOT_MCP_PROJECT_COMMANDS");
    if let Some(config) = config {
        let path = tree.base.join("server.toml");
        std::fs::write(&path, config).expect("write the configuration");
        command.arg("--config").arg(path);
    }
    StdioSession::handshake(StdioSession::start_command(command)).0
}

fn ok(response: &Value) -> Value {
    assert!(
        response.get("error").is_none() && response["result"]["isError"] != json!(true),
        "the call must succeed: {response}"
    );
    response["result"]["structuredContent"].clone()
}

fn connected(session: &mut StdioSession, next_id: &mut u64) -> Value {
    let started = Instant::now();
    loop {
        *next_id += 1;
        let status = ok(&session.call_tool(*next_id, "editor.status.get", json!({})));
        if status["connected"] == json!(true) {
            return status;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the plugin never connected: {status}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn sent(plugin: &FakeEditorPlugin) -> Vec<Value> {
    plugin
        .received()
        .into_iter()
        .filter(|(method, _)| method == METHOD)
        .map(|(_, payload)| payload)
        .collect()
}

fn offers_commands(status: &Value) -> bool {
    status["methods"]
        .as_array()
        .expect("methods")
        .iter()
        .any(|method| method == METHOD)
}

fn plugin_answering(tree: &ProjectTree, answer: FakeAnswer) -> FakeEditorPlugin {
    FakeEditorPlugin::spawn(
        &rendezvous(tree),
        FakePluginScript::implementing(&["editor.state", METHOD]).answering(METHOD, answer),
    )
}

fn command<'a>(listed: &'a Value, provider: &str, name: &str) -> &'a Value {
    listed["providers"]
        .as_array()
        .expect("providers")
        .iter()
        .find(|found| found["id"] == provider)
        .and_then(|found| {
            found["commands"]
                .as_array()
                .expect("commands")
                .iter()
                .find(|found| found["name"] == name)
        })
        .unwrap_or_else(|| panic!("{provider}/{name} is listed: {listed}"))
}

/// The answer of a mcp_call that returned `{ok: true, value: {rooms: 3}}`.
fn done(value_literal: &str) -> Value {
    json!({
        "stage": "done",
        "ran": true,
        "ok": true,
        "literal": value_literal,
        "source_file": "res://tools/level_commands.gd",
        "log_capture": true,
        "log": [
            { "kind": "message", "text": "rebuilt", "stderr": false },
            {
                "kind": "warning", "text": "slow seed", "file": "res://tools/level_commands.gd",
                "line": 4, "function": "mcp_call", "backtrace": []
            }
        ],
        "log_skipped": 0,
        "log_truncated": false,
        "action": "godot-mcp: editor.commands.call"
    })
}

#[test]
fn the_list_shows_every_command_and_why_one_cannot_be_called() {
    let tree = project("commands-list");
    let mut session = server(&tree, None);
    let listed = ok(&session.call_tool(2, "editor.commands.list", json!({})));
    let bytes = std::fs::read(tree.root.join("godot_mcp_commands.json")).expect("declaration");
    assert_eq!(listed["file"], "godot_mcp_commands.json", "{listed}");
    assert_eq!(listed["status"], "valid", "{listed}");
    assert_eq!(listed["sha256"], sha256_hex(&bytes));
    assert_eq!(listed["bytes"], bytes.len());
    assert_eq!(listed["enabled"], true);
    assert_eq!(listed["disabled_reason"], Value::Null);
    assert_eq!(listed["problems"], json!([]));
    assert_eq!(listed["commands"], 5);
    assert_eq!(
        listed["editor"],
        json!({ "connected": false, "serves_commands": false })
    );
    let rebuild = command(&listed, "levels", "rebuild");
    assert_eq!(rebuild["available"], true, "{rebuild}");
    assert_eq!(rebuild["unavailable_reason"], Value::Null);
    assert_eq!(rebuild["mutates"], true);
    assert_eq!(rebuild["title"], "Rebuild the level");
    assert_eq!(
        rebuild["input_schema"]["required"],
        json!(["seed"]),
        "{rebuild}"
    );
    for (provider, name, reason) in [
        ("levels", "mail", "format"),
        ("ghost", "boo", "does not exist"),
        ("plain", "hello", "@tool"),
    ] {
        let found = command(&listed, provider, name);
        assert_eq!(found["available"], false, "{found}");
        assert!(
            found["unavailable_reason"]
                .as_str()
                .is_some_and(|text| text.contains(reason)),
            "{provider}/{name}: {found}"
        );
    }
    let levels = &listed["providers"][0];
    assert_eq!(levels["script"], "res://tools/level_commands.gd");
    assert_eq!(levels["script_sha256"], sha256_hex(PROVIDER.as_bytes()));
    assert_eq!(listed["providers"][1]["script_sha256"], Value::Null);
    let _ = session.finish();
}

#[test]
fn the_list_reports_an_absent_or_an_invalid_declaration() {
    let tree = ProjectTree::new("commands-absent");
    let mut session = server(&tree, None);
    let absent = ok(&session.call_tool(2, "editor.commands.list", json!({})));
    assert_eq!(absent["status"], "absent", "{absent}");
    assert_eq!(absent["providers"], json!([]));
    assert_eq!(absent["problems"], json!([]));
    assert_eq!(absent["sha256"], Value::Null);
    tree.write(
        "godot_mcp_commands.json",
        "{ \"version\": 2, \"providers\": [] }",
    );
    let invalid = ok(&session.call_tool(3, "editor.commands.list", json!({})));
    assert_eq!(invalid["status"], "invalid", "{invalid}");
    assert_eq!(invalid["problems"][0]["pointer"], "/version", "{invalid}");
    let _ = session.finish();
}

#[test]
fn the_tools_are_listed_with_the_annotations_of_running_project_code() {
    let tree = ProjectTree::new("commands-tools");
    let mut session = server(&tree, None);
    let listed = session.request(2, "tools/list", json!({}));
    let tools = listed["result"]["tools"].as_array().expect("tools");
    let tool = |name: &str| {
        tools
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap_or_else(|| panic!("{name} is listed"))
            .clone()
    };
    let list = tool("editor.commands.list");
    assert_eq!(list["annotations"]["readOnlyHint"], true, "{list}");
    let call = tool("editor.commands.call");
    assert_eq!(
        call["annotations"],
        json!({
            "title": "Call a project command",
            "readOnlyHint": false,
            "destructiveHint": true,
            "idempotentHint": false,
            "openWorldHint": true
        }),
        "{call}"
    );
    let schema = &call["inputSchema"];
    assert_eq!(schema["additionalProperties"], false, "{schema}");
    for property in [
        "provider",
        "command",
        "arguments",
        "deadline_ms",
        "idempotencyId",
    ] {
        assert!(
            schema["properties"].get(property).is_some(),
            "{property}: {schema}"
        );
    }
    let mut required: Vec<&str> = schema["required"]
        .as_array()
        .expect("required")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    required.sort_unstable();
    assert_eq!(required, ["command", "idempotencyId", "provider"]);
    let _ = session.finish();
}

#[test]
fn a_call_the_declaration_does_not_allow_never_reaches_the_editor() {
    let tree = project("commands-refused");
    let mut session = server(&tree, None);
    let plugin = plugin_answering(&tree, FakeAnswer::Payload(done("null")));
    let mut id = 1;
    let status = connected(&mut session, &mut id);
    assert!(offers_commands(&status), "{status}");
    for (arguments, code, part) in [
        (
            json!({ "provider": "levels", "command": "rebuild", "arguments": { "seed": 1 } }),
            "invalid_input",
            "idempotencyId",
        ),
        (
            json!({ "provider": "nobody", "command": "rebuild", "idempotencyId": "a" }),
            "invalid_input",
            "nobody",
        ),
        (
            json!({ "provider": "levels", "command": "fly", "idempotencyId": "b" }),
            "invalid_input",
            "fly",
        ),
        (
            json!({ "provider": "levels", "command": "rebuild", "arguments": { "seed": "x" }, "idempotencyId": "c" }),
            "invalid_input",
            "/seed",
        ),
        (
            json!({ "provider": "levels", "command": "rebuild", "idempotencyId": "d" }),
            "invalid_input",
            "seed",
        ),
        (
            json!({ "provider": "levels", "command": "mail", "idempotencyId": "e" }),
            "unsupported",
            "format",
        ),
        (
            json!({ "provider": "ghost", "command": "boo", "idempotencyId": "f" }),
            "invalid_input",
            "does not exist",
        ),
        (
            json!({ "provider": "plain", "command": "hello", "idempotencyId": "g" }),
            "invalid_input",
            "@tool",
        ),
        (
            json!({ "provider": "levels", "command": "rebuild", "arguments": { "seed": 1 }, "deadline_ms": 999, "idempotencyId": "h" }),
            "invalid_input",
            "deadline_ms",
        ),
        (
            json!({ "provider": "levels", "command": "rebuild", "arguments": [1], "idempotencyId": "i" }),
            "invalid_input",
            "arguments",
        ),
    ] {
        id += 1;
        let refused = session.call_tool(id, "editor.commands.call", arguments.clone());
        assert_eq!(error_code(&refused), json!(code), "{arguments}: {refused}");
        assert!(refused.to_string().contains(part), "{arguments}: {refused}");
    }
    tree.write("godot_mcp_commands.json", "{ \"version\": 1 }");
    id += 1;
    let invalid = session.call_tool(
        id,
        "editor.commands.call",
        json!({ "provider": "levels", "command": "rebuild", "arguments": { "seed": 1 }, "idempotencyId": "j" }),
    );
    assert_eq!(error_code(&invalid), json!("invalid_input"), "{invalid}");
    assert!(
        invalid.to_string().contains("editor.commands.list"),
        "{invalid}"
    );
    std::fs::remove_file(tree.root.join("godot_mcp_commands.json")).expect("remove");
    id += 1;
    let absent = session.call_tool(
        id,
        "editor.commands.call",
        json!({ "provider": "levels", "command": "rebuild", "arguments": { "seed": 1 }, "idempotencyId": "k" }),
    );
    assert_eq!(error_code(&absent), json!("invalid_input"), "{absent}");
    assert!(sent(&plugin).is_empty(), "nothing reached the editor");
    id += 1;
    ok(&session.call_tool(
        id,
        "file.create",
        json!({ "path": "after.txt", "content": "x\n", "idempotencyId": "after" }),
    ));
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn a_call_answers_the_value_the_log_and_the_changes_and_a_retry_replays_it() {
    let tree = project("commands-call");
    let mut session = server(&tree, None);
    let plugin = plugin_answering(
        &tree,
        FakeAnswer::SaveThenAnswer {
            path: tree.root.join("levels").join("level.tres"),
            contents: b"[gd_resource]\n".to_vec(),
            payload: done("{\n\"rooms\": 3\n}"),
        },
    );
    std::fs::create_dir_all(tree.root.join("levels")).expect("levels directory");
    let mut id = 1;
    connected(&mut session, &mut id);
    id += 1;
    let arguments = json!({
        "provider": "levels",
        "command": "rebuild",
        "arguments": { "seed": 7 },
        "idempotencyId": "rebuild-1"
    });
    let answer = ok(&session.call_tool(id, "editor.commands.call", arguments.clone()));
    assert_eq!(answer["classification"], "passed", "{answer}");
    assert_eq!(
        answer["result"],
        json!({ "type": "dictionary", "value": { "rooms": { "type": "integer", "value": 3 } } }),
        "{answer}"
    );
    assert_eq!(
        answer["command"],
        json!({ "provider": "levels", "name": "rebuild", "mutates": true })
    );
    assert_eq!(
        answer["output"],
        json!([{ "text": "rebuilt", "stderr": false }])
    );
    assert_eq!(answer["diagnostics"][0]["severity"], "warning", "{answer}");
    assert_eq!(answer["diagnostics"][0]["source_line"], 4, "{answer}");
    assert_eq!(answer["mutation_status"], "committed");
    assert_eq!(answer["applied"], true);
    assert_eq!(answer["rolled_back"], false);
    assert_eq!(answer["declaration_mismatch"], Value::Null, "{answer}");
    assert_eq!(answer["changes"]["total"], 1, "{answer}");
    assert_eq!(answer["changes"]["added"][0]["path"], "levels/level.tres");
    assert!(answer["transaction_id"].is_string(), "{answer}");
    let requests = sent(&plugin);
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["provider"], "levels");
    assert_eq!(requests[0]["command"], "rebuild");
    assert_eq!(requests[0]["script"], "res://tools/level_commands.gd");
    assert_eq!(
        requests[0]["arguments"],
        json!({ "type": "dictionary", "value": { "seed": { "type": "integer", "value": 7 } } })
    );
    assert!(
        requests[0].get("deadline_ms").is_none(),
        "the editor is sent the call, not the server's wait: {}",
        requests[0]
    );

    id += 1;
    let replay = ok(&session.call_tool(id, "editor.commands.call", arguments));
    assert_eq!(replay["replayed"], true, "{replay}");
    assert!(replay["result_unavailable"].is_string(), "{replay}");
    assert_eq!(
        sent(&plugin).len(),
        1,
        "a replay never calls the command again"
    );
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn a_command_declared_not_to_change_files_that_did_is_flagged() {
    let tree = project("commands-mismatch");
    let mut session = server(&tree, None);
    let plugin = plugin_answering(
        &tree,
        FakeAnswer::SaveThenAnswer {
            path: tree.root.join("counted.txt"),
            contents: b"12\n".to_vec(),
            payload: done("12"),
        },
    );
    let mut id = 1;
    connected(&mut session, &mut id);
    id += 1;
    let answer = ok(&session.call_tool(
        id,
        "editor.commands.call",
        json!({ "provider": "levels", "command": "count", "idempotencyId": "count-1" }),
    ));
    assert_eq!(answer["result"], json!({ "type": "integer", "value": 12 }));
    assert!(
        answer["declaration_mismatch"]
            .as_str()
            .is_some_and(|text| text.contains("mutates")),
        "{answer}"
    );
    assert_eq!(
        sent(&plugin)[0]["arguments"],
        json!({ "type": "dictionary", "value": {} }),
        "omitted arguments are an empty object"
    );
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn a_command_that_reports_an_error_has_failed() {
    let tree = project("commands-failed");
    let mut session = server(&tree, None);
    let mut answer = done("null");
    answer["ok"] = json!(false);
    answer["error"] = json!("no level is open");
    answer["literal"] = Value::Null;
    let plugin = plugin_answering(&tree, FakeAnswer::Payload(answer));
    let mut id = 1;
    connected(&mut session, &mut id);
    id += 1;
    let answer = ok(&session.call_tool(
        id,
        "editor.commands.call",
        json!({ "provider": "levels", "command": "count", "idempotencyId": "count-2" }),
    ));
    assert_eq!(answer["classification"], "failed", "{answer}");
    assert_eq!(answer["error"], "no level is open", "{answer}");
    assert!(
        answer["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("no level is open")),
        "{answer}"
    );
    assert_eq!(answer["result"], Value::Null);
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn an_editor_refusing_before_it_loads_the_script_leaves_writes_open() {
    let tree = project("commands-not-started");
    let mut session = server(&tree, None);
    let plugin = plugin_answering(
        &tree,
        FakeAnswer::Error(ErrorKind::InvalidInput, MutationStatus::NotStarted),
    );
    let mut id = 1;
    connected(&mut session, &mut id);
    id += 1;
    let refused = session.call_tool(
        id,
        "editor.commands.call",
        json!({ "provider": "levels", "command": "count", "idempotencyId": "refused" }),
    );
    assert_eq!(error_code(&refused), json!("invalid_input"), "{refused}");
    id += 1;
    ok(&session.call_tool(
        id,
        "file.create",
        json!({ "path": "after.txt", "content": "x\n", "idempotencyId": "after-refused" }),
    ));
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn a_call_the_editor_does_not_answer_in_time_is_uncertain_and_blocks_writes() {
    let tree = project("commands-late");
    let mut session = server(&tree, None);
    let plugin = plugin_answering(&tree, FakeAnswer::Ignore);
    let mut id = 1;
    connected(&mut session, &mut id);
    id += 1;
    let started = Instant::now();
    let late = session.call_tool(
        id,
        "editor.commands.call",
        json!({
            "provider": "levels", "command": "count", "deadline_ms": 1000,
            "idempotencyId": "late"
        }),
    );
    let waited = started.elapsed();
    assert_eq!(error_code(&late), json!("uncertain"), "{late}");
    assert!(
        late.to_string().contains("may still be running"),
        "the answer says the command may still run: {late}"
    );
    assert!(
        waited >= Duration::from_secs(1) && waited < Duration::from_secs(15),
        "the server waits for the caller's deadline: {waited:?}"
    );
    id += 1;
    let blocked = session.call_tool(
        id,
        "file.create",
        json!({ "path": "after.txt", "content": "x\n", "idempotencyId": "after-late" }),
    );
    assert!(
        blocked.get("error").is_some() || blocked["result"]["isError"] == json!(true),
        "a write after an unanswered call is refused: {blocked}"
    );
    assert!(!tree.root.join("after.txt").exists());
    let _ = session.finish();
    let _ = plugin.join();
}

#[test]
fn a_server_with_project_commands_off_never_offers_or_sends_them() {
    let tree = project("commands-off");
    let mut session = server(&tree, Some("project_commands = false\n"));
    let plugin = plugin_answering(&tree, FakeAnswer::Payload(done("null")));
    let mut id = 1;
    let status = connected(&mut session, &mut id);
    assert!(
        !offers_commands(&status),
        "the session must not carry the method: {status}"
    );
    id += 1;
    let listed = ok(&session.call_tool(id, "editor.commands.list", json!({})));
    assert_eq!(listed["enabled"], false, "{listed}");
    assert!(listed["disabled_reason"].is_string(), "{listed}");
    assert_eq!(command(&listed, "levels", "rebuild")["available"], false);
    assert_eq!(listed["editor"]["connected"], true, "{listed}");
    assert_eq!(listed["editor"]["serves_commands"], false, "{listed}");
    id += 1;
    let refused = session.call_tool(
        id,
        "editor.commands.call",
        json!({ "provider": "levels", "command": "rebuild", "arguments": { "seed": 1 }, "idempotencyId": "off" }),
    );
    assert_eq!(
        error_code(&refused),
        json!("permission_denied"),
        "{refused}"
    );
    assert!(
        refused.to_string().contains("project_commands"),
        "the refusal names the switch: {refused}"
    );
    assert!(sent(&plugin).is_empty(), "nothing reached the editor");
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn an_add_on_without_the_method_is_told_to_update() {
    let tree = project("commands-old-addon");
    let mut session = server(&tree, None);
    let plugin = FakeEditorPlugin::spawn(
        &rendezvous(&tree),
        FakePluginScript::implementing(&["editor.state"]).versions("0.4.0", "4.7.1.stable.fake"),
    );
    let mut id = 1;
    let status = connected(&mut session, &mut id);
    assert!(!offers_commands(&status), "{status}");
    id += 1;
    let listed = ok(&session.call_tool(id, "editor.commands.list", json!({})));
    assert_eq!(
        listed["editor"],
        json!({ "connected": true, "serves_commands": false }),
        "{listed}"
    );
    id += 1;
    let refused = session.call_tool(
        id,
        "editor.commands.call",
        json!({ "provider": "levels", "command": "rebuild", "arguments": { "seed": 1 }, "idempotencyId": "old" }),
    );
    assert_eq!(error_code(&refused), json!("unsupported"), "{refused}");
    assert!(refused.to_string().contains("0.4.0"), "{refused}");
    assert!(
        refused.to_string().contains("editor.addon.update"),
        "{refused}"
    );
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}
