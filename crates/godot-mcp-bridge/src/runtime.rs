//! The runtime probe of a managed game session.
//!
//! These are the exact bytes the server writes into a session's private
//! directory before it starts the game with `--script <dir>/runtime_launcher.gd`:
//! the launcher, the probe with its value rules (`runtime_values.gd`) and its
//! recorded input events (`runtime_events.gd`), and the add-on's own bridge
//! client, dispatcher, codec, tagged value codec, log capture and script
//! runner, which the probe shares with the editor plugin. Nothing is written
//! into the project.

use crate::addon::AddonFile;

/// The script Godot runs instead of the project's main scene.
pub const LAUNCHER: &str = "runtime_launcher.gd";

/// The environment variable naming the session's private rendezvous file.
pub const RENDEZVOUS_ENV: &str = "GODOT_MCP_RUNTIME_RENDEZVOUS";

/// The environment variable naming the scene to run instead of the main scene.
pub const SCENE_ENV: &str = "GODOT_MCP_RUNTIME_SCENE";

/// The probe version, as it reports itself in every envelope.
pub const PROBE_VERSION: &str = "0.4.0";

/// The bridge methods the probe implements, as the server may send them.
pub const METHODS: &[&str] = &[
    "runtime.capture.chunk",
    "runtime.capture.recording",
    "runtime.capture.screenshot",
    "runtime.image.compare",
    "runtime.image.reference",
    "runtime.input.record.start",
    "runtime.input.record.stop",
    "runtime.input.release",
    "runtime.input.replay",
    "runtime.input.send",
    "runtime.input.sequence",
    "runtime.input.state",
    "runtime.navigation.path",
    "runtime.node.move",
    "runtime.nodes.find",
    "runtime.performance",
    "runtime.properties",
    "runtime.properties.batch",
    "runtime.properties.set",
    "runtime.sample",
    "runtime.scenario.check",
    "runtime.scenario.stress",
    "runtime.session.stop",
    "runtime.signals",
    "runtime.tree",
    "runtime.ui",
    "runtime.ui.press",
    "runtime.wait",
];

/// The method that runs a caller's script in the game (ADR 0012).
pub const SCRIPT_RUN_METHOD: &str = "runtime.script.run";

/// [`METHODS`] and [`SCRIPT_RUN_METHOD`]: what a server whose operator turned
/// script execution on may send.
pub const METHODS_WITH_SCRIPTS: &[&str] = &[
    "runtime.capture.chunk",
    "runtime.capture.recording",
    "runtime.capture.screenshot",
    "runtime.image.compare",
    "runtime.image.reference",
    "runtime.input.record.start",
    "runtime.input.record.stop",
    "runtime.input.release",
    "runtime.input.replay",
    "runtime.input.send",
    "runtime.input.sequence",
    "runtime.input.state",
    "runtime.navigation.path",
    "runtime.node.move",
    "runtime.nodes.find",
    "runtime.performance",
    "runtime.properties",
    "runtime.properties.batch",
    "runtime.properties.set",
    "runtime.sample",
    "runtime.scenario.check",
    "runtime.scenario.stress",
    SCRIPT_RUN_METHOD,
    "runtime.session.stop",
    "runtime.signals",
    "runtime.tree",
    "runtime.ui",
    "runtime.ui.press",
    "runtime.wait",
];

/// Every file of the probe, by name.
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
        name: "command_queue.gd",
        bytes: include_bytes!("../addon/godot_mcp/command_queue.gd"),
    },
    AddonFile {
        name: "editor_log.gd",
        bytes: include_bytes!("../addon/godot_mcp/editor_log.gd"),
    },
    AddonFile {
        name: "runtime_events.gd",
        bytes: include_bytes!("../runtime/runtime_events.gd"),
    },
    AddonFile {
        name: "runtime_launcher.gd",
        bytes: include_bytes!("../runtime/runtime_launcher.gd"),
    },
    AddonFile {
        name: "runtime_probe.gd",
        bytes: include_bytes!("../runtime/runtime_probe.gd"),
    },
    AddonFile {
        name: "runtime_values.gd",
        bytes: include_bytes!("../runtime/runtime_values.gd"),
    },
    AddonFile {
        name: "script_runner.gd",
        bytes: include_bytes!("../addon/godot_mcp/script_runner.gd"),
    },
    AddonFile {
        name: "variant_codec.gd",
        bytes: include_bytes!("../addon/godot_mcp/variant_codec.gd"),
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    fn text(name: &str) -> String {
        String::from_utf8(
            FILES
                .iter()
                .find(|file| file.name == name)
                .unwrap_or_else(|| panic!("{name} ships"))
                .bytes
                .to_vec(),
        )
        .expect("utf-8")
    }

    #[test]
    fn the_probe_ships_its_own_files_and_the_add_ons_shared_ones_verbatim() {
        let runtime = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("runtime");
        let mut own: Vec<String> = std::fs::read_dir(&runtime)
            .expect("runtime directory")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        own.sort();
        assert_eq!(
            own,
            [
                "runtime_events.gd",
                "runtime_launcher.gd",
                "runtime_probe.gd",
                "runtime_values.gd"
            ]
        );
        for file in FILES {
            let shared = crate::addon::FILES
                .iter()
                .find(|addon| addon.name == file.name);
            match shared {
                Some(addon) => assert_eq!(addon.bytes, file.bytes, "{} is the add-on's", file.name),
                None => assert_eq!(
                    std::fs::read(runtime.join(file.name)).expect("read"),
                    file.bytes,
                    "{} is embedded verbatim",
                    file.name
                ),
            }
        }
    }

    #[test]
    fn the_probe_and_the_server_agree_on_names_and_methods() {
        let probe = text("runtime_probe.gd");
        assert!(probe.contains(&format!("RENDEZVOUS_ENV := \"{RENDEZVOUS_ENV}\"")));
        assert!(probe.contains(&format!("PROBE_VERSION := \"{PROBE_VERSION}\"")));
        assert!(text(LAUNCHER).contains(&format!("SCENE_ENV := \"{SCENE_ENV}\"")));
        for method in METHODS_WITH_SCRIPTS {
            assert!(
                probe.contains(&format!("\"{method}\"")),
                "the probe implements {method}"
            );
        }
        let declared = probe.matches("\n\t\"runtime.").count();
        assert_eq!(
            declared,
            METHODS_WITH_SCRIPTS.len(),
            "the probe declares no method the server omits"
        );
    }

    #[test]
    fn only_a_server_allowing_scripts_may_send_the_script_method() {
        let mut without: Vec<&str> = METHODS_WITH_SCRIPTS
            .iter()
            .copied()
            .filter(|method| *method != SCRIPT_RUN_METHOD)
            .collect();
        without.sort_unstable();
        assert_eq!(without, METHODS);
        assert!(!METHODS.contains(&SCRIPT_RUN_METHOD));
        let mut sorted = METHODS_WITH_SCRIPTS.to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted, METHODS_WITH_SCRIPTS, "the list is sorted");
    }
}
