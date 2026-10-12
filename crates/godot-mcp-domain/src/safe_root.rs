//! Handle-relative in-root reads and compare-and-swap writes.

use std::cell::RefCell;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

#[cfg(feature = "test-hooks")]
use std::sync::atomic::{AtomicU64, Ordering};

use crate::ErrorKind;
use crate::lease::LeaseInner;
use crate::sysfs::{
    self, CasOutcome, DirectoryEntryKind, PROJECT_MARKER, SysError, identity_of_dir,
    open_child_dir_no_symlink, open_child_regular_file, open_in_root_parent, read_child_file,
    read_dir, reopen_dir, walk_dir_no_symlinks,
};

/// A failure of a `SafeRoot` operation.
#[derive(Debug, thiserror::Error)]
pub enum SafeRootError {
    #[error("path traversal (`..`) is not permitted inside the root")]
    Traversal { path: PathBuf },
    #[error("path names a location outside the project root")]
    OutsideRoot { path: PathBuf },
    #[error("a symlink component is not permitted inside the root")]
    Symlink { path: PathBuf },
    #[error("the project display path no longer names the held object (retarget detected)")]
    IdentityChanged,
    #[error("compare-and-swap preimage did not match the on-disk state")]
    StalePreimage { path: PathBuf },
    #[error("filesystem I/O error: {0}")]
    Io(String),
}

impl SafeRootError {
    #[must_use]
    pub fn error_kind(&self) -> ErrorKind {
        match self {
            Self::Traversal { .. } | Self::OutsideRoot { .. } | Self::Symlink { .. } => {
                ErrorKind::PermissionDenied
            }
            Self::IdentityChanged | Self::StalePreimage { .. } => ErrorKind::Conflict,
            Self::Io(_) => ErrorKind::Internal,
        }
    }
}

/// Descriptor-relative access confined to one leased project root.
pub struct SafeRoot {
    lease: Arc<LeaseInner>,
}

/// Result of one bounded, handle-relative regular-file enumeration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileListing {
    files: Vec<PathBuf>,
    complete: bool,
}

impl FileListing {
    /// Deterministically sorted project-relative regular-file paths.
    #[must_use]
    pub fn files(&self) -> &[PathBuf] {
        &self.files
    }

    /// Whether the entry and depth bounds covered the whole reachable tree.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.complete
    }
}

/// Which CAS publication point a test hook pauses at.
#[cfg(feature = "test-hooks")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CasPublishKind {
    Create,
    Replace,
    Delete,
}

#[cfg(feature = "test-hooks")]
struct CasPublishHook {
    id: u64,
    kind: CasPublishKind,
    path: PathBuf,
    reached: Arc<std::sync::Barrier>,
    resume: Arc<std::sync::Barrier>,
}

#[cfg(feature = "test-hooks")]
static HOOK_COUNTER: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-hooks")]
static CAS_PUBLISH_HOOKS: std::sync::LazyLock<std::sync::Mutex<Vec<Arc<CasPublishHook>>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(Vec::new()));

#[cfg(feature = "test-hooks")]
pub struct CasPublishHookGuard {
    id: u64,
}

#[cfg(feature = "test-hooks")]
impl Drop for CasPublishHookGuard {
    fn drop(&mut self) {
        CAS_PUBLISH_HOOKS
            .lock()
            .expect("CAS hook mutex")
            .retain(|hook| hook.id != self.id);
    }
}

#[cfg(feature = "test-hooks")]
#[must_use]
pub fn install_cas_publish_hook(
    kind: CasPublishKind,
    path: PathBuf,
    reached: Arc<std::sync::Barrier>,
    resume: Arc<std::sync::Barrier>,
) -> CasPublishHookGuard {
    let id = HOOK_COUNTER.fetch_add(1, Ordering::Relaxed);
    CAS_PUBLISH_HOOKS
        .lock()
        .expect("CAS hook mutex")
        .push(Arc::new(CasPublishHook {
            id,
            kind,
            path,
            reached,
            resume,
        }));
    CasPublishHookGuard { id }
}

#[cfg(feature = "test-hooks")]
fn pause_before_publish(kind: CasPublishKind, path: &Path) {
    let hook = CAS_PUBLISH_HOOKS
        .lock()
        .expect("CAS hook mutex")
        .iter()
        .find(|hook| hook.kind == kind && hook.path == path)
        .cloned();
    if let Some(hook) = hook {
        hook.reached.wait();
        hook.resume.wait();
    }
}

#[cfg(not(feature = "test-hooks"))]
fn pause_before_create_publish(_path: &Path) {}
#[cfg(feature = "test-hooks")]
fn pause_before_create_publish(path: &Path) {
    pause_before_publish(CasPublishKind::Create, path);
}

