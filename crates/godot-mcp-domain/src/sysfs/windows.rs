//! Windows backend for handle-relative filesystem primitives.
//!
//! Child authority is always a held directory `HANDLE`: every child open uses
//! `NtCreateFile` with `OBJECT_ATTRIBUTES.RootDirectory`, and every open includes
//! `FILE_OPEN_REPARSE_POINT` so the object itself can be inspected before use.
//! Directory junctions are the sole supported reparse alias: their object id,
//! tag, and raw substitute-name evidence are revalidated around the relative
//! follow open. Symlinks and every other reparse tag fail closed.

use std::ffi::{OsStr, OsString, c_void};
use std::io;
use std::mem::{offset_of, size_of};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::AsRawHandle as _;
use std::path::{Component, Path, PathBuf, Prefix};
use std::ptr::{null, null_mut};
#[cfg(feature = "test-hooks")]
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_CREATE as NT_FILE_CREATE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE,
    FILE_OPEN as NT_FILE_OPEN, FILE_OPEN_REPARSE_POINT, FILE_RENAME_INFORMATION,
    FILE_SYNCHRONOUS_IO_NONALERT, FileRenameInformation, NtCreateFile, NtSetInformationFile,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, CompareObjectHandles, DUPLICATE_SAME_ACCESS, DuplicateHandle,
    ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_INSUFFICIENT_BUFFER,
    ERROR_LOCK_VIOLATION, ERROR_NO_MORE_FILES, ERROR_PATH_NOT_FOUND, ERROR_SHARING_VIOLATION,
    ERROR_SUCCESS, GetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
    LocalFree, OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError, SetHandleInformation, SetLastError,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACL, ACL_SIZE_INFORMATION, AclSizeInformation, DACL_SECURITY_INFORMATION,
    EqualSid, GetAce, GetAclInformation, GetSecurityDescriptorControl, GetTokenInformation,
    INHERITED_ACE, IsValidAcl, IsValidSecurityDescriptor, OWNER_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_ADD_FILE, FILE_ADD_SUBDIRECTORY, FILE_ALL_ACCESS, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO, FILE_BEGIN,
    FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO, FILE_DISPOSITION_INFO_EX,
    FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_ID_BOTH_DIR_INFO, FILE_ID_INFO,
    FILE_INFO_BY_HANDLE_CLASS, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO, FILE_TRAVERSE, FILE_TYPE_UNKNOWN,
    FILE_WRITE_DATA, FileAttributeTagInfo, FileDispositionInfo, FileDispositionInfoEx,
    FileIdBothDirectoryInfo, FileIdBothDirectoryRestartInfo, FileIdInfo, FileStandardInfo,
    FlushFileBuffers, GetFileInformationByHandleEx, GetFileType, LOCKFILE_EXCLUSIVE_LOCK,
    LOCKFILE_FAIL_IMMEDIATELY, LockFileEx, MAXIMUM_REPARSE_DATA_BUFFER_SIZE, READ_CONTROL,
    ReadFile, SYNCHRONIZE, SetEndOfFile, SetFileInformationByHandle, SetFilePointerEx, WriteFile,
};
use windows_sys::Win32::System::IO::{DeviceIoControl, IO_STATUS_BLOCK, OVERLAPPED};
use windows_sys::Win32::System::SystemServices::{
    ACCESS_ALLOWED_ACE_TYPE, IO_REPARSE_TAG_MOUNT_POINT,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use super::windows_decision::{
    HandleKind, HandleKindError, aliases_stdout_by_kind, cas_create_preflight,
    classify_handle_kind, regular_file_by_kind, standard_handles_alias_by_kind,
};
use super::{CasOutcome, DirectoryEntry, DirectoryEntryKind, SysError};
use crate::objid::{AliasEntry, StableObjectId};

const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
const SYNC_OPEN: u32 = FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT;
const DIRECTORY_ACCESS: u32 =
    FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE;
// No DELETE: a rename into a directory opens that directory sharing only read
// and write, so a held handle with DELETE access makes every publish into the
// directory a sharing violation (os error 32), the state directory's own
// staged-file publications included.
const PRIVATE_DIRECTORY_ACCESS: u32 = FILE_GENERIC_READ
    | FILE_GENERIC_WRITE
    | FILE_ADD_FILE
    | FILE_ADD_SUBDIRECTORY
    | READ_CONTROL
    | SYNCHRONIZE;
const READ_FILE_ACCESS: u32 = FILE_GENERIC_READ | READ_CONTROL | SYNCHRONIZE;
const LOCK_FILE_ACCESS: u32 = FILE_GENERIC_READ | FILE_GENERIC_WRITE | READ_CONTROL | SYNCHRONIZE;
const STAGING_FILE_ACCESS: u32 =
    FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_WRITE_DATA | READ_CONTROL | DELETE | SYNCHRONIZE;
// CTL_CODE(FILE_DEVICE_FILE_SYSTEM, 42, METHOD_BUFFERED, FILE_ANY_ACCESS).
// Kept local to avoid enabling the broad Win32_System_Ioctl feature solely for
// this one stable SDK constant.
const FSCTL_GET_REPARSE_POINT: u32 = 589_992;

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[cfg(feature = "test-hooks")]
#[derive(Clone, Copy, PartialEq, Eq)]
enum JunctionFollowPhase {
    AfterInitialEvidence,
    AfterFirstFollow,
}

#[cfg(feature = "test-hooks")]
struct JunctionFollowHook {
    id: u64,
    alias_object: StableObjectId,
    after_initial_reached: Arc<std::sync::Barrier>,
    after_initial_resume: Arc<std::sync::Barrier>,
    after_first_follow_reached: Arc<std::sync::Barrier>,
    after_first_follow_resume: Arc<std::sync::Barrier>,
}

#[cfg(feature = "test-hooks")]
static JUNCTION_HOOK_COUNTER: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-hooks")]
static JUNCTION_FOLLOW_HOOKS: std::sync::LazyLock<std::sync::Mutex<Vec<Arc<JunctionFollowHook>>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(Vec::new()));

#[cfg(feature = "test-hooks")]
pub(super) fn install_junction_follow_hook(
    alias_object: StableObjectId,
    after_initial_reached: Arc<std::sync::Barrier>,
    after_initial_resume: Arc<std::sync::Barrier>,
    after_first_follow_reached: Arc<std::sync::Barrier>,
    after_first_follow_resume: Arc<std::sync::Barrier>,
) -> u64 {
    let id = JUNCTION_HOOK_COUNTER.fetch_add(1, Ordering::Relaxed);
    JUNCTION_FOLLOW_HOOKS
        .lock()
        .expect("junction follow hook mutex")
        .push(Arc::new(JunctionFollowHook {
            id,
            alias_object,
            after_initial_reached,
            after_initial_resume,
            after_first_follow_reached,
            after_first_follow_resume,
        }));
    id
}

#[cfg(feature = "test-hooks")]
pub(super) fn remove_junction_follow_hook(id: u64) {
    JUNCTION_FOLLOW_HOOKS
        .lock()
        .expect("junction follow hook mutex")
        .retain(|hook| hook.id != id);
}

