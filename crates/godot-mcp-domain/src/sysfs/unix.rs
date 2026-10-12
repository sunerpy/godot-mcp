//! Unix backend for the handle-relative filesystem primitives.
//!
//! Every traversal is `openat(..., O_NOFOLLOW | O_CLOEXEC)` from a held
//! descriptor, preceded by an explicit symlink-nofollow type probe: an
//! `O_NOFOLLOW` open reports `ELOOP` for a symlinked file but `ENOTDIR` for a
//! symlinked directory, so errno alone is ambiguous and the probe removes that
//! ambiguity. Mount ids are recorded (where the kernel reports them) as alias
//! evidence only — they are never part of the ownership key.

use std::ffi::{OsStr, OsString};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rustix::fs::{
    AtFlags, FileType, FlockOperation, Mode, OFlags, fchmod, flock, fstat, linkat, mkdirat, openat,
    unlinkat,
};
use rustix::io::Errno;

use super::{CasOutcome, DirectoryEntry, DirectoryEntryKind, SysError};
use crate::objid::{AliasEntry, StableObjectId};

/// A held directory descriptor.
pub(super) type Dir = OwnedFd;
/// A held file descriptor.
pub(super) type File = OwnedFd;

const STATE_DIR_MODE: u32 = 0o700;
const LOCK_FILE_MODE: u32 = 0o600;

/// `STATE_DIR_MODE` as `Mode` bitflags, for the calls that create the directory.
///
/// Not `Mode::from_raw_mode`: its `RawMode` argument is `u32` on Linux but
/// `u16` on Darwin, so a `u32` constant fails to compile on macOS. Named
/// permission bits need no conversion, so this path can neither panic like
/// `try_into().unwrap()` nor truncate like `as`. The assertion fails the build
/// if `RWXU` stops matching the mode `verify_private` enforces; both sides
/// widen to `u64` so neither is a no-op cast on either platform.
const STATE_DIR_PERMS: Mode = Mode::RWXU;
const _: () = assert!(STATE_DIR_PERMS.bits() as u64 == STATE_DIR_MODE as u64);

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn map_errno(error: Errno) -> SysError {
    if error == Errno::LOOP {
        SysError::SymlinkComponent
    } else if error == Errno::NOENT {
        SysError::NotFound
    } else if error == Errno::EXIST {
        SysError::AlreadyExists
    } else if error == Errno::WOULDBLOCK || error == Errno::AGAIN {
        SysError::WouldBlock
    } else {
        SysError::Io(std::io::Error::from_raw_os_error(error.raw_os_error()))
    }
}

/// Fail closed if `name` under `dir` is a symlink, classified without following
/// the link.
fn reject_if_symlink(dir: BorrowedFd<'_>, name: &OsStr) -> Result<(), SysError> {
    let stat =
        rustix::fs::statat(dir, name.as_bytes(), AtFlags::SYMLINK_NOFOLLOW).map_err(map_errno)?;
    if FileType::from_raw_mode(stat.st_mode) == FileType::Symlink {
        return Err(SysError::SymlinkComponent);
    }
    Ok(())
}

