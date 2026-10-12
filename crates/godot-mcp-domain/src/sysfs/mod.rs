//! Handle-relative filesystem primitives for handle-safe project ownership.
//!
//! This module is the *only* place platform filesystem calls are issued. Higher
//! layers work exclusively in terms of held handles, [`StableObjectId`] object
//! identities and [`AliasWitness`] alias evidence, never path strings.
//!
//! Invariants every platform backend upholds:
//!
//! * A path is walked component by component from a trusted anchor, opening each
//!   component relative to the previously held handle. A symlink component fails
//!   the open (Unix `O_NOFOLLOW`, Windows `FILE_OPEN_REPARSE_POINT` plus an
//!   explicit reparse-tag check) rather than being followed. Lexical
//!   normalisation never erases a component before it has been inspected, so
//!   `symlink/..` is rejected.
//! * A platform-supported same-object alias (a Linux bind mount, a Windows
//!   directory junction) is traversed, but its evidence is recorded in the
//!   [`AliasWitness`] so a later retarget or namespace substitution is detected.
//! * Every handle is non-inheritable (`O_CLOEXEC`; Windows handles created
//!   without `OBJ_INHERIT`), so no child process can inherit a root, project,
//!   lock or staging handle.
//! * Compare-and-swap publication is atomic and never overwrites or unlinks an
//!   object an external actor published in the meantime.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Component, Path, PathBuf};

use crate::objid::{AliasWitness, StableObjectId};

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as imp;

#[cfg(any(windows, test))]
mod windows_decision;

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows;
#[cfg(windows)]
use windows as imp;

/// The single project-marker filename discovered/held for a project.
pub const PROJECT_MARKER: &str = "project.godot";

/// The private name a compare-and-swap stages its bytes under beside the
/// target, and parks what it replaces or deletes under: `.cas-<pid>-<n>.tmp`.
fn staging_name(counter: u64) -> String {
    format!(".cas-{}-{counter}.tmp", std::process::id())
}

/// Whether `name`, one path component, is such a private staging name. A
/// file under one is a write in flight — the server's own bytes, which a
/// moment later are published under the target's name or removed — and never
/// project source.
#[must_use]
pub fn is_staging_name(name: &str) -> bool {
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    name.strip_prefix(".cas-")
        .and_then(|rest| rest.strip_suffix(".tmp"))
        .and_then(|middle| middle.split_once('-'))
        .is_some_and(|(pid, counter)| digits(pid) && digits(counter))
}

/// A failure of a handle-relative filesystem primitive.
#[derive(Debug)]
pub enum SysError {
    /// A component of the path is (or became) a symlink, or an unsupported
    /// reparse point. Never followed.
    SymlinkComponent,
    /// The path was not absolute where an absolute path was required.
    NotAbsolute,
    /// The path contained a component that is not permitted (an unsupported
    /// prefix, or a traversal where none is allowed).
    BadComponent,
    /// The named object does not exist.
    NotFound,
    /// The named object already exists where exclusive creation was required.
    AlreadyExists,
    /// An advisory lock is already held by another owner.
    WouldBlock,
    /// The held object is not a regular file where one was required.
    NotRegularFile,
    /// The held object is not private to (and owned by) the current user.
    NotPrivate(String),
    /// A genuine I/O error.
    Io(io::Error),
}

impl std::fmt::Display for SysError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SymlinkComponent => f.write_str("a symlink or unsupported reparse component"),
            Self::NotAbsolute => f.write_str("the path is not absolute"),
            Self::BadComponent => f.write_str("the path contains an unsupported component"),
            Self::NotFound => f.write_str("the object does not exist"),
            Self::AlreadyExists => f.write_str("the object already exists"),
            Self::WouldBlock => f.write_str("the advisory lock is held by another owner"),
            Self::NotRegularFile => f.write_str("the object is not a regular file"),
            Self::NotPrivate(reason) => write!(f, "the object is not user-private: {reason}"),
            Self::Io(error) => write!(f, "{error}"),
        }
    }
}