#[cfg(feature = "test-hooks")]
fn pause_junction_follow(alias_object: StableObjectId, phase: JunctionFollowPhase) {
    let hook = JUNCTION_FOLLOW_HOOKS
        .lock()
        .expect("junction follow hook mutex")
        .iter()
        .find(|hook| hook.alias_object == alias_object)
        .cloned();
    if let Some(hook) = hook {
        let (reached, resume) = match phase {
            JunctionFollowPhase::AfterInitialEvidence => {
                (&hook.after_initial_reached, &hook.after_initial_resume)
            }
            JunctionFollowPhase::AfterFirstFollow => (
                &hook.after_first_follow_reached,
                &hook.after_first_follow_resume,
            ),
        };
        reached.wait();
        resume.wait();
    }
}

#[cfg(not(feature = "test-hooks"))]
fn pause_after_initial_junction_evidence(_alias_object: StableObjectId) {}
#[cfg(feature = "test-hooks")]
fn pause_after_initial_junction_evidence(alias_object: StableObjectId) {
    pause_junction_follow(alias_object, JunctionFollowPhase::AfterInitialEvidence);
}

#[cfg(not(feature = "test-hooks"))]
fn pause_after_first_junction_follow(_alias_object: StableObjectId) {}
#[cfg(feature = "test-hooks")]
fn pause_after_first_junction_follow(alias_object: StableObjectId) {
    pause_junction_follow(alias_object, JunctionFollowPhase::AfterFirstFollow);
}

/// One owned, non-inheritable Windows handle.
///
/// The handle value is stored as an integer so the wrapper inherits the kernel
/// handle's Send/Sync semantics without an unsafe trait implementation. No
/// constructor sets `OBJ_INHERIT`, and `DuplicateHandle` is called with
/// `bInheritHandle = FALSE`.
struct OwnedRawHandle(isize);

impl OwnedRawHandle {
    fn new(handle: HANDLE) -> Result<Self, SysError> {
        let value = handle as isize;
        if value == 0 || handle == INVALID_HANDLE_VALUE {
            return Err(SysError::Io(io::Error::last_os_error()));
        }
        let owned = Self(value);
        // Do not rely solely on every creator remembering to omit OBJ_INHERIT.
        // Clear the kernel handle flag and query it back for mechanical proof.
        // SAFETY: `owned` contains the successful live handle and owns it on all
        // error paths below.
        if unsafe { SetHandleInformation(owned.raw(), HANDLE_FLAG_INHERIT, 0) } == 0 {
            return Err(map_win32(last_error_code()));
        }
        let mut flags = 0_u32;
        // SAFETY: the handle is live and `flags` is writable output storage.
        if unsafe { GetHandleInformation(owned.raw(), &mut flags) } == 0 {
            return Err(map_win32(last_error_code()));
        }
        if flags & HANDLE_FLAG_INHERIT != 0 {
            return Err(SysError::Io(io::Error::other(
                "Windows handle remained inheritable after clearing the flag",
            )));
        }
        Ok(owned)
    }

    fn raw(&self) -> HANDLE {
        self.0 as HANDLE
    }
}

impl Drop for OwnedRawHandle {
    fn drop(&mut self) {
        // SAFETY: `OwnedRawHandle` is constructed only from one successful API
        // result and is never cloned. This is therefore its sole `CloseHandle`.
        unsafe {
            let _ = CloseHandle(self.raw());
        }
    }
}

/// A held Windows directory handle.
pub(super) struct Dir(OwnedRawHandle);
/// A held Windows file handle.
pub(super) struct File(OwnedRawHandle);

impl Dir {
    fn raw(&self) -> HANDLE {
        self.0.raw()
    }
}

impl File {
    fn raw(&self) -> HANDLE {
        self.0.raw()
    }
}

struct LocalAllocation(*mut c_void);

impl LocalAllocation {
    fn new(pointer: *mut c_void) -> Result<Self, SysError> {
        if pointer.is_null() {
            return Err(SysError::Io(io::Error::other(
                "Windows returned an empty local allocation",
            )));
        }
        Ok(Self(pointer))
    }
}

impl Drop for LocalAllocation {
    fn drop(&mut self) {
        // SAFETY: the pointer came from a Win32 API documented to allocate with
        // `LocalAlloc`; this wrapper owns it exactly once.
        unsafe {
            let _ = LocalFree(self.0 as _);
        }
    }
}

struct PrivateSecurityDescriptor(LocalAllocation);

impl PrivateSecurityDescriptor {
    fn as_ptr(&self) -> *const windows_sys::Win32::Security::SECURITY_DESCRIPTOR {
        self.0.0.cast()
    }
}

fn last_error_code() -> u32 {
    io::Error::last_os_error().raw_os_error().unwrap_or(0) as u32
}

fn map_win32(code: u32) -> SysError {
    match code {
        ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => SysError::NotFound,
        ERROR_ALREADY_EXISTS | ERROR_FILE_EXISTS => SysError::AlreadyExists,
        ERROR_LOCK_VIOLATION => SysError::WouldBlock,
        _ => SysError::Io(io::Error::from_raw_os_error(code as i32)),
    }
}

fn map_ntstatus(status: i32) -> SysError {
    // SAFETY: `RtlNtStatusToDosError` accepts every NTSTATUS value.
    let code = unsafe { RtlNtStatusToDosError(status) };
    map_win32(code)
}

fn validate_child_name(name: &OsStr) -> Result<Vec<u16>, SysError> {
    let units: Vec<u16> = name.encode_wide().collect();
    if units.is_empty()
        || units.iter().any(|unit| {
            *unit == 0
                || *unit == u16::from(b'\\')
                || *unit == u16::from(b'/')
                || *unit == u16::from(b':')
        })
        || units == [b'.' as u16]
        || units == [b'.' as u16, b'.' as u16]
    {
        return Err(SysError::BadComponent);
    }
    Ok(units)
}

fn unicode_string(
    units: &mut [u16],
) -> Result<windows_sys::Win32::Foundation::UNICODE_STRING, SysError> {
    let byte_len = units
        .len()
        .checked_mul(size_of::<u16>())
        .and_then(|length| u16::try_from(length).ok())
        .ok_or(SysError::BadComponent)?;
    Ok(windows_sys::Win32::Foundation::UNICODE_STRING {
        Length: byte_len,
        MaximumLength: byte_len,
        Buffer: units.as_mut_ptr(),
    })
}

fn nt_create(
    root: HANDLE,
    name: Vec<u16>,
    desired_access: u32,
    disposition: u32,
    options: u32,
    security_descriptor: *const windows_sys::Win32::Security::SECURITY_DESCRIPTOR,
) -> Result<(OwnedRawHandle, usize), SysError> {
    nt_create_shared(
        root,
        name,
        desired_access,
        disposition,
        options,
        security_descriptor,
        SHARE_ALL,
    )
}

fn nt_create_shared(
    root: HANDLE,
    mut name: Vec<u16>,
    desired_access: u32,
    disposition: u32,
    options: u32,
    security_descriptor: *const windows_sys::Win32::Security::SECURITY_DESCRIPTOR,
    share_access: u32,
) -> Result<(OwnedRawHandle, usize), SysError> {
    let object_name = unicode_string(&mut name)?;
    let attributes = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: root,
        ObjectName: &object_name,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: security_descriptor,
        SecurityQualityOfService: null(),
    };
    let mut handle: HANDLE = null_mut();
    let mut io_status = IO_STATUS_BLOCK::default();
    // SAFETY: every pointer references live storage for the duration of the
    // synchronous call. `OBJECT_ATTRIBUTES` does not request inheritance, and
    // `name` is a counted UTF-16 string without interior NUL interpretation.
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            desired_access,
            &attributes,
            &mut io_status,
            null(),
            FILE_ATTRIBUTE_NORMAL,
            share_access,
            disposition,
            options,
            null(),
            0,
        )
    };
    if status < 0 {
        return Err(map_ntstatus(status));
    }
    Ok((OwnedRawHandle::new(handle)?, io_status.Information))
}

