//! Project-relative path policy for the authoring tools.
//!
//! Path *authority* stays with the held lease: every byte is read and written
//! through `SafeRoot`, which refuses traversal, absolute spellings and symlink
//! components on its own. This module adds the policy a lease cannot know —
//! which in-root names are engine- or server-owned, and which a generic file
//! tool must leave to a typed one — and refuses those before anything is
//! journaled.

use std::path::{Component, Path};

use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::contracts::OutcomeError;
use godot_mcp_domain::lease::PROJECT_STATE_DIR_NAME;
use godot_mcp_domain::sysfs::is_staging_name;
use godot_mcp_engine::canonical::helper::HELPER_ROOT;

use super::outcome_error;

/// The engine's generated cache directory. Never project source.
const ENGINE_CACHE_DIR: &str = ".godot";
/// Longest accepted project-relative spelling, in bytes.
const MAX_PATH_BYTES: usize = 1024;
/// Longest accepted single component, in bytes.
const MAX_COMPONENT_BYTES: usize = 255;
/// Names Windows reserves in every directory, compared case-insensitively and
/// ignoring any extension.
const WINDOWS_RESERVED: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// Normalize one caller-supplied project-relative path.
///
/// `res://` is accepted because that is how Godot spells its own references,
/// and `\` is folded to `/`. The result never starts with `/`, contains no `.`
/// or `..` component and names at least one component.
///
/// # Errors
/// Returns [`ErrorKind::InvalidInput`] for an empty, absolute, escaping,
/// over-long or non-portable spelling.
pub(super) fn normalize(raw: &str, field: &str) -> Result<String, OutcomeError> {
    if raw.trim().is_empty() {
        return Err(invalid(field, "must not be empty"));
    }
    if raw.trim() != raw {
        // Trimming would silently address a different file than the one named.
        return Err(invalid(field, "must not start or end with whitespace"));
    }
    let stripped = raw.strip_prefix("res://").unwrap_or(raw);
    let forward = stripped.replace('\\', "/");
    if forward.starts_with('/') || Path::new(&forward).is_absolute() || forward.contains(':') {
        return Err(invalid(field, "must be project-relative, not absolute"));
    }
    let mut parts = Vec::new();
    for component in Path::new(&forward).components() {
        match component {
            Component::Normal(part) => {
                let part = part
                    .to_str()
                    .ok_or_else(|| invalid(field, "must be valid UTF-8"))?;
                check_component(part, field)?;
                parts.push(part.to_owned());
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(invalid(field, "must stay inside the project"));
            }
        }
    }
    if parts.is_empty() {
        return Err(invalid(field, "must name a file"));
    }
    let joined = parts.join("/");
    if joined.len() > MAX_PATH_BYTES {
        return Err(invalid(
            field,
            &format!("must not exceed {MAX_PATH_BYTES} bytes"),
        ));
    }
    Ok(joined)
}

fn check_component(part: &str, field: &str) -> Result<(), OutcomeError> {
    if part.len() > MAX_COMPONENT_BYTES {
        return Err(invalid(
            field,
            &format!("must not have a component longer than {MAX_COMPONENT_BYTES} bytes"),
        ));
    }
    if part.chars().any(|character| {
        character.is_control() || matches!(character, '<' | '>' | '"' | '|' | '?' | '*')
    }) {
        return Err(invalid(
            field,
            "must not contain control characters or <>\"|?*",
        ));
    }
    if part.ends_with('.') || part.ends_with(' ') {
        return Err(invalid(
            field,
            "must not have a component ending in a dot or a space",
        ));
    }
    let stem = part.split('.').next().unwrap_or(part).to_ascii_lowercase();
    if WINDOWS_RESERVED.contains(&stem.as_str()) {
        return Err(invalid(
            field,
            &format!("must not use the reserved name `{part}`"),
        ));
    }
    Ok(())
}

/// Whether `path` lies in the engine cache, the private state root or the
/// server-owned helper scratch, or names a file a compare-and-swap stages or
/// parks beside its target.
#[must_use]
pub(super) fn is_private(path: &str) -> bool {
    let first = path.split('/').next().unwrap_or_default();
    first == ENGINE_CACHE_DIR
        || first == PROJECT_STATE_DIR_NAME
        || first == HELPER_ROOT
        || path.rsplit('/').next().is_some_and(is_staging_name)
}

/// Refuse a path no public tool may read or write.
///
/// # Errors
/// Returns [`ErrorKind::PermissionDenied`] for the engine cache, the private
/// state root, the helper scratch and a staging file name.
pub(super) fn readable(path: &str) -> Result<(), OutcomeError> {
    if is_private(path) {
        return Err(outcome_error(
            ErrorKind::PermissionDenied,
            format!(
                "{path} is engine- or server-owned state, not project source, and is not \
                 readable through the authoring tools"
            ),
        ));
    }
    Ok(())
}

/// The lowercase extension of `path`, if any.
#[must_use]
pub(super) fn extension(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default()
}