impl SysError {
    fn private(reason: impl Into<String>) -> Self {
        Self::NotPrivate(reason.into())
    }
}

/// A non-inheritable handle to a directory object.
pub struct DirHandle(imp::Dir);

/// A non-inheritable handle to a file object.
pub struct FileHandle(imp::File);

/// Nofollow classification of one held-directory child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectoryEntryKind {
    /// A regular file.
    RegularFile,
    /// A directory.
    Directory,
    /// A symlink or Windows reparse point that must not be followed.
    Symlink,
    /// A device, socket, FIFO, or another unsupported object type.
    Other,
}

/// One child observed relative to a held directory handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryEntry {
    name: OsString,
    kind: DirectoryEntryKind,
}

impl DirectoryEntry {
    /// Child name without any parent path components.
    #[must_use]
    pub fn file_name(&self) -> &OsStr {
        &self.name
    }

    /// Object type observed without following a symlink or reparse point.
    #[must_use]
    pub const fn kind(&self) -> DirectoryEntryKind {
        self.kind
    }
}

/// Outcome of a compare-and-swap publication attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CasOutcome {
    /// The new bytes were published atomically.
    Published,
    /// The on-disk preimage did not match the caller's expectation (or the
    /// expected-absent target already existed), so nothing was published.
    Stale,
    /// The caller's pre-publication re-check refused to proceed. Nothing was
    /// published.
    Aborted,
}

/// A path walked with symlink components forbidden, retaining a handle for the
/// anchor and for every traversed component so ascending never re-resolves a
/// path string.
pub struct WalkedPath {
    /// Display spelling of the anchor (`/` on Unix, `C:\` on Windows).
    anchor_display: PathBuf,
    /// Anchor handle plus one handle per traversed component.
    handles: Vec<DirHandle>,
    /// Display component names, parallel to `handles[1..]`.
    names: Vec<std::ffi::OsString>,
    /// Alias evidence, parallel to `handles[1..]`.
    witness: AliasWitness,
}

impl WalkedPath {
    /// The handle of the object the walk currently designates.
    #[must_use]
    pub fn dir(&self) -> &DirHandle {
        self.handles
            .last()
            .expect("a walked path always retains its anchor handle")
    }

    /// Normalised display metadata for the current position. Never an authority.
    #[must_use]
    pub fn display(&self) -> PathBuf {
        let mut path = self.anchor_display.clone();
        for name in &self.names {
            path.push(name);
        }
        path
    }

    /// Alias evidence for the traversed spelling.
    #[must_use]
    pub fn witness(&self) -> &AliasWitness {
        &self.witness
    }

    /// Ascend to the parent by dropping the deepest held handle. Returns `false`
    /// when already at the anchor (the filesystem/volume root).
    pub fn ascend(&mut self) -> bool {
        if self.handles.len() <= 1 {
            return false;
        }
        self.handles.pop();
        self.names.pop();
        self.witness.pop();
        true
    }

    /// Consume the walk, yielding the designated handle, its display metadata
    /// and its alias evidence.
    #[must_use]
    pub fn into_parts(mut self) -> (DirHandle, PathBuf, AliasWitness) {
        let display = self.display();
        let handle = self
            .handles
            .pop()
            .expect("a walked path always retains its anchor handle");
        (handle, display, self.witness)
    }
}

