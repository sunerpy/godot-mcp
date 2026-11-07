//! Reading, writing and removing one installed skill directory.
//!
//! A skill directory holds `SKILL.md` and the marker `.godot-mcp-skill.json`
//! that records the SHA-256 of the `SKILL.md` bytes godot-mcp wrote. The read
//! rules fail closed:
//!
//! * only `NotFound` from a no-follow look at `SKILL.md` means "not installed";
//! * a `SKILL.md` that is a symbolic link, a directory or anything but a regular
//!   file, and any other error while reading it, is an error for that skill:
//!   nothing is written or removed there, `--force` or not;
//! * a skill directory position that holds something other than a directory or
//!   a link to one is an error too, while a skill directory, or any directory
//!   above it, that is a link to a directory is followed;
//! * a marker that is missing, unreadable or not a JSON object with a `sha256`
//!   string is "no usable marker", and the reason is kept for the report.
//!
//! Every write goes to a temporary file in the same directory and is renamed
//! over the target, `SKILL.md` first and the marker second.

use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// The skill file every agent reads.
pub const SKILL_FILE: &str = "SKILL.md";

/// The marker godot-mcp writes beside each `SKILL.md` it installs.
pub const MARKER_FILE: &str = ".godot-mcp-skill.json";

/// Lowercase hex SHA-256 of `bytes`.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// What sits at `<skill dir>/SKILL.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Installed {
    /// Nothing: the skill is not installed.
    Absent,
    /// A regular file with these bytes.
    Present(Vec<u8>),
}

/// What the marker says, if anything usable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Marker {
    /// A marker naming the SHA-256 of the bytes godot-mcp last wrote.
    Usable {
        sha256: String,
        version: Option<String>,
    },
    /// No marker file.
    Missing,
    /// A marker that cannot be used, and why.
    Unusable(String),
}

impl Marker {
    /// Why there is no usable marker, as a clause of a report line.
    pub(super) fn absence_reason(&self) -> Option<String> {
        match self {
            Self::Usable { .. } => None,
            Self::Missing => Some("it has no marker".to_owned()),
            Self::Unusable(reason) => Some(reason.clone()),
        }
    }
}

/// One skill directory as it is on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SkillState {
    pub(super) installed: Installed,
    pub(super) marker: Marker,
}

fn kind_of(file_type: fs::FileType) -> &'static str {
    if file_type.is_symlink() {
        "a symbolic link"
    } else if file_type.is_dir() {
        "a directory"
    } else {
        "not a regular file"
    }
}

/// Read one skill directory under the rules in the module documentation.
///
/// # Errors
/// Returns the reason this skill must be left alone.
pub(super) fn read_state(skill_dir: &Path) -> Result<SkillState, String> {
    match fs::symlink_metadata(skill_dir) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(SkillState {
                installed: Installed::Absent,
                marker: Marker::Missing,
            });
        }
        Err(error) => return Err(format!("cannot inspect {}: {error}", skill_dir.display())),
        Ok(metadata) if metadata.is_dir() => {}
        Ok(metadata) if metadata.file_type().is_symlink() => match fs::metadata(skill_dir) {
            Ok(target) if target.is_dir() => {}
            Ok(_) => {
                return Err(format!(
                    "{} is a symbolic link to something that is not a directory",
                    skill_dir.display()
                ));
            }
            Err(error) => {
                return Err(format!(
                    "{} is a symbolic link that cannot be followed: {error}",
                    skill_dir.display()
                ));
            }
        },
        Ok(_) => {
            return Err(format!(
                "{} exists and is not a directory",
                skill_dir.display()
            ));
        }
    }

    let skill_file = skill_dir.join(SKILL_FILE);
    let installed = match fs::symlink_metadata(&skill_file) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Installed::Absent,
        Err(error) => return Err(format!("cannot inspect {}: {error}", skill_file.display())),
        Ok(metadata) if metadata.file_type().is_file() => match fs::read(&skill_file) {
            Ok(bytes) => Installed::Present(bytes),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Installed::Absent,
            Err(error) => return Err(format!("cannot read {}: {error}", skill_file.display())),
        },
        Ok(metadata) => {
            return Err(format!(
                "{} is {}, not a regular file",
                skill_file.display(),
                kind_of(metadata.file_type())
            ));
        }
    };

    let marker = match fs::read(skill_dir.join(MARKER_FILE)) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Marker::Missing,
        Err(error) => Marker::Unusable(format!("its marker cannot be read: {error}")),
        Ok(bytes) => parse_marker(&bytes),
    };
    Ok(SkillState { installed, marker })
}

fn parse_marker(bytes: &[u8]) -> Marker {
    let Ok(value) = serde_json::from_slice::<Value>(bytes) else {
        return Marker::Unusable("its marker is not valid JSON".to_owned());
    };
    let Some(sha256) = value.get("sha256").and_then(Value::as_str) else {
        return Marker::Unusable("its marker names no sha256".to_owned());
    };
    Marker::Usable {
        sha256: sha256.to_owned(),
        version: value
            .get("version")
            .and_then(Value::as_str)
            .map(str::to_owned),
    }
}

