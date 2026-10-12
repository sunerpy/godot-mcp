//! A held workspace authority: the one directory a bootstrap server may create
//! projects under and bind one project from.
//!
//! The workspace is walked once with symlink components forbidden and then held
//! by handle. Every later effect — listing, creating directories, publishing a
//! new `project.godot`, resolving a project to bind — is relative to that
//! handle, and the display spelling is re-proven to name the same object before
//! each one. A project is never replaced, never nested inside another project
//! and never created in a non-empty directory.

use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};

use crate::ErrorKind;
use crate::objid::StableObjectId;
use crate::sysfs::{
    self, CasOutcome, DirHandle, DirectoryEntryKind, PROJECT_MARKER, SysError, identity_of_dir,
    open_child_dir_no_symlink, open_child_regular_file, read_dir, reopen_dir, walk_dir_no_symlinks,
};

/// A failure of a workspace operation. Messages name workspace-relative paths
/// only, never the workspace's own location.
#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    /// The workspace path does not exist.
    #[error("the workspace directory does not exist")]
    Missing,
    /// The workspace path names something other than a directory.
    #[error("the workspace path is not a directory")]
    NotADirectory,
    /// A path is absolute, escapes the workspace or names nothing.
    #[error("a project path must be workspace-relative and stay inside the workspace")]
    OutsideWorkspace,
    /// A component of a path is a symlink. It is never followed.
    #[error("a symlink component is not permitted inside the workspace")]
    Symlink,
    /// The target already holds a Godot project.
    #[error("{0} already holds a Godot project")]
    ProjectExists(PathBuf),
    /// The target lies inside an existing Godot project.
    #[error("{0} is inside the Godot project at {1}")]
    NestedProject(PathBuf, PathBuf),
    /// The target directory already holds other files.
    #[error("{0} is not empty")]
    NotEmpty(PathBuf),
    /// The directory holds no `project.godot`.
    #[error("{0} holds no project.godot")]
    NoProject(PathBuf),
    /// The workspace spelling no longer names the held directory.
    #[error("the workspace path no longer names the held directory")]
    IdentityChanged,
    /// A genuine I/O failure.
    #[error("workspace I/O failed: {0}")]
    Io(String),
}

impl WorkspaceError {
    /// The stable public error kind.
    #[must_use]
    pub const fn error_kind(&self) -> ErrorKind {
        match self {
            Self::Missing | Self::NoProject(_) => ErrorKind::NotFound,
            Self::NotADirectory => ErrorKind::InvalidInput,
            Self::OutsideWorkspace | Self::Symlink => ErrorKind::PermissionDenied,
            Self::ProjectExists(_)
            | Self::NestedProject(..)
            | Self::NotEmpty(_)
            | Self::IdentityChanged => ErrorKind::Conflict,
            Self::Io(_) => ErrorKind::Internal,
        }
    }
}

fn map_sys(error: SysError) -> WorkspaceError {
    match error {
        SysError::SymlinkComponent => WorkspaceError::Symlink,
        SysError::NotFound => WorkspaceError::Missing,
        SysError::BadComponent | SysError::NotAbsolute => WorkspaceError::OutsideWorkspace,
        SysError::NotRegularFile => WorkspaceError::NotADirectory,
        other => WorkspaceError::Io(other.to_string()),
    }
}

/// A project created by [`WorkspaceRoot::create_project`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedProject {
    relative: PathBuf,
}

impl CreatedProject {
    /// The new project directory, relative to the workspace.
    #[must_use]
    pub fn relative(&self) -> &Path {
        &self.relative
    }
}

/// A project directory resolved for binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedWorkspaceProject {
    relative: PathBuf,
    display_root: PathBuf,
    root_identity: StableObjectId,
}

impl ResolvedWorkspaceProject {
    /// The project directory, relative to the workspace.
    #[must_use]
    pub fn relative(&self) -> &Path {
        &self.relative
    }

    /// Absolute, symlink-free display spelling of the project root, for the
    /// project lease to acquire. Never public.
    #[must_use]
    pub fn display_root(&self) -> &Path {
        &self.display_root
    }

