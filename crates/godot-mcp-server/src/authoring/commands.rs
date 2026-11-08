//! `editor.commands.list`: the commands the project declares in
//! `godot_mcp_commands.json` (ADR 0016), read from one coherent workspace
//! revision. Nothing the declaration names is loaded or run; the provider
//! scripts are read only to say whether the editor could run them.

use godot_mcp_domain::contracts::OutcomeError;
use godot_mcp_domain::project_commands::DECLARATION_FILE;
use serde_json::{Value, json};

use super::{AuthoringRuntime, object_schema};
use crate::editor::bridge::COMMANDS_CALL_METHOD;
use crate::project_commands::{EditorView, list_value};

pub(super) fn list_schema() -> Value {
    object_schema(json!({}), &[], false)
}

pub(super) fn list(runtime: &AuthoringRuntime) -> Result<Value, OutcomeError> {
    let (snapshot, contents) = runtime.coherent_read(&[DECLARATION_FILE.to_owned()])?;
    let editor = runtime.editor.as_ref().map_or(
        EditorView {
            connected: false,
            serves_commands: false,
        },
        |bridge| {
            let status = bridge.status();
            EditorView {
                connected: status.connected,
                serves_commands: status.connected
                    && status
                        .methods
                        .iter()
                        .any(|method| method == COMMANDS_CALL_METHOD),
            }
        },
    );
    Ok(list_value(
        contents.first().and_then(Option::as_deref),
        &runtime.root,
        runtime.project_commands,
        editor,
        snapshot.to_value(),
    ))
}