/// Which family of tool owns writes to a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WriteOwner {
    /// Ordinary project text: the generic file tools.
    Text,
    /// A `.gd` script: also writable by the generic tools, which skip the
    /// static parse gate the script tools apply.
    Script,
    /// A binary asset: the asset tools.
    Asset,
}

/// Refuse a write target no authoring tool may create or replace, and say
/// which family owns the rest.
///
/// # Errors
/// Returns [`ErrorKind::PermissionDenied`] for private state, and
/// [`ErrorKind::InvalidInput`] with the owning tool for `project.godot`, an
/// engine-owned sidecar or a binary Godot container.
pub(super) fn writable(path: &str) -> Result<WriteOwner, OutcomeError> {
    readable(path)?;
    if path == "project.godot" {
        return Err(outcome_error(
            ErrorKind::InvalidInput,
            "project.godot is changed through project.settings.patch, which preserves its \
             structure; it is not written as raw text",
        ));
    }
    let extension = extension(path);
    match extension.as_str() {
        "import" | "uid" => Err(outcome_error(
            ErrorKind::InvalidInput,
            format!(
                "{path} is an engine-owned .{extension} sidecar; it is written by the importer \
                 and moved or deleted together with its source file"
            ),
        )),
        "scn" | "res" => Err(outcome_error(
            ErrorKind::Unsupported,
            format!(
                "{path} is a binary Godot container; text scenes (.tscn) and resources (.tres) \
                 are supported instead"
            ),
        )),
        "gd" => Ok(WriteOwner::Script),
        _ if is_binary_asset_extension(&extension) => Ok(WriteOwner::Asset),
        _ => Ok(WriteOwner::Text),
    }
}

/// Extensions whose content is binary asset data, written by `asset.put`.
#[must_use]
pub(super) fn is_binary_asset_extension(extension: &str) -> bool {
    matches!(
        extension,
        "png"
            | "jpg"
            | "jpeg"
            | "webp"
            | "bmp"
            | "tga"
            | "exr"
            | "hdr"
            | "ktx"
            | "dds"
            | "wav"
            | "ogg"
            | "mp3"
            | "ttf"
            | "otf"
            | "woff"
            | "woff2"
            | "glb"
            | "fbx"
            | "blend"
            | "zip"
            | "bin"
    )
}

/// The engine sidecars that travel with a source file: its `.import` metadata
/// and its `.uid` identity file.
#[must_use]
pub(super) fn sidecars(path: &str) -> [String; 2] {
    [format!("{path}.import"), format!("{path}.uid")]
}

fn invalid(field: &str, problem: &str) -> OutcomeError {
    outcome_error(ErrorKind::InvalidInput, format!("`{field}` {problem}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_accepts_res_uris_and_folds_separators() {
        assert_eq!(
            normalize("res://scripts\\player.gd", "path").expect("normalized"),
            "scripts/player.gd"
        );
        assert_eq!(
            normalize("./a/./b.txt", "path").expect("normalized"),
            "a/b.txt"
        );
    }

    #[test]
    fn normalization_refuses_escapes_and_non_portable_names() {
        for raw in [
            "",
            "/etc/passwd",
            "../outside.gd",
            "a/../../b",
            "C:/x.gd",
            "a/con.txt",
            "a/b.",
            "a/b ",
            "a/b\u{0}c",
            "a/what?.gd",
        ] {
            let error = normalize(raw, "path").expect_err(raw);
            assert_eq!(error.kind(), ErrorKind::InvalidInput, "{raw}");
        }
    }

    /// A compare-and-swap's staged or parked file is the server's own write in
    /// flight, which the workspace never indexes; no tool reads or writes one.
    #[test]
    fn a_staging_file_name_is_server_owned_wherever_it_lies() {
        for path in [".cas-4242-7.tmp", "scripts/.cas-4242-7.tmp"] {
            assert_eq!(
                writable(path).expect_err(path).kind(),
                ErrorKind::PermissionDenied
            );
            assert_eq!(
                readable(path).expect_err(path).kind(),
                ErrorKind::PermissionDenied
            );
        }
        readable("scripts/.cas-notes.tmp").expect("an ordinary dot-file");
    }

    #[test]
    fn private_and_engine_owned_targets_are_refused() {
        for path in [
            ".godot/x.cfg",
            ".godot-mcp/journal",
            "godot_mcp_helpers/2/x.gd",
        ] {
            assert_eq!(
                writable(path).expect_err(path).kind(),
                ErrorKind::PermissionDenied
            );
            assert_eq!(
                readable(path).expect_err(path).kind(),
                ErrorKind::PermissionDenied
            );
        }
        for path in ["project.godot", "icon.png.import", "player.gd.uid"] {
            assert_eq!(
                writable(path).expect_err(path).kind(),
                ErrorKind::InvalidInput
            );
            readable(path).expect("readable");
        }
        assert_eq!(
            writable("level.scn").expect_err("binary").kind(),
            ErrorKind::Unsupported
        );
        assert_eq!(writable("a/b.gd").expect("script"), WriteOwner::Script);
        assert_eq!(writable("a/b.png").expect("asset"), WriteOwner::Asset);
        assert_eq!(writable("a/b.tscn").expect("text"), WriteOwner::Text);
    }
}
