//! The bounds of `editor.shortcuts.get` and `editor.shortcuts.set`.
//!
//! A shortcut is not project content: the editor keeps a changed one in the
//! user's own settings file, beside the editor settings, and applies it to
//! every project it opens. The server holds a read or a change to these bounds
//! before the editor is asked; the editor plugin carries a copy, which a test
//! holds to this module, and holds it to them again.

/// Most shortcuts one change sets.
pub const MAX_CHANGES: usize = 64;
/// Most key events one shortcut is given.
pub const MAX_EVENTS: usize = 16;
/// Most shortcuts one read names.
pub const MAX_NAMES: usize = 256;
/// Most shortcuts one page of a read returns.
pub const MAX_PAGE: u64 = 500;
/// How many shortcuts a page holds when the caller names no limit.
pub const DEFAULT_PAGE: u64 = 100;
/// Longest shortcut name or prefix accepted, in characters.
pub const MAX_NAME_CHARS: usize = 256;
/// Longest key name accepted, in characters.
pub const MAX_KEY_CHARS: usize = 64;

/// The plugin's copy of these bounds, as the GDScript constants it declares.
#[must_use]
pub fn gdscript_bounds() -> String {
    format!(
        "# The bounds of the shortcut tools, as crates/godot-mcp-domain/src/editor_shortcuts.rs\n\
         # states them; a test holds this copy to it. Change the module, not this copy.\n\
         const MAX_CHANGES := {MAX_CHANGES}\n\
         const MAX_EVENTS := {MAX_EVENTS}\n\
         const MAX_PAGE := {MAX_PAGE}\n\
         const DEFAULT_PAGE := {DEFAULT_PAGE}\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plugin_copy_declares_every_bound_it_holds_a_change_to() {
        let copy = gdscript_bounds();
        for line in [
            "const MAX_CHANGES := 64\n",
            "const MAX_EVENTS := 16\n",
            "const MAX_PAGE := 500\n",
            "const DEFAULT_PAGE := 100\n",
        ] {
            assert!(copy.contains(line), "{line:?} in {copy}");
        }
    }
}
