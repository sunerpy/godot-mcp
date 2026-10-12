//! M2 acceptance: coherent file, GDScript, settings and asset authoring through
//! the production stdio assembly.
//!
//! Every mutation is driven over MCP exactly as a client would: it needs an
//! idempotency id, a content precondition for what it replaces or removes, and
//! answers with the workspace revision that already contains it.

mod support;

use std::path::Path;

use serde_json::{Value, json};
use support::{ProjectTree, Spawn, StdioSession, error_code, sha256_hex};

fn full(project: &Path) -> Spawn<'_> {
    Spawn {
        project,
        current_dir: None,
        profile: "full",
        godot: None,
    }
}

/// The structured payload of a successful call, refusing a tool error too.
fn ok(response: &Value) -> &Value {
    assert!(
        response.get("error").is_none() && response["result"]["isError"] != json!(true),
        "the call must succeed: {response}"
    );
    &response["result"]["structuredContent"]
}

fn revision(payload: &Value) -> u64 {
    payload["workspace"]["revision"]
        .as_u64()
        .unwrap_or_else(|| panic!("an answer carries its workspace revision: {payload}"))
}

struct Session {
    inner: StdioSession,
    next: u64,
}

impl Session {
    fn open(project: &Path) -> Self {
        Self {
            inner: StdioSession::initialized(&full(project)),
            next: 10,
        }
    }

    fn call(&mut self, name: &str, arguments: Value) -> Value {
        self.next += 1;
        self.inner.call_tool(self.next, name, arguments)
    }

    fn transcript(&self) -> &str {
        &self.inner.transcript
    }

    /// Close the session and wait for the server to release the project.
    fn finish(self) {
        let exited = self.inner.finish();
        assert_eq!(
            exited.code,
            Some(0),
            "the server exits cleanly: {}",
            exited.stderr
        );
    }
}

