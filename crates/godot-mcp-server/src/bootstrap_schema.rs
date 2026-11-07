//! The input schemas of the bootstrap tools.
//!
//! They belong to every build's catalog, whichever transports it carries; the
//! bootstrap state that serves them exists only with stdio.

use godot_mcp_domain::catalog::BootstrapOperation;
use serde_json::{Map, Value, json};

/// Longest accepted application name, in characters: Unicode scalar values,
/// as a schema's `maxLength` counts them.
pub(crate) const MAX_NAME_CHARS: usize = 256;

/// An application name: no control character, and at least one character
/// that is not white space as [`char::is_whitespace`] reads it, so that the
/// name is not empty once trimmed.
pub(crate) const NAME_PATTERN: &str = r"^[^\x00-\x1F\x7F-\x9F]*[^\x00-\x20\x7F-\xA0\u1680\u2000-\u200A\u2028\u2029\u202F\u205F\u3000][^\x00-\x1F\x7F-\x9F]*$";

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
                    "maxLength": MAX_NAME_CHARS,
                    "pattern": NAME_PATTERN,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The white space [`NAME_PATTERN`] names beside the control characters
    /// is exactly what [`char::is_whitespace`] reads as white space.
    #[test]
    fn the_name_pattern_names_every_white_space_character() {
        let named: Vec<u32> = [0x20, 0xA0, 0x1680]
            .into_iter()
            .chain(0x2000..=0x200A)
            .chain([0x2028, 0x2029, 0x202F, 0x205F, 0x3000])
            .collect();
        let white: Vec<u32> = (0..=0x10_FFFF)
            .filter_map(char::from_u32)
            .filter(|character| character.is_whitespace() && !character.is_control())
            .map(u32::from)
            .collect();
        assert_eq!(white, named);
        for code in named {
            let text = char::from_u32(code).expect("a character").to_string();
            assert!(
                !crate::schema::conformance::pattern_matches(NAME_PATTERN, &text),
                "U+{code:04X}"
            );
        }
    }
}
