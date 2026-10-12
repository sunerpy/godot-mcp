//! Architecture-boundary gate for Todo 1.
//!
//! Proves the domain/analysis/engine crates never import a transport/UI layer
//! crate (`godot-mcp-server`, `godot-mcp-cli`). This test lives in the lowest
//! crate and is intentionally self-contained (it may not depend on
//! `godot-mcp-cli`, which would itself be a boundary violation): it reads the
//! sibling member manifests as text and asserts none of the lower-layer crates
//! declares a forbidden dependency.

use std::path::{Path, PathBuf};

const LOWER_LAYER: &[&str] = &["godot-mcp-domain", "godot-mcp-analysis", "godot-mcp-engine"];
const TRANSPORT_UI: &[&str] = &["godot-mcp-server", "godot-mcp-cli"];

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = <root>/crates/godot-mcp-domain
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root above crates/godot-mcp-domain")
        .to_path_buf()
}

/// Return the dependency-declaration lines from a manifest's dependency tables
/// only (ignores commented lines and doc prose).
fn dependency_names(manifest: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut in_dep_table = false;
    for raw in manifest.lines() {
        let line = raw.trim();
        if line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            in_dep_table = line.contains("dependencies");
            continue;
        }
        if in_dep_table && let Some((key, _)) = line.split_once('=') {
            let key = key.trim().trim_matches('"');
            if !key.is_empty() {
                names.push(key.to_string());
            }
        }
    }
    names
}

#[test]
fn no_transport_ui_imports() {
    let root = workspace_root();
    let mut violations = Vec::new();
    for member in LOWER_LAYER {
        let manifest_path = root.join("crates").join(member).join("Cargo.toml");
        let text = std::fs::read_to_string(&manifest_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", manifest_path.display()));
        let deps = dependency_names(&text);
        for ui in TRANSPORT_UI {
            if deps.iter().any(|d| d == ui) {
                violations.push(format!("{member} -> {ui}"));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "forbidden lower-layer -> transport/UI dependency edges: {violations:?}"
    );
}
