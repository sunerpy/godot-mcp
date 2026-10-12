//! The pinned headless GDScript codec harness.
//!
//! # Why the other half of the protocol is committed GDScript
//!
//! The bridge's byte-stability guarantee is a claim about **two** JSON
//! implementations: `serde_json` here, and Godot's built-in `JSON` there. A golden
//! vector that only Rust ever renders proves nothing about the pairing. So the
//! GDScript half is committed in-tree at
//! `crates/godot-mcp-bridge/scripts/bridge/bridge_codec.gd`, embedded here at
//! compile time, and pinned by SHA-256 in [`GDSCRIPT_HARNESS_SHA256`]. A unit test
//! asserts the digest, so editing, substituting or truncating the harness is a
//! **test failure** rather than a silent divergence. This is the same discipline
//! Todo 17 applies to the canonical-write helper.
//!
//! The harness embeds, for each of the ten kinds, both the message rebuilt field by
//! field as a `Dictionary` **and** the exact canonical text the Rust side emits. It
//! asserts they agree in both directions. A GDScript `Dictionary` preserves
//! insertion order, and `JSON.stringify` with default arguments emits no indentation
//! and no space after `:` or `,`. The codec additionally extracts the five declared
//! u64 paths from the original JSON text and reinserts their validated decimal
//! strings during encoding, without routing them through Godot's `TYPE_FLOAT` or
//! signed `TYPE_INT`. Inserting keys in sorted order therefore reproduces
//! `serde_json`'s `BTreeMap` ordering while the textual integer path preserves every
//! value through `u64::MAX` byte for byte.
//!
//! # Runtime and offline verification
//!
//! The integration-only runtime support under `tests/support/` locates an
//! operator-configured Godot through `GODOT_MCP_GODOT_BINARY`, then falls back to
//! the engine crate's deterministic platform discovery. Keeping process creation
//! in the test tree preserves this production module's role as committed-byte
//! protocol data rather than a second engine-launch path.
//!
//! The remaining tests verify everything decidable from committed bytes:
//!
//! * the embedded harness matches its pinned digest;
//! * every golden vector's exact canonical text appears verbatim inside the
//!   harness, so the two halves cannot drift apart without a Rust test failing;
//! * the harness names all ten kinds, and no eleventh;
//! * the harness is `@tool`-safe and side-effect free: it opens no socket, touches
//!   no editor API, reads no file and needs no scene, which is asserted by scanning
//!   its source for those constructs.
//!
//! The runtime test is deliberately optional because contributors and generic CI
//! runners need not install an engine. Release qualification can make it execute by
//! setting `GODOT_MCP_GODOT_BINARY` to one of Todo 13's digest-verified builds.

use super::digest::hex_encode;

/// The harness's bytes, embedded from the committed file.
pub const GDSCRIPT_HARNESS_SOURCE: &str = include_str!("../../scripts/bridge/bridge_codec.gd");

/// The pinned SHA-256 of [`GDSCRIPT_HARNESS_SOURCE`].
///
/// Asserted by [`tests::the_embedded_harness_matches_its_pinned_digest`]. Editing
/// the harness without updating this constant fails that test.
pub const GDSCRIPT_HARNESS_SHA256: &str =
    "6fbaa96e1f8de81d5b1ac4f900aa25a83ea731d666a73e89ecf26a9e046d629d";

/// Where the harness installs inside a project, if a project wants to run it:
/// under the installer's `godot_mcp/` root, not an `addons/` tree, because no
/// editor plug-in ships.
pub const GDSCRIPT_HARNESS_RES_PATH: &str = "res://godot_mcp/bridge/bridge_codec.gd";

/// The command that runs the harness on a real Godot build.
pub const GDSCRIPT_HARNESS_COMMAND: &str = "godot --headless --script";

/// The harness's source text.
#[must_use]
pub const fn gdscript_harness_source() -> &'static str {
    GDSCRIPT_HARNESS_SOURCE
}

/// The SHA-256 of the embedded harness, recomputed from its bytes.
#[must_use]
pub fn harness_digest() -> String {
    hex_encode(&sha256(GDSCRIPT_HARNESS_SOURCE.as_bytes()))
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest as _, Sha256};
    let digest = Sha256::digest(bytes);
    let mut out = [0_u8; 32];
    out.copy_from_slice(&digest);
    out
}

