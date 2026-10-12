//! Advisory project ownership keyed by stable opened-object identities.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::ErrorKind;
use crate::objid::{AliasWitness, LeaseKey, StableObjectId};
use crate::project::{DiscoveredProject, DiscoveryError, discover_project_root};
use crate::safe_root::SafeRoot;
use crate::sysfs::{
    CasOutcome, DirHandle, FileHandle, SysError, cas_create, cas_replace, identity_of_dir,
    identity_of_file, link_count_of_file, open_child_dir_no_symlink, open_private_child_state_dir,
    open_private_lock_file, open_private_state_dir, read_child_file, reopen_dir,
    try_lock_exclusive, walk_dir_no_symlinks,
};

/// Name of the private state directory a project-local lease keeps inside the
/// acquired project root.
///
/// The directory is opened relative to the *held* root handle, never derived
/// from a caller's candidate spelling, so every spelling of one project object
/// converges on one state directory, one lock file and one journal.
pub const PROJECT_STATE_DIR_NAME: &str = ".godot-mcp";

/// A project-ownership failure. Each maps to the domain error taxonomy.
#[derive(Debug, thiserror::Error)]
pub enum LeaseError {
    /// A symlink component was found in the root or `project.godot` path.
    #[error("a symlink component in the project root or project.godot path is not permitted")]
    SymlinkComponent { path: PathBuf },
    /// No `project.godot` was discoverable at or above the candidate.
    #[error("no project.godot discoverable: {reason}")]
    NotFound { reason: String },
    /// Another process already holds this project.
    #[error("project is already owned by another process")]
    Conflict { path: PathBuf },
    /// The advisory-lock state directory or lock file is not private and owned.
    #[error("insecure project-lease state: {reason}")]
    InsecureStateDir { path: PathBuf, reason: String },
    /// This platform backend is unavailable.
    #[error("project ownership is unsupported on this platform")]
    Unsupported,
    /// An underlying I/O error.
    #[error("filesystem I/O error: {0}")]
    Io(String),
}

impl LeaseError {
    #[must_use]
    pub fn error_kind(&self) -> ErrorKind {
        match self {
            Self::SymlinkComponent { .. } | Self::InsecureStateDir { .. } => {
                ErrorKind::PermissionDenied
            }
            Self::Conflict { .. } => ErrorKind::Conflict,
            Self::NotFound { .. } => ErrorKind::NotFound,
            Self::Unsupported => ErrorKind::Unsupported,
            Self::Io(_) => ErrorKind::Internal,
        }
    }
}

/// Derive a deterministic lock filename from the platform-tagged stable root and
/// marker identities. Alias witness/mount evidence is intentionally excluded.
#[must_use]
pub fn lock_file_name(key: &LeaseKey) -> String {
    fn bind_identity(hasher: &mut Sha256, label: &[u8], identity: StableObjectId) {
        hasher.update((label.len() as u64).to_le_bytes());
        hasher.update(label);
        identity.encode(|field| {
            hasher.update((field.len() as u64).to_le_bytes());
            hasher.update(field);
        });
    }

    let mut hasher = Sha256::new();
    hasher.update(b"godot-mcp-project-lease-v2");
    bind_identity(&mut hasher, b"root", key.root);
    bind_identity(&mut hasher, b"project.godot", key.project_file);
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut hex, "{byte:02x}").expect("writing to String cannot fail");
    }
    format!("lease-{hex}.lock")
}

/// Derive the deterministic name of the lock that makes one project *root*
/// object single-owner, whatever object its `project.godot` currently is.
///
/// [`lock_file_name`] binds the marker too, so on its own it cannot exclude a
/// second owner once the marker has been replaced — which the Godot editor does
/// on startup and on every settings save. Every acquisition therefore takes
/// this lock first and the composite lock second (the composite one still
/// excludes an older build that only knows it).
#[must_use]
pub fn root_lock_file_name(root: StableObjectId) -> String {
    format!(
        "root-lease-{}.lock",
        identity_digest(b"godot-mcp-project-root-lock-v1", root)
    )
}

fn identity_digest(domain: &[u8], identity: StableObjectId) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    identity.encode(|field| {
        hasher.update((field.len() as u64).to_le_bytes());
        hasher.update(field);
    });
    let mut hex = String::with_capacity(64);
    for byte in hasher.finalize() {
        use std::fmt::Write as _;
        write!(&mut hex, "{byte:02x}").expect("writing to String cannot fail");
    }
    hex
}

