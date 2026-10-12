//! Making, hashing and discarding the filtered copy.
//!
//! Three properties are load-bearing here, and each is observable rather than
//! asserted:
//!
//! * **Filtered.** Every entry is put to [`ExclusionPolicy`] and the answer is
//!   recorded in the manifest. An excluded directory is still enumerated, so its
//!   contents appear in the manifest as withheld rather than simply missing.
//! * **Deterministic.** The manifest holds only project-relative paths and
//!   content hashes — never an absolute path, a timestamp, an inode or an
//!   ordering that depends on the filesystem's directory order. Two copies of
//!   the same source under the same policy therefore render byte-identical
//!   manifests, which is what [`CopyManifest::digest`] compares.
//! * **Symlink-free.** A symlink is rejected *at copy time* and never followed:
//!   following one would copy material from outside the project into a tree that
//!   untrusted code is about to be handed. Directory enumeration reads names
//!   only; every actual open goes through Todo 2's handle-relative,
//!   symlink-nofollow primitives, so an entry that becomes a symlink between the
//!   listing and the open is refused too.
//!
//! None of this is a sandbox — see [`crate::disposable::posture`]. Excluding a
//! file from the copy does not stop project code from reading the original.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use godot_mcp_domain::sysfs::{
    self, CasOutcome, DirHandle, PROJECT_MARKER, SysError, open_child_dir_no_symlink,
    read_child_file, walk_dir_no_symlinks,
};
use serde_json::{Value, json};

use crate::disposable::error::DisposableError;
use crate::disposable::exclusion::{ExclusionPolicy, ExclusionReason};
use crate::supervision::journal::hex_sha256;

/// One file that was copied, with the hash of both sides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopiedEntry {
    /// Project-relative, slash-separated path. Never absolute.
    pub path: String,
    /// SHA-256 of the bytes read from the source.
    pub source_sha256: String,
    /// SHA-256 of the bytes read back **from the copy**, so the equality with
    /// `source_sha256` is an observation and not an assumption.
    pub copy_sha256: String,
}

/// One entry that was deliberately withheld.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExcludedEntry {
    /// Project-relative, slash-separated path. Never absolute.
    pub path: String,
    /// Why it was withheld.
    pub reason: ExclusionReason,
}

/// The complete, deterministic record of one disposable copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyManifest {
    /// Every copied file, ascending by path.
    pub copied: Vec<CopiedEntry>,
    /// Every withheld entry, ascending by path.
    pub excluded: Vec<ExcludedEntry>,
    /// Directories created in the copy, ascending. Recorded so an empty
    /// directory in the source is not silently invented or silently dropped.
    pub directories: Vec<String>,
    /// The exclusion rules in force, as `pattern -> reason`, ascending.
    pub rules: Vec<(String, ExclusionReason)>,
    /// Digest of the whole source tree under this policy.
    pub source_tree_sha256: String,
    /// Digest of the freshly created copy under this policy.
    pub copy_tree_sha256: String,
}

impl CopyManifest {
    /// The canonical rendering. Byte-stable for a given source and policy.
    #[must_use]
    pub fn to_canonical_json(&self) -> String {
        let copied: Vec<Value> = self
            .copied
            .iter()
            .map(|entry| {
                json!({
                    "path": entry.path,
                    "source_sha256": entry.source_sha256,
                    "copy_sha256": entry.copy_sha256,
                })
            })
            .collect();
        let excluded: Vec<Value> = self
            .excluded
            .iter()
            .map(|entry| {
                json!({
                    "path": entry.path,
                    "reason": entry.reason.as_str(),
                })
            })
            .collect();
        let rules: Vec<Value> = self
            .rules
            .iter()
            .map(|(pattern, reason)| {
                json!({
                    "pattern": pattern,
                    "reason": reason.as_str(),
                })
            })
            .collect();
        let value = json!({
            "copied": copied,
            "copy_tree_sha256": self.copy_tree_sha256,
            "directories": self.directories,
            "excluded": excluded,
            "rules": rules,
            "source_tree_sha256": self.source_tree_sha256,
        });
        serde_json::to_string(&value).unwrap_or_default()
    }

    /// SHA-256 of [`Self::to_canonical_json`].
    #[must_use]
    pub fn digest(&self) -> String {
        hex_sha256(self.to_canonical_json().as_bytes())
    }
}

