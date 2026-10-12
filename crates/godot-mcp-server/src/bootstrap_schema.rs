//! The input schemas of the bootstrap tools.
//!
//! They belong to every build's catalog, whichever transports it carries; the
//! bootstrap state that serves them exists only with stdio.

use godot_mcp_domain::catalog::BootstrapOperation;
use serde_json::{Map, Value, json};

/// Longest accepted application name.
pub(crate) const MAX_NAME_BYTES: usize = 256;

/// The strict input schema of one bootstrap tool.
pub(crate) fn input_schema(operation: BootstrapOperation) -> Map<String, Value> {
    let schema = match operation {
        BootstrapOperation::ProjectsList => json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {},
            "required": [],
        }),
        BootstrapOperation::ProjectCreate => json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "path": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 1024,
                    "description": "Workspace-relative directory for the project; absent or empty."
                },
                "name": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": MAX_NAME_BYTES,
                    "description": "The application name, config/name."
                },
                "renderer": {
                    "type": "string",
                    "enum": ["forward_plus", "mobile", "gl_compatibility"],
                    "description": "The rendering method (default forward_plus)."
                }
            },
            "required": ["path", "name", "idempotencyId"],
        }),
        BootstrapOperation::ProjectBind => json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "path": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 1024,
                    "description": "Workspace-relative project directory; `.` is the workspace itself."
                }
            },
            "required": ["path"],
        }),
    };
    schema
        .as_object()
        .cloned()
        .expect("bootstrap schemas are objects")
}