fn nt_create_child(
    parent: &Dir,
    name: &OsStr,
    desired_access: u32,
    disposition: u32,
    options: u32,
    security_descriptor: *const windows_sys::Win32::Security::SECURITY_DESCRIPTOR,
) -> Result<OwnedRawHandle, SysError> {
    let name = validate_child_name(name)?;
    nt_create(
        parent.raw(),
        name,
        desired_access,
        disposition,
        options,
        security_descriptor,
    )
    .map(|(handle, _)| handle)
}

fn query_file_info<T: Default>(
    handle: HANDLE,
    information_class: FILE_INFO_BY_HANDLE_CLASS,
) -> Result<T, SysError> {
    let mut info = T::default();
    // SAFETY: `info` is writable storage of exactly the size passed to Win32,
    // and the information class at each call site matches `T`.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            handle,
            information_class,
            (&mut info as *mut T).cast(),
            size_of::<T>() as u32,
        )
    };
    if ok == 0 {
        return Err(map_win32(last_error_code()));
    }
    Ok(info)
}

fn reject_reparse(handle: HANDLE) -> Result<(), SysError> {
    let info: FILE_ATTRIBUTE_TAG_INFO = query_file_info(handle, FileAttributeTagInfo)?;
    if info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(SysError::SymlinkComponent);
    }
    Ok(())
}

#[derive(Clone, PartialEq, Eq)]
struct JunctionEvidence {
    object: StableObjectId,
    target: Vec<u16>,
}

fn reparse_tag(handle: HANDLE) -> Result<Option<u32>, SysError> {
    let info: FILE_ATTRIBUTE_TAG_INFO = query_file_info(handle, FileAttributeTagInfo)?;
    Ok((info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0).then_some(info.ReparseTag))
}

fn junction_target(handle: HANDLE) -> Result<Vec<u16>, SysError> {
    let mut storage = vec![0_u8; MAXIMUM_REPARSE_DATA_BUFFER_SIZE as usize];
    let mut returned = 0_u32;
    // SAFETY: the output vector is writable for the advertised size. This is a
    // synchronous metadata query, so neither an input buffer nor OVERLAPPED is
    // required.
    let ok = unsafe {
        DeviceIoControl(
            handle,
            FSCTL_GET_REPARSE_POINT,
            null(),
            0,
            storage.as_mut_ptr().cast(),
            storage.len() as u32,
            &mut returned,
            null_mut(),
        )
    };
    if ok == 0 {
        return Err(map_win32(last_error_code()));
    }
    let returned = returned as usize;
    if returned < 16 {
        return Err(SysError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "mount-point reparse data is shorter than its fixed header",
        )));
    }
    let read_u16 = |offset: usize| u16::from_le_bytes([storage[offset], storage[offset + 1]]);
    let read_u32 = |offset: usize| {
        u32::from_le_bytes([
            storage[offset],
            storage[offset + 1],
            storage[offset + 2],
            storage[offset + 3],
        ])
    };
    if read_u32(0) != IO_REPARSE_TAG_MOUNT_POINT {
        return Err(SysError::SymlinkComponent);
    }
    let data_length = usize::from(read_u16(4));
    let substitute_offset = usize::from(read_u16(8));
    let substitute_length = usize::from(read_u16(10));
    let data_end = 8_usize.checked_add(data_length).ok_or_else(|| {
        SysError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "reparse data length overflow",
        ))
    })?;
    let start = 16_usize.checked_add(substitute_offset).ok_or_else(|| {
        SysError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "reparse target offset overflow",
        ))
    })?;
    let end = start.checked_add(substitute_length).ok_or_else(|| {
        SysError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "reparse target length overflow",
        ))
    })?;
    if data_length < 8
        || data_end > returned
        || substitute_offset % size_of::<u16>() != 0
        || substitute_length == 0
        || substitute_length % size_of::<u16>() != 0
        || start < 16
        || end > data_end
    {
        return Err(SysError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid mount-point substitute-name extent",
        )));
    }
    Ok(storage[start..end]
        .chunks_exact(size_of::<u16>())
        .map(|unit| u16::from_le_bytes([unit[0], unit[1]]))
        .collect())
}

fn inspect_junction(handle: HANDLE) -> Result<JunctionEvidence, SysError> {
    match reparse_tag(handle)? {
        Some(IO_REPARSE_TAG_MOUNT_POINT) => Ok(JunctionEvidence {
            object: identity_of(handle)?,
            target: junction_target(handle)?,
        }),
        Some(_) => Err(SysError::SymlinkComponent),
        None => Err(SysError::SymlinkComponent),
    }
}

#[cfg(feature = "test-hooks")]
pub(super) fn raw_dir_handle_for_test(dir: &Dir) -> isize {
    dir.raw() as isize
}

#[cfg(feature = "test-hooks")]
pub(super) fn raw_file_handle_for_test(file: &File) -> isize {
    file.raw() as isize
}

fn standard_info(handle: HANDLE) -> Result<FILE_STANDARD_INFO, SysError> {
    query_file_info(handle, FileStandardInfo)
}

fn identity_of(handle: HANDLE) -> Result<StableObjectId, SysError> {
    let info: FILE_ID_INFO = query_file_info(handle, FileIdInfo)?;
    Ok(StableObjectId::Windows {
        volume_serial: info.VolumeSerialNumber,
        file_id: info.FileId.Identifier,
    })
}

fn handle_kind(handle: HANDLE) -> Result<HandleKind, SysError> {
    // SAFETY: last-error state is thread-local, and callers pass a live borrowed
    // handle that `GetFileType` neither takes ownership of nor mutates.
    let file_type = unsafe {
        SetLastError(ERROR_SUCCESS);
        GetFileType(handle)
    };
    let error = if file_type == FILE_TYPE_UNKNOWN {
        last_error_code()
    } else {
        ERROR_SUCCESS
    };
    match classify_handle_kind(file_type, error) {
        Ok(kind) => Ok(kind),
        Err(HandleKindError::ApiFailure(error)) => Err(map_win32(error)),
        Err(HandleKindError::Unsupported(other)) => Err(SysError::Io(io::Error::other(format!(
            "GetFileType returned unsupported handle type {other}"
        )))),
    }
}

pub(super) fn std_file_aliases_stdout(file: &std::fs::File) -> Result<bool, SysError> {
    let sink = file.as_raw_handle();
    let sink_kind = handle_kind(sink)?;
    let stdout = io::stdout();
    let stdout = stdout.as_raw_handle();
    let stdout_kind = handle_kind(stdout)?;
    aliases_stdout_by_kind(sink_kind, stdout_kind, || {
        Ok(identity_of(sink)? == identity_of(stdout)?)
    })
}