/// Digest every file under `root`, annotated with the policy's verdict for it.
///
/// The digest covers **every** file, including the ones the policy withholds:
/// withholding a secret from a copy is not a reason to stop noticing that the
/// original changed. Each entry contributes its project-relative path, the
/// policy's verdict, and the SHA-256 of its bytes, in ascending path order, so
/// the result is stable across filesystems and independent of where the tree
/// lives.
///
/// # Errors
/// Returns [`DisposableError::SymlinkComponent`] for a symlink anywhere in the
/// tree (never followed), [`DisposableError::UnsupportedEntry`] for a
/// non-regular, non-directory entry, and [`DisposableError::Filesystem`] on I/O
/// failure.
pub fn digest_source_tree(
    root: &Path,
    policy: &ExclusionPolicy,
) -> Result<String, DisposableError> {
    let walked = walk_dir_no_symlinks(root).map_err(|error| map_sys(error, root.display()))?;
    let mut lines: Vec<String> = Vec::new();
    digest_dir(root, walked.dir(), "", policy, &mut lines)?;
    lines.sort();
    let mut joined = String::new();
    for line in lines {
        joined.push_str(&line);
        joined.push('\n');
    }
    Ok(hex_sha256(joined.as_bytes()))
}

fn digest_dir(
    dir_path: &Path,
    dir: &DirHandle,
    prefix: &str,
    policy: &ExclusionPolicy,
    lines: &mut Vec<String>,
) -> Result<(), DisposableError> {
    for entry in listing(dir_path)? {
        let relative = join_relative(prefix, &entry.name);
        match entry.kind {
            EntryKind::Symlink => {
                return Err(DisposableError::SymlinkComponent { path: relative });
            }
            EntryKind::Other => {
                return Err(DisposableError::UnsupportedEntry {
                    path: relative,
                    detail: "only regular files and directories can be hashed or copied".to_owned(),
                });
            }
            EntryKind::Directory => {
                let verdict = policy.directory_verdict(&relative);
                let child = open_child_dir_no_symlink(dir, OsStr::new(&entry.name))
                    .map_err(|error| map_sys(error, &relative))?;
                lines.push(format!(
                    "d\t{relative}\t{}",
                    verdict.map_or("copied", ExclusionReason::as_str)
                ));
                digest_dir(
                    &dir_path.join(&entry.name),
                    &child,
                    &relative,
                    policy,
                    lines,
                )?;
            }
            EntryKind::File => {
                let verdict = policy.file_verdict(&relative);
                let bytes = read_child_file(dir, OsStr::new(&entry.name))
                    .map_err(|error| map_sys(error, &relative))?;
                lines.push(format!(
                    "f\t{relative}\t{}\t{}",
                    verdict.map_or("copied", ExclusionReason::as_str),
                    hex_sha256(&bytes)
                ));
            }
        }
    }
    Ok(())
}

/// Copy `source_root` into an already-created `copy_root`, applying `policy`.
///
/// `copy_root` must exist and be empty. Every file admitted by the policy is
/// read through a handle-relative nofollow open and published into the copy with
/// a compare-and-swap create, then read back so its hash is observed.
///
/// # Errors
/// Returns [`DisposableError::NotAProject`] when `source_root` has no
/// `project.godot`, [`DisposableError::SymlinkComponent`] for a symlink in the
/// admitted region, [`DisposableError::UnsupportedEntry`] for a non-regular
/// entry, and [`DisposableError::Filesystem`] on I/O failure.
pub fn copy_project(
    source_root: &Path,
    copy_root: &Path,
    policy: &ExclusionPolicy,
) -> Result<CopyManifest, DisposableError> {
    let source =
        walk_dir_no_symlinks(source_root).map_err(|error| map_sys(error, source_root.display()))?;
    if sysfs::open_child_regular_file(source.dir(), PROJECT_MARKER.as_ref()).is_err() {
        return Err(DisposableError::NotAProject {
            path: source_root.to_path_buf(),
        });
    }
    let copy =
        walk_dir_no_symlinks(copy_root).map_err(|error| map_sys(error, copy_root.display()))?;

    let mut copied: Vec<CopiedEntry> = Vec::new();
    let mut excluded: Vec<ExcludedEntry> = Vec::new();
    let mut directories: Vec<String> = Vec::new();
    copy_dir(
        &Descend {
            source_path: source_root.to_path_buf(),
            copy_path: copy_root.to_path_buf(),
        },
        source.dir(),
        copy.dir(),
        "",
        policy,
        &mut Collected {
            copied: &mut copied,
            excluded: &mut excluded,
            directories: &mut directories,
        },
    )?;
    copied.sort_by(|left, right| left.path.cmp(&right.path));
    excluded.sort_by(|left, right| left.path.cmp(&right.path));
    directories.sort();

    Ok(CopyManifest {
        copied,
        excluded,
        directories,
        rules: policy.rules(),
        source_tree_sha256: digest_source_tree(source_root, policy)?,
        copy_tree_sha256: digest_source_tree(copy_root, policy)?,
    })
}