/// Walk an absolute directory path component by component, refusing to follow a
/// symlink at any component and honouring `..` only *after* the preceding
/// component has been inspected and opened.
///
/// # Errors
/// Returns [`SysError`] describing the first failing component.
pub fn walk_dir_no_symlinks(path: &Path) -> Result<WalkedPath, SysError> {
    if !path.is_absolute() {
        return Err(SysError::NotAbsolute);
    }
    let (anchor, anchor_display, rest) = imp::open_anchor(path)?;
    let mut walked = WalkedPath {
        anchor_display,
        handles: vec![DirHandle(anchor)],
        names: Vec::new(),
        witness: AliasWitness::new(),
    };
    for component in rest.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Prefix(_) => return Err(SysError::BadComponent),
            Component::Normal(name) => {
                let (child, entry) = imp::open_child_dir(&walked.dir().0, name)?;
                walked.handles.push(DirHandle(child));
                walked.names.push(name.to_os_string());
                walked.witness.push(entry);
            }
            Component::ParentDir => {
                if !walked.ascend() {
                    // `..` at the filesystem root designates the root itself.
                }
            }
        }
    }
    Ok(walked)
}

/// Stable object identity of a held directory handle.
///
/// # Errors
/// Returns [`SysError`] if the handle cannot be inspected.
pub fn identity_of_dir(dir: &DirHandle) -> Result<StableObjectId, SysError> {
    imp::identity_of_dir(&dir.0)
}

/// Stable object identity of a held file handle.
///
/// # Errors
/// Returns [`SysError`] if the handle cannot be inspected.
pub fn identity_of_file(file: &FileHandle) -> Result<StableObjectId, SysError> {
    imp::identity_of_file(&file.0)
}

/// How many directory entries name the object a held file handle refers to.
///
/// # Errors
/// Returns [`SysError`] if the handle cannot be inspected.
pub fn link_count_of_file(file: &FileHandle) -> Result<u64, SysError> {
    imp::link_count_of_file(&file.0)
}

/// Whether an already-open standard-library file aliases protocol stdout on Windows.
///
/// This narrow adapter keeps Win32 handle classification and disk identity
/// queries inside the sysfs layer, where their unsafe boundary is owned.
///
/// # Errors
/// Returns [`SysError`] if the sink is not a disk file or Windows cannot
/// classify either borrowed handle. Pipe and character stdout are valid and do
/// not alias a disk sink.
#[cfg(windows)]
pub fn std_file_aliases_stdout(file: &std::fs::File) -> Result<bool, SysError> {
    imp::std_file_aliases_stdout(file)
}

/// Whether process stderr and protocol stdout target the same Windows stream.
///
/// Disk handles compare stable physical file identity; pipes and character
/// devices compare kernel object identity. Unknown handle kinds fail closed.
///
/// # Errors
/// Returns [`SysError`] if Windows cannot compare the standard handles.
#[cfg(windows)]
pub fn stderr_aliases_stdout() -> Result<bool, SysError> {
    imp::stderr_aliases_stdout()
}

/// Raw Windows directory-handle value used only by process-inheritance tests.
#[cfg(all(windows, feature = "test-hooks"))]
#[doc(hidden)]
#[must_use]
pub fn raw_dir_handle_for_test(dir: &DirHandle) -> isize {
    imp::raw_dir_handle_for_test(&dir.0)
}

/// Raw Windows file-handle value used only by process-inheritance tests.
#[cfg(all(windows, feature = "test-hooks"))]
#[doc(hidden)]
#[must_use]
pub fn raw_file_handle_for_test(file: &FileHandle) -> isize {
    imp::raw_file_handle_for_test(&file.0)
}

/// Guard for one deterministic Windows junction-follow race hook.
#[cfg(all(windows, feature = "test-hooks"))]
#[doc(hidden)]
pub struct JunctionFollowHookGuard {
    id: u64,
}

#[cfg(all(windows, feature = "test-hooks"))]
impl Drop for JunctionFollowHookGuard {
    fn drop(&mut self) {
        imp::remove_junction_follow_hook(self.id);
    }
}