pub(super) fn stderr_aliases_stdout() -> Result<bool, SysError> {
    let stderr = io::stderr();
    let stdout = io::stdout();
    let stderr = stderr.as_raw_handle();
    let stdout = stdout.as_raw_handle();
    if stderr.is_null()
        || stderr == INVALID_HANDLE_VALUE
        || stdout.is_null()
        || stdout == INVALID_HANDLE_VALUE
    {
        return Err(SysError::Io(io::Error::other(
            "a process standard handle is unavailable",
        )));
    }
    let stderr_kind = handle_kind(stderr)?;
    let stdout_kind = handle_kind(stdout)?;
    standard_handles_alias_by_kind(
        stderr_kind,
        stdout_kind,
        || Ok(identity_of(stderr)? == identity_of(stdout)?),
        || {
            // SAFETY: both values are live borrowed process-standard handles. The API
            // only compares their underlying kernel objects and takes no ownership.
            Ok(unsafe { CompareObjectHandles(stderr, stdout) } != 0)
        },
    )
}

fn followed_directory_identity(handle: HANDLE) -> Result<StableObjectId, SysError> {
    if !standard_info(handle)?.Directory || reparse_tag(handle)?.is_some() {
        return Err(SysError::SymlinkComponent);
    }
    identity_of(handle)
}

fn duplicate_handle(handle: HANDLE) -> Result<OwnedRawHandle, SysError> {
    let process = unsafe { GetCurrentProcess() };
    let mut duplicate: HANDLE = null_mut();
    // SAFETY: source and target are the current process, `duplicate` is valid
    // output storage, and FALSE explicitly makes the new handle non-inheritable.
    let ok = unsafe {
        DuplicateHandle(
            process,
            handle,
            process,
            &mut duplicate,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    };
    if ok == 0 {
        return Err(map_win32(last_error_code()));
    }
    OwnedRawHandle::new(duplicate)
}

fn append_os(target: &mut OsString, value: &OsStr) {
    target.push(value);
}

fn drive_roots(letter: u8) -> (PathBuf, OsString) {
    let letter = char::from(letter);
    (
        PathBuf::from(format!("{letter}:\\")),
        OsString::from(format!("\\??\\{letter}:\\")),
    )
}

fn unc_roots(server: &OsStr, share: &OsStr) -> (PathBuf, OsString) {
    let mut display = OsString::from("\\\\");
    append_os(&mut display, server);
    display.push("\\");
    append_os(&mut display, share);
    display.push("\\");

    let mut nt = OsString::from("\\??\\UNC\\");
    append_os(&mut nt, server);
    nt.push("\\");
    append_os(&mut nt, share);
    nt.push("\\");
    (PathBuf::from(display), nt)
}

pub(super) fn open_anchor(path: &Path) -> Result<(Dir, PathBuf, PathBuf), SysError> {
    let mut components = path.components();
    let prefix = match components.next() {
        Some(Component::Prefix(prefix)) => prefix.kind(),
        _ => return Err(SysError::NotAbsolute),
    };
    if !matches!(components.next(), Some(Component::RootDir)) {
        return Err(SysError::NotAbsolute);
    }
    let (anchor_display, nt_root) = match prefix {
        Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => drive_roots(letter),
        Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => unc_roots(server, share),
        Prefix::DeviceNS(_) | Prefix::Verbatim(_) => return Err(SysError::BadComponent),
    };

    let mut rest = PathBuf::new();
    for component in components {
        rest.push(component.as_os_str());
    }
    let units: Vec<u16> = nt_root.encode_wide().collect();
    let (handle, _) = nt_create(
        null_mut(),
        units,
        DIRECTORY_ACCESS,
        NT_FILE_OPEN,
        SYNC_OPEN | FILE_DIRECTORY_FILE,
        null(),
    )?;
    reject_reparse(handle.raw())?;
    Ok((Dir(handle), anchor_display, rest))
}

pub(super) fn open_child_dir(dir: &Dir, name: &OsStr) -> Result<(Dir, AliasEntry), SysError> {
    let alias = nt_create_child(
        dir,
        name,
        DIRECTORY_ACCESS,
        NT_FILE_OPEN,
        SYNC_OPEN | FILE_DIRECTORY_FILE,
        null(),
    )?;
    let Some(tag) = reparse_tag(alias.raw())? else {
        return Ok((Dir(alias), AliasEntry::default()));
    };
    if tag != IO_REPARSE_TAG_MOUNT_POINT {
        return Err(SysError::SymlinkComponent);
    }
    let before = inspect_junction(alias.raw())?;
    pause_after_initial_junction_evidence(before.object);

    // Follow only after the no-follow handle has established that this exact
    // component is a junction. RootDirectory remains the held parent authority.
    let first_followed = nt_create_child(
        dir,
        name,
        DIRECTORY_ACCESS,
        NT_FILE_OPEN,
        FILE_SYNCHRONOUS_IO_NONALERT | FILE_DIRECTORY_FILE,
        null(),
    )?;
    pause_after_first_junction_follow(before.object);

    // Stabilize the alias evidence around two independent relative follow opens.
    // Comparing only before/after evidence around one follow admits A→B→A: that
    // follow can retain B while the witness records A. The second follow and final
    // no-follow recheck bind the returned handle to a repeatable target identity.
    let first_rechecked = nt_create_child(
        dir,
        name,
        DIRECTORY_ACCESS,
        NT_FILE_OPEN,
        SYNC_OPEN | FILE_DIRECTORY_FILE,
        null(),
    )?;
    let middle = inspect_junction(first_rechecked.raw())?;
    if before != middle {
        return Err(SysError::SymlinkComponent);
    }

    let second_followed = nt_create_child(
        dir,
        name,
        DIRECTORY_ACCESS,
        NT_FILE_OPEN,
        FILE_SYNCHRONOUS_IO_NONALERT | FILE_DIRECTORY_FILE,
        null(),
    )?;
    let final_rechecked = nt_create_child(
        dir,
        name,
        DIRECTORY_ACCESS,
        NT_FILE_OPEN,
        SYNC_OPEN | FILE_DIRECTORY_FILE,
        null(),
    )?;
    let after = inspect_junction(final_rechecked.raw())?;
    if before != after {
        return Err(SysError::SymlinkComponent);
    }

    let first_identity = followed_directory_identity(first_followed.raw())?;
    let second_identity = followed_directory_identity(second_followed.raw())?;
    if first_identity != second_identity {
        return Err(SysError::SymlinkComponent);
    }
    Ok((
        Dir(second_followed),
        AliasEntry {
            mount_id: None,
            followed_reparse_tag: Some(IO_REPARSE_TAG_MOUNT_POINT),
            followed_reparse_object: Some(after.object),
            followed_reparse_target: Some(after.target),
        },
    ))
}

pub(super) fn open_child_file_read(dir: &Dir, name: &OsStr) -> Result<File, SysError> {
    // Do not constrain the type in NtCreateFile: the public wrapper classifies a
    // directory/device as `NotRegularFile` from the held handle.
    let handle = nt_create_child(dir, name, READ_FILE_ACCESS, NT_FILE_OPEN, SYNC_OPEN, null())?;
    reject_reparse(handle.raw())?;
    Ok(File(handle))
}

pub(super) fn reopen_dir(dir: &Dir) -> Result<Dir, SysError> {
    duplicate_handle(dir.raw()).map(Dir)
}

pub(super) fn read_dir(dir: &Dir) -> Result<Vec<DirectoryEntry>, SysError> {
    const BUFFER_BYTES: usize = 64 * 1024;

    let handle = duplicate_handle(dir.raw())?;
    let mut entries = Vec::new();
    let mut restart = true;
    loop {
        let mut storage = vec![0_u64; BUFFER_BYTES / size_of::<u64>()];
        let information_class = if restart {
            FileIdBothDirectoryRestartInfo
        } else {
            FileIdBothDirectoryInfo
        };
        // SAFETY: `storage` is aligned writable memory for the advertised byte
        // length, and `handle` is a live directory handle for this synchronous
        // metadata query.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                handle.raw(),
                information_class,
                storage.as_mut_ptr().cast(),
                BUFFER_BYTES as u32,
            )
        };
        if ok == 0 {
            let code = last_error_code();
            if code == ERROR_NO_MORE_FILES {
                break;
            }
            return Err(map_win32(code));
        }
        restart = false;

        let bytes = storage.as_ptr().cast::<u8>();
        let name_offset = offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
        let mut offset = 0_usize;
        loop {
            if offset
                .checked_add(name_offset)
                .is_none_or(|end| end > BUFFER_BYTES)
            {
                return Err(SysError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "directory enumeration returned an invalid entry offset",
                )));
            }
            // SAFETY: the fixed header extent was checked above. Individual
            // fields are read unaligned because the Win32 chain controls offsets.
            let info = unsafe { bytes.add(offset).cast::<FILE_ID_BOTH_DIR_INFO>() };
            let next = unsafe { std::ptr::addr_of!((*info).NextEntryOffset).read_unaligned() };
            let attributes = unsafe { std::ptr::addr_of!((*info).FileAttributes).read_unaligned() };
            let name_bytes = unsafe { std::ptr::addr_of!((*info).FileNameLength).read_unaligned() };
            let name_bytes = usize::try_from(name_bytes).map_err(|_| {
                SysError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "directory entry name length does not fit memory",
                ))
            })?;
            if name_bytes % size_of::<u16>() != 0
                || offset
                    .checked_add(name_offset)
                    .and_then(|start| start.checked_add(name_bytes))
                    .is_none_or(|end| end > BUFFER_BYTES)
            {
                return Err(SysError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "directory enumeration returned an invalid file name extent",
                )));
            }
            let name = unsafe {
                let start = bytes.add(offset + name_offset).cast::<u16>();
                OsString::from_wide(std::slice::from_raw_parts(
                    start,
                    name_bytes / size_of::<u16>(),
                ))
            };
            if name != OsStr::new(".") && name != OsStr::new("..") {
                let kind = if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                    DirectoryEntryKind::Symlink
                } else if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
                    DirectoryEntryKind::Directory
                } else {
                    DirectoryEntryKind::RegularFile
                };
                entries.push(DirectoryEntry { name, kind });
            }
            if next == 0 {
                break;
            }
            let next = usize::try_from(next).map_err(|_| {
                SysError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "directory entry offset does not fit memory",
                ))
            })?;
            if next < name_offset
                || offset
                    .checked_add(next)
                    .is_none_or(|end| end >= BUFFER_BYTES)
            {
                return Err(SysError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "directory enumeration returned an invalid next-entry offset",
                )));
            }
            offset += next;
        }
    }
    Ok(entries)
}

