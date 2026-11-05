//! `editor.script.run` (ADR 0012) at the MCP surface, with a scripted editor
//! plugin: the operator's switch decides whether the editor is ever asked, an
//! answer says what the run returned, logged and changed, and a run the
//! editor does not answer in time is uncertain, blocks writes, and may still be
//! running. What a real editor runs is checked by `live_editor_script.rs`.

#![cfg(target_os = "linux")]

mod support;

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use godot_mcp_server::test_support::fake_editor_plugin::{
    FakeAnswer, FakeEditorPlugin, FakePluginScript,
};
use serde_json::{Value, json};
use support::{ProjectTree, StdioSession, error_code};

const SOURCE: &str = "extends RefCounted\n\nfunc run():\n\treturn 3\n";

fn rendezvous(tree: &ProjectTree) -> PathBuf {
    tree.root
        .join(".godot-mcp")
        .join("editor-bridge.rendezvous")
}

/// An editor-profile server on `tree`, running scripts when `scripts`.
fn server(tree: &ProjectTree, scripts: bool) -> StdioSession {
    let mut command = Command::new(env!("CARGO_BIN_EXE_qa-journal-foundation"));
    command
        .arg("--project")
        .arg(&tree.root)
        .arg("--profile")
        .arg("editor")
        .env_remove("GODOT_MCP_TEST_LAUNCH_CRASH_STAGE");
    if scripts {
        command.arg("--allow-scripts");
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
        .filter(|(method, _)| method == "editor.script.run")
        .map(|(_, payload)| payload)
        .collect()
}

fn session_offers_scripts(status: &Value) -> bool {
    status["methods"]
        .as_array()
        .expect("methods")
        .iter()
        .any(|method| method == "editor.script.run")
}

fn running_plugin(tree: &ProjectTree, answer: FakeAnswer) -> FakeEditorPlugin {
    FakeEditorPlugin::spawn(
        &rendezvous(tree),
        FakePluginScript::implementing(&["editor.state", "editor.script.run"])
            .answering("editor.script.run", answer),
    )
}

#[test]
fn the_editor_is_never_asked_to_run_a_script_unless_the_server_allows_scripts() {
    let tree = ProjectTree::new("editor-script-off");
    let mut session = server(&tree, false);
    let plugin = running_plugin(&tree, FakeAnswer::Payload(json!({ "stage": "done" })));
    let mut id = 1;
    let status = connected(&mut session, &mut id);
    assert!(
        !session_offers_scripts(&status),
        "the session must not carry the method: {status}"
    );
    id += 1;
    let listed = session.request(id, "tools/list", json!({}));
    assert!(
        listed["result"]["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .any(|tool| tool["name"] == "editor.script.run"),
        "{listed}"
    );
    id += 1;
    let refused = session.call_tool(
        id,
        "editor.script.run",
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
    assert!(sent(&plugin).is_empty(), "nothing reached the editor");
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn an_allowed_run_is_answered_with_its_value_its_log_and_what_it_changed() {
    let tree = ProjectTree::new("editor-script-run");
    let mut session = server(&tree, true);
    let plugin = running_plugin(
        &tree,
        FakeAnswer::SaveThenAnswer {
            path: tree.root.join("made.txt"),
            contents: b"made".to_vec(),
            payload: json!({
                "stage": "done",
                "ran": true,
                "literal": "3",
                "source_file": "gdscript://5.gd",
                "log_capture": true,
                "log": [
                    { "kind": "message", "text": "hello", "stderr": false },
                    {
                        "kind": "warning", "text": "careful", "file": "gdscript://5.gd",
                        "line": 3, "function": "run", "backtrace": []
                    }
                ],
                "log_skipped": 0,
                "log_truncated": false,
                "action": "godot-mcp: editor.script.run"
            }),
        },
    );
    let mut id = 1;
    let status = connected(&mut session, &mut id);
    assert!(session_offers_scripts(&status), "{status}");
    id += 1;
    let answer = ok(&session.call_tool(
        id,
        "editor.script.run",
        json!({ "source": SOURCE, "idempotencyId": "run-1" }),
    ));
    assert_eq!(answer["classification"], "passed", "{answer}");
    assert_eq!(answer["result"], json!({ "type": "integer", "value": 3 }));
    assert_eq!(answer["result_literal"], "3");
    assert_eq!(
        answer["output"],
        json!([{ "text": "hello", "stderr": false }])
    );
    assert_eq!(answer["diagnostics"][0]["severity"], "warning", "{answer}");
    assert_eq!(answer["diagnostics"][0]["source_line"], 3, "{answer}");
    assert_eq!(answer["mutation_status"], "committed");
    assert_eq!(answer["applied"], true);
    assert_eq!(answer["rolled_back"], false);
    assert_eq!(answer["changes"]["total"], 1, "{answer}");
    assert_eq!(
        answer["changes"]["added"][0]["path"], "made.txt",
        "{answer}"
    );
    assert!(answer["transaction_id"].is_string(), "{answer}");
    let requests = sent(&plugin);
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["source"], SOURCE);
    assert!(
        requests[0].get("deadline_ms").is_none(),
        "the editor is sent the source, not the server's wait: {}",
        requests[0]
    );

    id += 1;
    let replay = ok(&session.call_tool(
        id,
        "editor.script.run",
        json!({ "source": SOURCE, "idempotencyId": "run-1" }),
    ));
    assert_eq!(replay["replayed"], true, "{replay}");
    assert!(replay["result_unavailable"].is_string(), "{replay}");
    assert_eq!(
        sent(&plugin).len(),
        1,
        "a replay never runs the source again"
    );
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn a_run_the_editor_does_not_answer_in_time_is_uncertain_and_blocks_writes() {
    let tree = ProjectTree::new("editor-script-late");
    let mut session = server(&tree, true);
    let plugin = running_plugin(&tree, FakeAnswer::Ignore);
    let mut id = 1;
    connected(&mut session, &mut id);
    id += 1;
    let started = Instant::now();
    let late = session.call_tool(
        id,
        "editor.script.run",
        json!({ "source": SOURCE, "deadline_ms": 1000, "idempotencyId": "late" }),
    );
    let waited = started.elapsed();
    assert_eq!(error_code(&late), json!("uncertain"), "{late}");
    assert!(
        late.to_string().contains("may still be running"),
        "the answer says the code may still run: {late}"
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
        "a write after an unanswered run is refused: {blocked}"
    );
    assert!(!tree.root.join("after.txt").exists());
    let _ = session.finish();
    let _ = plugin.join();
}