/// Pause one selected junction after initial evidence and after its first follow.
#[cfg(all(windows, feature = "test-hooks"))]
#[doc(hidden)]
#[must_use]
pub fn install_junction_follow_hook(
    alias_object: StableObjectId,
    after_initial_reached: std::sync::Arc<std::sync::Barrier>,
    after_initial_resume: std::sync::Arc<std::sync::Barrier>,
    after_first_follow_reached: std::sync::Arc<std::sync::Barrier>,
    after_first_follow_resume: std::sync::Arc<std::sync::Barrier>,
) -> JunctionFollowHookGuard {
    JunctionFollowHookGuard {
        id: imp::install_junction_follow_hook(
            alias_object,
            after_initial_reached,
            after_initial_resume,
            after_first_follow_reached,
            after_first_follow_resume,
        ),
    }
}

/// Open a child directory relative to a held directory handle without following
/// a symlink at the final component.
///
/// # Errors
/// Returns [`SysError::SymlinkComponent`] for a symlink/unsupported reparse
/// entry, or another [`SysError`] on failure.
pub fn open_child_dir_no_symlink(dir: &DirHandle, name: &OsStr) -> Result<DirHandle, SysError> {
    imp::open_child_dir(&dir.0, name).map(|(handle, _)| DirHandle(handle))
}

/// Open a child file relative to a held directory handle for reading, without
/// following a symlink at the final component. The result is proven to be a
/// regular file before it is returned.
///
/// # Errors
/// Returns [`SysError::SymlinkComponent`] for a symlink entry,
/// [`SysError::NotRegularFile`] for a directory/device/other object, or another
/// [`SysError`] on failure.
pub fn open_child_regular_file(dir: &DirHandle, name: &OsStr) -> Result<FileHandle, SysError> {
    let file = imp::open_child_file_read(&dir.0, name)?;
    if !imp::file_is_regular(&file)? {
        return Err(SysError::NotRegularFile);
    }
    Ok(FileHandle(file))
}

/// Read the full bytes of a held file handle.
///
/// # Errors
/// Returns [`SysError::Io`] on a read failure.
pub fn read_file(file: &FileHandle) -> Result<Vec<u8>, SysError> {
    imp::read_file(&file.0)
}

/// Read the full bytes of an in-root child file, nofollow at the final
/// component and proven to be a regular file.
///
/// # Errors
/// Returns [`SysError`] on a symlink entry, a non-regular object or I/O failure.
pub fn read_child_file(dir: &DirHandle, name: &OsStr) -> Result<Vec<u8>, SysError> {
    let file = open_child_regular_file(dir, name)?;
    read_file(&file)
}

/// Re-open a held directory handle as a fresh handle to the same object.
///
/// # Errors
/// Returns [`SysError`] on failure.
pub fn reopen_dir(dir: &DirHandle) -> Result<DirHandle, SysError> {
    imp::reopen_dir(&dir.0).map(DirHandle)
}

/// Enumerate immediate children relative to a held directory handle without
/// following symlinks or reparse points.
///
/// # Errors
/// Returns [`SysError`] when enumeration or nofollow classification fails.
pub fn read_dir(dir: &DirHandle) -> Result<Vec<DirectoryEntry>, SysError> {
    imp::read_dir(&dir.0)
}

/// Walk each intermediate component of an in-root relative path from the held
/// root handle with symlink components forbidden, returning the parent handle
/// and the final entry name.
///
/// # Errors
/// Returns [`SysError`] on a symlink component or I/O failure.
pub fn open_in_root_parent<'a>(
    root: &DirHandle,
    names: &'a [&'a OsStr],
) -> Result<(DirHandle, &'a OsStr), SysError> {
    let (last, parents) = names.split_last().ok_or(SysError::BadComponent)?;
    let mut cur = reopen_dir(root)?;
    for name in parents {
        cur = open_child_dir_no_symlink(&cur, name)?;
    }
    Ok((cur, last))
}

