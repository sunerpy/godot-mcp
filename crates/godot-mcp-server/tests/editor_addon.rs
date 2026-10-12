//! The bundled `godot_mcp` editor add-on as project content: install, status,
//! update and remove through MCP, with recorded ownership.

#![cfg(target_os = "linux")]

mod support;

use godot_mcp_bridge::addon::{ADDON_VERSION, FILES, PLUGIN_CONFIG};
use serde_json::{Value, json};
use support::{ProjectTree, Spawn, StdioSession, error_code};

fn open(tree: &ProjectTree) -> StdioSession {
    StdioSession::initialized(&Spawn {
        project: &tree.root,
        current_dir: None,
        profile: "full",
        godot: None,
    })
}

fn ok(response: &Value) -> Value {
    assert!(
        response.get("error").is_none() && response["result"]["isError"] != json!(true),
        "the call must succeed: {response}"
    );
    response["result"]["structuredContent"].clone()
}

fn states(status: &Value) -> Vec<(String, String)> {
    status["files"]
        .as_array()
        .expect("files")
        .iter()
        .map(|file| {
            (
                file["path"].as_str().unwrap_or_default().to_owned(),
                file["state"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

#[test]
fn the_add_on_installs_updates_and_removes_only_what_it_owns() {
    let tree = ProjectTree::new("m4-addon");
    let mut session = open(&tree);

    let before = ok(&session.call_tool(2, "editor.addon.status", json!({})));
    assert_eq!(before["installed"], json!(false), "{before}");
    assert_eq!(before["install_allowed"], json!(true), "{before}");
    assert_eq!(before["bundled_version"], ADDON_VERSION);

    let installed = ok(&session.call_tool(
        3,
        "editor.addon.install",
        json!({ "enable": true, "idempotencyId": "install-1" }),
    ));
    assert_eq!(installed["conclusion"], "proven_post_state", "{installed}");
    for file in FILES {
        let path = tree.root.join("addons/godot_mcp").join(file.name);
        assert_eq!(
            std::fs::read(&path).expect("installed"),
            file.bytes,
            "{}",
            file.name
        );
    }
    let project = std::fs::read_to_string(tree.root.join("project.godot")).expect("project");
    assert!(
        project.contains(PLUGIN_CONFIG),
        "the plugin is enabled: {project}"
    );
    assert!(
        tree.root
            .join("addons/godot_mcp/install-manifest.json")
            .is_file()
    );

    let current = ok(&session.call_tool(4, "editor.addon.status", json!({})));
    assert_eq!(current["installed"], json!(true));
    assert_eq!(current["enabled"], json!(true));
    assert_eq!(current["update_changes"], json!(false), "{current}");
    assert!(
        states(&current).iter().all(|(_, state)| state == "current"),
        "{current}"
    );

    // A second install refuses; an update with nothing to do changes nothing.
    let again = session.call_tool(
        5,
        "editor.addon.install",
        json!({ "idempotencyId": "install-2" }),
    );
    assert_eq!(error_code(&again), json!("conflict"), "{again}");
    let idle = ok(&session.call_tool(
        6,
        "editor.addon.update",
        json!({ "idempotencyId": "update-0" }),
    ));
    assert_eq!(idle["changed"], json!(false), "{idle}");

    // A file the user edited is kept beside the bundled one on update.
    let plugin_gd = tree.root.join("addons/godot_mcp/plugin.gd");
    std::fs::write(&plugin_gd, b"# edited by hand\n").expect("edit");
    // Godot writes a UID file beside every script it indexes.
    std::fs::write(
        tree.root.join("addons/godot_mcp/plugin.gd.uid"),
        b"uid://bxyzexample\n",
    )
    .expect("uid file");
    let edited = ok(&session.call_tool(7, "editor.addon.status", json!({})));
    assert!(
        states(&edited)
            .iter()
            .any(|(path, state)| path.ends_with("/plugin.gd") && state == "edited"),
        "{edited}"
    );
    let updated = ok(&session.call_tool(
        8,
        "editor.addon.update",
        json!({ "idempotencyId": "update-1" }),
    ));
    assert_eq!(updated["changed"], json!(true), "{updated}");
    assert_eq!(
        std::fs::read(
            tree.root
                .join("addons/godot_mcp/plugin.gd.godot-mcp-backup")
        )
        .expect("backup"),
        b"# edited by hand\n"
    );
    let bundled_plugin = FILES
        .iter()
        .find(|file| file.name == "plugin.gd")
        .expect("plugin.gd");
    assert_eq!(
        std::fs::read(&plugin_gd).expect("replaced"),
        bundled_plugin.bytes
    );

    // A file the server never wrote blocks an install elsewhere.
    let other = ProjectTree::new("m4-addon-foreign");
    other.write(
        "addons/godot_mcp/plugin.cfg",
        "[plugin]\nname=\"someone else\"\n",
    );
    let mut foreign_session = open(&other);
    let foreign = foreign_session.call_tool(
        2,
        "editor.addon.install",
        json!({ "idempotencyId": "install-foreign" }),
    );
    assert_eq!(error_code(&foreign), json!("conflict"), "{foreign}");
    assert_eq!(
        std::fs::read_to_string(other.root.join("addons/godot_mcp/plugin.cfg")).expect("kept"),
        "[plugin]\nname=\"someone else\"\n"
    );
    assert!(!other.root.join("addons/godot_mcp/plugin.gd").exists());
    let _ = foreign_session.finish();

    // Remove deletes the owned files and their UID files, keeps the backup it
    // never owned, and disables the plugin.
    let removed = ok(&session.call_tool(
        9,
        "editor.addon.remove",
        json!({ "idempotencyId": "remove-1" }),
    ));
    assert_eq!(removed["conclusion"], "proven_post_state", "{removed}");
    for file in FILES {
        assert!(
            !tree.root.join("addons/godot_mcp").join(file.name).exists(),
            "{} is removed",
            file.name
        );
    }
    assert!(!tree.root.join("addons/godot_mcp/plugin.gd.uid").exists());
    assert!(
        !tree
            .root
            .join("addons/godot_mcp/install-manifest.json")
            .exists()
    );
    assert!(
        tree.root
            .join("addons/godot_mcp/plugin.gd.godot-mcp-backup")
            .exists()
    );
    let project = std::fs::read_to_string(tree.root.join("project.godot")).expect("project");
    assert!(
        !project.contains(PLUGIN_CONFIG),
        "the plugin is disabled: {project}"
    );
    let after = ok(&session.call_tool(10, "editor.addon.status", json!({})));
    assert_eq!(after["installed"], json!(false), "{after}");
    let _ = session.finish();
}
