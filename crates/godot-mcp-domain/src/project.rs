//! Upward `project.godot` discovery through the unified handle abstraction.

use std::path::{Path, PathBuf};

use crate::objid::AliasWitness;
use crate::sysfs::{
    DirHandle, FileHandle, PROJECT_MARKER, SysError, identity_of_dir, open_child_regular_file,
    walk_dir_no_symlinks,
};

/// Failure to locate a Godot project root.
#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    /// The candidate path could not be resolved to an absolute path.
    #[error("candidate path could not be made absolute: {0}")]
    NotAbsolute(String),
    /// No `project.godot` was found at the candidate or any ancestor.
    #[error("no {marker} found at or above {start}")]
    NotFound {
        /// The marker filename searched for.
        marker: &'static str,
        /// The starting candidate path.
        start: String,
    },
    /// A component encountered while walking the candidate or marker was a
    /// symlink. Components later cancelled by `..` are included.
    #[error("a symlink component is not permitted in {path}")]
    SymlinkComponent {
        /// The display spelling being inspected when the symlink was found.
        path: PathBuf,
    },
    /// Descriptor-relative discovery failed for another filesystem reason.
    #[error("filesystem discovery failed at {path}: {reason}")]
    Io {
        /// The display spelling being inspected.
        path: PathBuf,
        /// The underlying failure.
        reason: String,
    },
}

/// Descriptor-owned result of upward project discovery.
pub(crate) struct DiscoveredProject {
    /// Normalized display metadata for the discovered root.
    pub(crate) display_root: PathBuf,
    /// Held directory handle authorizing the project root.
    pub(crate) root: DirHandle,
    /// Held nofollow regular-file handle authorizing `project.godot`.
    pub(crate) project_file: FileHandle,
    /// Alias/mount evidence captured while walking the acquisition spelling.
    pub(crate) alias_witness: AliasWitness,
}

/// Locate the nearest ancestor of `candidate` (inclusive) that directly
/// contains a regular `project.godot` file. Every candidate component is
/// inspected before a later `..` is honored.
pub(crate) fn discover_project_root(candidate: &Path) -> Result<DiscoveredProject, DiscoveryError> {
    let absolute = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(candidate))
            .map_err(|error| DiscoveryError::NotAbsolute(error.to_string()))?
    };
    let mut walked = walk_dir_no_symlinks(&absolute).map_err(|error| map_sys(error, &absolute))?;

    loop {
        let marker_path = walked.display().join(PROJECT_MARKER);
        match open_child_regular_file(walked.dir(), PROJECT_MARKER.as_ref()) {
            Ok(project_file) => {
                let (root, display_root, alias_witness) = walked.into_parts();
                return Ok(DiscoveredProject {
                    display_root,
                    root,
                    project_file,
                    alias_witness,
                });
            }
            Err(SysError::NotFound) => {}
            Err(error) => return Err(map_sys(error, &marker_path)),
        }

        if !walked.ascend() {
            return Err(DiscoveryError::NotFound {
                marker: PROJECT_MARKER,
                start: absolute.display().to_string(),
            });
        }
        // Inspecting the retained parent handle proves ascent remains entirely
        // descriptor based; no display path is reopened during discovery.
        identity_of_dir(walked.dir()).map_err(|error| map_sys(error, &walked.display()))?;
    }
}

fn map_sys(error: SysError, path: &Path) -> DiscoveryError {
    match error {
        SysError::SymlinkComponent => DiscoveryError::SymlinkComponent {
            path: path.to_path_buf(),
        },
        SysError::NotAbsolute => DiscoveryError::NotAbsolute(path.display().to_string()),
        SysError::NotFound => DiscoveryError::NotFound {
            marker: PROJECT_MARKER,
            start: path.display().to_string(),
        },
        other => DiscoveryError::Io {
            path: path.to_path_buf(),
            reason: other.to_string(),
        },
    }
}