pub(super) fn file_is_regular(file: &File) -> Result<bool, SysError> {
    let info = standard_info(file.raw())?;
    Ok(regular_file_by_kind(
        info.Directory,
        handle_kind(file.raw())?,
    ))
}

pub(super) fn identity_of_dir(dir: &Dir) -> Result<StableObjectId, SysError> {
    identity_of(dir.raw())
}

pub(super) fn identity_of_file(file: &File) -> Result<StableObjectId, SysError> {
    identity_of(file.raw())
}

pub(super) fn link_count_of_file(file: &File) -> Result<u64, SysError> {
    Ok(u64::from(standard_info(file.raw())?.NumberOfLinks))
}

pub(super) fn read_file(file: &File) -> Result<Vec<u8>, SysError> {
    let duplicate = duplicate_handle(file.raw())?;
    // DuplicateHandle refers to the same file object; seeking to zero mirrors the
    // Unix `dup` backend and makes every full read deterministic.
    // SAFETY: the duplicate is a live synchronous disk handle and all output
    // pointers are either valid or null as documented.
    if unsafe { SetFilePointerEx(duplicate.raw(), 0, null_mut(), FILE_BEGIN) } == 0 {
        return Err(map_win32(last_error_code()));
    }

    let mut result = Vec::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let mut read = 0_u32;
        // SAFETY: `buffer` is writable for the passed length, `read` is valid
        // output storage, and a synchronous handle requires no OVERLAPPED value.
        let ok = unsafe {
            ReadFile(
                duplicate.raw(),
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                &mut read,
                null_mut(),
            )
        };
        if ok == 0 {
            return Err(map_win32(last_error_code()));
        }
        if read == 0 {
            break;
        }
        result.extend_from_slice(&buffer[..read as usize]);
    }
    Ok(result)
}

fn with_current_user_sid<T>(
    operation: impl FnOnce(PSID) -> Result<T, SysError>,
) -> Result<T, SysError> {
    let mut token: HANDLE = null_mut();
    // SAFETY: output storage is valid and the process pseudo-handle is always
    // valid in the current process.
    let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
    if opened == 0 {
        return Err(map_win32(last_error_code()));
    }
    let token = OwnedRawHandle::new(token)?;

    let mut required = 0_u32;
    // SAFETY: the first query intentionally supplies no buffer to obtain size.
    let first =
        unsafe { GetTokenInformation(token.raw(), TokenUser, null_mut(), 0, &mut required) };
    if first != 0 || last_error_code() != ERROR_INSUFFICIENT_BUFFER || required == 0 {
        return Err(map_win32(last_error_code()));
    }
    let word = size_of::<usize>();
    let mut storage = vec![0_usize; (required as usize).div_ceil(word)];
    // SAFETY: the word vector is aligned for `TOKEN_USER` and has at least the
    // exact byte length returned by the first query.
    let ok = unsafe {
        GetTokenInformation(
            token.raw(),
            TokenUser,
            storage.as_mut_ptr().cast(),
            required,
            &mut required,
        )
    };
    if ok == 0 {
        return Err(map_win32(last_error_code()));
    }
    // SAFETY: successful TokenUser output begins with a fully initialized
    // `TOKEN_USER`, whose SID remains inside `storage` during the callback.
    let user = unsafe { &*storage.as_ptr().cast::<TOKEN_USER>() };
    if user.User.Sid.is_null() {
        return Err(SysError::private("the process token has no user SID"));
    }
    operation(user.User.Sid)
}