/// What a lease does when `project.godot` turns out to be a different object
/// than the one it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MarkerPolicy {
    /// Any replacement of the marker refuses every later effect.
    #[default]
    Strict,
    /// A replacement by a regular, single-link file directly in the same held
    /// root is accepted and becomes the held marker; the lease keeps the key
    /// it was first bound with, recorded in its state directory, so a restart
    /// after a replacement is the same project. A marker that is a symlink, a
    /// directory or a second link to another file still refuses, and a
    /// retargeted root always refuses.
    ///
    /// This is the policy for a server working alongside a live Godot editor,
    /// which saves `project.godot` by writing a new file and renaming it over
    /// the old one.
    FollowReplacement,
}

/// Schema of the per-root key record a [`MarkerPolicy::FollowReplacement`]
/// lease keeps in its state directory.
const KEY_RECORD_SCHEMA: &str = "godot-mcp.project-key.v1";

fn key_record_name(project_local: bool, root: StableObjectId) -> String {
    if project_local {
        "project-key.json".to_owned()
    } else {
        let digest = identity_digest(b"godot-mcp-project-key-record-v1", root);
        format!("project-key-{}.json", &digest[..32])
    }
}

fn key_record(key: LeaseKey) -> Vec<u8> {
    let mut text = serde_json::to_string(&serde_json::json!({
        "schema": KEY_RECORD_SCHEMA,
        "root": key.root.to_record_string(),
        "project_file": key.project_file.to_record_string(),
    }))
    .expect("a key record serializes");
    text.push('\n');
    text.into_bytes()
}

fn parse_key_record(bytes: &[u8]) -> Option<LeaseKey> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    if value.get("schema")?.as_str()? != KEY_RECORD_SCHEMA {
        return None;
    }
    Some(LeaseKey {
        root: StableObjectId::parse_record_string(value.get("root")?.as_str()?)?,
        project_file: StableObjectId::parse_record_string(value.get("project_file")?.as_str()?)?,
    })
}

/// The marker identity this root was first bound with, read from (or, on the
/// first acquisition, written to) the key record. Runs under the root lock, so
/// no other owner of this root is writing the record concurrently. A record
/// naming another root — a project copied together with its state directory —
/// is replaced rather than trusted.
fn recorded_marker(
    state_dir: &DirHandle,
    project_local: bool,
    root: StableObjectId,
    current: StableObjectId,
    display_root: &Path,
) -> Result<StableObjectId, LeaseError> {
    let name = key_record_name(project_local, root);
    let existing = match read_child_file(state_dir, name.as_ref()) {
        Ok(bytes) => Some(bytes),
        Err(SysError::NotFound) => None,
        Err(error) => return Err(classify_private(error, display_root)),
    };
    if let Some(recorded) = existing.as_deref().and_then(parse_key_record)
        && recorded.root == root
    {
        return Ok(recorded.project_file);
    }
    let record = key_record(LeaseKey {
        root,
        project_file: current,
    });
    let outcome = match &existing {
        None => cas_create(state_dir, name.as_ref(), &record, &|| true),
        Some(previous) => cas_replace(state_dir, name.as_ref(), previous, &record, &|| true),
    }
    .map_err(|error| classify_private(error, display_root))?;
    match outcome {
        CasOutcome::Published => Ok(current),
        CasOutcome::Stale | CasOutcome::Aborted => Err(LeaseError::Conflict {
            path: display_root.to_path_buf(),
        }),
    }
}

/// How long a contended lock is retried before it is reported as held.
///
/// An `flock` lock belongs to the open file description, and `FD_CLOEXEC`
/// closes a descriptor at `exec`, not at `fork`: while a thread of this process
/// starts a child, the child holds a duplicate of every descriptor open here,
/// a lock file this process has just released included. That hold ends when
/// the child executes its program, milliseconds later on a loaded host, while a
/// live owner keeps the lock for as long as it runs. Retrying for a moment tells
/// the two apart; a live owner is still refused, only that much later.
const LOCK_CONTENTION_GRACE: Duration = Duration::from_millis(500);
const LOCK_CONTENTION_RETRY: Duration = Duration::from_millis(10);

fn lock_exclusive_within_grace(file: &FileHandle) -> Result<(), SysError> {
    let deadline = Instant::now() + LOCK_CONTENTION_GRACE;
    loop {
        match try_lock_exclusive(file) {
            Err(SysError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(LOCK_CONTENTION_RETRY);
            }
            outcome => return outcome,
        }
    }
}