#[test]
fn file_crud_round_trips_with_hash_preconditions_and_revisions() {
    let tree = ProjectTree::new("m2-files");
    let mut session = Session::open(&tree.root);

    let created = session.call(
        "file.create",
        json!({ "path": "notes/readme.txt", "content": "hello\n", "idempotencyId": "create-1" }),
    );
    let created = ok(&created).clone();
    assert_eq!(created["conclusion"], "proven_post_state", "{created}");
    assert_eq!(created["after_sha256"], sha256_hex(b"hello\n"));
    assert_eq!(created["operation"], "create");
    assert!(
        created["diff"]["text"]
            .as_str()
            .is_some_and(|text| text.contains("+hello")),
        "{created}"
    );
    assert_eq!(
        std::fs::read(tree.root.join("notes/readme.txt")).expect("created"),
        b"hello\n",
        "a missing directory is created for the new file"
    );
    let created_revision = revision(&created);

    let read = session.call("file.get", json!({ "path": "notes/readme.txt" }));
    let read = ok(&read).clone();
    assert_eq!(read["content"], "hello\n");
    assert_eq!(read["encoding"], "utf8");
    assert_eq!(read["sha256"], sha256_hex(b"hello\n"));
    assert!(
        revision(&read) >= created_revision,
        "read-your-writes: {read}"
    );

    let stale = session.call(
        "file.patch",
        json!({
            "path": "notes/readme.txt",
            "expectedSha256": sha256_hex(b"something else"),
            "change": { "content": "clobber\n" },
            "idempotencyId": "patch-stale"
        }),
    );
    assert_eq!(error_code(&stale), json!("conflict"), "{stale}");
    assert_eq!(
        std::fs::read(tree.root.join("notes/readme.txt")).expect("unchanged"),
        b"hello\n"
    );

    let patched = session.call(
        "file.patch",
        json!({
            "path": "notes/readme.txt",
            "expectedSha256": sha256_hex(b"hello\n"),
            "change": { "edits": [{ "startLine": 1, "endLine": 1, "text": "hello, world" }] },
            "idempotencyId": "patch-1"
        }),
    );
    let patched = ok(&patched).clone();
    assert_eq!(patched["after_sha256"], sha256_hex(b"hello, world\n"));
    let diff = patched["diff"]["text"].as_str().expect("diff text");
    assert!(
        diff.contains("-hello\n") && diff.contains("+hello, world\n"),
        "{diff}"
    );
    assert!(revision(&patched) > created_revision);

    let diffed = session.call(
        "file.patch",
        json!({
            "path": "notes/readme.txt",
            "expectedSha256": sha256_hex(b"hello, world\n"),
            "change": { "unifiedDiff": "@@ -1,1 +1,2 @@\n hello, world\n+second line\n" },
            "idempotencyId": "patch-2"
        }),
    );
    let diffed = ok(&diffed).clone();
    assert_eq!(
        std::fs::read(tree.root.join("notes/readme.txt")).expect("diffed"),
        b"hello, world\nsecond line\n"
    );

    let moved = session.call(
        "file.move",
        json!({
            "from": "notes/readme.txt",
            "to": "docs/guide/readme.txt",
            "expectedSha256": sha256_hex(b"hello, world\nsecond line\n"),
            "idempotencyId": "move-1"
        }),
    );
    let moved = ok(&moved).clone();
    assert_eq!(moved["conclusion"], "proven_post_state", "{moved}");
    assert!(!tree.root.join("notes/readme.txt").exists());
    assert_eq!(
        std::fs::read(tree.root.join("docs/guide/readme.txt")).expect("moved"),
        b"hello, world\nsecond line\n"
    );
    assert!(revision(&moved) > revision(&diffed));

    let listed = session.call("project.files.list", json!({ "prefix": "docs" }));
    let listed = ok(&listed).clone();
    assert_eq!(
        listed["files"],
        json!([{
            "path": "docs/guide/readme.txt",
            "kind": "file",
            "bytes": 25,
            "sha256": sha256_hex(b"hello, world\nsecond line\n"),
        }]),
        "{listed}"
    );
    assert!(revision(&listed) >= revision(&moved));

    let deleted = session.call(
        "file.delete",
        json!({
            "path": "docs/guide/readme.txt",
            "expectedSha256": sha256_hex(b"hello, world\nsecond line\n"),
            "idempotencyId": "delete-1"
        }),
    );
    let deleted = ok(&deleted).clone();
    assert_eq!(deleted["conclusion"], "proven_post_state", "{deleted}");
    assert!(!tree.root.join("docs/guide/readme.txt").exists());
    let gone = session.call("file.get", json!({ "path": "docs/guide/readme.txt" }));
    assert_eq!(error_code(&gone), json!("not_found"), "{gone}");

    let root = tree.root.display().to_string();
    assert!(
        !session.transcript().contains(&root),
        "no response may carry the local project path"
    );
}

#[test]
fn mutations_need_an_idempotency_id_and_replay_exactly() {
    let tree = ProjectTree::new("m2-idem");
    let mut session = Session::open(&tree.root);

    let anonymous = session.call("file.create", json!({ "path": "a.txt", "content": "a\n" }));
    assert_eq!(
        error_code(&anonymous),
        json!("invalid_input"),
        "{anonymous}"
    );
    assert!(!tree.root.join("a.txt").exists());

    let arguments = json!({ "path": "a.txt", "content": "a\n", "idempotencyId": "same-id" });
    let first = session.call("file.create", arguments.clone());
    let first = ok(&first).clone();
    let second = session.call("file.create", arguments);
    let second = ok(&second).clone();
    assert_eq!(second["replayed"], json!(true), "{second}");
    assert_eq!(second["transaction_id"], first["transaction_id"]);
    assert_eq!(std::fs::read(tree.root.join("a.txt")).expect("a"), b"a\n");

    let reused = session.call(
        "file.create",
        json!({ "path": "b.txt", "content": "b\n", "idempotencyId": "same-id" }),
    );
    assert_eq!(error_code(&reused), json!("conflict"), "{reused}");
    assert!(!tree.root.join("b.txt").exists());

    let exists = session.call(
        "file.create",
        json!({ "path": "a.txt", "content": "other\n", "idempotencyId": "create-over" }),
    );
    assert_eq!(error_code(&exists), json!("conflict"), "{exists}");
    assert_eq!(std::fs::read(tree.root.join("a.txt")).expect("a"), b"a\n");
}