/// Distinguishes the temporary files of concurrent writers in one process.
static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

/// Write and flush `bytes`, closing the file before it is renamed.
fn fill(mut file: fs::File, bytes: &[u8]) -> io::Result<()> {
    file.write_all(bytes)?;
    file.sync_all()
}

/// Write `bytes` to `path` through a temporary file in the same directory that
/// is renamed over it.
fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("the path has no parent directory"))?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("the path has no file name"))?
        .to_string_lossy();
    let mut attempt = 0;
    let (temporary, file) = loop {
        let candidate = parent.join(format!(
            ".{name}.{}.{}.godot-mcp-tmp",
            std::process::id(),
            NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed)
        ));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => break (candidate, file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists && attempt < 16 => {
                attempt += 1;
            }
            Err(error) => return Err(error),
        }
    };
    let written = fill(file, bytes).and_then(|()| fs::rename(&temporary, path));
    if written.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    written
}

/// Write the marker for `SKILL.md` bytes with this SHA-256.
///
/// # Errors
/// Returns why the marker could not be written.
pub(super) fn write_marker(skill_dir: &Path, sha256: &str, version: &str) -> Result<(), String> {
    let installed_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let marker = json!({
        "sha256": sha256,
        "version": version,
        "installed_at": installed_at,
    });
    let mut bytes = serde_json::to_vec_pretty(&marker)
        .map_err(|error| format!("cannot serialize the marker: {error}"))?;
    bytes.push(b'\n');
    let path = skill_dir.join(MARKER_FILE);
    write_atomically(&path, &bytes)
        .map_err(|error| format!("cannot write {}: {error}", path.display()))
}

/// Write `SKILL.md`, then its marker.
///
/// # Errors
/// Returns why a file could not be written; a failure after `SKILL.md` was
/// written says so.
pub(super) fn write_skill(skill_dir: &Path, content: &str, version: &str) -> Result<(), String> {
    fs::create_dir_all(skill_dir)
        .map_err(|error| format!("cannot create {}: {error}", skill_dir.display()))?;
    let path = skill_dir.join(SKILL_FILE);
    write_atomically(&path, content.as_bytes())
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    write_marker(skill_dir, &sha256_hex(content.as_bytes()), version)
        .map_err(|error| format!("wrote {} but not its marker: {error}", path.display()))
}

/// What a removal left of the skill directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Leftover {
    /// Nothing: the directory was empty and is gone.
    Nothing,
    /// The directory, which still holds other files.
    Directory,
    /// The symbolic link the directory was reached through, which godot-mcp
    /// did not create and leaves in place.
    Link,
}

fn remove_if_present(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("cannot remove {}: {error}", path.display())),
    }
}

/// Remove `SKILL.md` (when `skill_file` is set) and the marker, then the skill
/// directory itself when it is a real directory left empty.
///
/// # Errors
/// Returns why a file could not be removed.
pub(super) fn remove_skill(skill_dir: &Path, skill_file: bool) -> Result<Leftover, String> {
    if skill_file {
        remove_if_present(&skill_dir.join(SKILL_FILE))?;
    }
    let marker = skill_dir.join(MARKER_FILE);
    if fs::symlink_metadata(&marker).is_ok_and(|metadata| metadata.file_type().is_file()) {
        remove_if_present(&marker)?;
    }
    match fs::symlink_metadata(skill_dir) {
        Ok(metadata) if metadata.file_type().is_symlink() => Ok(Leftover::Link),
        Ok(metadata) if metadata.is_dir() => match fs::remove_dir(skill_dir) {
            Ok(()) => Ok(Leftover::Nothing),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Leftover::Nothing),
            // Anything else left in it keeps the directory.
            Err(_) if skill_dir.is_dir() => Ok(Leftover::Directory),
            Err(error) => Err(format!("cannot remove {}: {error}", skill_dir.display())),
        },
        Ok(_) => Ok(Leftover::Directory),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Leftover::Nothing),
        Err(error) => Err(format!("cannot inspect {}: {error}", skill_dir.display())),
    }
}

/// Whether a regular marker file sits in `skill_dir`.
pub(super) fn has_marker_file(skill_dir: &Path) -> bool {
    fs::symlink_metadata(skill_dir.join(MARKER_FILE))
        .is_ok_and(|metadata| metadata.file_type().is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_hex_matches_a_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn a_marker_needs_a_json_object_with_a_sha256_string() {
        assert_eq!(
            parse_marker(b"{not json"),
            Marker::Unusable("its marker is not valid JSON".to_owned())
        );
        assert_eq!(
            parse_marker(b"{\"sha256\": 7}"),
            Marker::Unusable("its marker names no sha256".to_owned())
        );
        assert_eq!(
            parse_marker(b"{\"sha256\": \"ab\", \"installed_at\": \"later\"}"),
            Marker::Usable {
                sha256: "ab".to_owned(),
                version: None
            }
        );
    }
}