fn take_lock(
    state_dir: &DirHandle,
    state_root: &Path,
    name: &str,
    display_root: &Path,
) -> Result<FileHandle, LeaseError> {
    let lock_path = state_root.join(name);
    let lock_file = open_private_lock_file(state_dir, name)
        .map_err(|error| classify_private(error, &lock_path))?;
    match lock_exclusive_within_grace(&lock_file) {
        Ok(()) => Ok(lock_file),
        Err(SysError::WouldBlock) => Err(LeaseError::Conflict {
            path: display_root.to_path_buf(),
        }),
        Err(error) => Err(classify_sys_io(error)),
    }
}

/// The `project.godot` object a lease currently holds.
struct HeldMarker {
    _handle: FileHandle,
    identity: StableObjectId,
}

/// Shared state pinned by every lease and `SafeRoot`.
pub struct LeaseInner {
    pub(crate) key: LeaseKey,
    pub(crate) display_root: PathBuf,
    pub(crate) alias_witness: AliasWitness,
    pub(crate) root: DirHandle,
    state_root: PathBuf,
    project_local: bool,
    marker: Mutex<HeldMarker>,
    marker_policy: MarkerPolicy,
    marker_replacements: AtomicU64,
    _root_lock_file: FileHandle,
    _lock_file: FileHandle,
    state_dir: DirHandle,
}

impl LeaseInner {
    /// Check a freshly opened `project.godot` against the held marker.
    ///
    /// The same object is accepted. Under [`MarkerPolicy::FollowReplacement`] a
    /// regular single-link replacement (the caller opened it as a regular file
    /// without following symlinks) is accepted too and becomes the held marker.
    /// Anything else returns `false`, and the caller refuses the effect.
    pub(crate) fn accept_marker(&self, opened: FileHandle) -> Result<bool, SysError> {
        let identity = identity_of_file(&opened)?;
        let mut held = self.marker.lock().expect("lease marker mutex");
        if held.identity == identity {
            return Ok(true);
        }
        if self.marker_policy == MarkerPolicy::Strict || link_count_of_file(&opened)? != 1 {
            return Ok(false);
        }
        *held = HeldMarker {
            _handle: opened,
            identity,
        };
        self.marker_replacements.fetch_add(1, Ordering::Relaxed);
        Ok(true)
    }
}

/// Public ownership handle.
#[derive(Clone)]
pub struct Lease {
    inner: Arc<LeaseInner>,
}

impl std::fmt::Debug for Lease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lease")
            .field("key", &self.inner.key)
            .field("display_root", &self.inner.display_root)
            .finish()
    }
}

impl Lease {
    #[must_use]
    pub fn lease_key(&self) -> LeaseKey {
        self.inner.key
    }

    /// Normalized display spelling captured when the project was acquired.
    /// This is metadata only; effects continue to use the held root handle.
    #[must_use]
    pub fn display_root(&self) -> &Path {
        &self.inner.display_root
    }