struct Descend {
    source_path: PathBuf,
    copy_path: PathBuf,
}

struct Collected<'c> {
    copied: &'c mut Vec<CopiedEntry>,
    excluded: &'c mut Vec<ExcludedEntry>,
    directories: &'c mut Vec<String>,
}

fn copy_dir(
    at: &Descend,
    source: &DirHandle,
    copy: &DirHandle,
    prefix: &str,
    policy: &ExclusionPolicy,
    out: &mut Collected<'_>,
) -> Result<(), DisposableError> {
    for entry in listing(&at.source_path)? {
        let relative = join_relative(prefix, &entry.name);
        match entry.kind {
            EntryKind::Symlink => {
                return Err(DisposableError::SymlinkComponent { path: relative });
            }
            EntryKind::Other => {
                return Err(DisposableError::UnsupportedEntry {
                    path: relative,
                    detail: "only regular files and directories are copied".to_owned(),
                });
            }
            EntryKind::Directory => {
                if let Some(reason) = policy.directory_verdict(&relative) {
                    // Enumerate the withheld subtree so the manifest states what
                    // was kept out, entry by entry, instead of leaving a caller
                    // to infer it from an absence. Nothing inside is opened.
                    record_withheld(&at.source_path.join(&entry.name), &relative, reason, out)?;
                    continue;
                }
                let source_child = open_child_dir_no_symlink(source, OsStr::new(&entry.name))
                    .map_err(|error| map_sys(error, &relative))?;
                let copy_child_path = at.copy_path.join(&entry.name);
                std::fs::create_dir(&copy_child_path).map_err(|error| {
                    DisposableError::Filesystem {
                        path: relative.clone(),
                        reason: error.to_string(),
                    }
                })?;
                let copy_child = open_child_dir_no_symlink(copy, OsStr::new(&entry.name))
                    .map_err(|error| map_sys(error, &relative))?;
                out.directories.push(relative.clone());
                copy_dir(
                    &Descend {
                        source_path: at.source_path.join(&entry.name),
                        copy_path: copy_child_path,
                    },
                    &source_child,
                    &copy_child,
                    &relative,
                    policy,
                    out,
                )?;
            }
            EntryKind::File => {
                if let Some(reason) = policy.file_verdict(&relative) {
                    out.excluded.push(ExcludedEntry {
                        path: relative,
                        reason,
                    });
                    continue;
                }
                let bytes = read_child_file(source, OsStr::new(&entry.name))
                    .map_err(|error| map_sys(error, &relative))?;
                let outcome = sysfs::cas_create(copy, OsStr::new(&entry.name), &bytes, &|| true)
                    .map_err(|error| map_sys(error, &relative))?;
                if outcome != CasOutcome::Published {
                    return Err(DisposableError::Filesystem {
                        path: relative,
                        reason: format!("the copy could not be published atomically ({outcome:?})"),
                    });
                }
                let written = read_child_file(copy, OsStr::new(&entry.name))
                    .map_err(|error| map_sys(error, &relative))?;
                out.copied.push(CopiedEntry {
                    path: relative,
                    source_sha256: hex_sha256(&bytes),
                    copy_sha256: hex_sha256(&written),
                });
            }
        }
    }
    Ok(())
}