#[cfg(not(feature = "test-hooks"))]
fn pause_before_replace_publish(_path: &Path) {}
#[cfg(feature = "test-hooks")]
fn pause_before_replace_publish(path: &Path) {
    pause_before_publish(CasPublishKind::Replace, path);
}

#[cfg(not(feature = "test-hooks"))]
fn pause_before_delete_publish(_path: &Path) {}
#[cfg(feature = "test-hooks")]
fn pause_before_delete_publish(path: &Path) {
    pause_before_publish(CasPublishKind::Delete, path);
}

impl SafeRoot {
    pub(crate) fn new(lease: Arc<LeaseInner>) -> Self {
        Self { lease }
    }

    /// Whether `display_root` is exactly the project spelling this handle was
    /// acquired for. Effects remain descriptor-relative; this metadata check is
    /// for binding another project-scoped capability to the same acquisition.
    #[must_use]
    pub fn is_bound_to(&self, display_root: &Path) -> bool {
        self.lease.display_root == display_root
    }

    /// Re-prove that the acquisition spelling still names the held root and
    /// `project.godot` objects, for callers that bind a further capability to
    /// this acquisition.
    pub(crate) fn guard(&self) -> Result<(), SafeRootError> {
        self.guard_identity()
    }

    /// Find every in-root directory whose own name is `name`, without
    /// following symlinks and without descending into a match.
    ///
    /// `max_entries` bounds every observed child and `max_depth` bounds path
    /// components below the root, exactly as [`Self::list_regular_files`] does; a
    /// reached bound returns `complete == false` rather than a silently partial
    /// answer. Directories whose name is in `excluded_components` are skipped.
    ///
    /// # Errors
    /// Returns [`SafeRootError`] if identity changes or a held-handle operation
    /// fails.
    pub fn find_directories_named(
        &self,
        name: &OsStr,
        max_entries: usize,
        max_depth: usize,
        excluded_components: &[&OsStr],
    ) -> Result<FileListing, SafeRootError> {
        self.guard_identity()?;
        let mut found = Vec::new();
        let mut observed = 0_usize;
        let mut complete = true;
        let mut pending = vec![(
            PathBuf::new(),
            reopen_dir(&self.lease.root).map_err(|error| map_sys(error, Path::new(".")))?,
            0_usize,
        )];
        while let Some((parent_path, parent, parent_depth)) = pending.pop() {
            let mut entries = read_dir(&parent).map_err(|error| map_sys(error, &parent_path))?;
            entries.sort_by(|left, right| left.file_name().cmp(right.file_name()));
            for entry in entries {
                observed = observed.saturating_add(1);
                if observed > max_entries {
                    complete = false;
                    pending.clear();
                    break;
                }
                if entry.kind() != DirectoryEntryKind::Directory
                    || excluded_components.contains(&entry.file_name())
                {
                    continue;
                }
                let mut relative = parent_path.clone();
                relative.push(entry.file_name());
                if entry.file_name() == name {
                    found.push(relative);
                    continue;
                }
                let child_depth = parent_depth.saturating_add(1);
                if child_depth >= max_depth {
                    complete = false;
                    continue;
                }
                let child = open_child_dir_no_symlink(&parent, entry.file_name())
                    .map_err(|error| map_sys(error, &relative))?;
                pending.push((relative, child, child_depth));
            }
        }
        found.sort();
        self.guard_identity()?;
        Ok(FileListing {
            files: found,
            complete,
        })
    }

    /// Rewalk the acquisition spelling and recompare both stable identities and
    /// the full alias witness before every effect.
    fn guard_identity(&self) -> Result<(), SafeRootError> {
        let walked = walk_dir_no_symlinks(&self.lease.display_root)
            .map_err(|error| map_sys(error, &self.lease.display_root))?;
        if walked.witness() != &self.lease.alias_witness
            || identity_of_dir(walked.dir())
                .map_err(|error| map_sys(error, &self.lease.display_root))?
                != self.lease.key.root
        {
            return Err(SafeRootError::IdentityChanged);
        }
        let project = open_child_regular_file(walked.dir(), PROJECT_MARKER.as_ref())
            .map_err(|error| map_sys(error, Path::new(PROJECT_MARKER)))?;
        if !self
            .lease
            .accept_marker(project)
            .map_err(|error| map_sys(error, Path::new(PROJECT_MARKER)))?
        {
            return Err(SafeRootError::IdentityChanged);
        }
        Ok(())
    }

