//! Canonical project identity and `project.godot` semantics.
//!
//! This module resolves *any* in-project path — a nested script, a subdirectory,
//! or the root itself — to one canonical project, and parses that project's
//! `project.godot` manifest.
//!
//! ## Identity model (reuses Todo 2 verbatim)
//!
//! Path authority is **not** re-implemented here. Discovery walks with the
//! domain layer's handle-relative primitives, so a symlink component anywhere in
//! the candidate spelling or in the `project.godot` path fails the walk
//! unconditionally — before any lease is acquired and before the manifest is
//! read — even when the spelling happens to resolve to the very object that is
//! already held. Ownership identity itself is the domain layer's
//! [`LeaseKey`](godot_mcp_domain::objid::LeaseKey): the stable OS-object identity
//! of the root directory composed with that of its `project.godot` file. Two
//! symlink-free spellings that reach the same objects — a lexical/case alias, or
//! an explicitly supported junction/bind alias — therefore converge onto one
//! identity and one lease, while a retargeted alias is rejected.
//!
//! A [`ResolvedProject`] never invents a path-only ownership identity and never
//! reacquires an effect handle by display path: every read goes through the held
//! lease's [`SafeRoot`], which re-verifies both stable identities and the full
//! alias witness before each effect.
//!
//! ## Boundaries
//!
//! * The parser never starts the engine, never shells out and never executes
//!   project code; it only reads text through held descriptors.
//! * Discovery never traverses above the chosen root once the root is chosen, and
//!   in-root reads reject `..`, absolute paths and in-root symlinks.
//! * `.godot/` is the engine's cache directory, never project source: reading
//!   through it is refused.

mod settings;

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::contracts::{ContractViolation, ProjectIdentity};
use godot_mcp_domain::lease::{Lease, LeaseError, LeaseManager};
use godot_mcp_domain::objid::LeaseKey;
use godot_mcp_domain::safe_root::{SafeRoot, SafeRootError};
use godot_mcp_domain::sysfs::{
    PROJECT_MARKER, SysError, open_child_dir_no_symlink, open_child_regular_file,
    walk_dir_no_symlinks,
};

pub(crate) use settings::resource_target;
pub use settings::{
    ApplicationMetadata, AutoloadEntry, InputActionEntry, PROJECT_MANIFEST, ProjectComment,
    ProjectDiagnostic, ProjectDiagnosticKind, ProjectSettings, RawSetting, ResourceBinding,
    ResourceTarget, SettingInterpretation,
};

/// The engine's cache directory name. Never project source.
pub const ENGINE_CACHE_DIR: &str = ".godot";

/// A failure to resolve or read a Godot project.
#[derive(Debug, thiserror::Error)]
pub enum ProjectError {
    /// The candidate path was not absolute, so no trusted anchor exists.
    #[error("the candidate path must be absolute: {path}")]
    NotAbsolute {
        /// The rejected spelling.
        path: PathBuf,
    },
    /// A component of the candidate spelling, or of the `project.godot` path, is
    /// a symlink. Rejected unconditionally, before lease acquisition, even when
    /// the spelling resolves to an already-held object.
    #[error("a symlink component is not permitted in {path}")]
    SymlinkComponent {
        /// The spelling being inspected when the symlink was found.
        path: PathBuf,
    },
    /// No `project.godot` exists at or above the candidate.
    #[error("no {marker} found at or above {start}")]
    NotFound {
        /// The marker filename searched for.
        marker: &'static str,
        /// The starting spelling.
        start: PathBuf,
    },
    /// A `project.godot` entry exists but is not a regular file.
    #[error("{PROJECT_MANIFEST} at {path} is not a regular file")]
    MarkerNotRegularFile {
        /// Directory holding the offending entry.
        path: PathBuf,
    },
    /// Another owner holds this project.
    #[error("the project is already owned by another process")]
    Conflict {
        /// Display spelling of the contested root.
        path: PathBuf,
    },
    /// The display path no longer names the held objects, or the traversed alias
    /// evidence changed: a retarget or namespace substitution.
    #[error("the project display path no longer names the held objects")]
    IdentityChanged,
    /// A relative path named a location outside the project root, used `..`, was
    /// absolute, or traversed an in-root symlink.
    #[error("path names a location outside the project root: {path}")]
    OutsideRoot {
        /// The rejected relative spelling.
        path: PathBuf,
    },
    /// A path reached into the engine cache directory, which is not source.
    #[error("{ENGINE_CACHE_DIR} is engine cache, not project source: {path}")]
    EngineCache {
        /// The rejected relative spelling.
        path: PathBuf,
    },
    /// The manifest bytes are not valid UTF-8 and will not be lossily reread.
    #[error("{PROJECT_MANIFEST} is not valid UTF-8")]
    NotUtf8 {
        /// Byte offset of the first invalid sequence.
        offset: usize,
    },
    /// The manifest parser could not produce an honest record.
    #[error("{PROJECT_MANIFEST} could not be modelled: {message}")]
    Parse {
        /// Stable outcome category of the violated invariant.
        kind: ErrorKind,
        /// Explanation of the violated invariant.
        message: String,
    },
    /// The lease state directory or lock file is not private and owned.
    #[error("insecure project-lease state: {reason}")]
    InsecureState {
        /// Explanation of the rejection.
        reason: String,
    },
    /// Project ownership is unavailable on this platform.
    #[error("project ownership is unsupported on this platform")]
    Unsupported,
    /// A genuine filesystem failure.
    #[error("filesystem error: {0}")]
    Io(String),
}

