//! M2 acceptance: a server launched with a workspace authority starts in
//! bootstrap state, creates a project through MCP alone and binds exactly one
//! project for its lifetime (plan section 5.2).

mod support;

use std::collections::BTreeSet;

use serde_json::{Value, json};
use support::{StdioSession, error_code};

fn ok(response: &Value) -> &Value {
    assert!(
        response.get("error").is_none() && response["result"]["isError"] != json!(true),
        "the call must succeed: {response}"
    );
    &response["result"]["structuredContent"]
}

fn tool_names(session: &mut StdioSession, id: u64) -> BTreeSet<String> {
    let listed = session.request(id, "tools/list", json!({}));
    listed["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools/list answers a tool array: {listed}"))
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name").to_owned())
        .collect()
}

fn names(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

struct Workspace {
    _temp: tempfile::TempDir,
    root: std::path::PathBuf,
}

fn workspace(label: &str) -> Workspace {
    let temp = tempfile::Builder::new()
        .prefix(&format!("godot-mcp-{label}-"))
        .tempdir()
        .expect("temp workspace");
    let root = temp.path().canonicalize().expect("canonical").join("ws");
    std::fs::create_dir(&root).expect("workspace dir");
    Workspace { _temp: temp, root }
}

#[test]
fn an_empty_workspace_becomes_one_bound_project_through_mcp_alone() {
    let workspace = workspace("m2-bootstrap");
    let (mut session, initialized) =
        StdioSession::handshake(StdioSession::spawn_workspace(&workspace.root, "full"));
    assert_eq!(
        initialized["result"]["capabilities"]["tools"]["listChanged"],
        json!(true),
        "a bootstrap server announces that its tool list changes: {initialized}"
    );

    assert_eq!(
        tool_names(&mut session, 2),
        names(&[
            "capabilities.get",
            "health.get",
            "project.bind",
            "project.create",
            "workspace.projects.list",
        ]),
        "bootstrap lists only the bootstrap tools"
    );
    let health = session.call_tool(3, "health.get", json!({}));
    assert_eq!(ok(&health)["state"], "bootstrap", "{health}");
    let listed = session.call_tool(4, "workspace.projects.list", json!({}));
    assert_eq!(ok(&listed)["projects"], json!([]));
    let unbound = session.call_tool(5, "file.create", json!({ "path": "x", "content": "" }));
    assert_eq!(error_code(&unbound), json!("not_found"), "{unbound}");

    let anonymous = session.call_tool(
        6,
        "project.create",
        json!({ "path": "game", "name": "Demo" }),
    );
    assert_eq!(
        error_code(&anonymous),
        json!("invalid_input"),
        "{anonymous}"
    );
    let create = json!({
        "path": "game", "name": "Demo", "renderer": "gl_compatibility",
        "idempotencyId": "create-game"
    });
    let created = session.call_tool(7, "project.create", create.clone());
    assert_eq!(ok(&created)["path"], "game", "{created}");
    let manifest =
        std::fs::read_to_string(workspace.root.join("game/project.godot")).expect("manifest");
    assert!(manifest.contains("config/name=\"Demo\""), "{manifest}");
    assert!(manifest.contains("renderer/rendering_method=\"gl_compatibility\""));
    let retried = session.call_tool(8, "project.create", create);
    assert_eq!(ok(&retried)["replayed"], json!(true), "{retried}");

    for (path, code) in [
        ("game/inner", "conflict"),
        ("../escape", "permission_denied"),
        ("/abs", "permission_denied"),
    ] {
        let refused = session.call_tool(
            9,
            "project.create",
            json!({ "path": path, "name": "x", "idempotencyId": format!("refused-{path}") }),
        );
        assert_eq!(error_code(&refused), json!(code), "{path}: {refused}");
    }
    let listed = session.call_tool(10, "workspace.projects.list", json!({}));
    assert_eq!(
        ok(&listed)["projects"],
        json!([{ "path": "game", "name": "Demo" }])
    );

    let bound = session.call_tool(11, "project.bind", json!({ "path": "game" }));
    let bound = ok(&bound).clone();
    assert_eq!(bound["bound"], json!(true), "{bound}");
    assert_eq!(bound["workspace"]["revision"], json!(1), "{bound}");
    assert_eq!(bound["write_gate"]["open"], json!(true), "{bound}");
    assert!(
        session
            .transcript
            .contains("\"method\":\"notifications/tools/list_changed\""),
        "binding announces the changed tool list: {}",
        session.transcript
    );

    let after = tool_names(&mut session, 12);
    for expected in ["file.create", "project.settings.patch", "project.info.get"] {
        assert!(
            after.contains(expected),
            "{expected} is listed after binding"
        );
    }
    for gone in ["project.bind", "project.create", "workspace.projects.list"] {
        assert!(!after.contains(gone), "{gone} is not listed after binding");
    }
    let rebind = session.call_tool(13, "project.bind", json!({ "path": "game" }));
    assert_eq!(
        error_code(&rebind),
        json!("not_found"),
        "a second bind is impossible: {rebind}"
    );

    let written = session.call_tool(
        14,
        "file.create",
        json!({ "path": "notes.txt", "content": "bound\n", "idempotencyId": "after-bind" }),
    );
    assert_eq!(ok(&written)["conclusion"], "proven_post_state", "{written}");
    assert_eq!(
        std::fs::read(workspace.root.join("game/notes.txt")).expect("written"),
        b"bound\n"
    );
    assert!(workspace.root.join("game/.godot-mcp").is_dir());
    assert!(
        !workspace.root.join(".godot-mcp").exists(),
        "the workspace itself gets no project state"
    );
    assert!(
        !session
            .transcript
            .contains(&workspace.root.display().to_string()),
        "no response carries the workspace location"
    );
    let exited = session.finish();
    assert_eq!(exited.code, Some(0), "{}", exited.stderr);
}

#[test]
fn a_read_only_profile_cannot_create_a_project() {
    let workspace = workspace("m2-bootstrap-core");
    std::fs::create_dir(workspace.root.join("existing")).expect("project dir");
    std::fs::write(
        workspace.root.join("existing/project.godot"),
        "config_version=5\n\n[application]\n\nconfig/name=\"Existing\"\n",
    )
    .expect("manifest");
    let (mut session, _) =
        StdioSession::handshake(StdioSession::spawn_workspace(&workspace.root, "core"));
    let listed = tool_names(&mut session, 2);
    assert!(!listed.contains("project.create"), "{listed:?}");
    assert!(listed.contains("project.bind"));
    let bound = session.call_tool(3, "project.bind", json!({ "path": "existing" }));
    assert_eq!(ok(&bound)["bound"], json!(true), "{bound}");
    let after = tool_names(&mut session, 4);
    assert!(
        after.contains("project.info.get") && !after.contains("file.create"),
        "{after:?}"
    );
}

/// An idempotency id names exactly one `project.create` request: its retry
/// replays the answer, the same id with other arguments is refused, and
/// another id never claims the project as its own replay.
#[test]
fn an_idempotency_id_names_exactly_one_create_request() {
    let workspace = workspace("m2-create-idem");
    let (mut session, _) =
        StdioSession::handshake(StdioSession::spawn_workspace(&workspace.root, "full"));
    let create = json!({ "path": "game", "name": "Demo", "idempotencyId": "create-1" });
    let created = session.call_tool(2, "project.create", create.clone());
    assert_eq!(ok(&created)["replayed"], json!(false), "{created}");
    let retried = session.call_tool(3, "project.create", create);
    assert_eq!(ok(&retried)["replayed"], json!(true), "{retried}");
    assert_eq!(
        ok(&retried)["manifest_sha256"],
        ok(&created)["manifest_sha256"]
    );

    let reused = session.call_tool(
        4,
        "project.create",
        json!({ "path": "other", "name": "Demo", "idempotencyId": "create-1" }),
    );
    assert_eq!(error_code(&reused), json!("conflict"), "{reused}");
    assert!(
        !workspace.root.join("other").exists(),
        "a reused id creates nothing"
    );

    let another = session.call_tool(
        5,
        "project.create",
        json!({ "path": "game", "name": "Demo", "idempotencyId": "create-2" }),
    );
    assert_eq!(error_code(&another), json!("conflict"), "{another}");

    let private = session.call_tool(
        6,
        "project.create",
        json!({ "path": ".godot-mcp/inner", "name": "Demo", "idempotencyId": "create-3" }),
    );
    assert_eq!(
        error_code(&private),
        json!("permission_denied"),
        "{private}"
    );
    assert!(!workspace.root.join(".godot-mcp/inner").exists());
    let records = session.call_tool(
        7,
        "project.create",
        json!({ "path": ".godot-mcp-bootstrap/inner", "name": "Demo", "idempotencyId": "create-4" }),
    );
    assert_eq!(
        error_code(&records),
        json!("permission_denied"),
        "{records}"
    );
    let _ = session.finish();
}