#[test]
fn dry_runs_describe_the_change_and_write_nothing() {
    let tree = ProjectTree::new("m2-dry");
    tree.write("kept.txt", "one\ntwo\n");
    let mut session = Session::open(&tree.root);

    let create = session.call(
        "file.create",
        json!({ "path": "new.txt", "content": "x\n", "dryRun": true, "idempotencyId": "dry-1" }),
    );
    let create = ok(&create).clone();
    assert_eq!(create["dry_run"], json!(true));
    assert!(create.get("transaction_id").is_none(), "{create}");
    assert!(!tree.root.join("new.txt").exists());

    let patch = session.call(
        "file.patch",
        json!({
            "path": "kept.txt",
            "expectedSha256": sha256_hex(b"one\ntwo\n"),
            "change": { "content": "one\nTWO\n" },
            "dryRun": true,
            "idempotencyId": "dry-2"
        }),
    );
    let patch = ok(&patch).clone();
    assert!(
        patch["diff"]["text"]
            .as_str()
            .is_some_and(|text| text.contains("-two\n+TWO\n")),
        "{patch}"
    );
    assert_eq!(
        std::fs::read(tree.root.join("kept.txt")).expect("kept"),
        b"one\ntwo\n"
    );
}

#[test]
fn engine_and_server_owned_paths_are_never_authored() {
    let tree = ProjectTree::new("m2-private");
    let mut session = Session::open(&tree.root);
    for (path, code) in [
        (".godot/editor/x.cfg", "permission_denied"),
        (".godot-mcp/journal/x", "permission_denied"),
        ("godot_mcp_helpers/x.gd", "permission_denied"),
        ("../outside.txt", "invalid_input"),
        ("/tmp/absolute.txt", "invalid_input"),
        ("project.godot", "invalid_input"),
        ("icon.png.import", "invalid_input"),
        ("player.gd.uid", "invalid_input"),
    ] {
        let response = session.call(
            "file.create",
            json!({ "path": path, "content": "x\n", "idempotencyId": format!("private-{path}") }),
        );
        assert_eq!(error_code(&response), json!(code), "{path}: {response}");
    }
    assert!(!tree.base.join("outside.txt").exists());
    let cache = session.call("file.get", json!({ "path": ".godot/editor/x.cfg" }));
    assert_eq!(error_code(&cache), json!("permission_denied"), "{cache}");
    let manifest = session.call("file.get", json!({ "path": "project.godot" }));
    assert!(
        ok(&manifest)["content"]
            .as_str()
            .is_some_and(|text| text.contains("config_version=5"))
    );
}

#[test]
fn an_expected_revision_must_still_be_current() {
    let tree = ProjectTree::new("m2-revision");
    let mut session = Session::open(&tree.root);
    let info = session.call("project.info.get", json!({}));
    let current = revision(ok(&info));

    let first = session.call(
        "file.create",
        json!({
            "path": "one.txt", "content": "1\n", "expectedRevision": current,
            "idempotencyId": "rev-1"
        }),
    );
    assert!(revision(ok(&first)) > current);

    let stale = session.call(
        "file.create",
        json!({
            "path": "two.txt", "content": "2\n", "expectedRevision": current,
            "idempotencyId": "rev-2"
        }),
    );
    assert_eq!(error_code(&stale), json!("conflict"), "{stale}");
    assert!(!tree.root.join("two.txt").exists());
}

/// A commit that no coherent revision contains is never answered as a plain
/// success (plan section 5.4): the caller is told the change is durable and
/// that the workspace could not be refreshed around it.
#[test]
fn a_commit_no_coherent_revision_contains_is_not_a_plain_success() {
    let tree = ProjectTree::new("m2-commit-stale");
    tree.write("scripts/ok.gd", "extends Node\n");
    std::fs::write(
        tree.root.join("scripts/broken.gd"),
        [0xff, 0xfe, 0x00, 0x80],
    )
    .expect("a script that is not UTF-8 cannot be indexed");
    let mut session = Session::open(&tree.root);
    let created = session.call(
        "file.create",
        json!({ "path": "one.txt", "content": "1\n", "idempotencyId": "stale-commit-1" }),
    );
    assert_eq!(error_code(&created), json!("unavailable"), "{created}");
    assert!(
        created.to_string().contains("committed"),
        "the refusal says the change is durable: {created}"
    );
    assert!(
        tree.root.join("one.txt").exists(),
        "the change itself was committed"
    );
}

