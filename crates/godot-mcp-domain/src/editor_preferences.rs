//! The editor settings `editor.preferences.set` refuses to change, and the
//! ones whose values `editor.preferences.get` withholds.
//!
//! An editor setting is not project content: the editor keeps it in the
//! user's own settings file and applies it to every project it opens. Some
//! name a program the editor starts, a file or directory it reads, a host it
//! reaches or a credential it holds. Changing one could make the editor run a
//! program, download, listen on the network or send data, so those are
//! refused; the ones that hold paths, hosts or credentials are not read
//! either. A few more are refused because they make the editor rescan and
//! import files on its own, which can start an importer program.
//!
//! The editor plugin carries a copy of these lists, which a test holds to
//! them, and adds the rule only it can apply: a setting whose property hint
//! names a file, a directory or a password is withheld and refused whatever
//! its name. The server refuses a change to a listed setting before the
//! editor is asked.

/// Settings whose values are neither read nor changed: they hold paths to
/// export tools and keystores, hosts, ports and certificates the editor
/// listens on or connects to, the password and port of the file server it runs
/// for remote file systems, and version-control credentials.
pub const WITHHELD_PREFIXES: &[&str] = &[
    "export/",
    "filesystem/file_server/",
    "network/",
    "version_control/",
];

/// The property hints, by their GDScript constant names, whose settings are
/// neither read nor changed: a path to a file or directory, or a password.
pub const WITHHELD_HINTS: &[&str] = &[
    "PROPERTY_HINT_FILE",
    "PROPERTY_HINT_DIR",
    "PROPERTY_HINT_GLOBAL_FILE",
    "PROPERTY_HINT_GLOBAL_DIR",
    "PROPERTY_HINT_PASSWORD",
];

/// Settings that are read but never changed: they choose the programs the
/// editor starts (external editors, importers, denoisers, terminals) or where
/// it downloads assets from. Godot 4.7 calls the Asset Library the Asset
/// Store.
pub const FIXED_PREFIXES: &[&str] = &[
    "asset_library/",
    "asset_store/",
    "filesystem/external_programs/",
    "filesystem/import/",
    "filesystem/tools/",
    "text_editor/external/",
];

/// Single settings that are read but never changed: a change to the file
/// extensions the FileSystem dock lists makes the editor rescan the project,
/// and importing while unfocused makes it rescan and import on its own; either
/// can import a file through an importer program. Godot 4.7 moved the second
/// setting under `interface/editor/behavior/`.
pub const FIXED_SETTINGS: &[&str] = &[
    "docks/filesystem/other_file_extensions",
    "docks/filesystem/textfile_extensions",
    "interface/editor/behavior/import_resources_when_unfocused",
    "interface/editor/import_resources_when_unfocused",
];

/// Most settings one change sets.
pub const MAX_CHANGES: usize = 64;
/// Most settings one read names.
pub const MAX_NAMES: usize = 256;
/// Most settings one page of a read returns.
pub const MAX_PAGE: u64 = 500;
/// How many settings a page holds when the caller names no limit.
pub const DEFAULT_PAGE: u64 = 100;
/// Longest setting name or prefix accepted, in characters.
pub const MAX_NAME_CHARS: usize = 256;

/// Why a change to the setting `name` is refused by its name alone, or `None`
/// when only the editor can tell (by the setting's hint, or because it has no
/// such setting).
#[must_use]
pub fn refusal(name: &str) -> Option<&'static str> {
    if WITHHELD_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
    {
        return Some(
            "it holds a path, a host, a port or a credential, so it is neither read nor changed \
             from here",
        );
    }
    if FIXED_PREFIXES.iter().any(|prefix| name.starts_with(prefix)) {
        return Some(
            "it chooses a program the editor starts or where it downloads from, so it is changed \
             only in the editor's own Editor Settings",
        );
    }
    if FIXED_SETTINGS.contains(&name) {
        return Some(
            "changing it makes the editor rescan and import files on its own, which can start an \
             importer program, so it is changed only in the editor's own Editor Settings",
        );
    }
    None
}

/// The GDScript constants the editor plugin carries, exactly as its file must
/// contain them.
#[must_use]
pub fn gdscript_table() -> String {
    let quoted = |names: &[&str]| {
        names
            .iter()
            .map(|name| format!("\"{name}\""))
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(
        "# The protected editor settings, as crates/godot-mcp-domain/src/editor_preferences.rs\n\
         # lists them; a test holds this copy to that table. Change the table, not this copy.\n\
         const WITHHELD_PREFIXES := [{}]\n\
         const WITHHELD_HINTS := [{}]\n\
         const FIXED_PREFIXES := [{}]\n\
         const FIXED_SETTINGS := [{}]\n\
         const MAX_CHANGES := {MAX_CHANGES}\n",
        quoted(WITHHELD_PREFIXES),
        WITHHELD_HINTS.join(", "),
        quoted(FIXED_PREFIXES),
        quoted(FIXED_SETTINGS),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_setting_is_refused_by_name_and_nothing_else_is() {
        for prefix in WITHHELD_PREFIXES.iter().chain(FIXED_PREFIXES) {
            assert!(refusal(&format!("{prefix}anything")).is_some(), "{prefix}");
        }
        for name in FIXED_SETTINGS {
            assert!(refusal(name).is_some(), "{name}");
        }
        for name in [
            "text_editor/behavior/indent/size",
            "interface/editor/editor_language",
            "docks/filesystem/thumbnail_size",
            "filesystem/file_dialog/show_hidden_files",
            "run/auto_save/save_before_running",
        ] {
            assert_eq!(refusal(name), None, "{name}");
        }
    }

    #[test]
    fn the_lists_are_sorted_and_hold_no_duplicate() {
        for list in [WITHHELD_PREFIXES, FIXED_PREFIXES, FIXED_SETTINGS] {
            let mut sorted = list.to_vec();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(sorted, list, "sorted and unique: {list:?}");
        }
    }
}
