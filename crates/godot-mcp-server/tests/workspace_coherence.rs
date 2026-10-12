//! M1 acceptance: one live, coherent workspace snapshot.
//!
//! Finding F-03 is reproduced over the wire: before the workspace coordinator,
//! a file created after the first query stayed invisible while every answer
//! still claimed a coherent snapshot. Every answer now carries the workspace
//! revision and coherence it was computed from, external create, modify, rename
//! and delete advance the revision, and a snapshot that cannot be refreshed is
//! reported stale rather than coherent.

mod support;

use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::{ProjectTree, Spawn, StdioSession, structured};

const PLAYER: &str = "extends Node\n\nfunc jump() -> void:\n\tpass\n";

/// The domain error code of a refusal, whichever JSON-RPC shape carries it.
fn domain_code(response: &Value) -> Value {
    if response.get("error").is_some() {
        response["error"]["data"]["code"].clone()
    } else {
        response["result"]["structuredContent"]["code"].clone()
    }
}

fn workspace_of(response: &Value) -> Value {
    structured(response)["workspace"].clone()
}

fn revision_of(response: &Value) -> u64 {
    workspace_of(response)["revision"]
        .as_u64()
        .unwrap_or_else(|| panic!("every answer must carry its workspace revision: {response}"))
}

/// Poll `project.info.get` until the workspace revision passes `after`. The
/// watcher debounces, so the wait is a bounded condition poll, never a guess.
fn wait_for_revision(session: &mut StdioSession, id: &mut u64, after: u64) -> Value {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        *id += 1;
        let response = session.call_tool(*id, "project.info.get", json!({}));
        if revision_of(&response) > after {
            return response;
        }
        assert!(
            Instant::now() < deadline,
            "the workspace revision never advanced past {after}: {response}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// F-03: a file created, modified, renamed and removed after the first query is
/// reflected by the next coherent snapshot, and each change advances the
/// revision.
#[test]
fn external_changes_after_the_first_query_reach_the_next_coherent_snapshot() {
    let tree = ProjectTree::new("m1-coherence");
    tree.write("scripts/player.gd", PLAYER);
    let mut session = StdioSession::initialized(&Spawn::core(&tree.root));
    let mut id = 1;

    id += 1;
    let first = session.call_tool(
        id,
        "gdscript.references.get",
        json!({ "script": "scripts/player.gd", "symbol": "jump" }),
    );
    let start = revision_of(&first);
    assert!(start >= 1, "the startup scan is revision 1 or later");
    assert_eq!(workspace_of(&first)["coherent"], json!(true), "{first}");

    // Create.
    tree.write(
        "scripts/enemy.gd",
        "extends Node\n\nfunc attack() -> void:\n\tpass\n",
    );
    let created = revision_of(&wait_for_revision(&mut session, &mut id, start));
    id += 1;
    let symbols = session.call_tool(
        id,
        "gdscript.symbols.get",
        json!({ "script": "scripts/enemy.gd" }),
    );
    assert!(
        structured(&symbols).to_string().contains("attack"),
        "a created script must be answerable: {symbols}"
    );
    assert_eq!(workspace_of(&symbols)["coherent"], json!(true));
    assert!(revision_of(&symbols) >= created);

    // Modify.
    tree.write(
        "scripts/enemy.gd",
        "extends Node\n\nfunc retreat() -> void:\n\tpass\n",
    );
    let modified = revision_of(&wait_for_revision(&mut session, &mut id, created));
    id += 1;
    let symbols = session.call_tool(
        id,
        "gdscript.references.get",
        json!({ "script": "scripts/enemy.gd", "symbol": "retreat" }),
    );
    assert_ne!(
        symbols["result"]["isError"],
        json!(true),
        "a modified symbol must be answerable from the index: {symbols}"
    );

    // Rename.
    std::fs::rename(
        tree.root.join("scripts/enemy.gd"),
        tree.root.join("scripts/foe.gd"),
    )
    .expect("rename the script");
    let renamed = revision_of(&wait_for_revision(&mut session, &mut id, modified));
    id += 1;
    let old = session.call_tool(
        id,
        "gdscript.symbols.get",
        json!({ "script": "scripts/enemy.gd" }),
    );
    assert_eq!(
        domain_code(&old),
        json!("not_found"),
        "the old spelling is gone: {old}"
    );
    id += 1;
    let new = session.call_tool(
        id,
        "gdscript.references.get",
        json!({ "script": "scripts/foe.gd", "symbol": "retreat" }),
    );
    assert_ne!(new["result"]["isError"], json!(true), "{new}");

    // Delete.
    std::fs::remove_file(tree.root.join("scripts/foe.gd")).expect("delete the script");
    let deleted = revision_of(&wait_for_revision(&mut session, &mut id, renamed));
    id += 1;
    let gone = session.call_tool(
        id,
        "gdscript.references.get",
        json!({ "script": "scripts/foe.gd" }),
    );
    assert_eq!(domain_code(&gone), json!("not_found"), "{gone}");
    assert!(deleted > renamed);
    let _ = session.finish();
}

/// A refresh that fails keeps the previous snapshot readable but reports it
/// stale; it is never presented as coherent, and the next good refresh
/// publishes a new coherent revision.
#[test]
fn a_snapshot_that_cannot_be_refreshed_is_reported_stale() {
    let tree = ProjectTree::new("m1-stale");
    tree.write("scripts/player.gd", PLAYER);
    let mut session = StdioSession::initialized(&Spawn::core(&tree.root));
    let mut id = 1;
    id += 1;
    let start = revision_of(&session.call_tool(id, "project.info.get", json!({})));

    std::fs::write(
        tree.root.join("scripts/broken.gd"),
        [0xff, 0xfe, 0x00, 0x80],
    )
    .expect("write a script that is not UTF-8");
    let deadline = Instant::now() + Duration::from_secs(15);
    let stale = loop {
        id += 1;
        let response = session.call_tool(id, "project.info.get", json!({}));
        if workspace_of(&response)["coherent"] == json!(false) {
            break response;
        }
        assert!(
            Instant::now() < deadline,
            "an unrefreshable workspace must be reported stale: {response}"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(
        workspace_of(&stale)["stale_reason"].is_string(),
        "a stale snapshot says why: {stale}"
    );
    id += 1;
    let answer = session.call_tool(
        id,
        "gdscript.references.get",
        json!({ "script": "scripts/player.gd", "symbol": "jump" }),
    );
    assert_eq!(
        workspace_of(&answer)["coherent"],
        json!(false),
        "an answer from a stale snapshot must not claim coherence: {answer}; the stale answer \
         before it was {stale}"
    );

    std::fs::remove_file(tree.root.join("scripts/broken.gd")).expect("remove the bad script");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        id += 1;
        let response = session.call_tool(id, "project.info.get", json!({}));
        if workspace_of(&response)["coherent"] == json!(true) {
            assert!(revision_of(&response) > start);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "a repaired workspace must become coherent again: {response}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = session.finish();
}

/// A `project.godot` change reaches the next snapshot's settings digest.
#[test]
fn a_settings_change_reaches_the_next_snapshot() {
    let tree = ProjectTree::new("m1-settings");
    let mut session = StdioSession::initialized(&Spawn::core(&tree.root));
    let mut id = 2;
    let before = session.call_tool(id, "project.info.get", json!({}));
    let digest = workspace_of(&before)["settings_digest"].clone();
    assert!(digest.is_string(), "{before}");

    std::fs::write(
        tree.root.join("project.godot"),
        "config_version=5\n\n[application]\nconfig/name=\"renamed\"\n",
    )
    .expect("edit project.godot");
    let after = wait_for_revision(&mut session, &mut id, revision_of(&before));
    assert_ne!(workspace_of(&after)["settings_digest"], digest, "{after}");
    let _ = session.finish();
}