/// An expected revision is a precondition on a coherent workspace; a stale
/// snapshot cannot prove it, so the mutation is refused before anything is
/// written.
#[test]
fn an_expected_revision_is_never_proven_against_a_stale_workspace() {
    let tree = ProjectTree::new("m2-revision-stale");
    tree.write("scripts/ok.gd", "extends Node\n");
    let mut session = Session::open(&tree.root);
    let current = revision(ok(&session.call("project.info.get", json!({}))));
    std::fs::write(
        tree.root.join("scripts/broken.gd"),
        [0xff, 0xfe, 0x00, 0x80],
    )
    .expect("a script that is not UTF-8 cannot be indexed");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let info = session.call("project.info.get", json!({}));
        if ok(&info)["workspace"]["coherent"] == json!(false) {
            assert_eq!(
                revision(ok(&info)),
                current,
                "a failed refresh keeps the revision"
            );
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the unindexable script makes the workspace stale: {info}"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let refused = session.call(
        "file.create",
        json!({
            "path": "two.txt", "content": "2\n", "expectedRevision": current,
            "idempotencyId": "stale-revision-1"
        }),
    );
    assert_eq!(error_code(&refused), json!("conflict"), "{refused}");
    assert!(!tree.root.join("two.txt").exists());
}

#[test]
fn script_authoring_gates_on_the_static_parse() {
    let tree = ProjectTree::new("m2-scripts");
    let mut session = Session::open(&tree.root);

    let valid = session.call(
        "script.create",
        json!({
            "path": "scripts/player.gd",
            "content": "extends Node\n\nfunc jump() -> void:\n\tpass\n",
            "idempotencyId": "script-1"
        }),
    );
    let valid = ok(&valid).clone();
    assert_eq!(valid["static_check"]["verdict"], "valid_static", "{valid}");
    assert_eq!(valid["static_check"]["engine_checked"], json!(false));

    let broken = session.call(
        "script.create",
        json!({
            "path": "scripts/broken.gd",
            "content": "extends Node\n\nfunc jump( -> void:\n\tpass\n",
            "idempotencyId": "script-2"
        }),
    );
    assert_eq!(error_code(&broken), json!("invalid_input"), "{broken}");
    assert!(!tree.root.join("scripts/broken.gd").exists());

    let accepted = session.call(
        "script.create",
        json!({
            "path": "scripts/broken.gd",
            "content": "extends Node\n\nfunc jump( -> void:\n\tpass\n",
            "allowParseErrors": true,
            "idempotencyId": "script-3"
        }),
    );
    let accepted = ok(&accepted).clone();
    assert_eq!(accepted["static_check"]["verdict"], "static_errors");
    assert_eq!(
        accepted["static_check"]["published_with_errors"],
        json!(true)
    );

    let fixed = session.call(
        "script.patch",
        json!({
            "path": "scripts/broken.gd",
            "expectedSha256": sha256_hex(b"extends Node\n\nfunc jump( -> void:\n\tpass\n"),
            "change": { "edits": [{ "startLine": 3, "endLine": 3, "text": "func jump() -> void:" }] },
            "idempotencyId": "script-4"
        }),
    );
    let fixed = ok(&fixed).clone();
    assert_eq!(fixed["static_check"]["verdict"], "valid_static", "{fixed}");

    let validated = session.call("script.validate", json!({ "script": "scripts/broken.gd" }));
    assert_eq!(
        ok(&validated)["data"]["verdict"],
        "valid_static",
        "{validated}"
    );

    let not_a_script = session.call(
        "script.create",
        json!({ "path": "notes.txt", "content": "x\n", "idempotencyId": "script-5" }),
    );
    assert_eq!(error_code(&not_a_script), json!("invalid_input"));
}