/// Walk each intermediate component of an in-root relative path from the held
/// root handle, creating any missing directory, with symlink components
/// forbidden. Returns the handle of the final component's parent directory.
///
/// # Errors
/// Returns [`SysError::SymlinkComponent`] when a component is (or becomes) a
/// symlink, or another [`SysError`] on failure.
pub fn create_in_root_parent(root: &DirHandle, names: &[&OsStr]) -> Result<DirHandle, SysError> {
    let (_last, parents) = names.split_last().ok_or(SysError::BadComponent)?;
    let mut cur = reopen_dir(root)?;
    for name in parents {
        cur = create_child_dir_no_symlink(&cur, name)?;
    }
    Ok(cur)
}

/// Open a child directory of a held directory handle, creating it first when
/// it is absent. An existing entry is opened exactly as
/// [`open_child_dir_no_symlink`] opens it, so a symlink is refused.
///
/// # Errors
/// Returns [`SysError::SymlinkComponent`] for a symlink entry, or another
/// [`SysError`] on failure.
pub fn create_child_dir_no_symlink(dir: &DirHandle, name: &OsStr) -> Result<DirHandle, SysError> {
    imp::create_child_dir(&dir.0, name).map(DirHandle)
}

/// Open (creating if absent) a user-private state directory, proving the result
/// is a directory that only the current user can access.
///
/// # Errors
/// Returns [`SysError::NotPrivate`] when the directory is not private/owned, or
/// another [`SysError`] on failure.
pub fn open_private_state_dir(path: &Path) -> Result<DirHandle, SysError> {
    imp::open_private_state_dir(path).map(DirHandle)
}

/// Open (creating if absent) a user-private state directory as a direct child of
/// a held directory handle, so the directory object is derived from the held
/// parent rather than from any path spelling.
///
/// # Errors
/// Returns [`SysError::SymlinkComponent`] for a symlink/reparse entry,
/// [`SysError::NotPrivate`] when the directory is not private/owned, or another
/// [`SysError`] on failure.
pub fn open_private_child_state_dir(dir: &DirHandle, name: &OsStr) -> Result<DirHandle, SysError> {
    imp::open_private_child_state_dir(&dir.0, name).map(DirHandle)
}

/// Open (creating if absent) a user-private lock file inside a held state
/// directory, proving the result is a regular file only the current user can
/// access.
///
/// # Errors
/// Returns [`SysError::NotPrivate`] when the file is not private/owned, or
/// another [`SysError`] on failure.
pub fn open_private_lock_file(dir: &DirHandle, name: &str) -> Result<FileHandle, SysError> {
    imp::open_private_lock_file(&dir.0, name).map(FileHandle)
}

/// Take an exclusive advisory OS lock on a held file handle without blocking.
///
/// # Errors
/// Returns [`SysError::WouldBlock`] when another owner holds the lock, or
/// another [`SysError`] on failure.
pub fn try_lock_exclusive(file: &FileHandle) -> Result<(), SysError> {
    imp::try_lock_exclusive(&file.0)
}

/// Open an absolute-or-relative path as a user-private regular file for reading:
/// nofollow at every component, proven regular, proven inaccessible to other
/// users and owned by the current user. Used for secret-file indirections.
///
/// # Errors
/// Returns [`SysError::NotPrivate`] when the file is not private/owned,
/// [`SysError::NotRegularFile`] for a non-regular object, or another
/// [`SysError`].
pub fn open_private_regular_file(path: &Path) -> Result<FileHandle, SysError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(SysError::Io)
            .map(|cwd| cwd.join(path))?
    };
    let parent = absolute.parent().ok_or(SysError::BadComponent)?;
    let name = absolute.file_name().ok_or(SysError::BadComponent)?;
    let walked = walk_dir_no_symlinks(parent)?;
    let file = imp::open_child_file_read(&walked.dir().0, name)?;
    if !imp::file_is_regular(&file)? {
        return Err(SysError::NotRegularFile);
    }
    imp::verify_file_is_user_private(&file)?;
    Ok(FileHandle(file))
}