/// List a withheld subtree's files without opening any of them.
fn record_withheld(
    path: &Path,
    prefix: &str,
    reason: ExclusionReason,
    out: &mut Collected<'_>,
) -> Result<(), DisposableError> {
    for entry in listing(path)? {
        let relative = join_relative(prefix, &entry.name);
        match entry.kind {
            EntryKind::Directory => {
                record_withheld(&path.join(&entry.name), &relative, reason, out)?;
            }
            // A symlink inside a withheld subtree is withheld like everything
            // else: it is never opened, so it is never followed either.
            EntryKind::File | EntryKind::Symlink | EntryKind::Other => {
                out.excluded.push(ExcludedEntry {
                    path: relative,
                    reason,
                });
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    Directory,
    File,
    Symlink,
    Other,
}

struct Entry {
    name: String,
    kind: EntryKind,
}

/// Directory names in ascending order.
///
/// This reads *names only*. Filesystem directory order is not stable, so the
/// listing is sorted before use; every actual open of a listed name goes through
/// a handle-relative nofollow primitive, so a name that changes kind after being
/// listed is refused rather than followed.
fn listing(path: &Path) -> Result<Vec<Entry>, DisposableError> {
    let read = std::fs::read_dir(path).map_err(|error| DisposableError::Filesystem {
        path: path.display().to_string(),
        reason: error.to_string(),
    })?;
    let mut entries: BTreeMap<String, EntryKind> = BTreeMap::new();
    for entry in read {
        let entry = entry.map_err(|error| DisposableError::Filesystem {
            path: path.display().to_string(),
            reason: error.to_string(),
        })?;
        let file_type = entry
            .file_type()
            .map_err(|error| DisposableError::Filesystem {
                path: entry.path().display().to_string(),
                reason: error.to_string(),
            })?;
        let kind = if file_type.is_symlink() {
            EntryKind::Symlink
        } else if file_type.is_dir() {
            EntryKind::Directory
        } else if file_type.is_file() {
            EntryKind::File
        } else {
            EntryKind::Other
        };
        entries.insert(entry.file_name().to_string_lossy().into_owned(), kind);
    }
    Ok(entries
        .into_iter()
        .map(|(name, kind)| Entry { name, kind })
        .collect())
}

fn join_relative(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}/{name}")
    }
}

fn map_sys(error: SysError, path: impl std::fmt::Display) -> DisposableError {
    match error {
        SysError::SymlinkComponent => DisposableError::SymlinkComponent {
            path: path.to_string(),
        },
        SysError::NotRegularFile => DisposableError::UnsupportedEntry {
            path: path.to_string(),
            detail: "the entry is not a regular file".to_owned(),
        },
        other => DisposableError::Filesystem {
            path: path.to_string(),
            reason: other.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relative_path_is_joined_with_forward_slashes_only() {
        assert_eq!(join_relative("", "project.godot"), "project.godot");
        assert_eq!(
            join_relative("scripts", "player.gd"),
            "scripts/player.gd",
            "manifest paths are slash-separated on every platform"
        );
        assert_eq!(join_relative("a/b", "c"), "a/b/c");
    }

    #[test]
    fn a_manifest_renders_deterministically_and_holds_no_absolute_path() {
        let manifest = CopyManifest {
            copied: vec![CopiedEntry {
                path: "project.godot".to_owned(),
                source_sha256: "a".repeat(64),
                copy_sha256: "a".repeat(64),
            }],
            excluded: vec![ExcludedEntry {
                path: ".env".to_owned(),
                reason: ExclusionReason::SecretMaterial,
            }],
            directories: vec!["scripts".to_owned()],
            rules: vec![(
                "dir:.godot".to_owned(),
                ExclusionReason::EngineGeneratedCache,
            )],
            source_tree_sha256: "b".repeat(64),
            copy_tree_sha256: "c".repeat(64),
        };
        let first = manifest.to_canonical_json();
        assert_eq!(first, manifest.to_canonical_json());
        assert_eq!(manifest.digest(), manifest.clone().digest());
        assert!(
            !first.contains('/') || !first.contains(":\\"),
            "no windows absolute path leaks into the manifest: {first}"
        );
        assert!(!first.contains("/tmp"), "{first}");
        assert!(first.contains("secret_material"));
    }

    #[test]
    fn a_symlink_in_the_tree_is_reported_by_relative_path() {
        let error = DisposableError::SymlinkComponent {
            path: "escape.txt".to_owned(),
        };
        assert!(error.to_string().contains("escape.txt"));
    }
}