impl ProjectError {
    /// Stable outcome category for this failure.
    #[must_use]
    pub fn error_kind(&self) -> ErrorKind {
        match self {
            Self::NotAbsolute { .. } | Self::NotUtf8 { .. } | Self::MarkerNotRegularFile { .. } => {
                ErrorKind::InvalidInput
            }
            Self::SymlinkComponent { .. }
            | Self::OutsideRoot { .. }
            | Self::EngineCache { .. }
            | Self::InsecureState { .. } => ErrorKind::PermissionDenied,
            Self::NotFound { .. } => ErrorKind::NotFound,
            Self::Conflict { .. } | Self::IdentityChanged => ErrorKind::Conflict,
            Self::Unsupported => ErrorKind::Unsupported,
            Self::Parse { kind, .. } => *kind,
            Self::Io(_) => ErrorKind::Internal,
        }
    }
}

impl From<ContractViolation> for ProjectError {
    fn from(violation: ContractViolation) -> Self {
        Self::Parse {
            kind: violation.kind(),
            message: violation.to_string(),
        }
    }
}

/// Resolves in-project paths to canonical projects, converging aliases of the
/// same objects onto one owned project.
pub struct ProjectResolver {
    leases: LeaseManager,
}

impl ProjectResolver {
    /// Build a resolver whose advisory-lock state lives under `state_dir`. The
    /// directory is created, and proven user-private, only when a project is
    /// actually acquired.
    #[must_use]
    pub fn new(state_dir: &Path) -> Self {
        Self {
            leases: LeaseManager::new(state_dir),
        }
    }

    /// Resolve `candidate` — a project root, a subdirectory, or a nested file —
    /// to its canonical project, acquiring (or converging onto) the project
    /// lease and parsing `project.godot`.
    ///
    /// `generation` is the project generation the read is attributed to and is
    /// carried into every emitted provenance record.
    ///
    /// # Errors
    /// Returns [`ProjectError::SymlinkComponent`] for a symlink component
    /// anywhere in the spelling (before any lease or file effect),
    /// [`ProjectError::NotFound`] when no manifest exists at or above the
    /// candidate, [`ProjectError::Conflict`] when another owner holds the
    /// project, or another [`ProjectError`] on failure.
    pub fn resolve(
        &self,
        candidate: &Path,
        generation: u64,
    ) -> Result<ResolvedProject, ProjectError> {
        let display_root = discover_display_root(candidate)?;
        let lease = self
            .leases
            .acquire(&display_root)
            .map_err(|error| map_lease(error, &display_root))?;
        let settings = read_settings(&lease.safe_root(), generation)?;
        let identity = ProjectIdentity::from_lease_key(lease.lease_key(), generation);
        Ok(ResolvedProject {
            display_root,
            lease,
            identity,
            settings,
        })
    }
}

/// One canonically identified, owned Godot project.
///
/// The display root is metadata only. Every effect flows through the held lease,
/// so a retargeted display path is detected and refused rather than followed.
pub struct ResolvedProject {
    display_root: PathBuf,
    lease: Lease,
    identity: ProjectIdentity,
    settings: ProjectSettings,
}