    #[must_use]
    pub fn same_lease(&self, other: &Lease) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// How many times this lease followed a replaced `project.godot`.
    #[must_use]
    pub fn marker_replacements(&self) -> u64 {
        self.inner.marker_replacements.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn safe_root(&self) -> SafeRoot {
        SafeRoot::new(Arc::clone(&self.inner))
    }

    /// Display spelling of the private state directory holding this lease's
    /// lock. For a [`LeaseManager::project_local`] lease it is the discovered
    /// root joined with [`PROJECT_STATE_DIR_NAME`]; it is metadata only, and the
    /// held state-directory handle remains the authority.
    #[must_use]
    pub fn state_root(&self) -> &Path {
        &self.inner.state_root
    }

    /// Re-walk the discovered root and the state root and prove both spellings
    /// still name the held objects.
    ///
    /// # Errors
    /// Returns [`LeaseError::Conflict`] when either spelling now names another
    /// object, or another [`LeaseError`] when the walk fails.
    pub fn revalidate(&self) -> Result<(), LeaseError> {
        self.safe_root()
            .guard()
            .map_err(|error| LeaseError::Io(error.to_string()))?;
        let held_state = identity_of_dir(&self.inner.state_dir).map_err(classify_sys_io)?;
        let observed_state = if self.inner.project_local {
            let walked = walk_dir_no_symlinks(&self.inner.display_root)
                .map_err(|error| classify_private(error, &self.inner.display_root))?;
            let state = open_child_dir_no_symlink(walked.dir(), PROJECT_STATE_DIR_NAME.as_ref())
                .map_err(|error| classify_private(error, &self.inner.state_root))?;
            identity_of_dir(&state).map_err(classify_sys_io)?
        } else {
            let walked = walk_dir_no_symlinks(&self.inner.state_root)
                .map_err(|error| classify_private(error, &self.inner.state_root))?;
            identity_of_dir(walked.dir()).map_err(classify_sys_io)?
        };
        if observed_state != held_state {
            return Err(LeaseError::Conflict {
                path: self.inner.state_root.clone(),
            });
        }
        Ok(())
    }

    /// Take this lease's advisory lock inside another state directory found
    /// under the held root, such as a candidate-derived `.godot-mcp` directory a
    /// version 0.1 server created when it was launched from a subdirectory.
    ///
    /// The directory is reached component by component from the held root with
    /// symlinks refused, and the lock file uses this lease's own key-derived
    /// name, which is exactly the name a version 0.1 owner of the same project
    /// object used. Holding the returned guard therefore keeps such an owner from
    /// starting, and a live owner is reported as a conflict.
    ///
    /// # Errors
    /// Returns [`LeaseError::Conflict`] when another process holds the lock,
    /// [`LeaseError::SymlinkComponent`] or [`LeaseError::InsecureStateDir`] for a
    /// directory that is not a private, symlink-free state directory, or
    /// [`LeaseError::Io`] on failure.
    pub fn lock_sibling_state_dir(&self, relative: &Path) -> Result<SiblingStateLock, LeaseError> {
        let mut directory = reopen_dir(&self.inner.root).map_err(classify_sys_io)?;
        let mut display = self.inner.display_root.clone();
        for component in relative.components() {
            let std::path::Component::Normal(name) = component else {
                return Err(LeaseError::Io(
                    "a sibling state directory must be a plain in-root relative path".to_owned(),
                ));
            };
            display.push(name);
            directory = open_child_dir_no_symlink(&directory, name)
                .map_err(|error| classify_private(error, &display))?;
        }
        let lock_name = lock_file_name(&self.inner.key);
        let lock_path = display.join(&lock_name);
        let lock_file = open_private_lock_file(&directory, &lock_name)
            .map_err(|error| classify_private(error, &lock_path))?;
        match lock_exclusive_within_grace(&lock_file) {
            Ok(()) => Ok(SiblingStateLock {
                _lock_file: lock_file,
                _directory: directory,
            }),
            Err(SysError::WouldBlock) => Err(LeaseError::Conflict {
                path: self.inner.display_root.clone(),
            }),
            Err(error) => Err(classify_sys_io(error)),
        }
    }
}

/// An advisory lease lock held inside a sibling (legacy) state directory. The
/// lock is released when the guard is dropped.
pub struct SiblingStateLock {
    _lock_file: FileHandle,
    _directory: DirHandle,
}

impl std::fmt::Debug for SiblingStateLock {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SiblingStateLock")
    }
}

/// Where a manager keeps the private directory holding its lock files.
enum StateLocation {
    /// One fixed private directory chosen by the caller.
    Fixed(PathBuf),
    /// [`PROJECT_STATE_DIR_NAME`] inside each acquired project root.
    ProjectLocal,
}

/// In-process convergence registry plus cross-process advisory locking.
pub struct LeaseManager {
    state: StateLocation,
    marker_policy: MarkerPolicy,
    /// Keyed by the root object: one project root has one lease, whatever its
    /// marker currently is.
    held: Mutex<HashMap<StableObjectId, Weak<LeaseInner>>>,
}