pub(super) fn open_anchor(path: &Path) -> Result<(Dir, PathBuf, PathBuf), SysError> {
    let anchor = rustix::fs::open(
        "/",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(map_errno)?;
    Ok((anchor, PathBuf::from("/"), path.to_path_buf()))
}

pub(super) fn open_child_dir(dir: &Dir, name: &OsStr) -> Result<(Dir, AliasEntry), SysError> {
    reject_if_symlink(dir.as_fd(), name)?;
    let child = openat(
        dir.as_fd(),
        name.as_bytes(),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(map_errno)?;
    let entry = AliasEntry {
        mount_id: mount_id_of(child.as_fd()),
        followed_reparse_tag: None,
        followed_reparse_object: None,
        followed_reparse_target: None,
    };
    Ok((child, entry))
}

pub(super) fn open_child_file_read(dir: &Dir, name: &OsStr) -> Result<File, SysError> {
    reject_if_symlink(dir.as_fd(), name)?;
    let file = openat(
        dir.as_fd(),
        name.as_bytes(),
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(map_errno)?;
    if !file_is_regular(&file)? {
        return Err(SysError::NotRegularFile);
    }
    let mut flags = rustix::fs::fcntl_getfl(file.as_fd()).map_err(map_errno)?;
    flags.remove(OFlags::NONBLOCK);
    rustix::fs::fcntl_setfl(file.as_fd(), flags).map_err(map_errno)?;
    Ok(file)
}

pub(super) fn reopen_dir(dir: &Dir) -> Result<Dir, SysError> {
    openat(
        dir.as_fd(),
        c".",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(map_errno)
}

pub(super) fn read_dir(dir: &Dir) -> Result<Vec<DirectoryEntry>, SysError> {
    let mut entries = Vec::new();
    let mut stream = rustix::fs::Dir::read_from(dir.as_fd()).map_err(map_errno)?;
    while let Some(entry) = stream.read() {
        let entry = entry.map_err(map_errno)?;
        let bytes = entry.file_name().to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        let stat =
            rustix::fs::statat(dir.as_fd(), bytes, AtFlags::SYMLINK_NOFOLLOW).map_err(map_errno)?;
        let kind = match FileType::from_raw_mode(stat.st_mode) {
            FileType::RegularFile => DirectoryEntryKind::RegularFile,
            FileType::Directory => DirectoryEntryKind::Directory,
            FileType::Symlink => DirectoryEntryKind::Symlink,
            _ => DirectoryEntryKind::Other,
        };
        entries.push(DirectoryEntry {
            name: OsString::from_vec(bytes.to_vec()),
            kind,
        });
    }
    Ok(entries)
}

pub(super) fn file_is_regular(file: &File) -> Result<bool, SysError> {
    let stat = fstat(file.as_fd()).map_err(map_errno)?;
    Ok(FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile)
}

pub(super) fn identity_of_dir(dir: &Dir) -> Result<StableObjectId, SysError> {
    identity_of(dir.as_fd())
}

pub(super) fn identity_of_file(file: &File) -> Result<StableObjectId, SysError> {
    identity_of(file.as_fd())
}

pub(super) fn link_count_of_file(file: &File) -> Result<u64, SysError> {
    let stat = fstat(file.as_fd()).map_err(map_errno)?;
    Ok(stat.st_nlink as u64)
}

/// Ownership identity of a held object: `(st_dev, st_ino)` only. The mount id is
/// deliberately excluded so a supported bind-mount alias converges.
fn identity_of(fd: BorrowedFd<'_>) -> Result<StableObjectId, SysError> {
    let stat = fstat(fd).map_err(map_errno)?;
    Ok(StableObjectId::Unix {
        dev: stat.st_dev as u64,
        ino: stat.st_ino as u64,
    })
}

/// Kernel mount id of a held object, where the platform reports it.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn mount_id_of(fd: BorrowedFd<'_>) -> Option<u64> {
    use rustix::fs::{StatxFlags, statx};

    let stat = statx(fd, c"", AtFlags::EMPTY_PATH, StatxFlags::MNT_ID).ok()?;
    if stat.stx_mask & StatxFlags::MNT_ID.bits() == 0 {
        return None;
    }
    Some(stat.stx_mnt_id)
}

/// No portable mount-identity source outside Linux; alias evidence then rests on
/// the object identities alone.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn mount_id_of(_fd: BorrowedFd<'_>) -> Option<u64> {
    None
}

pub(super) fn read_file(file: &File) -> Result<Vec<u8>, SysError> {
    let dup = rustix::io::fcntl_dupfd_cloexec(file.as_fd(), 0).map_err(map_errno)?;
    let mut handle = std::fs::File::from(dup);
    handle.seek(SeekFrom::Start(0)).map_err(SysError::Io)?;
    let mut bytes = Vec::new();
    handle.read_to_end(&mut bytes).map_err(SysError::Io)?;
    Ok(bytes)
}

pub(super) fn verify_file_is_user_private(file: &File) -> Result<(), SysError> {
    let stat = fstat(file.as_fd()).map_err(map_errno)?;
    if stat.st_mode as u32 & 0o077 != 0 {
        return Err(SysError::private("group/world-accessible permissions"));
    }
    if !super::owner_matches_effective_user(
        u64::from(stat.st_uid),
        u64::from(rustix::process::geteuid().as_raw()),
    ) {
        return Err(SysError::private("not owned by the effective user"));
    }
    Ok(())
}

fn verify_private(
    fd: BorrowedFd<'_>,
    expected: FileType,
    expected_mode: u32,
) -> Result<(), SysError> {
    let stat = fstat(fd).map_err(map_errno)?;
    if FileType::from_raw_mode(stat.st_mode) != expected {
        return Err(SysError::private("wrong filesystem object type"));
    }
    let mode = stat.st_mode as u32 & 0o777;
    if mode != expected_mode {
        return Err(SysError::private(format!(
            "permissions must be {expected_mode:04o}, got {mode:04o}"
        )));
    }
    if !super::owner_matches_effective_user(
        u64::from(stat.st_uid),
        u64::from(rustix::process::geteuid().as_raw()),
    ) {
        return Err(SysError::private("not owned by the effective user"));
    }
    Ok(())
}

pub(super) fn open_private_state_dir(path: &Path) -> Result<Dir, SysError> {
    let parent = path.parent().ok_or(SysError::BadComponent)?;
    let name = path.file_name().ok_or(SysError::BadComponent)?;
    let walked = super::walk_dir_no_symlinks(parent)?;
    open_private_child_state_dir(&walked.dir().0, name)
}

pub(super) fn open_private_child_state_dir(parent_fd: &Dir, name: &OsStr) -> Result<Dir, SysError> {
    let created = match mkdirat(parent_fd.as_fd(), name.as_bytes(), STATE_DIR_PERMS) {
        Ok(()) => true,
        Err(Errno::EXIST) => false,
        Err(error) => return Err(map_errno(error)),
    };
    reject_if_symlink(parent_fd.as_fd(), name)?;
    let dir = openat(
        parent_fd.as_fd(),
        name.as_bytes(),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(map_errno)?;
    if created {
        fchmod(dir.as_fd(), STATE_DIR_PERMS).map_err(map_errno)?;
    }
    verify_private(dir.as_fd(), FileType::Directory, STATE_DIR_MODE)?;
    Ok(dir)
}

pub(super) fn open_private_lock_file(dir: &Dir, name: &str) -> Result<File, SysError> {
    let (file, created) = match openat(
        dir.as_fd(),
        name,
        OFlags::CREATE | OFlags::EXCL | OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    ) {
        Ok(fd) => (fd, true),
        Err(Errno::EXIST) => (
            openat(
                dir.as_fd(),
                name,
                OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(map_errno)?,
            false,
        ),
        Err(error) => return Err(map_errno(error)),
    };
    if created {
        fchmod(file.as_fd(), Mode::RUSR | Mode::WUSR).map_err(map_errno)?;
    }
    verify_private(file.as_fd(), FileType::RegularFile, LOCK_FILE_MODE)?;
    Ok(file)
}

pub(super) fn try_lock_exclusive(file: &File) -> Result<(), SysError> {
    flock(file.as_fd(), FlockOperation::NonBlockingLockExclusive).map_err(map_errno)
}

/// A privately named staging entry in the destination directory, unlinked on
/// drop unless publication took ownership of it.
struct Staged {
    parent: OwnedFd,
    name: String,
    file: Option<std::fs::File>,
    exists: bool,
}

impl Staged {
    fn create(parent: &Dir) -> Result<Self, SysError> {
        let parent_copy = rustix::io::fcntl_dupfd_cloexec(parent.as_fd(), 0).map_err(map_errno)?;
        for _ in 0..128 {
            let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let name = super::staging_name(n);
            match openat(
                parent.as_fd(),
                name.as_str(),
                OFlags::CREATE | OFlags::EXCL | OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            ) {
                Ok(fd) => {
                    return Ok(Self {
                        parent: parent_copy,
                        name,
                        file: Some(std::fs::File::from(fd)),
                        exists: true,
                    });
                }
                Err(Errno::EXIST) => continue,
                Err(error) => return Err(map_errno(error)),
            }
        }
        Err(SysError::Io(std::io::Error::other(
            "unable to allocate a private CAS staging name",
        )))
    }

    fn write_and_sync(&mut self, bytes: &[u8]) -> Result<(), SysError> {
        let file = self
            .file
            .as_mut()
            .expect("the staging descriptor is held until publication");
        file.write_all(bytes).map_err(SysError::Io)?;
        file.sync_data().map_err(SysError::Io)
    }

    fn identity(&self) -> Result<StableObjectId, SysError> {
        let file = self
            .file
            .as_ref()
            .expect("the staging descriptor is held until publication");
        identity_of(file.as_fd())
    }

    fn name(&self) -> &OsStr {
        self.name.as_ref()
    }

    fn remove(&mut self) -> Result<(), SysError> {
        unlinkat(self.parent.as_fd(), self.name.as_str(), AtFlags::empty()).map_err(map_errno)?;
        self.exists = false;
        Ok(())
    }

    /// Publication consumed the staged name (or an external actor now owns it);
    /// never unlink it on drop.
    fn disown(&mut self) {
        self.exists = false;
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if self.exists {
            let _ = unlinkat(self.parent.as_fd(), self.name.as_str(), AtFlags::empty());
        }
    }
}

#[derive(PartialEq, Eq)]
struct Observed {
    identity: StableObjectId,
    bytes: Vec<u8>,
}

fn observe(parent: &Dir, name: &OsStr) -> Result<Option<Observed>, SysError> {
    let file = match open_child_file_read(parent, name) {
        Ok(file) => file,
        Err(SysError::NotFound) => return Ok(None),
        Err(SysError::SymlinkComponent) => return Err(SysError::SymlinkComponent),
        Err(error) => return Err(error),
    };
    if !file_is_regular(&file)? {
        return Err(SysError::NotRegularFile);
    }
    Ok(Some(Observed {
        identity: identity_of(file.as_fd())?,
        bytes: read_file(&file)?,
    }))
}

pub(super) fn cas_create(
    parent: &Dir,
    name: &OsStr,
    new: &[u8],
    before_publish: &dyn Fn() -> bool,
) -> Result<CasOutcome, SysError> {
    // A symlink already occupying the destination is rejected before staging.
    match rustix::fs::statat(parent.as_fd(), name.as_bytes(), AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::Symlink => {
            return Err(SysError::SymlinkComponent);
        }
        Ok(_) => return Ok(CasOutcome::Stale),
        Err(Errno::NOENT) => {}
        Err(error) => return Err(map_errno(error)),
    }

    let mut staged = Staged::create(parent)?;
    staged.write_and_sync(new)?;
    if !before_publish() {
        return Ok(CasOutcome::Aborted);
    }
    // `linkat` publishes atomically and fails closed if any object (including a
    // symlink an adversary just created) now occupies the destination.
    match linkat(
        parent.as_fd(),
        staged.name().as_bytes(),
        parent.as_fd(),
        name.as_bytes(),
        AtFlags::empty(),
    ) {
        Ok(()) => {}
        Err(Errno::EXIST) => return Ok(CasOutcome::Stale),
        Err(error) => return Err(map_errno(error)),
    }
    staged.remove()?;
    rustix::fs::fsync(parent.as_fd()).map_err(map_errno)?;
    Ok(CasOutcome::Published)
}

pub(super) fn cas_replace(
    parent: &Dir,
    name: &OsStr,
    expected: &[u8],
    new: &[u8],
    before_publish: &dyn Fn() -> bool,
) -> Result<CasOutcome, SysError> {
    let Some(observed) = observe(parent, name)? else {
        return Ok(CasOutcome::Stale);
    };
    if observed.bytes != expected {
        return Ok(CasOutcome::Stale);
    }

    let mut staged = Staged::create(parent)?;
    let staged_identity = staged.identity()?;
    staged.write_and_sync(new)?;

    if !before_publish() {
        return Ok(CasOutcome::Aborted);
    }
    // Re-prove the exact target object *and* its bytes immediately before the
    // exchange, so a replacement inside the window is caught here or by the
    // post-exchange inspection below.
    match observe(parent, name)? {
        Some(current) if current == observed => {}
        _ => return Ok(CasOutcome::Stale),
    }

    exchange(parent, staged.name(), name)?;
    // The exchange moved the previously published object to the staging name.
    // Inspect both ends: if an external actor won the remaining window, restore
    // its exact state and fail closed rather than overwrite or unlink it.
    let displaced_matches = observe(parent, staged.name())
        .ok()
        .flatten()
        .is_some_and(|displaced| displaced == observed);
    let published_matches = observe(parent, name)
        .ok()
        .flatten()
        .is_some_and(|published| published.identity == staged_identity && published.bytes == new);
    if !published_matches {
        // Someone else published at the destination after our exchange. Leave
        // their object untouched and retain the displaced object under the
        // private staging name; unlinking here would destroy external state.
        staged.disown();
        return Ok(CasOutcome::Stale);
    }
    if !displaced_matches {
        exchange(parent, staged.name(), name)?;
        return Ok(CasOutcome::Stale);
    }

    staged.remove()?;
    rustix::fs::fsync(parent.as_fd()).map_err(map_errno)?;
    Ok(CasOutcome::Published)
}

pub(super) fn cas_overwrite(
    parent: &Dir,
    name: &OsStr,
    expected: &[u8],
    new: &[u8],
    before_publish: &dyn Fn() -> bool,
) -> Result<CasOutcome, SysError> {
    match reject_if_symlink(parent.as_fd(), name) {
        Ok(()) => {}
        Err(SysError::NotFound) => return Ok(CasOutcome::Stale),
        Err(error) => return Err(error),
    }
    let file = match openat(
        parent.as_fd(),
        name.as_bytes(),
        OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(file) => file,
        Err(Errno::NOENT) => return Ok(CasOutcome::Stale),
        Err(error) => return Err(map_errno(error)),
    };
    if !file_is_regular(&file)? {
        return Err(SysError::NotRegularFile);
    }
    let mut flags = rustix::fs::fcntl_getfl(file.as_fd()).map_err(map_errno)?;
    flags.remove(OFlags::NONBLOCK);
    rustix::fs::fcntl_setfl(file.as_fd(), flags).map_err(map_errno)?;
    if read_file(&file)? != expected {
        return Ok(CasOutcome::Stale);
    }
    if !before_publish() {
        return Ok(CasOutcome::Aborted);
    }
    // The object is held, so nothing can redirect the write; an in-place edit
    // by another writer inside the window is caught here instead.
    if read_file(&file)? != expected {
        return Ok(CasOutcome::Stale);
    }
    let mut offset = 0_usize;
    while offset < new.len() {
        let written =
            rustix::io::pwrite(file.as_fd(), &new[offset..], offset as u64).map_err(map_errno)?;
        if written == 0 {
            return Err(SysError::Io(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "an in-place rewrite wrote zero bytes",
            )));
        }
        offset += written;
    }
    rustix::fs::ftruncate(file.as_fd(), new.len() as u64).map_err(map_errno)?;
    rustix::fs::fsync(file.as_fd()).map_err(map_errno)?;
    Ok(CasOutcome::Published)
}

/// Open a child directory, creating it first when it is absent. An existing
/// entry is reused only when it is a real directory: a symlink is refused by
/// the nofollow open exactly as for an existing path.
pub(super) fn create_child_dir(parent: &Dir, name: &OsStr) -> Result<Dir, SysError> {
    match mkdirat(
        parent.as_fd(),
        name.as_bytes(),
        Mode::RWXU | Mode::RWXG | Mode::RWXO,
    ) {
        Ok(()) | Err(Errno::EXIST) => {}
        Err(error) => return Err(map_errno(error)),
    }
    open_child_dir(parent, name).map(|(dir, _entry)| dir)
}

pub(super) fn cas_delete(
    parent: &Dir,
    name: &OsStr,
    expected: &[u8],
    before_publish: &dyn Fn() -> bool,
) -> Result<CasOutcome, SysError> {
    let Some(observed) = observe(parent, name)? else {
        return Ok(CasOutcome::Stale);
    };
    if observed.bytes != expected {
        return Ok(CasOutcome::Stale);
    }
    if !before_publish() {
        return Ok(CasOutcome::Aborted);
    }

    // Move whatever object holds the name *now* to a fresh private name, then
    // prove it is the observed object before removing it. An unlink by name
    // could remove a replacement that won the window; a rename moves exactly
    // one directory entry atomically, so the proof below binds the object that
    // is actually removed.
    let parked = park(parent, name)?;
    let Some(parked) = parked else {
        return Ok(CasOutcome::Stale);
    };
    let parked_name = OsStr::new(&parked);
    let parked_is_observed = observe(parent, parked_name)
        .ok()
        .flatten()
        .is_some_and(|candidate| candidate == observed);
    if parked_is_observed {
        unlinkat(parent.as_fd(), parked_name.as_bytes(), AtFlags::empty()).map_err(map_errno)?;
        rustix::fs::fsync(parent.as_fd()).map_err(map_errno)?;
        return Ok(CasOutcome::Published);
    }
    // Someone else's object: put it back without replacing anything that has
    // appeared at the name since. If a newer object owns the name, the
    // displaced one stays under the private name rather than being destroyed.
    restore(parent, parked_name, name)?;
    Ok(CasOutcome::Stale)
}

/// Rename `name` to a fresh private name in the same directory. `None` means
/// the name was already gone.
fn park(parent: &Dir, name: &OsStr) -> Result<Option<String>, SysError> {
    for _ in 0..128 {
        let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let parked = super::staging_name(n);
        match rename_no_replace(parent, name, OsStr::new(&parked)) {
            Ok(()) => return Ok(Some(parked)),
            Err(Errno::EXIST) => continue,
            Err(Errno::NOENT) => return Ok(None),
            Err(Errno::INVAL | Errno::NOSYS | Errno::OPNOTSUPP) => {
                // The filesystem cannot refuse an existing destination. The
                // private name was just allocated, so an ordinary rename is
                // the same move.
                return match rustix::fs::renameat(
                    parent.as_fd(),
                    name.as_bytes(),
                    parent.as_fd(),
                    parked.as_bytes(),
                ) {
                    Ok(()) => Ok(Some(parked)),
                    Err(Errno::NOENT) => Ok(None),
                    Err(error) => Err(map_errno(error)),
                };
            }
            Err(error) => return Err(map_errno(error)),
        }
    }
    Err(SysError::Io(std::io::Error::other(
        "unable to allocate a private CAS staging name",
    )))
}

/// Move a parked object back to `name` without replacing anything there.
fn restore(parent: &Dir, parked: &OsStr, name: &OsStr) -> Result<(), SysError> {
    match rename_no_replace(parent, parked, name) {
        Ok(()) | Err(Errno::EXIST) => Ok(()),
        Err(Errno::INVAL | Errno::NOSYS | Errno::OPNOTSUPP) => {
            // `linkat` refuses an existing destination on every filesystem.
            match linkat(
                parent.as_fd(),
                parked.as_bytes(),
                parent.as_fd(),
                name.as_bytes(),
                AtFlags::empty(),
            ) {
                Ok(()) => {
                    unlinkat(parent.as_fd(), parked.as_bytes(), AtFlags::empty()).map_err(map_errno)
                }
                Err(Errno::EXIST) => Ok(()),
                Err(error) => Err(map_errno(error)),
            }
        }
        Err(error) => Err(map_errno(error)),
    }
}

/// Rename within one directory, failing with `EEXIST` rather than replacing an
/// existing destination.
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "visionos"
))]
fn rename_no_replace(parent: &Dir, from: &OsStr, to: &OsStr) -> Result<(), Errno> {
    rustix::fs::renameat_with(
        parent.as_fd(),
        from.as_bytes(),
        parent.as_fd(),
        to.as_bytes(),
        rustix::fs::RenameFlags::NOREPLACE,
    )
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "visionos"
)))]
fn rename_no_replace(_parent: &Dir, _from: &OsStr, _to: &OsStr) -> Result<(), Errno> {
    Err(Errno::NOSYS)
}

