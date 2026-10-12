//! M3 acceptance: semantic scene/resource batches through the production
//! stdio assembly, against a fake engine that runs the bundled helper's
//! contract (the engine crate's canonical fixture).
//!
//! The batch semantics themselves are the engine's and are exercised against
//! real Godot builds by `scripts/real_client/m3_smoke.py`. This suite pins
//! what the server owns: preconditions before any engine runs, a refusal that
//! publishes nothing, a change made while the engine ran, dry runs, replay,
//! and that a process killed at each durable phase restarts into exactly the
//! pre-state or the post-state.

#![cfg(target_os = "linux")]

mod support;

use std::path::{Path, PathBuf};
use std::time::Duration;

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

/// A fake Godot editor build whose launches run the canonical fixture after
/// `before` (shell, run with the launch's own arguments).
fn engine(directory: &Path, before: &str) -> PathBuf {
    let body = format!(
        "{before}\nexec /bin/sh '{}' \"$@\"",
        canonical_fixture().display()
    );
    fake_godot(directory, &body).0
}

fn ok(response: &Value) -> &Value {
    assert!(
        response.get("error").is_none() && response["result"]["isError"] != json!(true),
        "the call must succeed: {response}"
    );
    &response["result"]["structuredContent"]
}

fn open(tree: &ProjectTree, godot: &Path) -> StdioSession {
    StdioSession::initialized(&Spawn {
        project: &tree.root,
        current_dir: None,
        profile: "full",
        godot: Some(godot),
    })
}

fn batch(expected: &str, idempotency: &str) -> Value {
    json!({
        "path": "scenes/main.tscn",
        "expectedSha256": expected,
        "operations": [{ "op": "add_node", "parent": ".", "name": "Batched", "type": "Node" }],
        "idempotencyId": idempotency
    })
}