#[test]
fn typed_settings_survive_a_server_restart() {
    let tree = ProjectTree::new("m2-settings");
    tree.write(
        "project.godot",
        "; keep this comment\nconfig_version=5\n\n[application]\n\nconfig/name=\"m2\"\n",
    );
    tree.write(
        "scenes/main.tscn",
        "[gd_scene format=3]\n\n[node name=\"Main\" type=\"Node2D\"]\n",
    );
    tree.write("autoload/game.gd", "extends Node\n");
    {
        let mut session = Session::open(&tree.root);
        let before = session.call("project.settings.get", json!({}));
        let sha = ok(&before)["sha256"].as_str().expect("sha").to_owned();

        let missing = session.call(
            "project.settings.patch",
            json!({
                "expectedSha256": sha,
                "operations": [{ "op": "set_main_scene", "scene": "res://scenes/absent.tscn" }],
                "idempotencyId": "settings-missing"
            }),
        );
        assert_eq!(error_code(&missing), json!("invalid_input"), "{missing}");

        let unlisted = session.call(
            "project.settings.patch",
            json!({
                "expectedSha256": sha,
                "operations": [{ "op": "set_value", "setting": "editor/run/main_run_args", "value": "--x" }],
                "idempotencyId": "settings-unlisted"
            }),
        );
        assert_eq!(error_code(&unlisted), json!("invalid_input"), "{unlisted}");

        let patched = session.call(
            "project.settings.patch",
            json!({
                "expectedSha256": sha,
                "operations": [
                    { "op": "set_main_scene", "scene": "res://scenes/main.tscn" },
                    { "op": "set_autoload", "name": "Game", "path": "res://autoload/game.gd" },
                    { "op": "set_input_action", "name": "jump", "events": [
                        { "type": "key", "key": "Space" },
                        { "type": "mouse_button", "button": "left" }
                    ] },
                    { "op": "set_value", "setting": "application/config/name", "value": "Renamed" }
                ],
                "idempotencyId": "settings-1"
            }),
        );
        let patched = ok(&patched).clone();
        assert_eq!(patched["conclusion"], "proven_post_state", "{patched}");

        let stale = session.call(
            "project.settings.patch",
            json!({
                "expectedSha256": sha,
                "operations": [{ "op": "clear_main_scene" }],
                "idempotencyId": "settings-stale"
            }),
        );
        assert_eq!(error_code(&stale), json!("conflict"), "{stale}");
        let info = session.call("project.info.get", json!({}));
        assert_eq!(
            ok(&info)["data"]["main_scene"],
            "scenes/main.tscn",
            "{info}"
        );
        session.finish();
    }
    let text = std::fs::read_to_string(tree.root.join("project.godot")).expect("manifest");
    assert!(text.starts_with("; keep this comment\n"), "{text}");

    // A fresh server reads the same settings back.
    let mut session = Session::open(&tree.root);
    let settings = session.call("project.settings.get", json!({}));
    let settings = ok(&settings).clone();
    assert_eq!(
        settings["main_scene"], "res://scenes/main.tscn",
        "{settings}"
    );
    assert_eq!(settings["name"], "Renamed");
    assert_eq!(
        settings["autoloads"],
        json!([{ "name": "Game", "path": "res://autoload/game.gd", "singleton": true }])
    );
    let jump = &settings["input_actions"][0];
    assert_eq!(jump["name"], "jump");
    assert_eq!(jump["events"][0]["key"], "Space");
    assert_eq!(jump["events"][1]["type"], "mouse_button");
    assert_eq!(jump["events"][1]["button_index"], 1);
}