    fn classify<'a>(&self, rel: &'a Path) -> Result<Vec<&'a std::ffi::OsStr>, SafeRootError> {
        if rel.is_absolute() {
            return Err(SafeRootError::OutsideRoot {
                path: rel.to_path_buf(),
            });
        }
        let mut names = Vec::new();
        for component in rel.components() {
            match component {
                Component::Normal(name) => names.push(name),
                Component::CurDir => {}
                Component::ParentDir => {
                    return Err(SafeRootError::Traversal {
                        path: rel.to_path_buf(),
                    });
                }
                _ => {
                    return Err(SafeRootError::OutsideRoot {
                        path: rel.to_path_buf(),
                    });
                }
            }
        }
        if names.is_empty() {
            return Err(SafeRootError::OutsideRoot {
                path: rel.to_path_buf(),
            });
        }
        Ok(names)
    }

    pub fn read(&self, rel: &Path) -> Result<Vec<u8>, SafeRootError> {
        self.guard_identity()?;
        let names = self.classify(rel)?;
        let (parent, last) =
            open_in_root_parent(&self.lease.root, &names).map_err(|error| map_sys(error, rel))?;
        read_child_file(&parent, last).map_err(|error| map_sys(error, rel))
    }

    /// Enumerate regular files under the held root without reopening the root by
    /// path or following symlink components.
    ///
    /// `max_entries` bounds every observed child, including directories and
    /// skipped objects. `max_depth` bounds path components below the root. A
    /// reached bound returns an incomplete listing rather than silently claiming
    /// the returned paths are exhaustive.
    ///
    /// # Errors
    /// Returns [`SafeRootError`] if identity changes or a held-handle operation
    /// fails.
    pub fn list_regular_files(
        &self,
        max_entries: usize,
        max_depth: usize,
        excluded_components: &[&OsStr],
    ) -> Result<FileListing, SafeRootError> {
        self.guard_identity()?;
        let mut files = Vec::new();
        let mut observed = 0_usize;
        let mut complete = true;
        let mut pending = vec![(
            PathBuf::new(),
            reopen_dir(&self.lease.root).map_err(|error| map_sys(error, Path::new(".")))?,
            0_usize,
        )];

        while let Some((parent_path, parent, parent_depth)) = pending.pop() {
            let mut entries = read_dir(&parent).map_err(|error| map_sys(error, &parent_path))?;
            entries.sort_by(|left, right| left.file_name().cmp(right.file_name()));
            for entry in entries {
                observed = observed.saturating_add(1);
                if observed > max_entries {
                    complete = false;
                    pending.clear();
                    break;
                }
                if excluded_components.contains(&entry.file_name()) {
                    continue;
                }
                let mut relative = parent_path.clone();
                relative.push(entry.file_name());
                match entry.kind() {
                    DirectoryEntryKind::RegularFile => files.push(relative),
                    DirectoryEntryKind::Directory => {
                        let child_depth = parent_depth.saturating_add(1);
                        if child_depth >= max_depth {
                            complete = false;
                            continue;
                        }
                        let child = open_child_dir_no_symlink(&parent, entry.file_name())
                            .map_err(|error| map_sys(error, &relative))?;
                        pending.push((relative, child, child_depth));
                    }
                    DirectoryEntryKind::Symlink | DirectoryEntryKind::Other => {}
                }
            }
        }
        files.sort();
        self.guard_identity()?;
        Ok(FileListing { files, complete })
    }

    pub fn cas_write(
        &self,
        rel: &Path,
        expected: Option<&[u8]>,
        new: &[u8],
    ) -> Result<(), SafeRootError> {
        self.guard_identity()?;
        let names = self.classify(rel)?;
        let (parent, last) =
            open_in_root_parent(&self.lease.root, &names).map_err(|error| map_sys(error, rel))?;
        let guard_error = RefCell::new(None);
        let before_publish = || {
            match expected {
                None => pause_before_create_publish(rel),
                Some(_) => pause_before_replace_publish(rel),
            }
            match self.guard_identity() {
                Ok(()) => true,
                Err(error) => {
                    *guard_error.borrow_mut() = Some(error);
                    false
                }
            }
        };
        let outcome = match expected {
            None => sysfs::cas_create(&parent, last, new, &before_publish),
            Some(preimage) => sysfs::cas_replace(&parent, last, preimage, new, &before_publish),
        }
        .map_err(|error| map_sys(error, rel))?;

        match outcome {
            CasOutcome::Published => Ok(()),
            CasOutcome::Stale => Err(SafeRootError::StalePreimage {
                path: rel.to_path_buf(),
            }),
            CasOutcome::Aborted => Err(guard_error
                .into_inner()
                .unwrap_or(SafeRootError::IdentityChanged)),
        }
    }

    /// Compare-and-swap delete of one in-root regular file, requiring it to
    /// hold exactly `expected`.
    ///
    /// The object that is removed is proven, by identity and bytes, to be the
    /// one that was observed: a replacement that takes the name inside the
    /// remaining window survives and the call reports a stale preimage. A
    /// symlink is never deleted or traversed.
    ///
    /// # Errors
    /// Returns [`SafeRootError::StalePreimage`] when the file is absent or
    /// holds other bytes, and another [`SafeRootError`] on a symlink, traversal
    /// or identity change.
    pub fn cas_delete(&self, rel: &Path, expected: &[u8]) -> Result<(), SafeRootError> {
        self.guard_identity()?;
        let names = self.classify(rel)?;
        let (parent, last) =
            open_in_root_parent(&self.lease.root, &names).map_err(|error| map_sys(error, rel))?;
        let guard_error = RefCell::new(None);
        let before_publish = || {
            pause_before_delete_publish(rel);
            match self.guard_identity() {
                Ok(()) => true,
                Err(error) => {
                    *guard_error.borrow_mut() = Some(error);
                    false
                }
            }
        };
        let outcome = sysfs::cas_delete(&parent, last, expected, &before_publish)
            .map_err(|error| map_sys(error, rel))?;
        match outcome {
            CasOutcome::Published => Ok(()),
            CasOutcome::Stale => Err(SafeRootError::StalePreimage {
                path: rel.to_path_buf(),
            }),
            CasOutcome::Aborted => Err(guard_error
                .into_inner()
                .unwrap_or(SafeRootError::IdentityChanged)),
        }
    }

    /// Compare-and-swap rewrite of one in-root regular file *in place*,
    /// requiring it to hold exactly `expected`.
    ///
    /// Unlike [`Self::cas_write`], the file keeps its object identity. That is
    /// what `project.godot` needs: the lease is bound to that object, so a
    /// replacing publication would read as a retarget. The trade-off is that an
    /// interrupted rewrite can leave a mixed file, which the journal's
    /// pre/post-state proof then classifies as neither state (quarantine).
    ///
    /// # Errors
    /// Returns [`SafeRootError::StalePreimage`] when the file is absent or
    /// holds other bytes, and another [`SafeRootError`] on a symlink,
    /// traversal or identity change.
    pub fn cas_overwrite(
        &self,
        rel: &Path,
        expected: &[u8],
        new: &[u8],
    ) -> Result<(), SafeRootError> {
        self.guard_identity()?;
        let names = self.classify(rel)?;
        let (parent, last) =
            open_in_root_parent(&self.lease.root, &names).map_err(|error| map_sys(error, rel))?;
        let guard_error = RefCell::new(None);
        let before_publish = || {
            pause_before_replace_publish(rel);
            match self.guard_identity() {
                Ok(()) => true,
                Err(error) => {
                    *guard_error.borrow_mut() = Some(error);
                    false
                }
            }
        };
        let outcome = sysfs::cas_overwrite(&parent, last, expected, new, &before_publish)
            .map_err(|error| map_sys(error, rel))?;
        match outcome {
            CasOutcome::Published => Ok(()),
            CasOutcome::Stale => Err(SafeRootError::StalePreimage {
                path: rel.to_path_buf(),
            }),
            CasOutcome::Aborted => Err(guard_error
                .into_inner()
                .unwrap_or(SafeRootError::IdentityChanged)),
        }
    }

    /// Create every missing directory above `rel`, walking from the held root
    /// handle with symlink components forbidden. Existing directories are
    /// reused; the final component is never created.
    ///
    /// # Errors
    /// Returns [`SafeRootError`] on a symlink component, traversal, identity
    /// change or I/O failure.
    pub fn create_parent_dirs(&self, rel: &Path) -> Result<(), SafeRootError> {
        self.guard_identity()?;
        let names = self.classify(rel)?;
        sysfs::create_in_root_parent(&self.lease.root, &names)
            .map_err(|error| map_sys(error, rel))?;
        Ok(())
    }
}

fn map_sys(error: SysError, path: &Path) -> SafeRootError {
    match error {
        SysError::SymlinkComponent => SafeRootError::Symlink {
            path: path.to_path_buf(),
        },
        SysError::AlreadyExists | SysError::NotFound => SafeRootError::StalePreimage {
            path: path.to_path_buf(),
        },
        SysError::BadComponent | SysError::NotAbsolute => SafeRootError::OutsideRoot {
            path: path.to_path_buf(),
        },
        other => SafeRootError::Io(other.to_string()),
    }
}