    /// The stable identity of the resolved project directory. A binding must
    /// lease exactly this object.
    #[must_use]
    pub const fn root_identity(&self) -> StableObjectId {
        self.root_identity
    }
}

/// One bounded listing of the projects under a workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectListing {
    projects: Vec<PathBuf>,
    complete: bool,
}

impl ProjectListing {
    /// Workspace-relative project directories, sorted.
    #[must_use]
    pub fn projects(&self) -> &[PathBuf] {
        &self.projects
    }

    /// Whether the bounds covered the whole workspace.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.complete
    }
}

/// The workspace's own private directory: one record per bootstrap
/// idempotency key. It is not a project state root, so it is named apart from
/// one, and no project is ever created or bound inside it.
pub const BOOTSTRAP_STATE_DIR_NAME: &str = ".godot-mcp-bootstrap";

/// The held workspace directory.
pub struct WorkspaceRoot {
    root: DirHandle,
    display: PathBuf,
    identity: StableObjectId,
}

impl std::fmt::Debug for WorkspaceRoot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Path-free on purpose: the workspace location is never diagnostic output.
        formatter
            .debug_struct("WorkspaceRoot")
            .finish_non_exhaustive()
    }
}

impl WorkspaceRoot {
    /// Hold `path`, an existing directory, walked with symlink components
    /// forbidden. A relative path is resolved against the current directory.
    ///
    /// # Errors
    /// Returns [`WorkspaceError`] when the path is missing, not a directory or
    /// reached through a symlink.
    pub fn open(path: &Path) -> Result<Self, WorkspaceError> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|error| WorkspaceError::Io(error.to_string()))?
                .join(path)
        };
        if std::fs::symlink_metadata(&absolute).is_ok_and(|metadata| metadata.is_file()) {
            return Err(WorkspaceError::NotADirectory);
        }
        let walked = walk_dir_no_symlinks(&absolute).map_err(map_sys)?;
        let (root, display, _witness) = walked.into_parts();
        let identity = identity_of_dir(&root).map_err(map_sys)?;
        Ok(Self {
            root,
            display,
            identity,
        })
    }

    /// Absolute display spelling of the held workspace. Never public output;
    /// it exists so a transport can redact it.
    #[must_use]
    pub fn display_root(&self) -> &Path {
        &self.display
    }

    /// The stable identity of the held workspace directory.
    #[must_use]
    pub const fn identity(&self) -> StableObjectId {
        self.identity
    }

    /// Re-prove that the display spelling still names the held directory.
    fn guard(&self) -> Result<(), WorkspaceError> {
        let walked = walk_dir_no_symlinks(&self.display).map_err(map_sys)?;
        if identity_of_dir(walked.dir()).map_err(map_sys)? != self.identity {
            return Err(WorkspaceError::IdentityChanged);
        }
        Ok(())
    }

    /// The component names of a workspace-relative path; empty for the
    /// workspace itself.
    fn components(relative: &Path) -> Result<Vec<OsString>, WorkspaceError> {
        if relative.is_absolute() {
            return Err(WorkspaceError::OutsideWorkspace);
        }
        let mut names = Vec::new();
        for component in relative.components() {
            match component {
                Component::Normal(name) => names.push(name.to_os_string()),
                Component::CurDir => {}
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    return Err(WorkspaceError::OutsideWorkspace);
                }
            }
        }
        Ok(names)
    }

    fn has_project(dir: &DirHandle) -> Result<bool, WorkspaceError> {
        match open_child_regular_file(dir, OsStr::new(PROJECT_MARKER)) {
            Ok(_) => Ok(true),
            Err(SysError::NotFound) => Ok(false),
            Err(error) => Err(map_sys(error)),
        }
    }

    /// Create a new Godot project in the workspace-relative directory
    /// `relative`, publishing `manifest` as its `project.godot`.
    ///
    /// Missing directories are created handle-relative. The target must be
    /// absent or an empty directory, and neither it nor any directory above it
    /// (the workspace included) may already hold a project.
    ///
    /// # Errors
    /// Returns [`WorkspaceError`] for an escaping or symlinked path, an existing
    /// or enclosing project, a non-empty target or an identity change.
    pub fn create_project(
        &self,
        relative: &Path,
        manifest: &[u8],
    ) -> Result<CreatedProject, WorkspaceError> {
        self.guard()?;
        let names = Self::components(relative)?;
        let Some((last, parents)) = names.split_last() else {
            return Err(WorkspaceError::OutsideWorkspace);
        };
        let mut walked = PathBuf::new();
        let mut current = reopen_dir(&self.root).map_err(map_sys)?;
        if Self::has_project(&current)? {
            return Err(WorkspaceError::NestedProject(
                relative.to_path_buf(),
                PathBuf::from("."),
            ));
        }
        for name in parents {
            walked.push(name);
            current = sysfs::create_child_dir_no_symlink(&current, name).map_err(map_sys)?;
            if Self::has_project(&current)? {
                return Err(WorkspaceError::NestedProject(
                    relative.to_path_buf(),
                    walked.clone(),
                ));
            }
        }
        let target = match open_child_dir_no_symlink(&current, last) {
            Ok(target) => {
                let entries = read_dir(&target).map_err(map_sys)?;
                if entries
                    .iter()
                    .any(|entry| entry.file_name() == OsStr::new(PROJECT_MARKER))
                {
                    return Err(WorkspaceError::ProjectExists(relative.to_path_buf()));
                }
                if !entries.is_empty() {
                    return Err(WorkspaceError::NotEmpty(relative.to_path_buf()));
                }
                target
            }
            Err(SysError::NotFound) => {
                sysfs::create_child_dir_no_symlink(&current, last).map_err(map_sys)?
            }
            Err(error) => return Err(map_sys(error)),
        };
        let guard_failed = std::cell::Cell::new(false);
        let before_publish = || {
            let held = self.guard().is_ok();
            guard_failed.set(!held);
            held
        };
        match sysfs::cas_create(
            &target,
            OsStr::new(PROJECT_MARKER),
            manifest,
            &before_publish,
        )
        .map_err(map_sys)?
        {
            CasOutcome::Published => Ok(CreatedProject {
                relative: names.iter().collect(),
            }),
            CasOutcome::Stale => Err(WorkspaceError::ProjectExists(relative.to_path_buf())),
            CasOutcome::Aborted => Err(if guard_failed.get() {
                WorkspaceError::IdentityChanged
            } else {
                WorkspaceError::Io("the project manifest was not published".to_owned())
            }),
        }
    }

    /// The private directory under the workspace that holds the record of
    /// every bootstrap request, created on first use.
    fn bootstrap_records(&self) -> Result<DirHandle, WorkspaceError> {
        self.guard()?;
        sysfs::open_private_child_state_dir(&self.root, OsStr::new(BOOTSTRAP_STATE_DIR_NAME))
            .map_err(map_sys)
    }

    /// The bytes of the bootstrap record named `key`, when it exists.
    ///
    /// # Errors
    /// Returns [`WorkspaceError`] when the private record directory cannot be
    /// held or read.
    pub fn bootstrap_record(&self, key: &str) -> Result<Option<Vec<u8>>, WorkspaceError> {
        let records = self.bootstrap_records()?;
        match sysfs::read_child_file(&records, OsStr::new(key)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(SysError::NotFound) => Ok(None),
            Err(error) => Err(map_sys(error)),
        }
    }

    /// Publish the bootstrap record named `key`: created when `expected` is
    /// `None`, otherwise replacing exactly `expected`. Answers whether it was
    /// published; `false` means another writer changed it first.
    ///
    /// # Errors
    /// Returns [`WorkspaceError`] when the private record directory cannot be
    /// held or written.
    pub fn publish_bootstrap_record(
        &self,
        key: &str,
        expected: Option<&[u8]>,
        record: &[u8],
    ) -> Result<bool, WorkspaceError> {
        let records = self.bootstrap_records()?;
        let publish = || true;
        let outcome = match expected {
            None => sysfs::cas_create(&records, OsStr::new(key), record, &publish),
            Some(expected) => {
                sysfs::cas_replace(&records, OsStr::new(key), expected, record, &publish)
            }
        }
        .map_err(map_sys)?;
        Ok(matches!(outcome, CasOutcome::Published))
    }

    /// Resolve the project in the workspace-relative directory `relative` for
    /// binding. The empty path names the workspace itself.
    ///
    /// # Errors
    /// Returns [`WorkspaceError`] for an escaping or symlinked path, a missing
    /// directory or a directory without a `project.godot`.
    pub fn resolve_project(
        &self,
        relative: &Path,
    ) -> Result<ResolvedWorkspaceProject, WorkspaceError> {
        self.guard()?;
        let names = Self::components(relative)?;
        let mut current = reopen_dir(&self.root).map_err(map_sys)?;
        for name in &names {
            current = open_child_dir_no_symlink(&current, name).map_err(map_sys)?;
        }
        let normalized: PathBuf = names.iter().collect();
        match open_child_regular_file(&current, OsStr::new(PROJECT_MARKER)) {
            Ok(_) => {}
            Err(SysError::NotFound) => return Err(WorkspaceError::NoProject(normalized)),
            Err(error) => return Err(map_sys(error)),
        }
        let root_identity = identity_of_dir(&current).map_err(map_sys)?;
        let mut display_root = self.display.clone();
        display_root.extend(&names);
        Ok(ResolvedWorkspaceProject {
            relative: normalized,
            display_root,
            root_identity,
        })
    }

    /// The `project.godot` bytes of the project at `relative`.
    ///
    /// # Errors
    /// Returns [`WorkspaceError`] for an escaping or symlinked path or a
    /// directory without a `project.godot`.
    pub fn read_manifest(&self, relative: &Path) -> Result<Vec<u8>, WorkspaceError> {
        self.guard()?;
        let names = Self::components(relative)?;
        let mut current = reopen_dir(&self.root).map_err(map_sys)?;
        for name in &names {
            current = open_child_dir_no_symlink(&current, name).map_err(map_sys)?;
        }
        match sysfs::read_child_file(&current, OsStr::new(PROJECT_MARKER)) {
            Ok(bytes) => Ok(bytes),
            Err(SysError::NotFound) => Err(WorkspaceError::NoProject(names.iter().collect())),
            Err(error) => Err(map_sys(error)),
        }
    }

    /// List the project directories under the workspace, breadth first,
    /// bounded by observed entries and directory depth. Hidden directories are
    /// not searched, and a project's own subdirectories are not searched.
    ///
    /// # Errors
    /// Returns [`WorkspaceError`] on an identity change or I/O failure.
    pub fn list_projects(
        &self,
        max_entries: usize,
        max_depth: usize,
    ) -> Result<ProjectListing, WorkspaceError> {
        self.guard()?;
        let mut projects = Vec::new();
        let mut complete = true;
        let mut observed = 0_usize;
        let mut pending = std::collections::VecDeque::from([(
            PathBuf::new(),
            reopen_dir(&self.root).map_err(map_sys)?,
            0_usize,
        )]);
        while let Some((relative, dir, depth)) = pending.pop_front() {
            let mut entries = read_dir(&dir).map_err(map_sys)?;
            entries.sort_by(|left, right| left.file_name().cmp(right.file_name()));
            if entries.iter().any(|entry| {
                entry.file_name() == OsStr::new(PROJECT_MARKER)
                    && entry.kind() == DirectoryEntryKind::RegularFile
            }) {
                projects.push(if relative.as_os_str().is_empty() {
                    PathBuf::from(".")
                } else {
                    relative
                });
                continue;
            }
            for entry in entries {
                observed = observed.saturating_add(1);
                if observed > max_entries {
                    complete = false;
                    pending.clear();
                    break;
                }
                if entry.kind() != DirectoryEntryKind::Directory
                    || entry
                        .file_name()
                        .to_str()
                        .is_none_or(|name| name.starts_with('.'))
                {
                    continue;
                }
                if depth + 1 > max_depth {
                    complete = false;
                    continue;
                }
                let child = open_child_dir_no_symlink(&dir, entry.file_name()).map_err(map_sys)?;
                pending.push_back((relative.join(entry.file_name()), child, depth + 1));
            }
        }
        projects.sort();
        Ok(ProjectListing { projects, complete })
    }
}