impl std::fmt::Debug for ResolvedProject {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResolvedProject")
            .field("display_root", &self.display_root)
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

impl ResolvedProject {
    /// Build analysis state from an already-held project lease without
    /// reacquiring the advisory lock.
    ///
    /// # Errors
    /// Returns a project read or parse failure when the held manifest cannot be
    /// modelled at `generation`.
    pub fn from_lease(lease: Lease, generation: u64) -> Result<Self, ProjectError> {
        let display_root = lease.display_root().to_path_buf();
        let settings = read_settings(&lease.safe_root(), generation)?;
        let identity = ProjectIdentity::from_lease_key(lease.lease_key(), generation);
        Ok(Self {
            display_root,
            lease,
            identity,
            settings,
        })
    }

    /// The held project lease this resolution owns.
    #[must_use]
    pub const fn lease(&self) -> &Lease {
        &self.lease
    }

    /// Normalized display spelling of the project root. Never an authority.
    #[must_use]
    pub fn display_root(&self) -> &Path {
        &self.display_root
    }

    /// Public, path-free project identity (opaque stable object id plus
    /// generation).
    #[must_use]
    pub const fn identity(&self) -> &ProjectIdentity {
        &self.identity
    }

    /// The held-object ownership key backing [`Self::identity`].
    #[must_use]
    pub fn lease_key(&self) -> LeaseKey {
        self.lease.lease_key()
    }

    /// Whether both handles designate the identical owned project.
    #[must_use]
    pub fn same_project(&self, other: &Self) -> bool {
        self.lease.same_lease(&other.lease)
    }

    /// Parsed `project.godot` settings as of the resolving read.
    #[must_use]
    pub const fn settings(&self) -> &ProjectSettings {
        &self.settings
    }

    /// Re-read and re-parse `project.godot` through the held lease.
    ///
    /// # Errors
    /// Returns [`ProjectError::IdentityChanged`] when the display path no longer
    /// names the held objects, or another [`ProjectError`] on failure.
    pub fn reload_settings(&self, generation: u64) -> Result<ProjectSettings, ProjectError> {
        read_settings(&self.lease.safe_root(), generation)
    }