#[test]
fn assets_upload_inline_and_in_chunks_and_report_import_state() {
    let tree = ProjectTree::new("m2-assets");
    let mut session = Session::open(&tree.root);
    let bytes = b"fake png bytes for status\n";

    let put = session.call(
        "asset.put",
        json!({
            "path": "art/icon.png",
            "contentBase64": encode(bytes),
            "sha256": sha256_hex(bytes),
            "idempotencyId": "asset-1"
        }),
    );
    let put = ok(&put).clone();
    assert_eq!(put["conclusion"], "proven_post_state", "{put}");
    assert_eq!(
        std::fs::read(tree.root.join("art/icon.png")).expect("asset"),
        bytes
    );
    let status = session.call("asset.status", json!({ "path": "art/icon.png" }));
    let status = ok(&status).clone();
    assert_eq!(status["import"]["state"], "not_imported", "{status}");
    assert_eq!(status["sha256"], sha256_hex(bytes));

    // A chunked upload is published only once complete and verified.
    let whole: Vec<u8> = (0_u8..30).collect();
    let first = session.call(
        "asset.put",
        json!({
            "path": "audio/clip.wav",
            "contentBase64": encode(&whole[..10]),
            "sha256": sha256_hex(&whole),
            "totalBytes": 30,
            "idempotencyId": "chunk-1"
        }),
    );
    let first = ok(&first).clone();
    assert_eq!(first["complete"], json!(false), "{first}");
    let upload = first["upload_id"].as_str().expect("upload id").to_owned();
    assert!(!tree.root.join("audio/clip.wav").exists());
    let chunk = |offset: usize, idempotency: &str| {
        json!({
            "path": "audio/clip.wav",
            "contentBase64": encode(&whole[offset..offset + 10]),
            "sha256": sha256_hex(&whole),
            "totalBytes": 30,
            "upload": { "id": upload, "offset": offset },
            "idempotencyId": idempotency
        })
    };
    let second = session.call("asset.put", chunk(10, "chunk-2"));
    assert_eq!(ok(&second)["received_bytes"], 20);
    let retried = session.call("asset.put", chunk(10, "chunk-2-retry"));
    assert_eq!(
        ok(&retried)["received_bytes"],
        20,
        "a repeated chunk is a no-op"
    );
    let skipped = session.call("asset.put", chunk(0, "chunk-skip"));
    assert_eq!(
        ok(&skipped)["received_bytes"],
        20,
        "an already-held chunk is a no-op"
    );
    let last = session.call("asset.put", chunk(20, "chunk-3"));
    let last = ok(&last).clone();
    assert_eq!(last["complete"], json!(true), "{last}");
    assert_eq!(last["conclusion"], "proven_post_state");
    assert_eq!(
        std::fs::read(tree.root.join("audio/clip.wav")).expect("clip"),
        whole
    );

    let wrong = session.call(
        "asset.put",
        json!({
            "path": "art/wrong.png",
            "contentBase64": encode(b"abc"),
            "sha256": sha256_hex(b"xyz"),
            "idempotencyId": "asset-wrong"
        }),
    );
    assert_eq!(error_code(&wrong), json!("invalid_input"), "{wrong}");
    assert!(!tree.root.join("art/wrong.png").exists());

    // The engine's import records are read without exposing their paths.
    tree.write(
        "art/icon.png.import",
        "[remap]\n\nimporter=\"texture\"\ntype=\"CompressedTexture2D\"\n\
         uid=\"uid://b2n8vbxuqyxk7\"\npath=\"res://.godot/imported/icon.png-0123.ctex\"\n\n\
         [deps]\n\nsource_file=\"res://art/icon.png\"\n\
         dest_files=[\"res://.godot/imported/icon.png-0123.ctex\"]\n",
    );
    tree.write(
        ".godot/imported/icon.png-0123.ctex",
        "imported texture artifact",
    );
    tree.write(
        ".godot/imported/icon.png-0123.md5",
        "source_md5=\"abf930e0aa5db0cc69dea0f1c38e2f28\"\ndest_md5=\"0\"\n",
    );
    let imported = session.call("asset.status", json!({ "path": "art/icon.png" }));
    let imported = ok(&imported).clone();
    assert_eq!(imported["import"]["state"], "imported", "{imported}");
    assert_eq!(imported["import"]["importer"], "texture");
    assert_eq!(imported["import"]["source_matches_import"], json!(true));
    assert_eq!(
        imported["import"]["artifacts"],
        json!([{
            "kind": "ctex",
            "bytes": 25,
            "sha256": sha256_hex(b"imported texture artifact"),
            "present": true,
        }])
    );
    assert!(
        !imported.to_string().contains(".godot/imported"),
        "no engine cache path is exposed: {imported}"
    );

    let deleted = session.call(
        "asset.delete",
        json!({
            "path": "art/icon.png",
            "expectedSha256": sha256_hex(bytes),
            "idempotencyId": "asset-delete"
        }),
    );
    let deleted = ok(&deleted).clone();
    assert_eq!(deleted["removed_sidecars"], json!(["art/icon.png.import"]));
    assert!(!tree.root.join("art/icon.png").exists());
    assert!(!tree.root.join("art/icon.png.import").exists());
}

fn encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::new();
    for chunk in bytes.chunks(3) {
        let value = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for index in 0..4 {
            if index <= chunk.len() {
                output.push(ALPHABET[((value >> (18 - 6 * index)) & 0x3f) as usize] as char);
            } else {
                output.push('=');
            }
        }
    }
    output
}
