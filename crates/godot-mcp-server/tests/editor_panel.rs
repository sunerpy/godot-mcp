//! What the production server tells a connected editor about itself, for the
//! `godot_mcp` add-on's panel: `bridge.server_status`.
//!
//! The add-on's panel in a real editor is checked by the live-editor suite;
//! this pins what the server owns against a scripted plugin: the status reaches
//! a plugin that lists the method and no other, it names exactly the tools the
//! profile lists, and what it says each need's availability is comes from the
//! same health facts `health.get` reports.

#![cfg(target_os = "linux")]

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use godot_mcp_server::test_support::fake_editor_plugin::{
    FakeAnswer, FakeEditorPlugin, FakePluginScript,
};
use serde_json::{Value, json};
use support::{ProjectTree, Spawn, StdioSession};

const SERVER_STATUS: &str = "bridge.server_status";

fn rendezvous(tree: &ProjectTree) -> PathBuf {
    tree.root
        .join(".godot-mcp")
        .join("editor-bridge.rendezvous")
}

fn ok(response: &Value) -> Value {
    assert!(
        response.get("error").is_none() && response["result"]["isError"] != json!(true),
        "the call must succeed: {response}"
    );
    response["result"]["structuredContent"].clone()
}

fn wait_connected(session: &mut StdioSession, next_id: &mut u64) {
    let started = Instant::now();
    loop {
        *next_id += 1;
        let status = ok(&session.call_tool(*next_id, "editor.status.get", json!({})));
        if status["connected"] == json!(true) {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the plugin never connected: {status}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn sent(plugin: &FakeEditorPlugin, method: &str) -> Vec<Value> {
    plugin
        .received()
        .into_iter()
        .filter(|(received, _)| received == method)
        .map(|(_, payload)| payload)
        .collect()
}

fn listed(session: &mut StdioSession, id: u64) -> BTreeSet<String> {
    let response = session.request(id, "tools/list", json!({}));
    response["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools/list must answer: {response}"))
        .iter()
        .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
        .collect()
}

fn plugin_taking_the_status(tree: &ProjectTree) -> FakeEditorPlugin {
    FakeEditorPlugin::spawn(
        &rendezvous(tree),
        FakePluginScript::implementing(&[SERVER_STATUS, "editor.state"]).answering(
            "editor.state",
            FakeAnswer::Payload(json!({ "open_scenes": [], "open_scripts": [] })),
        ),
    )
}

#[test]
fn the_editor_is_told_what_the_server_serves() {
    let tree = ProjectTree::new("panel-status");
    let (mut session, initialized) = StdioSession::handshake(StdioSession::spawn(&Spawn {
        project: &tree.root,
        current_dir: None,
        profile: "full",
        godot: None,
    }));
    let secret = std::fs::read_to_string(rendezvous(&tree))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|document| document["secret_hex"].as_str().map(str::to_owned))
        .expect("the rendezvous names its secret");
    let plugin = plugin_taking_the_status(&tree);
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    let started = Instant::now();
    let status = loop {
        if let Some(status) = sent(&plugin, SERVER_STATUS).pop() {
            break status;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the server never described itself: {:?}",
            plugin.received()
        );
        std::thread::sleep(Duration::from_millis(50));
    };

    assert_eq!(
        status["server_version"], initialized["result"]["serverInfo"]["version"],
        "{status}"
    );
    assert_eq!(status["profile"], "full", "{status}");
    assert_eq!(status["transport"], "stdio", "{status}");
    assert!(status["observed_at_unix_ms"].as_u64().is_some(), "{status}");
    assert_eq!(status["mutations_blocked"], Value::Null, "{status}");

    // The tools are exactly the ones the profile lists, with what each needs.
    let tools: BTreeMap<String, Value> = status["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|tool| {
            (
                tool["name"].as_str().expect("name").to_owned(),
                tool.clone(),
            )
        })
        .collect();
    id += 1;
    assert_eq!(
        tools.keys().cloned().collect::<BTreeSet<_>>(),
        listed(&mut session, id)
    );
    assert_eq!(status["tools_total"], json!(tools.len()), "{status}");
    assert_eq!(status["tools_truncated"], json!(false), "{status}");
    for (name, needs, read_only) in [
        ("project.info.get", "project_files", true),
        ("script.check", "headless_engine", false),
        ("editor.scene.tree.get", "live_editor", true),
        ("editor.scene.edit", "live_editor", false),
        ("runtime.tree.get", "game_session", true),
        ("debug.command", "editor_session", false),
    ] {
        assert_eq!(tools[name]["needs"], needs, "{name}: {}", tools[name]);
        assert_eq!(
            tools[name]["read_only"],
            json!(read_only),
            "{name}: {}",
            tools[name]
        );
        assert!(
            tools[name]["title"]
                .as_str()
                .is_some_and(|title| !title.is_empty()),
            "{name}: {}",
            tools[name]
        );
    }

    // Every need the tools have is described once, and its availability is
    // the one health.get reports for the same fact.
    let needs: BTreeMap<String, Value> = status["needs"]
        .as_array()
        .expect("needs")
        .iter()
        .map(|need| {
            (
                need["needs"].as_str().expect("needs").to_owned(),
                need.clone(),
            )
        })
        .collect();
    assert_eq!(
        needs.keys().cloned().collect::<BTreeSet<_>>(),
        tools
            .values()
            .filter_map(|tool| tool["needs"].as_str().map(str::to_owned))
            .collect::<BTreeSet<_>>()
    );
    id += 1;
    let health = ok(&session.call_tool(id, "health.get", json!({})));
    let components: BTreeMap<String, Value> = health["components"]
        .as_array()
        .expect("components")
        .iter()
        .map(|component| {
            (
                component["component"].as_str().expect("name").to_owned(),
                component.clone(),
            )
        })
        .collect();
    for (need, component) in [
        ("project_files", "project"),
        ("headless_engine", "godot"),
        ("live_editor", "editor"),
        ("game_session", "runtime"),
    ] {
        assert_eq!(needs[need]["component"], component, "{}", needs[need]);
        assert_eq!(
            needs[need]["status"], components[component]["status"],
            "{need}: {} against {}",
            needs[need], components[component]
        );
        assert_eq!(
            needs[need]["reason"], components[component]["message"],
            "{need}: {} against {}",
            needs[need], components[component]
        );
    }
    assert_eq!(needs["live_editor"]["status"], "ready", "{status}");
    let planes: BTreeMap<String, Value> = health["planes"]
        .as_array()
        .expect("planes")
        .iter()
        .map(|plane| {
            (
                plane["plane"].as_str().expect("plane").to_owned(),
                plane.clone(),
            )
        })
        .collect();
    let session_plane = &needs["editor_session"];
    assert_eq!(session_plane["plane"], "lsp_dap", "{session_plane}");
    assert_eq!(
        session_plane["status"] == "ready",
        planes["lsp_dap"]["usable"] == json!(true),
        "{session_plane} against {}",
        planes["lsp_dap"]
    );
    assert!(
        session_plane["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("editor.session.start")),
        "an idle editor-session plane says how to start one: {session_plane}"
    );
    assert!(
        !status.to_string().contains(&secret),
        "the status never carries the rendezvous secret"
    );
    let _ = session.finish();
    drop(plugin);
}

#[test]
fn an_add_on_that_does_not_take_the_status_is_served_as_before() {
    let tree = ProjectTree::new("panel-old-addon");
    let mut session = StdioSession::initialized(&Spawn {
        project: &tree.root,
        current_dir: None,
        profile: "editor",
        godot: None,
    });
    let plugin = FakeEditorPlugin::spawn(
        &rendezvous(&tree),
        FakePluginScript::implementing(&["editor.state"]).answering(
            "editor.state",
            FakeAnswer::Payload(json!({ "open_scenes": [], "open_scripts": [] })),
        ),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    // Long enough for a status to have been sent; the next read's answer comes
    // after anything sent before its request.
    std::thread::sleep(Duration::from_millis(600));
    id += 1;
    let status = ok(&session.call_tool(id, "editor.status.get", json!({})));
    assert_eq!(status["methods"], json!(["editor.state"]), "{status}");
    assert!(
        sent(&plugin, SERVER_STATUS).is_empty(),
        "a plugin that does not list the method is never sent it: {:?}",
        plugin.received()
    );
    let _ = session.finish();
    drop(plugin);
}

#[test]
fn a_fresh_server_describes_each_need_once_and_blocks_nothing() {
    // The status is sampled again while the session lasts, and a plugin that
    // took one is sent the next only when what it says changed: a fresh
    // server, which nothing blocks, sends one.
    let tree = ProjectTree::new("panel-fresh");
    let mut session = StdioSession::initialized(&Spawn {
        project: &tree.root,
        current_dir: None,
        profile: "editor",
        godot: None,
    });
    let plugin = plugin_taking_the_status(&tree);
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    let started = Instant::now();
    while sent(&plugin, SERVER_STATUS).is_empty() {
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the server never described itself"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let statuses = sent(&plugin, SERVER_STATUS);
    assert_eq!(statuses.len(), 1, "{statuses:?}");
    let status = &statuses[0];
    assert_eq!(status["profile"], "editor", "{status}");
    assert_eq!(status["mutations_blocked"], Value::Null, "{status}");
    let needs: Vec<&str> = status["needs"]
        .as_array()
        .expect("needs")
        .iter()
        .filter_map(|need| need["needs"].as_str())
        .collect();
    let distinct: BTreeSet<&str> = needs.iter().copied().collect();
    assert_eq!(needs.len(), distinct.len(), "{status}");
    let _ = session.finish();
    drop(plugin);
}

#[cfg(feature = "http")]
#[tokio::test(flavor = "multi_thread")]
async fn over_http_the_status_names_the_http_transport() -> anyhow::Result<()> {
    use godot_mcp_domain::CapabilityProfile;
    use godot_mcp_domain::config::Limits;
    use godot_mcp_domain::contracts::PrincipalId;
    use godot_mcp_server::http::{HttpTransportOptions, bind_http_transport};
    use godot_mcp_server::stdio::{ProjectRuntime, StdioOptions, state_dir_for};

    let temp = tempfile::tempdir()?;
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project)?;
    std::fs::write(project.join("project.godot"), b"config_version=5\n")?;
    let runtime = ProjectRuntime::open(&StdioOptions {
        project: project.clone(),
        godot_binary: None,
        profile: CapabilityProfile::Editor,
        principal: PrincipalId::parse("principal_00112233445566778899aabbccddeeff")?,
        observed_at_unix_ms: 4_000,
        limits: Limits::default(),
        process: Default::default(),
        allow_scripts: false,
        android: Default::default(),
    })?;
    let transport = bind_http_transport(
        HttpTransportOptions::trusted_local(
            "127.0.0.1:0".parse::<std::net::SocketAddr>()?,
            &state_dir_for(&project),
        ),
        runtime.kernel(),
        runtime.access(),
    )
    .await?;
    let rendezvous = project.join(".godot-mcp").join("editor-bridge.rendezvous");
    let plugin = tokio::task::spawn_blocking(move || {
        FakeEditorPlugin::spawn(
            &rendezvous,
            FakePluginScript::implementing(&[SERVER_STATUS, "editor.state"]),
        )
    })
    .await?;
    let started = Instant::now();
    let status = loop {
        if let Some(status) = sent(&plugin, SERVER_STATUS).pop() {
            break status;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the server never described itself: {:?}",
            plugin.received()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(status["transport"], "http", "{status}");
    assert_eq!(status["profile"], "editor", "{status}");
    drop(plugin);
    transport.shutdown().await?;
    Ok(())
}