#[test]
fn a_batch_publishes_engine_bytes_under_its_precondition() {
    let tree = ProjectTree::new("m3-batch");
    tree.write("scenes/main.tscn", SCENE);
    let godot = engine(&tree.base, "");
    let mut session = open(&tree, &godot);

    let applied = session.call_tool(
        2,
        "scene.batch.apply",
        batch(&sha256_hex(SCENE.as_bytes()), "batch-1"),
    );
    let applied = ok(&applied).clone();
    assert_eq!(applied["conclusion"], "proven_post_state", "{applied}");
    let written = std::fs::read_to_string(tree.root.join("scenes/main.tscn")).expect("scene");
    assert!(written.starts_with(SCENE), "{written}");
    assert!(written.contains("[node name=\"Batched\""), "{written}");
    assert_eq!(applied["after_sha256"], sha256_hex(written.as_bytes()));
    assert_eq!(applied["before_sha256"], sha256_hex(SCENE.as_bytes()));
    let paths: Vec<&str> = applied["tree"]["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .filter_map(|node| node["path"].as_str())
        .collect();
    assert_eq!(paths, vec![".", "Batched"], "{applied}");
    assert_eq!(applied["tree"]["uid"], "uid://b7rq2x1yb4kfl");
    assert!(
        applied["diff"]["text"]
            .as_str()
            .is_some_and(|text| text.contains("+[node name=\"Batched\"")),
        "{applied}"
    );

    // An exact retry answers from the journal.
    let replay = session.call_tool(
        3,
        "scene.batch.apply",
        batch(&sha256_hex(SCENE.as_bytes()), "batch-1"),
    );
    assert_eq!(ok(&replay)["replayed"], json!(true), "{replay}");
    assert_eq!(
        std::fs::read_to_string(tree.root.join("scenes/main.tscn")).expect("scene"),
        written
    );
    let _ = session.finish();
}

#[test]
fn stale_preconditions_refuse_before_any_engine_runs() {
    let tree = ProjectTree::new("m3-stale");
    tree.write("scenes/main.tscn", SCENE);
    let (godot, argv_log) = fake_godot(
        &tree.base,
        &format!("exec /bin/sh '{}' \"$@\"", canonical_fixture().display()),
    );
    let mut session = open(&tree, &godot);

    let stale = session.call_tool(
        2,
        "scene.batch.apply",
        batch(&sha256_hex(b"older bytes"), "stale-1"),
    );
    assert_eq!(error_code(&stale), json!("conflict"), "{stale}");
    let exists = session.call_tool(
        3,
        "scene.create",
        json!({ "path": "scenes/main.tscn", "rootType": "Node3D", "idempotencyId": "create-over" }),
    );
    assert_eq!(error_code(&exists), json!("conflict"), "{exists}");
    let anonymous = session.call_tool(4, "scene.create", json!({ "path": "scenes/other.tscn" }));
    assert_eq!(
        error_code(&anonymous),
        json!("invalid_input"),
        "{anonymous}"
    );
    let unknown = session.call_tool(
        5,
        "scene.batch.apply",
        json!({
            "path": "scenes/main.tscn",
            "expectedSha256": sha256_hex(SCENE.as_bytes()),
            "operations": [{ "op": "rm_rf" }],
            "idempotencyId": "unknown-op"
        }),
    );
    assert_eq!(error_code(&unknown), json!("invalid_input"), "{unknown}");

    let launched = std::fs::read_to_string(&argv_log).unwrap_or_default();
    assert!(
        !launched.contains("--script"),
        "no refused request may launch the helper: {launched}"
    );
    assert_eq!(
        std::fs::read_to_string(tree.root.join("scenes/main.tscn")).expect("scene"),
        SCENE
    );
    let _ = session.finish();
}

#[test]
fn an_engine_refusal_publishes_nothing() {
    let tree = ProjectTree::new("m3-refused");
    tree.write("scenes/main.tscn", SCENE);
    tree.write("fake-engine.conf", "helper=batch-refused\n");
    let godot = engine(&tree.base, "");
    let mut session = open(&tree, &godot);

    let refused = session.call_tool(
        2,
        "scene.batch.apply",
        batch(&sha256_hex(SCENE.as_bytes()), "refused-1"),
    );
    assert_eq!(error_code(&refused), json!("invalid_input"), "{refused}");
    assert!(refused.to_string().contains("node_not_found"), "{refused}");
    assert_eq!(
        std::fs::read_to_string(tree.root.join("scenes/main.tscn")).expect("scene"),
        SCENE
    );
    let _ = session.finish();
}

#[test]
fn a_change_made_while_the_engine_ran_refuses_without_publishing() {
    let tree = ProjectTree::new("m3-race");
    tree.write("scenes/main.tscn", SCENE);
    // Only the helper launch (the one carrying --script) edits the target.
    let godot = engine(
        &tree.base,
        &format!(
            "case \"$*\" in *--script*) printf '; external\\n' >> '{}' ;; esac",
            tree.root.join("scenes/main.tscn").display()
        ),
    );
    let mut session = open(&tree, &godot);

    let raced = session.call_tool(
        2,
        "scene.batch.apply",
        batch(&sha256_hex(SCENE.as_bytes()), "race-1"),
    );
    assert_eq!(error_code(&raced), json!("conflict"), "{raced}");
    let on_disk = std::fs::read_to_string(tree.root.join("scenes/main.tscn")).expect("scene");
    assert_eq!(
        on_disk,
        format!("{SCENE}; external\n"),
        "the external edit survives"
    );
    assert!(
        !on_disk.contains("Batched"),
        "the engine's bytes were not published"
    );
    let _ = session.finish();
}

#[test]
fn a_dry_run_runs_the_engine_but_publishes_nothing() {
    let tree = ProjectTree::new("m3-dry");
    tree.write("scenes/main.tscn", SCENE);
    let godot = engine(&tree.base, "");
    let mut session = open(&tree, &godot);

    let mut arguments = batch(&sha256_hex(SCENE.as_bytes()), "dry-1");
    arguments["dryRun"] = json!(true);
    let dry = session.call_tool(2, "scene.batch.apply", arguments);
    let dry = ok(&dry).clone();
    assert_eq!(dry["dry_run"], json!(true), "{dry}");
    assert!(dry.get("transaction_id").is_none(), "{dry}");
    assert!(
        dry["tree"]["nodes"].to_string().contains("Batched"),
        "{dry}"
    );
    assert_eq!(
        std::fs::read_to_string(tree.root.join("scenes/main.tscn")).expect("scene"),
        SCENE
    );

    let created = session.call_tool(
        3,
        "resource.create",
        json!({
            "path": "data/stats.tres",
            "class": "Resource",
            "idempotencyId": "create-resource"
        }),
    );
    assert_eq!(ok(&created)["conclusion"], "proven_post_state", "{created}");
    assert!(tree.root.join("data/stats.tres").is_file());
    let _ = session.finish();
}

/// Without a watcher nothing reports an external change, so a dry run's
/// `expectedRevision` is checked against a snapshot that rescans the project:
/// a revision an external edit superseded is refused, as a real mutation's is.
#[test]
fn without_a_watcher_a_dry_run_refuses_a_superseded_revision() {
    let tree = ProjectTree::new("m3-dry-unwatched");
    tree.write("scenes/main.tscn", SCENE);
    let godot = engine(&tree.base, "");
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_qa-journal-foundation"));
    command
        .arg("--project")
        .arg(&tree.root)
        .arg("--profile")
        .arg("full")
        .arg("--godot-binary")
        .arg(&godot)
        .env("GODOT_MCP_TEST_WATCHER_UNAVAILABLE", "1")
        .env_remove("GODOT_MCP_TEST_LAUNCH_CRASH_STAGE");
    let mut session = StdioSession::handshake(StdioSession::start_command(command)).0;
    let info = session.call_tool(2, "project.info.get", json!({}));
    let revision = ok(&info)["workspace"]["revision"].clone();
    assert!(revision.is_u64(), "{info}");

    tree.write("notes/external.txt", "written by another tool\n");
    let mut arguments = batch(&sha256_hex(SCENE.as_bytes()), "dry-unwatched");
    arguments["dryRun"] = json!(true);
    arguments["expectedRevision"] = revision.clone();
    let dry = session.call_tool(3, "scene.batch.apply", arguments);
    assert_eq!(error_code(&dry), json!("conflict"), "{dry}");
    let _ = session.finish();
}

const SCENE_WITH_PLAYER: &str = "[gd_scene format=3 uid=\"uid://b7rq2x1yb4kfl\"]\n\n\
    [node name=\"Main\" type=\"Node2D\"]\n\n\
    [node name=\"Player\" type=\"Node2D\" parent=\".\"]\n";

fn handle_of(nodes: &Value, path: &str) -> String {
    nodes
        .as_array()
        .expect("nodes")
        .iter()
        .find(|node| node["path"] == path)
        .and_then(|node| node["handle"].as_str())
        .unwrap_or_else(|| panic!("{path} carries a handle: {nodes}"))
        .to_owned()
}

#[test]
fn node_handles_are_bound_to_the_bytes_they_were_read_from() {
    let tree = ProjectTree::new("m3-handles");
    tree.write("scenes/main.tscn", SCENE_WITH_PLAYER);
    let recorded = tree.base.join("helper-request.txt");
    tree.write(
        "fake-engine.conf",
        &format!("record={}\n", recorded.display()),
    );
    let (godot, argv_log) = fake_godot(
        &tree.base,
        &format!("exec /bin/sh '{}' \"$@\"", canonical_fixture().display()),
    );
    let mut session = open(&tree, &godot);

    // A read mints one handle per node, bound to the bytes it read.
    let read = session.call_tool(2, "scene.tree.get", json!({ "scene": "scenes/main.tscn" }));
    let data = ok(&read)["data"].clone();
    assert_eq!(
        data["sha256"],
        sha256_hex(SCENE_WITH_PLAYER.as_bytes()),
        "{data}"
    );
    let player = handle_of(&data["nodes"], "Player");
    assert!(player.starts_with("nh1:"), "{player}");
    assert_eq!(
        data["root"]["handle"],
        json!(handle_of(&data["nodes"], "."))
    );

    // The scene changes outside the server. A caller that refreshes only the
    // hash still holds handles of the older bytes: they refuse as a conflict
    // before any engine runs, instead of naming whatever is called Player now.
    let changed = SCENE_WITH_PLAYER.replace("type=\"Node2D\" parent", "type=\"Sprite2D\" parent");
    tree.write("scenes/main.tscn", &changed);
    let stale = session.call_tool(
        3,
        "scene.batch.apply",
        json!({
            "path": "scenes/main.tscn",
            "expectedSha256": sha256_hex(changed.as_bytes()),
            "operations": [{ "op": "remove_node", "node": player }],
            "idempotencyId": "stale-handle"
        }),
    );
    assert_eq!(error_code(&stale), json!("conflict"), "{stale}");
    assert!(stale.to_string().contains("stale"), "{stale}");
    let launched = std::fs::read_to_string(&argv_log).unwrap_or_default();
    assert!(
        !launched.contains("--script"),
        "a stale handle launches no engine: {launched}"
    );
    assert_eq!(
        std::fs::read_to_string(tree.root.join("scenes/main.tscn")).expect("scene"),
        changed
    );

    // A handle of the current bytes reaches the engine resolved to its node.
    let reread = session.call_tool(4, "scene.tree.get", json!({ "scene": "scenes/main.tscn" }));
    let player = handle_of(&ok(&reread)["data"]["nodes"], "Player");
    let applied = session.call_tool(
        5,
        "scene.batch.apply",
        json!({
            "path": "scenes/main.tscn",
            "expectedSha256": sha256_hex(changed.as_bytes()),
            "operations": [{ "op": "add_group", "node": player, "group": "players" }],
            "idempotencyId": "fresh-handle"
        }),
    );
    let applied = ok(&applied).clone();
    assert_eq!(applied["conclusion"], "proven_post_state", "{applied}");
    let request = std::fs::read_to_string(&recorded).expect("the helper request was recorded");
    assert!(
        request.contains(&format!("handles={{\"{player}\":\"Player\"}}")),
        "{request}"
    );
    // The answer's tree carries handles of the bytes it published.
    let after = handle_of(&applied["tree"]["nodes"], "Player");
    assert!(after.starts_with("nh1:") && after != player, "{applied}");

    // A malformed handle is a caller mistake, and a scene that does not exist
    // yet has no nodes a handle could name.
    let malformed = session.call_tool(
        6,
        "scene.batch.apply",
        json!({
            "path": "scenes/main.tscn",
            "expectedSha256": applied["after_sha256"],
            "operations": [{ "op": "remove_node", "node": "nh1:not-a-handle" }],
            "idempotencyId": "malformed-handle"
        }),
    );
    assert_eq!(
        error_code(&malformed),
        json!("invalid_input"),
        "{malformed}"
    );
    let create = session.call_tool(
        7,
        "scene.create",
        json!({
            "path": "scenes/other.tscn",
            "operations": [{ "op": "add_group", "node": after, "group": "g" }],
            "idempotencyId": "create-with-handle"
        }),
    );
    assert_eq!(error_code(&create), json!("invalid_input"), "{create}");
    assert!(!tree.root.join("scenes/other.tscn").exists());
    let _ = session.finish();
}

/// Kill the server at one durable point of the scene transaction, restart it,
/// and return the scene bytes and the restarted server's reconciliation lines.
fn kill_at(point: &str) -> (String, String) {
    let tree = ProjectTree::new("m3-kill");
    tree.write("scenes/main.tscn", SCENE);
    let godot = engine(&tree.base, "");
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_qa-journal-foundation"));
    command
        .arg("--project")
        .arg(&tree.root)
        .arg("--profile")
        .arg("full")
        .arg("--godot-binary")
        .arg(&godot)
        .env("GODOT_MCP_TEST_JOURNAL_KILL", point);
    let (mut session, _) = StdioSession::handshake(StdioSession::start_command(command));
    session.send(&json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": { "name": "scene.batch.apply", "arguments": batch(&sha256_hex(SCENE.as_bytes()), "kill-1") }
    }));
    let exited = session.wait_exit(Duration::from_secs(60));
    assert_eq!(
        exited.code,
        Some(86),
        "the kill point {point} was reached: {}",
        exited.stderr
    );

    let restarted = StdioSession::initialized(&Spawn {
        project: &tree.root,
        current_dir: None,
        profile: "full",
        godot: Some(&godot),
    });
    let after = restarted.finish();
    assert_eq!(after.code, Some(0), "{}", after.stderr);
    let scene = std::fs::read_to_string(tree.root.join("scenes/main.tscn")).expect("scene");
    (scene, after.stderr)
}

#[test]
fn a_kill_at_every_durable_phase_restarts_into_one_proven_state() {
    for (point, published) in [
        ("after_fsync:intent_fsynced@file", false),
        ("after_fsync:staged_fsynced@file", false),
        ("after_fsync:effect_started_fsynced@file", false),
        ("before_fsync:committed_fsynced@file", true),
    ] {
        let (scene, stderr) = kill_at(point);
        if published {
            assert!(
                scene.starts_with(SCENE) && scene.contains("[node name=\"Batched\""),
                "{point}: the published bytes are the post-state: {scene}"
            );
            assert!(
                stderr.contains("reconciled proven_post_state=1"),
                "{point}: {stderr}"
            );
        } else {
            assert_eq!(scene, SCENE, "{point}: nothing was published");
            assert!(
                stderr.contains("reconciled proven_pre_state=1"),
                "{point}: {stderr}"
            );
        }
        assert!(
            stderr.contains("write gate open"),
            "{point}: a proven state never blocks writes: {stderr}"
        );
    }
}