/// Atomically exchange two directory entries in the same directory.
///
/// Available on Linux (`RENAME_EXCHANGE`) and Apple platforms (`RENAME_SWAP`).
/// Anywhere else replacement fails closed rather than falling back to a
/// non-atomic rename that could destroy an external actor's object.
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "visionos"
))]
fn exchange(parent: &Dir, from: &OsStr, to: &OsStr) -> Result<(), SysError> {
    rustix::fs::renameat_with(
        parent.as_fd(),
        from.as_bytes(),
        parent.as_fd(),
        to.as_bytes(),
        rustix::fs::RenameFlags::EXCHANGE,
    )
    .map_err(map_errno)
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "visionos"
)))]
fn exchange(_parent: &Dir, _from: &OsStr, _to: &OsStr) -> Result<(), SysError> {
    Err(SysError::Io(std::io::Error::other(
        "atomic directory-entry exchange is unavailable on this platform",
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readerless_fifo_is_rejected_without_waiting_for_a_writer() {
        const CHILD_PATH: &str = "GODOT_MCP_SYSFS_READERLESS_FIFO_CHILD";
        const TEST_NAME: &str =
            "sysfs::unix::tests::readerless_fifo_is_rejected_without_waiting_for_a_writer";

        if let Some(path) = std::env::var_os(CHILD_PATH) {
            let path = PathBuf::from(path);
            let walked = super::super::walk_dir_no_symlinks(
                path.parent().expect("FIFO has a parent directory"),
            )
            .expect("walk FIFO parent");
            assert!(matches!(
                open_child_file_read(
                    &walked.dir().0,
                    path.file_name().expect("FIFO has a file name")
                ),
                Err(SysError::NotRegularFile)
            ));
            return;
        }

        let temp = tempfile::tempdir().expect("temp dir");
        let fifo = temp.path().join("readerless.fifo");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("run mkfifo");
        assert!(status.success(), "mkfifo failed with {status}");
        let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .arg(TEST_NAME)
            .arg("--exact")
            .arg("--nocapture")
            .env(CHILD_PATH, &fifo)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn FIFO probe child");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while child.try_wait().expect("poll FIFO probe child").is_none() {
            if std::time::Instant::now() >= deadline {
                child.kill().expect("kill blocked FIFO probe child");
                let output = child.wait_with_output().expect("collect blocked child");
                panic!(
                    "reader-less FIFO open timed out: stdout={} stderr={}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let output = child.wait_with_output().expect("collect FIFO probe child");
        assert!(
            output.status.success(),
            "FIFO probe child failed: status={} stdout={} stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn regular_read_descriptor_is_returned_in_blocking_mode() {
        let temp = tempfile::tempdir().expect("temp dir");
        std::fs::write(temp.path().join("regular.txt"), b"content").expect("write regular file");
        let walked = super::super::walk_dir_no_symlinks(temp.path()).expect("walk temp dir");
        let file = open_child_file_read(&walked.dir().0, OsStr::new("regular.txt"))
            .expect("open regular file");
        let flags = rustix::fs::fcntl_getfl(file.as_fd()).expect("read descriptor flags");

        assert!(!flags.contains(OFlags::NONBLOCK));
    }
}