/// Pure ownership predicate used by the Unix backend and regression tests.
/// The backend supplies both values from the same held descriptor / direct
/// `geteuid()` observation; no procfs parsing is involved.
#[cfg(unix)]
#[doc(hidden)]
#[must_use]
pub fn owner_matches_effective_user(owner_uid: u64, effective_uid: u64) -> bool {
    owner_uid == effective_uid
}

/// Compare-and-swap create of an in-root file, requiring the target to be
/// currently absent. `before_publish` runs immediately before the atomic
/// publication; returning `false` aborts without publishing.
///
/// # Errors
/// Returns [`SysError`] on a symlink target or I/O failure.
pub fn cas_create(
    dir: &DirHandle,
    name: &OsStr,
    new: &[u8],
    before_publish: &dyn Fn() -> bool,
) -> Result<CasOutcome, SysError> {
    imp::cas_create(&dir.0, name, new, before_publish)
}

/// Compare-and-swap replacement of an in-root file, requiring the target to
/// currently hold exactly `expected`. `before_publish` runs immediately before
/// the atomic publication; returning `false` aborts without publishing.
///
/// An external actor that publishes at the same name inside the remaining
/// syscall window is never overwritten or unlinked: the operation fails closed
/// with [`CasOutcome::Stale`].
///
/// # Errors
/// Returns [`SysError`] on a symlink target or I/O failure.
pub fn cas_replace(
    dir: &DirHandle,
    name: &OsStr,
    expected: &[u8],
    new: &[u8],
    before_publish: &dyn Fn() -> bool,
) -> Result<CasOutcome, SysError> {
    imp::cas_replace(&dir.0, name, expected, new, before_publish)
}

/// Compare-and-swap delete of an in-root regular file, requiring it to
/// currently hold exactly `expected`. `before_publish` runs immediately before
/// the removal; returning `false` aborts without removing anything.
///
/// The removed object is proven, by identity and bytes, to be the observed
/// one. An external actor that takes the name inside the remaining window is
/// never removed: the operation fails closed with [`CasOutcome::Stale`].
///
/// # Errors
/// Returns [`SysError`] on a symlink target, a non-regular object or I/O
/// failure.
pub fn cas_delete(
    dir: &DirHandle,
    name: &OsStr,
    expected: &[u8],
    before_publish: &dyn Fn() -> bool,
) -> Result<CasOutcome, SysError> {
    imp::cas_delete(&dir.0, name, expected, before_publish)
}

/// Compare-and-swap rewrite of an in-root regular file in place, requiring it
/// to hold exactly `expected`. The file keeps its object identity.
/// `before_publish` runs immediately before the first byte is written;
/// returning `false` aborts without writing.
///
/// # Errors
/// Returns [`SysError`] on a symlink target, a non-regular object or I/O
/// failure.
pub fn cas_overwrite(
    dir: &DirHandle,
    name: &OsStr,
    expected: &[u8],
    new: &[u8],
    before_publish: &dyn Fn() -> bool,
) -> Result<CasOutcome, SysError> {
    imp::cas_overwrite(&dir.0, name, expected, new, before_publish)
}

#[cfg(test)]
mod staging_name_tests {
    use super::{is_staging_name, staging_name};

    #[test]
    fn every_staging_name_this_process_makes_is_recognised_and_nothing_else() {
        assert!(is_staging_name(&staging_name(0)));
        assert!(is_staging_name(&staging_name(u64::MAX)));
        assert!(is_staging_name(".cas-4242-7.tmp"));
        for name in [
            "a.gd",
            ".cas-.tmp",
            ".cas-1-.tmp",
            ".cas--1.tmp",
            ".cas-x-1.tmp",
            ".cas-1-2.tmp.gd",
            "cas-1-2.tmp",
            ".cas-1-2-3.tmp",
            "scripts/.cas-1-2.tmp",
        ] {
            assert!(!is_staging_name(name), "{name}");
        }
    }
}
