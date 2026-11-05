//! The bundled `godot_mcp` editor add-on.
//!
//! These are the exact bytes the server installs into a project's
//! `addons/godot_mcp/`: the GDScript editor plugin that dials the bridge, its
//! main-thread dispatcher, live reads, undoable scene batches, the tagged
//! value codec, the class catalogue, and the dock and "Godot MCP" bottom
//! panel that show the connection and let the user pause agent edits.
//! `bridge_codec.gd` is the frozen protocol codec, byte for byte.

/// The project directory the add-on lives in.
pub const ADDON_DIRECTORY: &str = "addons/godot_mcp";

/// The plugin's configuration file, as project settings name it.
pub const PLUGIN_CONFIG: &str = "res://addons/godot_mcp/plugin.cfg";

/// The add-on version, as `plugin.cfg` and the plugin itself report it.
pub const ADDON_VERSION: &str = "0.4.0";

/// One add-on file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddonFile {
    /// File name inside [`ADDON_DIRECTORY`].
    pub name: &'static str,
    /// Exact bytes.
    pub bytes: &'static [u8],
}

/// Every add-on file, by name.
pub const FILES: &[AddonFile] = &[
    AddonFile {
        name: "bridge_client.gd",
        bytes: include_bytes!("../addon/godot_mcp/bridge_client.gd"),
    },
    AddonFile {
        name: "bridge_codec.gd",
        bytes: include_bytes!("../addon/godot_mcp/bridge_codec.gd"),
    },
    AddonFile {
        name: "capture.gd",
        bytes: include_bytes!("../addon/godot_mcp/capture.gd"),
    },
    AddonFile {
        name: "command_queue.gd",
        bytes: include_bytes!("../addon/godot_mcp/command_queue.gd"),
    },
    AddonFile {
        name: "dialog_press.gd",
        bytes: include_bytes!("../addon/godot_mcp/dialog_press.gd"),
    },
    AddonFile {
        name: "editing_methods.gd",
        bytes: include_bytes!("../addon/godot_mcp/editing_methods.gd"),
    },
    AddonFile {
        name: "editor_camera.gd",
        bytes: include_bytes!("../addon/godot_mcp/editor_camera.gd"),
    },
    AddonFile {
        name: "editor_config.gd",
        bytes: include_bytes!("../addon/godot_mcp/editor_config.gd"),
    },
    AddonFile {
        name: "editor_dialogs.gd",
        bytes: include_bytes!("../addon/godot_mcp/editor_dialogs.gd"),
    },
    AddonFile {
        name: "editor_handlers.gd",
        bytes: include_bytes!("../addon/godot_mcp/editor_handlers.gd"),
    },
    AddonFile {
        name: "editor_log.gd",
        bytes: include_bytes!("../addon/godot_mcp/editor_log.gd"),
    },
    AddonFile {
        name: "editor_monitors.gd",
        bytes: include_bytes!("../addon/godot_mcp/editor_monitors.gd"),
    },
    AddonFile {
        name: "editor_restart.gd",
        bytes: include_bytes!("../addon/godot_mcp/editor_restart.gd"),
    },
    AddonFile {
        name: "filesystem.gd",
        bytes: include_bytes!("../addon/godot_mcp/filesystem.gd"),
    },
    AddonFile {
        name: "mutation_handlers.gd",
        bytes: include_bytes!("../addon/godot_mcp/mutation_handlers.gd"),
    },
    AddonFile {
        name: "node_reader.gd",
        bytes: include_bytes!("../addon/godot_mcp/node_reader.gd"),
    },
    AddonFile {
        name: "panel.gd",
        bytes: include_bytes!("../addon/godot_mcp/panel.gd"),
    },
    AddonFile {
        name: "panel_state.gd",
        bytes: include_bytes!("../addon/godot_mcp/panel_state.gd"),
    },
    AddonFile {
        name: "play.gd",
        bytes: include_bytes!("../addon/godot_mcp/play.gd"),
    },
    AddonFile {
        name: "plugin.cfg",
        bytes: include_bytes!("../addon/godot_mcp/plugin.cfg"),
    },
    AddonFile {
        name: "plugin.gd",
        bytes: include_bytes!("../addon/godot_mcp/plugin.gd"),
    },
    AddonFile {
        name: "preferences.gd",
        bytes: include_bytes!("../addon/godot_mcp/preferences.gd"),
    },
    AddonFile {
        name: "project_settings_apply.gd",
        bytes: include_bytes!("../addon/godot_mcp/project_settings_apply.gd"),
    },
    AddonFile {
        name: "reloader.gd",
        bytes: include_bytes!("../addon/godot_mcp/reloader.gd"),
    },
    AddonFile {
        name: "resource_preview.gd",
        bytes: include_bytes!("../addon/godot_mcp/resource_preview.gd"),
    },
    AddonFile {
        name: "restarter.gd",
        bytes: include_bytes!("../addon/godot_mcp/restarter.gd"),
    },
    AddonFile {
        name: "scene_reader.gd",
        bytes: include_bytes!("../addon/godot_mcp/scene_reader.gd"),
    },
    AddonFile {
        name: "script_runner.gd",
        bytes: include_bytes!("../addon/godot_mcp/script_runner.gd"),
    },
    AddonFile {
        name: "script_tabs.gd",
        bytes: include_bytes!("../addon/godot_mcp/script_tabs.gd"),
    },
    AddonFile {
        name: "shortcuts.gd",
        bytes: include_bytes!("../addon/godot_mcp/shortcuts.gd"),
    },
    AddonFile {
        name: "status_dock.gd",
        bytes: include_bytes!("../addon/godot_mcp/status_dock.gd"),
    },
    AddonFile {
        name: "type_catalog.gd",
        bytes: include_bytes!("../addon/godot_mcp/type_catalog.gd"),
    },
    AddonFile {
        name: "variant_codec.gd",
        bytes: include_bytes!("../addon/godot_mcp/variant_codec.gd"),
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plugin_carries_the_editing_methods_table_verbatim() {
        let shipped = FILES
            .iter()
            .find(|file| file.name == "editing_methods.gd")
            .expect("editing_methods.gd ships");
        let text = std::str::from_utf8(shipped.bytes).expect("GDScript is UTF-8");
        assert!(
            text.contains(&godot_mcp_domain::editing_methods::gdscript_table()),
            "editing_methods.gd must carry the table godot-mcp-domain renders, unchanged"
        );
    }

    #[test]
    fn the_plugin_carries_the_reading_methods_table_verbatim() {
        let shipped = FILES
            .iter()
            .find(|file| file.name == "scene_reader.gd")
            .expect("scene_reader.gd ships");
        let text = std::str::from_utf8(shipped.bytes).expect("GDScript is UTF-8");
        assert!(
            text.contains(&godot_mcp_domain::reading_methods::gdscript_table()),
            "scene_reader.gd must carry the table godot-mcp-domain renders, unchanged"
        );
    }

    #[test]
    fn the_plugin_carries_the_protected_settings_verbatim() {
        let shipped = FILES
            .iter()
            .find(|file| file.name == "preferences.gd")
            .expect("preferences.gd ships");
        let text = std::str::from_utf8(shipped.bytes).expect("GDScript is UTF-8");
        assert!(
            text.contains(&godot_mcp_domain::editor_preferences::gdscript_table()),
            "preferences.gd must carry the lists godot-mcp-domain renders, unchanged"
        );
    }

    #[test]
    fn the_plugin_holds_shortcut_changes_to_the_server_bounds() {
        let shipped = FILES
            .iter()
            .find(|file| file.name == "shortcuts.gd")
            .expect("shortcuts.gd ships");
        let text = std::str::from_utf8(shipped.bytes).expect("GDScript is UTF-8");
        assert!(
            text.contains(&godot_mcp_domain::editor_shortcuts::gdscript_bounds()),
            "shortcuts.gd must carry the bounds godot-mcp-domain renders, unchanged"
        );
    }

    #[test]
    fn every_add_on_file_ships_and_nothing_else_does() {
        let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("addon/godot_mcp");
        let mut on_disk: Vec<String> = std::fs::read_dir(&directory)
            .expect("addon directory")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        on_disk.sort();
        let shipped: Vec<&str> = FILES.iter().map(|file| file.name).collect();
        assert_eq!(on_disk, shipped);
        for file in FILES {
            let bytes = std::fs::read(directory.join(file.name)).expect("read");
            assert_eq!(bytes, file.bytes, "{} is embedded verbatim", file.name);
        }
    }

    #[test]
    fn the_add_on_codec_is_the_frozen_protocol_codec() {
        let codec = FILES
            .iter()
            .find(|file| file.name == "bridge_codec.gd")
            .expect("the codec ships");
        assert_eq!(
            codec.bytes,
            include_bytes!("../scripts/bridge/bridge_codec.gd"),
            "the add-on speaks the pinned codec"
        );
    }

    #[test]
    fn every_version_spelling_agrees() {
        let text = |name: &str| {
            String::from_utf8(
                FILES
                    .iter()
                    .find(|file| file.name == name)
                    .expect("file")
                    .bytes
                    .to_vec(),
            )
            .expect("utf-8")
        };
        assert!(text("plugin.cfg").contains(&format!("version=\"{ADDON_VERSION}\"")));
        assert!(
            text("editor_handlers.gd").contains(&format!("PLUGIN_VERSION := \"{ADDON_VERSION}\""))
        );
        assert!(text("bridge_client.gd").contains(&format!(
            "PROTOCOL_VERSION := \"{}\"",
            crate::protocol::compat::PROTOCOL_VERSION
        )));
        assert!(text("bridge_client.gd").contains(&format!(
            "SUBPROTOCOL := \"{}\"",
            crate::protocol::ws::SUBPROTOCOL
        )));
    }
}
