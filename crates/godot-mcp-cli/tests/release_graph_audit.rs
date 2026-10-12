//! Release-graph audit gate for Todo 1.
//!
//! Proves no shipping crate pulls in the native Godot bridge in the committed
//! workspace, and that each distinct forbidden shipping artifact — production
//! crate `godot = 0.5.4`, a shipping `gdext-bridge` crate, a shipping
//! `.gdextension` descriptor, and a shipping native bridge library inside the
//! addon tree — is reproduced by its own dedicated corrupt fixture and rejected
//! by name. Also proves the native-library scan is scoped to addon/bridge
//! shipping content and does not flag arbitrary ordinary dependency libraries.

use std::path::{Path, PathBuf};

use godot_mcp_cli::audit::audit_release_graph;
use godot_mcp_cli::corrupt::{self, CorruptFixture};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root above crates/godot-mcp-cli")
        .to_path_buf()
}

fn assert_exact_finding(
    found: &[godot_mcp_cli::audit::ForbiddenArtifact],
    artifact: &str,
    location: &str,
) {
    assert_eq!(
        found,
        &[godot_mcp_cli::audit::ForbiddenArtifact {
            artifact: artifact.to_string(),
            location: location.to_string(),
        }],
        "fixture must report exactly artifact {artifact:?} at {location:?}"
    );
}

#[test]
fn no_shipping_gdext_artifact() {
    let root = workspace_root();
    let found = audit_release_graph(&root).expect("release-graph audit must run");
    assert!(
        found.is_empty(),
        "forbidden native-bridge artifacts in shipping graph: {found:?}"
    );
}

#[test]
fn corrupted_shipping_godot_fixture_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let root = corrupt::build(CorruptFixture::ShippingGodot, tmp.path()).unwrap();
    let found = audit_release_graph(&root).expect("audit runs on fixture");
    assert_exact_finding(&found, "godot", "crates/godot-mcp-engine/Cargo.toml");
}

#[test]
fn corrupted_shipping_gdext_bridge_fixture_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let root = corrupt::build(CorruptFixture::ShippingGdextBridge, tmp.path()).unwrap();
    let found = audit_release_graph(&root).expect("audit runs on fixture");
    assert_exact_finding(&found, "gdext-bridge", "crates/godot-mcp-bridge/Cargo.toml");
}

#[test]
fn corrupted_shipping_gdextension_descriptor_fixture_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let root = corrupt::build(CorruptFixture::ShippingGdextension, tmp.path()).unwrap();
    let found = audit_release_graph(&root).expect("audit runs on fixture");
    assert_exact_finding(
        &found,
        ".gdextension",
        "addons/godot_mcp/godot_mcp.gdextension",
    );
}

#[test]
fn corrupted_shipping_native_bridge_library_fixture_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let root = corrupt::build(CorruptFixture::ShippingNativeBridgeLib, tmp.path()).unwrap();
    let found = audit_release_graph(&root).expect("audit runs on fixture");
    assert_exact_finding(
        &found,
        "native-bridge-library",
        "addons/godot_mcp/bin/libgodot_mcp_bridge.so",
    );
}

#[test]
fn ordinary_dependency_library_outside_addon_tree_is_not_flagged() {
    // A native library that is NOT shipping addon/bridge content (e.g. an
    // ordinary build artifact or a vendored dependency lib somewhere in the
    // tree) must never be treated as a forbidden native bridge library.
    let tmp = tempfile::tempdir().unwrap();
    let root = corrupt::build(CorruptFixture::OrdinaryDependencyLibrary, tmp.path()).unwrap();
    let found = audit_release_graph(&root).expect("audit runs on fixture");
    assert!(
        found.is_empty(),
        "an ordinary .so/.dylib/.dll outside the shipping addon/bridge tree must not be flagged, got {found:?}"
    );
}