#[cfg(test)]
mod tests {
    use super::super::envelope::MessageKind;
    use super::{
        GDSCRIPT_HARNESS_COMMAND, GDSCRIPT_HARNESS_RES_PATH, GDSCRIPT_HARNESS_SHA256,
        GDSCRIPT_HARNESS_SOURCE, gdscript_harness_source, harness_digest,
    };

    #[test]
    fn the_embedded_harness_matches_its_pinned_digest() {
        assert_eq!(
            harness_digest(),
            GDSCRIPT_HARNESS_SHA256,
            "the committed GDScript harness was edited without repinning its digest"
        );
        assert_eq!(GDSCRIPT_HARNESS_SHA256.len(), 64);
        assert!(
            GDSCRIPT_HARNESS_SHA256
                .chars()
                .all(|character| character.is_ascii_hexdigit() && !character.is_ascii_uppercase())
        );
    }

    #[test]
    fn the_harness_is_a_headless_script_entrypoint() {
        assert!(
            gdscript_harness_source().starts_with("@tool\nextends SceneTree\n"),
            "Godot --script requires a SceneTree or MainLoop entrypoint"
        );
    }

    #[test]
    fn the_harness_names_all_ten_kinds_and_no_eleventh() {
        let source = gdscript_harness_source();
        for kind in MessageKind::ALL {
            assert!(
                source.contains(&format!("\"{}\"", kind.as_str())),
                "the harness names {}",
                kind.as_str()
            );
        }
        let listed = source
            .split_once("const VECTOR_NAMES := [")
            .and_then(|(_before, after)| after.split_once(']'))
            .map(|(inside, _after)| inside.matches('"').count() / 2)
            .expect("the harness declares VECTOR_NAMES");
        assert_eq!(listed, MessageKind::ALL.len());
    }

    /// The harness must be safe to load inside a live editor: no socket, no
    /// editor API, no filesystem, no scene tree.
    #[test]
    fn the_harness_is_tool_safe_and_side_effect_free() {
        let source = gdscript_harness_source();
        assert!(source.starts_with("@tool\n"), "the harness is @tool");
        // Assembled at runtime so this test's own source cannot satisfy the search.
        let forbidden = [
            format!("WebSocket{}Peer", ""),
            format!("TCP{}Server", ""),
            format!("File{}Access", ""),
            format!("Dir{}Access", ""),
            format!("Editor{}Interface", ""),
            format!("Editor{}Plugin", ""),
            format!("OS.{}execute", ""),
            format!("get_{}tree()", ""),
            format!("call_{}deferred", ""),
        ];
        for spelling in forbidden {
            assert!(
                !source.contains(&spelling),
                "the harness must not reference {spelling}"
            );
        }
    }

    /// The harness is the plugin's half of the *bridge* protocol. No MCP wire
    /// vocabulary may appear in it.
    #[test]
    fn the_harness_speaks_no_mcp_wire_type() {
        let source = gdscript_harness_source();
        let forbidden = [
            format!("json{}rpc", ""),
            format!("tools/{}call", ""),
            format!("tools/{}list", ""),
        ];
        for spelling in forbidden {
            assert!(
                !source.contains(&spelling),
                "the harness must not reference the MCP wire type {spelling}"
            );
        }
    }

    #[test]
    fn the_harness_pins_the_same_frame_limit_the_rust_side_does() {
        let source = gdscript_harness_source();
        assert!(
            source.contains("4 * 1024 * 1024"),
            "the harness names the same 4 MiB frame limit"
        );
        assert!(
            source.contains("16 * 1024 * 1024"),
            "the harness rejects a 16 MiB message explicitly"
        );
    }

    #[test]
    fn the_harness_pins_the_same_protocol_and_schema_versions() {
        let source = gdscript_harness_source();
        assert!(source.contains(&format!(
            "const PROTOCOL_VERSION := \"{}\"",
            super::super::compat::PROTOCOL_VERSION
        )));
        assert!(source.contains(&format!(
            "const SCHEMA_VERSION := \"{}\"",
            super::super::envelope::ENVELOPE_SCHEMA_VERSION
        )));
    }

    #[test]
    fn the_documented_run_path_and_command_are_stable() {
        assert_eq!(
            GDSCRIPT_HARNESS_RES_PATH,
            "res://godot_mcp/bridge/bridge_codec.gd"
        );
        assert_eq!(GDSCRIPT_HARNESS_COMMAND, "godot --headless --script");
        assert!(
            GDSCRIPT_HARNESS_SOURCE.contains(GDSCRIPT_HARNESS_RES_PATH),
            "the harness documents its own install path"
        );
    }
}
