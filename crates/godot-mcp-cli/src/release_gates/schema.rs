//! The canonical release-gates output contract.
//!
//! `schemas/gates/release-gates.schema.json` is embedded at compile time so the
//! harness validates against the exact committed bytes rather than whatever
//! happens to be on disk at run time. Validation goes through
//! [`godot_mcp_domain::jsonschema`], the workspace's deterministic restrictive
//! subset validator: an unsupported construct is a validator error, never a
//! silent pass.

use std::path::{Path, PathBuf};

use serde_json::Value;

/// The committed canonical schema, embedded verbatim.
pub const RELEASE_GATES_SCHEMA: &str =
    include_str!("../../../../schemas/gates/release-gates.schema.json");

/// Workspace-relative path of the canonical schema.
pub const RELEASE_GATES_SCHEMA_PATH: &str = "schemas/gates/release-gates.schema.json";

/// `schema_version` every document produced by this harness carries.
pub const SCHEMA_VERSION: &str = "1";

/// The schema path relative to `workspace_root`.
#[must_use]
pub fn schema_path_in(workspace_root: &Path) -> PathBuf {
    workspace_root.join(RELEASE_GATES_SCHEMA_PATH)
}

/// Parse the embedded schema.
///
/// # Errors
/// Returns an error if the embedded schema is not valid JSON.
pub fn schema_value() -> Result<Value, String> {
    serde_json::from_str(RELEASE_GATES_SCHEMA)
        .map_err(|error| format!("parse {RELEASE_GATES_SCHEMA_PATH}: {error}"))
}

/// Validate `document` against the embedded canonical schema.
///
/// # Errors
/// Returns an error describing the first rejection, or an unsupported schema
/// construct. Either way the caller must treat the document as unusable.
pub fn validate(document: &Value) -> Result<(), String> {
    let schema = schema_value()?;
    godot_mcp_domain::jsonschema::validate(&schema, document).map_err(|error| {
        format!("release-gates document rejected by its canonical schema: {error}")
    })
}

/// Prove the on-disk schema still matches the embedded bytes.
///
/// A drift here means a gate would validate against a schema nobody reviewed.
///
/// # Errors
/// Returns an error if the file cannot be read or its bytes differ.
pub fn assert_embedded_matches_disk(workspace_root: &Path) -> Result<(), String> {
    let path = schema_path_in(workspace_root);
    let on_disk = std::fs::read_to_string(&path)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    if on_disk == RELEASE_GATES_SCHEMA {
        Ok(())
    } else {
        Err(format!(
            "{} differs from the schema embedded in this binary",
            path.display()
        ))
    }
}