impl LeaseManager {
    /// A manager whose lock files live in one fixed private directory.
    #[must_use]
    pub fn new(state_dir: &Path) -> Self {
        let state_dir = if state_dir.is_absolute() {
            state_dir.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(state_dir))
                .unwrap_or_else(|_| state_dir.to_path_buf())
        };
        Self {
            state: StateLocation::Fixed(state_dir),
            marker_policy: MarkerPolicy::Strict,
            held: Mutex::new(HashMap::new()),
        }
    }

    /// A manager whose lock lives in [`PROJECT_STATE_DIR_NAME`] inside each
    /// acquired project root.
    ///
    /// The state directory is created and opened relative to the held root
    /// handle *after* discovery, so a root, subdirectory, relative or alias
    /// spelling of one project object always reaches the same lock file. This is
    /// the only state location a server process may use for its own project.
    #[must_use]
    pub fn project_local() -> Self {
        Self {
            state: StateLocation::ProjectLocal,
            marker_policy: MarkerPolicy::Strict,
            held: Mutex::new(HashMap::new()),
        }
    }

    /// The same manager with `policy` for every lease it acquires.
    #[must_use]
    pub fn with_marker_policy(mut self, policy: MarkerPolicy) -> Self {
        self.marker_policy = policy;
        self
    }

    #[must_use]
    pub fn held_lease_count(&self) -> usize {
        let mut held = self.held.lock().expect("lease registry mutex");
        held.retain(|_, lease| lease.strong_count() > 0);
        held.len()
    }

    /// Acquire or converge onto the lease. Reservation and lock acquisition are
    /// serialized under the registry mutex, preventing same-manager contenders
    /// from racing into `flock` and self-conflicting while preserving the real
    /// cross-process lock.
    pub fn acquire(&self, candidate: &Path) -> Result<Lease, LeaseError> {
        let DiscoveredProject {
            display_root,
            root,
            project_file,
            alias_witness,
        } = discover_project_root(candidate).map_err(classify_discovery)?;
        let root_identity = identity_of_dir(&root).map_err(classify_sys_io)?;
        let marker_identity = identity_of_file(&project_file).map_err(classify_sys_io)?;

        let mut held = self.held.lock().expect("lease registry mutex");
        held.retain(|_, lease| lease.strong_count() > 0);
        if let Some(existing) = held.get(&root_identity).and_then(Weak::upgrade) {
            return Ok(Lease { inner: existing });
        }

        let (state_dir, state_root, project_local) = match &self.state {
            StateLocation::Fixed(path) => (
                open_private_state_dir(path).map_err(|error| classify_private(error, path))?,
                path.clone(),
                false,
            ),
            StateLocation::ProjectLocal => {
                let state_root = display_root.join(PROJECT_STATE_DIR_NAME);
                (
                    open_private_child_state_dir(&root, PROJECT_STATE_DIR_NAME.as_ref())
                        .map_err(|error| classify_private(error, &state_root))?,
                    state_root,
                    true,
                )
            }
        };
        // One owner per root object first, whatever its marker is now.
        let root_lock = take_lock(
            &state_dir,
            &state_root,
            &root_lock_file_name(root_identity),
            &display_root,
        )?;
        let key = LeaseKey {
            root: root_identity,
            project_file: match self.marker_policy {
                MarkerPolicy::Strict => marker_identity,
                MarkerPolicy::FollowReplacement => recorded_marker(
                    &state_dir,
                    project_local,
                    root_identity,
                    marker_identity,
                    &display_root,
                )?,
            },
        };
        let lock_file = take_lock(
            &state_dir,
            &state_root,
            &lock_file_name(&key),
            &display_root,
        )?;

        let inner = Arc::new(LeaseInner {
            key,
            display_root,
            alias_witness,
            root,
            state_root,
            project_local,
            marker: Mutex::new(HeldMarker {
                _handle: project_file,
                identity: marker_identity,
            }),
            marker_policy: self.marker_policy,
            marker_replacements: AtomicU64::new(0),
            _root_lock_file: root_lock,
            _lock_file: lock_file,
            state_dir,
        });
        held.insert(root_identity, Arc::downgrade(&inner));
        Ok(Lease { inner })
    }
}

fn classify_private(error: SysError, path: &Path) -> LeaseError {
    match error {
        SysError::SymlinkComponent => LeaseError::SymlinkComponent {
            path: path.to_path_buf(),
        },
        SysError::NotPrivate(reason) => LeaseError::InsecureStateDir {
            path: path.to_path_buf(),
            reason,
        },
        SysError::NotRegularFile => LeaseError::InsecureStateDir {
            path: path.to_path_buf(),
            reason: "object has the wrong filesystem type".to_owned(),
        },
        other => classify_sys_io(other),
    }
}

fn classify_sys_io(error: SysError) -> LeaseError {
    LeaseError::Io(error.to_string())
}

fn classify_discovery(error: DiscoveryError) -> LeaseError {
    match error {
        DiscoveryError::SymlinkComponent { path } => LeaseError::SymlinkComponent { path },
        DiscoveryError::NotFound { .. } => LeaseError::NotFound {
            reason: error.to_string(),
        },
        DiscoveryError::NotAbsolute(reason) => LeaseError::NotFound { reason },
        DiscoveryError::Io { reason, .. } => LeaseError::Io(reason),
    }
}