    /// Read one in-root project source file through the held lease.
    ///
    /// `relative` must stay inside the root: an absolute path, a `..` component,
    /// an in-root symlink, or any path through [`ENGINE_CACHE_DIR`] is refused.
    ///
    /// # Errors
    /// Returns [`ProjectError::EngineCache`] for a `.godot/` path,
    /// [`ProjectError::OutsideRoot`] for an escaping or symlinked path,
    /// [`ProjectError::IdentityChanged`] after a retarget, or another
    /// [`ProjectError`] on failure.
    pub fn read_source_file(&self, relative: &Path) -> Result<Vec<u8>, ProjectError> {
        if is_engine_cache(relative) {
            return Err(ProjectError::EngineCache {
                path: relative.to_path_buf(),
            });
        }
        self.lease
            .safe_root()
            .read(relative)
            .map_err(|error| map_safe_root(error, relative))
    }
}

/// Read and parse the manifest through the held lease.
fn read_settings(safe_root: &SafeRoot, generation: u64) -> Result<ProjectSettings, ProjectError> {
    let marker = Path::new(PROJECT_MARKER);
    let bytes = safe_root
        .read(marker)
        .map_err(|error| map_safe_root(error, marker))?;
    let text = String::from_utf8(bytes).map_err(|error| ProjectError::NotUtf8 {
        offset: error.utf8_error().valid_up_to(),
    })?;
    ProjectSettings::parse(&text, generation).map_err(ProjectError::from)
}

/// Walk upward from `candidate` to the nearest directory that directly contains a
/// regular `project.godot`, returning that directory's normalized display
/// spelling.
///
/// Every component of the candidate spelling is opened nofollow before a later
/// `..` is honored, so a symlink cancelled by `..` is still rejected. Ascent is
/// purely descriptor based: no display path is re-resolved mid-walk.
fn discover_display_root(candidate: &Path) -> Result<PathBuf, ProjectError> {
    if !candidate.is_absolute() {
        return Err(ProjectError::NotAbsolute {
            path: candidate.to_path_buf(),
        });
    }
    let start = start_directory(candidate)?;
    let mut walked =
        walk_dir_no_symlinks(&start).map_err(|error| map_sys(error, &start, &start))?;
    loop {
        let here = walked.display();
        // The engine writes its own artifacts — including a `project.godot` copy —
        // into `.godot/`. Nothing under that directory is project source, so a
        // marker found there is never a project root: ascend past it instead of
        // adopting the engine's cached copy as the project.
        if is_engine_cache(&here) {
            if !walked.ascend() {
                return Err(ProjectError::NotFound {
                    marker: PROJECT_MARKER,
                    start: candidate.to_path_buf(),
                });
            }
            continue;
        }
        match open_child_regular_file(walked.dir(), OsStr::new(PROJECT_MARKER)) {
            Ok(_marker) => return Ok(here),
            Err(SysError::NotFound) => {}
            Err(SysError::NotRegularFile) => {
                return Err(ProjectError::MarkerNotRegularFile { path: here });
            }
            Err(error) => {
                return Err(map_sys(error, &here.join(PROJECT_MARKER), candidate));
            }
        }
        if !walked.ascend() {
            return Err(ProjectError::NotFound {
                marker: PROJECT_MARKER,
                start: candidate.to_path_buf(),
            });
        }
    }
}

/// The directory the upward search starts from: `candidate` itself when it names
/// a directory, otherwise its lexical parent (so a nested script resolves its
/// project). Both spellings are inspected nofollow, so a symlink at any
/// component — including the final one — is rejected here, before any lease.
fn start_directory(candidate: &Path) -> Result<PathBuf, ProjectError> {
    let (Some(parent), Some(name)) = (candidate.parent(), candidate.file_name()) else {
        // The filesystem root, or a spelling whose final component is `.`/`..`:
        // the candidate itself is walked, which inspects every component.
        return Ok(candidate.to_path_buf());
    };
    let walked = walk_dir_no_symlinks(parent).map_err(|error| map_sys(error, parent, candidate))?;
    match open_child_dir_no_symlink(walked.dir(), name) {
        Ok(_directory) => Ok(candidate.to_path_buf()),
        Err(SysError::SymlinkComponent) => Err(ProjectError::SymlinkComponent {
            path: candidate.to_path_buf(),
        }),
        Err(SysError::NotFound) => Err(ProjectError::NotFound {
            marker: PROJECT_MARKER,
            start: candidate.to_path_buf(),
        }),
        // Anything else (most commonly "not a directory") means the candidate
        // named a file: its parent directory is the search start.
        Err(_) => Ok(parent.to_path_buf()),
    }
}

/// Whether `path` names, or lies inside, the engine's cache directory.
fn is_engine_cache(path: &Path) -> bool {
    path.components()
        .any(|component| component.as_os_str() == OsStr::new(ENGINE_CACHE_DIR))
}

fn map_sys(error: SysError, path: &Path, candidate: &Path) -> ProjectError {
    match error {
        SysError::SymlinkComponent => ProjectError::SymlinkComponent {
            path: path.to_path_buf(),
        },
        SysError::NotAbsolute => ProjectError::NotAbsolute {
            path: candidate.to_path_buf(),
        },
        SysError::NotFound => ProjectError::NotFound {
            marker: PROJECT_MARKER,
            start: candidate.to_path_buf(),
        },
        SysError::NotRegularFile => ProjectError::MarkerNotRegularFile {
            path: path.to_path_buf(),
        },
        SysError::NotPrivate(reason) => ProjectError::InsecureState { reason },
        other => ProjectError::Io(other.to_string()),
    }
}

fn map_lease(error: LeaseError, display_root: &Path) -> ProjectError {
    match error {
        LeaseError::SymlinkComponent { path } => ProjectError::SymlinkComponent { path },
        LeaseError::NotFound { .. } => ProjectError::NotFound {
            marker: PROJECT_MARKER,
            start: display_root.to_path_buf(),
        },
        LeaseError::Conflict { path } => ProjectError::Conflict { path },
        LeaseError::InsecureStateDir { reason, .. } => ProjectError::InsecureState { reason },
        LeaseError::Unsupported => ProjectError::Unsupported,
        LeaseError::Io(reason) => ProjectError::Io(reason),
    }
}

fn map_safe_root(error: SafeRootError, relative: &Path) -> ProjectError {
    match error {
        SafeRootError::Traversal { path } | SafeRootError::OutsideRoot { path } => {
            ProjectError::OutsideRoot { path }
        }
        SafeRootError::Symlink { path } => ProjectError::SymlinkComponent { path },
        SafeRootError::IdentityChanged => ProjectError::IdentityChanged,
        SafeRootError::StalePreimage { .. } => ProjectError::NotFound {
            marker: PROJECT_MARKER,
            start: relative.to_path_buf(),
        },
        SafeRootError::Io(reason) => ProjectError::Io(reason),
    }
}
