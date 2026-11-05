//! Every launch is gated by the supervisor this server runs with, wherever the
//! server's executable sits.
//!
//! A release puts `godot-mcp-supervisor` beside `godot-mcp`; the non-shipping
//! test server stands in for it itself. A copy of the test server in a
//! directory with nothing beside it must therefore still create the gated
//! child of a validation run (`script.check`, whose calibration canary is its
//! first launch), and `health.get` must not report the supervisor missing.
//! Validation runs used to look beside the executable only, so on a target
//! directory where no workspace build had made `godot-mcp-supervisor` they
//! failed with "gated child could not be created".

mod support;

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};
use support::{ProjectTree, StdioSession};

/// A copy of the test server in a directory of its own, so nothing that a
/// workspace build left in the target directory sits beside it.
fn lonely_server(tree: &ProjectTree) -> PathBuf {
    let bin = tree.base.join("lonely-bin");
    std::fs::create_dir_all(&bin).expect("a directory for the copy");
    let source = Path::new(env!("CARGO_BIN_EXE_qa-journal-foundation"));
    let copy = bin.join(source.file_name().expect("an executable name"));
    std::fs::copy(source, &copy).expect("copy the test server");
    copy
}

fn server(tree: &ProjectTree, executable: &Path) -> StdioSession {
    let mut command = Command::new(executable);
    command
        .arg("--project")
        .arg(&tree.root)
        .arg("--profile")
        .arg("full")
        .arg("--godot-binary")
        .arg(env!("CARGO_BIN_EXE_qa-fake-game"))
        .env_remove("GODOT_MCP_TEST_LAUNCH_CRASH_STAGE");
    StdioSession::handshake(StdioSession::start_command(command)).0
}

#[test]
fn a_server_with_no_supervisor_beside_it_still_gates_validation_runs() {
    let tree = ProjectTree::new("lonely-supervisor");
    tree.write("fake-game.conf", "exit:0\n");
    tree.write("player.gd", "extends Node\n");
    let executable = lonely_server(&tree);
    let beside: Vec<_> = std::fs::read_dir(executable.parent().expect("a directory"))
        .expect("list the copy's directory")
        .map(|entry| entry.expect("an entry").file_name())
        .collect();
    assert_eq!(beside.len(), 1, "only the copy is there: {beside:?}");
    let mut session = server(&tree, &executable);

    let check = session.call_tool(2, "script.check", json!({ "script": "player.gd" }));
    assert!(
        !check
            .to_string()
            .contains("gated child could not be created"),
        "{check}"
    );
    assert!(
        tree.root.join("fake-game.pid").is_file(),
        "the canary's gated child ran: {check}"
    );

    let health = session.call_tool(3, "health.get", json!({}));
    let godot = health["result"]["structuredContent"]["components"]
        .as_array()
        .and_then(|components| {
            components
                .iter()
                .find(|component| component["component"] == json!("godot"))
        })
        .cloned()
        .unwrap_or(Value::Null);
    assert_ne!(
        godot["status"],
        json!("degraded"),
        "the supervisor this server launches with is present: {health}"
    );
    let _ = session.finish();
}