fn sid_string(sid: PSID) -> Result<String, SysError> {
    let mut string_sid = null_mut();
    // SAFETY: `sid` comes from a live TokenUser buffer and output storage is valid.
    if unsafe { ConvertSidToStringSidW(sid, &mut string_sid) } == 0 {
        return Err(map_win32(last_error_code()));
    }
    let allocation = LocalAllocation::new(string_sid.cast())?;
    let mut length = 0_usize;
    // SAFETY: ConvertSidToStringSidW returns a NUL-terminated LocalAlloc string.
    unsafe {
        while *string_sid.add(length) != 0 {
            length += 1;
        }
    }
    // SAFETY: the preceding scan established the initialized UTF-16 range.
    let units = unsafe { std::slice::from_raw_parts(string_sid, length) };
    let value = String::from_utf16(units)
        .map_err(|_| SysError::Io(io::Error::other("the current user SID was not UTF-16")))?;
    drop(allocation);
    Ok(value)
}

fn private_security_descriptor(directory: bool) -> Result<PrivateSecurityDescriptor, SysError> {
    with_current_user_sid(|sid| {
        let sid = sid_string(sid)?;
        let inheritance = if directory { "OICI" } else { "" };
        let sddl = format!("O:{sid}D:P(A;{inheritance};FA;;;{sid})");
        let wide: Vec<u16> = OsStr::new(&sddl).encode_wide().chain(Some(0)).collect();
        let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: `wide` is NUL-terminated and output storage is valid. Revision
        // one is the documented SDDL revision.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                1,
                &mut descriptor,
                null_mut(),
            )
        };
        if ok == 0 {
            return Err(map_win32(last_error_code()));
        }
        Ok(PrivateSecurityDescriptor(LocalAllocation::new(
            descriptor.cast(),
        )?))
    })
}

fn verify_acl_is_user_private(handle: HANDLE) -> Result<(), SysError> {
    with_current_user_sid(|user_sid| {
        let mut owner: PSID = null_mut();
        let mut dacl: *mut ACL = null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: all requested output pointers are valid. Returned owner/DACL
        // pointers remain owned by `descriptor` until the checks finish.
        let status = unsafe {
            GetSecurityInfo(
                handle,
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                null_mut(),
                &mut dacl,
                null_mut(),
                &mut descriptor,
            )
        };
        if status != 0 {
            return Err(map_win32(status));
        }
        let descriptor_allocation = LocalAllocation::new(descriptor.cast())?;
        if owner.is_null() || dacl.is_null() {
            return Err(SysError::private("owner or DACL is missing"));
        }
        // SAFETY: all pointers came from the successful GetSecurityInfo call.
        if unsafe { IsValidSecurityDescriptor(descriptor) } == 0
            || unsafe { IsValidAcl(dacl) } == 0
            || unsafe { EqualSid(owner, user_sid) } == 0
        {
            return Err(SysError::private(
                "invalid ACL or object not owned by the current user",
            ));
        }

        let mut control = 0_u16;
        let mut revision = 0_u32;
        // SAFETY: descriptor and output pointers are valid for this call.
        if unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } == 0 {
            return Err(map_win32(last_error_code()));
        }
        if control & SE_DACL_PROTECTED == 0 {
            return Err(SysError::private("DACL inheritance is not protected"));
        }

        let mut acl_info = ACL_SIZE_INFORMATION::default();
        // SAFETY: DACL and output storage are valid and sizes match the class.
        if unsafe {
            GetAclInformation(
                dacl,
                (&mut acl_info as *mut ACL_SIZE_INFORMATION).cast(),
                size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
        } == 0
        {
            return Err(map_win32(last_error_code()));
        }
        if acl_info.AceCount == 0 {
            return Err(SysError::private("DACL grants no access to the owner"));
        }

        let mut grants_full_control = false;
        for index in 0..acl_info.AceCount {
            let mut raw_ace: *mut c_void = null_mut();
            // SAFETY: index is within the count reported by GetAclInformation.
            if unsafe { GetAce(dacl, index, &mut raw_ace) } == 0 || raw_ace.is_null() {
                return Err(map_win32(last_error_code()));
            }
            // SAFETY: every ACE begins with an ACE_HEADER; IsValidAcl succeeded.
            let header = unsafe { &*raw_ace.cast::<windows_sys::Win32::Security::ACE_HEADER>() };
            if header.AceType != ACCESS_ALLOWED_ACE_TYPE as u8
                || u32::from(header.AceFlags) & INHERITED_ACE != 0
                || usize::from(header.AceSize) < size_of::<ACCESS_ALLOWED_ACE>()
            {
                return Err(SysError::private("DACL contains a non-owner allow ACE"));
            }
            // SAFETY: the validated ACE size covers ACCESS_ALLOWED_ACE and its
            // inline SidStart field.
            let ace = unsafe { &*raw_ace.cast::<ACCESS_ALLOWED_ACE>() };
            let ace_sid = (&ace.SidStart as *const u32).cast_mut().cast::<c_void>();
            // SAFETY: IsValidAcl validated the inline SID extent for this ACE.
            if unsafe { EqualSid(ace_sid, user_sid) } == 0 {
                return Err(SysError::private("DACL grants access to another SID"));
            }
            grants_full_control |= ace.Mask & FILE_ALL_ACCESS == FILE_ALL_ACCESS;
        }
        if !grants_full_control {
            return Err(SysError::private(
                "DACL does not grant the owner full control",
            ));
        }
        drop(descriptor_allocation);
        Ok(())
    })
}

fn verify_private(handle: HANDLE, directory: bool) -> Result<(), SysError> {
    let info = standard_info(handle)?;
    if info.Directory != directory {
        return Err(SysError::private("wrong filesystem object type"));
    }
    verify_acl_is_user_private(handle)
}

pub(super) fn verify_file_is_user_private(file: &File) -> Result<(), SysError> {
    verify_acl_is_user_private(file.raw())
}

pub(super) fn open_private_state_dir(path: &Path) -> Result<Dir, SysError> {
    let parent = path.parent().ok_or(SysError::BadComponent)?;
    let name = path.file_name().ok_or(SysError::BadComponent)?;
    let walked = super::walk_dir_no_symlinks(parent)?;
    open_private_child_state_dir(&walked.dir().0, name)
}

pub(super) fn open_private_child_state_dir(parent: &Dir, name: &OsStr) -> Result<Dir, SysError> {
    let descriptor = private_security_descriptor(true)?;

    let handle = match nt_create_child(
        parent,
        name,
        PRIVATE_DIRECTORY_ACCESS,
        NT_FILE_CREATE,
        SYNC_OPEN | FILE_DIRECTORY_FILE,
        descriptor.as_ptr(),
    ) {
        Ok(handle) => handle,
        Err(SysError::AlreadyExists) => nt_create_child(
            parent,
            name,
            PRIVATE_DIRECTORY_ACCESS,
            NT_FILE_OPEN,
            SYNC_OPEN | FILE_DIRECTORY_FILE,
            null(),
        )?,
        Err(error) => return Err(error),
    };
    reject_reparse(handle.raw())?;
    verify_private(handle.raw(), true)?;
    Ok(Dir(handle))
}

pub(super) fn open_private_lock_file(dir: &Dir, name: &str) -> Result<File, SysError> {
    let descriptor = private_security_descriptor(false)?;
    let handle = match nt_create_child(
        dir,
        OsStr::new(name),
        LOCK_FILE_ACCESS,
        NT_FILE_CREATE,
        SYNC_OPEN | FILE_NON_DIRECTORY_FILE,
        descriptor.as_ptr(),
    ) {
        Ok(handle) => handle,
        Err(SysError::AlreadyExists) => nt_create_child(
            dir,
            OsStr::new(name),
            LOCK_FILE_ACCESS,
            NT_FILE_OPEN,
            SYNC_OPEN,
            null(),
        )?,
        Err(error) => return Err(error),
    };
    reject_reparse(handle.raw())?;
    verify_private(handle.raw(), false)?;
    Ok(File(handle))
}

pub(super) fn try_lock_exclusive(file: &File) -> Result<(), SysError> {
    let mut overlapped = OVERLAPPED::default();
    // SAFETY: the held handle and OVERLAPPED storage remain valid for this
    // synchronous, fail-immediately call. The maximal range represents the
    // whole lock file for all practical offsets.
    let ok = unsafe {
        LockFileEx(
            file.raw(),
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
    if ok != 0 {
        return Ok(());
    }
    match last_error_code() {
        ERROR_LOCK_VIOLATION | ERROR_SHARING_VIOLATION => Err(SysError::WouldBlock),
        code => Err(map_win32(code)),
    }
}

fn write_and_flush(file: &File, bytes: &[u8]) -> Result<(), SysError> {
    let mut remaining = bytes;
    while !remaining.is_empty() {
        let length = remaining.len().min(u32::MAX as usize) as u32;
        let mut written = 0_u32;
        // SAFETY: `remaining` is readable for `length`, output storage is valid,
        // and staging handles are synchronous.
        let ok = unsafe {
            WriteFile(
                file.raw(),
                remaining.as_ptr(),
                length,
                &mut written,
                null_mut(),
            )
        };
        if ok == 0 {
            return Err(map_win32(last_error_code()));
        }
        if written == 0 {
            return Err(SysError::Io(io::Error::new(
                io::ErrorKind::WriteZero,
                "Windows wrote zero bytes to a CAS staging file",
            )));
        }
        remaining = &remaining[written as usize..];
    }
    // SAFETY: the staging handle is live and opened for writes.
    if unsafe { FlushFileBuffers(file.raw()) } == 0 {
        return Err(map_win32(last_error_code()));
    }
    Ok(())
}

fn rename_buffer(name: &OsStr, root: HANDLE) -> Result<(Vec<usize>, u32), SysError> {
    let units = validate_child_name(name)?;
    let name_bytes = units
        .len()
        .checked_mul(size_of::<u16>())
        .ok_or(SysError::BadComponent)?;
    let total = offset_of!(FILE_RENAME_INFORMATION, FileName)
        .checked_add(name_bytes)
        .ok_or(SysError::BadComponent)?;
    let byte_len = u32::try_from(total).map_err(|_| SysError::BadComponent)?;
    let mut storage = vec![0_usize; total.div_ceil(size_of::<usize>())];
    let info = storage.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
    // SAFETY: usize storage supplies sufficient alignment and `total` covers the
    // fixed header plus every UTF-16 filename byte.
    unsafe {
        (*info).Anonymous.ReplaceIfExists = false;
        (*info).RootDirectory = root;
        (*info).FileNameLength = name_bytes as u32;
        std::ptr::copy_nonoverlapping(units.as_ptr(), (*info).FileName.as_mut_ptr(), units.len());
    }
    Ok((storage, byte_len))
}

fn rename_non_replacing(file: &File, parent: &Dir, name: &OsStr) -> Result<(), SysError> {
    let (storage, byte_len) = rename_buffer(name, parent.raw())?;
    let mut io_status = IO_STATUS_BLOCK::default();
    // SAFETY: the aligned buffer contains the WDK FILE_RENAME_INFORMATION ABI,
    // its byte length excludes backing-storage padding, the source handle has
    // DELETE access, and RootDirectory is the held parent authority.
    let status = unsafe {
        NtSetInformationFile(
            file.raw(),
            &mut io_status,
            storage.as_ptr().cast(),
            byte_len,
            FileRenameInformation,
        )
    };
    if status < 0 {
        return Err(map_ntstatus(status));
    }
    Ok(())
}

fn mark_delete(file: &File) {
    let extended = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE
            | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
            | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    };
    // SAFETY: the structure and size match FileDispositionInfoEx and the held
    // staging handle has DELETE access.
    let ok = unsafe {
        SetFileInformationByHandle(
            file.raw(),
            FileDispositionInfoEx,
            (&extended as *const FILE_DISPOSITION_INFO_EX).cast(),
            size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
        )
    };
    if ok != 0 {
        return;
    }
    let basic = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: fallback layout and size match FileDispositionInfo.
    unsafe {
        let _ = SetFileInformationByHandle(
            file.raw(),
            FileDispositionInfo,
            (&basic as *const FILE_DISPOSITION_INFO).cast(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        );
    }
}

struct Staged {
    file: File,
    named: bool,
}

impl Staged {
    fn create(parent: &Dir) -> Result<Self, SysError> {
        let descriptor = private_security_descriptor(false)?;
        for _ in 0..128 {
            let counter = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let name = super::staging_name(counter);
            match nt_create_child(
                parent,
                OsStr::new(&name),
                STAGING_FILE_ACCESS,
                NT_FILE_CREATE,
                SYNC_OPEN | FILE_NON_DIRECTORY_FILE,
                descriptor.as_ptr(),
            ) {
                Ok(handle) => {
                    reject_reparse(handle.raw())?;
                    return Ok(Self {
                        file: File(handle),
                        named: true,
                    });
                }
                Err(SysError::AlreadyExists) => continue,
                Err(error) => return Err(error),
            }
        }
        Err(SysError::Io(io::Error::other(
            "unable to allocate a private CAS staging name",
        )))
    }

    fn write_and_sync(&self, bytes: &[u8]) -> Result<(), SysError> {
        write_and_flush(&self.file, bytes)
    }

    fn publish_create(&mut self, parent: &Dir, destination: &OsStr) -> Result<(), SysError> {
        rename_non_replacing(&self.file, parent, destination)?;
        self.named = false;
        Ok(())
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if self.named {
            // Deletion is attached to our held object, not resolved again by its
            // staging pathname, so a replacement at that name is never removed.
            mark_delete(&self.file);
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
        Err(error) => return Err(error),
    };
    if !file_is_regular(&file)? {
        return Err(SysError::NotRegularFile);
    }
    Ok(Some(Observed {
        identity: identity_of_file(&file)?,
        bytes: read_file(&file)?,
    }))
}

pub(super) fn cas_create(
    parent: &Dir,
    name: &OsStr,
    new: &[u8],
    before_publish: &dyn Fn() -> bool,
) -> Result<CasOutcome, SysError> {
    validate_child_name(name)?;
    if let Some(outcome) = cas_create_preflight(observe(parent, name).map(|value| value.is_some()))?
    {
        return Ok(outcome);
    }

    let mut staged = Staged::create(parent)?;
    staged.write_and_sync(new)?;
    if !before_publish() {
        return Ok(CasOutcome::Aborted);
    }
    match staged.publish_create(parent, name) {
        Ok(()) => Ok(CasOutcome::Published),
        Err(SysError::AlreadyExists) => Ok(CasOutcome::Stale),
        Err(error) => Err(error),
    }
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
    staged.write_and_sync(new)?;
    if !before_publish() {
        return Ok(CasOutcome::Aborted);
    }
    let Some(current) = hold_observed(parent, name, &observed)? else {
        return Ok(CasOutcome::Stale);
    };

    // Windows has no atomic directory-entry exchange, and a replacing rename
    // could clobber an actor that wins after the proof. Instead the proven
    // object is held with every other writer and deleter excluded, moved aside
    // under a private name, and the staged object is published with a
    // non-replacing rename. Only when that publication succeeds is the held
    // original deleted, through its own handle.
    park_held(&current, parent)?;
    match staged.publish_create(parent, name) {
        Ok(()) => {
            mark_delete(&current);
            Ok(CasOutcome::Published)
        }
        Err(SysError::AlreadyExists) => {
            // An external actor created the name after the move. Its object is
            // kept, and the displaced original stays under the private name
            // rather than being destroyed. The staged object is deleted on drop.
            Ok(CasOutcome::Stale)
        }
        Err(error) => {
            let _ = rename_non_replacing(&current, parent, name);
            Err(error)
        }
    }
}

pub(super) fn cas_overwrite(
    parent: &Dir,
    name: &OsStr,
    expected: &[u8],
    new: &[u8],
    before_publish: &dyn Fn() -> bool,
) -> Result<CasOutcome, SysError> {
    validate_child_name(name)?;
    let Some(observed) = observe(parent, name)? else {
        return Ok(CasOutcome::Stale);
    };
    if observed.bytes != expected {
        return Ok(CasOutcome::Stale);
    }
    if !before_publish() {
        return Ok(CasOutcome::Aborted);
    }
    // Hold the proven object for writing while excluding every other writer
    // and deleter, then rewrite it through that handle.
    let handle = match nt_create_shared(
        parent.raw(),
        validate_child_name(name)?,
        READ_FILE_ACCESS | FILE_GENERIC_WRITE,
        NT_FILE_OPEN,
        SYNC_OPEN | FILE_NON_DIRECTORY_FILE,
        null(),
        FILE_SHARE_READ,
    ) {
        Ok((handle, _)) => handle,
        Err(SysError::NotFound) => return Ok(CasOutcome::Stale),
        Err(SysError::Io(error))
            if error.raw_os_error() == Some(ERROR_SHARING_VIOLATION as i32) =>
        {
            return Ok(CasOutcome::Stale);
        }
        Err(error) => return Err(error),
    };
    reject_reparse(handle.raw())?;
    let file = File(handle);
    if !file_is_regular(&file)?
        || identity_of_file(&file)? != observed.identity
        || read_file(&file)? != observed.bytes
    {
        return Ok(CasOutcome::Stale);
    }
    // SAFETY: the handle is live and synchronous; no output pointer is used.
    if unsafe { SetFilePointerEx(file.raw(), 0, null_mut(), FILE_BEGIN) } == 0 {
        return Err(map_win32(last_error_code()));
    }
    write_and_flush(&file, new)?;
    // SAFETY: the file pointer sits at the end of the new bytes, so this sets
    // the file length to exactly `new.len()`.
    if unsafe { SetEndOfFile(file.raw()) } == 0 {
        return Err(map_win32(last_error_code()));
    }
    // SAFETY: the handle is live and opened for writes.
    if unsafe { FlushFileBuffers(file.raw()) } == 0 {
        return Err(map_win32(last_error_code()));
    }
    Ok(CasOutcome::Published)
}

/// Open a child directory, creating it first when it is absent. An existing
/// entry goes through the same reparse-aware open as any other component.
pub(super) fn create_child_dir(parent: &Dir, name: &OsStr) -> Result<Dir, SysError> {
    match nt_create_child(
        parent,
        name,
        DIRECTORY_ACCESS,
        NT_FILE_CREATE,
        SYNC_OPEN | FILE_DIRECTORY_FILE,
        null(),
    ) {
        Ok(handle) => {
            reject_reparse(handle.raw())?;
            Ok(Dir(handle))
        }
        Err(SysError::AlreadyExists) => open_child_dir(parent, name).map(|(dir, _entry)| dir),
        Err(error) => Err(error),
    }
}

pub(super) fn cas_delete(
    parent: &Dir,
    name: &OsStr,
    expected: &[u8],
    before_publish: &dyn Fn() -> bool,
) -> Result<CasOutcome, SysError> {
    validate_child_name(name)?;
    let Some(observed) = observe(parent, name)? else {
        return Ok(CasOutcome::Stale);
    };
    if observed.bytes != expected {
        return Ok(CasOutcome::Stale);
    }
    if !before_publish() {
        return Ok(CasOutcome::Aborted);
    }
    let Some(current) = hold_observed(parent, name, &observed)? else {
        return Ok(CasOutcome::Stale);
    };
    // The deletion is attached to the held, proven object; no name is resolved
    // again, so a replacement cannot be removed in its place.
    set_delete_disposition(&current)?;
    Ok(CasOutcome::Published)
}

/// Re-open the object at `name` with delete access while excluding every other
/// writer and deleter, and prove it is still exactly `observed`. `None` means
/// the name now holds something else, or nothing.
fn hold_observed(
    parent: &Dir,
    name: &OsStr,
    observed: &Observed,
) -> Result<Option<File>, SysError> {
    let handle = match nt_create_shared(
        parent.raw(),
        validate_child_name(name)?,
        READ_FILE_ACCESS | DELETE,
        NT_FILE_OPEN,
        SYNC_OPEN | FILE_NON_DIRECTORY_FILE,
        null(),
        FILE_SHARE_READ,
    ) {
        Ok((handle, _)) => handle,
        Err(SysError::NotFound) => return Ok(None),
        Err(SysError::Io(error))
            if error.raw_os_error() == Some(ERROR_SHARING_VIOLATION as i32) =>
        {
            // Another actor holds the object open for writing or deletion; its
            // state is not ours to decide.
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    reject_reparse(handle.raw())?;
    let file = File(handle);
    if !file_is_regular(&file)? || identity_of_file(&file)? != observed.identity {
        return Ok(None);
    }
    if read_file(&file)? != observed.bytes {
        return Ok(None);
    }
    Ok(Some(file))
}

/// Move a held file to a fresh private name in `parent`.
fn park_held(file: &File, parent: &Dir) -> Result<(), SysError> {
    for _ in 0..128 {
        let counter = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = super::staging_name(counter);
        match rename_non_replacing(file, parent, OsStr::new(&name)) {
            Ok(()) => return Ok(()),
            Err(SysError::AlreadyExists) => continue,
            Err(error) => return Err(error),
        }
    }
    Err(SysError::Io(io::Error::other(
        "unable to allocate a private CAS staging name",
    )))
}

/// Mark a held file for deletion, reporting failure.
fn set_delete_disposition(file: &File) -> Result<(), SysError> {
    let extended = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE
            | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
            | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    };
    // SAFETY: the structure and size match FileDispositionInfoEx and the held
    // handle has DELETE access.
    let ok = unsafe {
        SetFileInformationByHandle(
            file.raw(),
            FileDispositionInfoEx,
            (&extended as *const FILE_DISPOSITION_INFO_EX).cast(),
            size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
        )
    };
    if ok != 0 {
        return Ok(());
    }
    let basic = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: fallback layout and size match FileDispositionInfo.
    let ok = unsafe {
        SetFileInformationByHandle(
            file.raw(),
            FileDispositionInfo,
            (&basic as *const FILE_DISPOSITION_INFO).cast(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(map_win32(last_error_code()));
    }
    Ok(())
}
