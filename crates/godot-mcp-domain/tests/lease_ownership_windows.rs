//! Native Windows qualification for Todo 2 B2.

#![cfg(windows)]

use std::ffi::OsStr;
#[cfg(feature = "test-hooks")]
use std::ffi::c_void;
use std::fs;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
#[cfg(feature = "test-hooks")]
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::lease::{LeaseError, LeaseManager};
use godot_mcp_domain::objid::StableObjectId;
use godot_mcp_domain::safe_root::SafeRootError;
#[cfg(feature = "test-hooks")]
use godot_mcp_domain::safe_root::{CasPublishKind, install_cas_publish_hook};
#[cfg(feature = "test-hooks")]
use godot_mcp_domain::sysfs::{
    install_junction_follow_hook, open_child_regular_file, open_private_lock_file,
    open_private_state_dir, raw_dir_handle_for_test, raw_file_handle_for_test,
    walk_dir_no_symlinks,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
#[cfg(feature = "test-hooks")]
use windows_sys::Win32::Foundation::{GetHandleInformation, HANDLE_FLAG_INHERIT};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileIdInfo, GetFileInformationByHandleEx,
    OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::SystemServices::IO_REPARSE_TAG_MOUNT_POINT;

const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
const FSCTL_SET_REPARSE_POINT: u32 = 589_988;
const FSCTL_DELETE_REPARSE_POINT: u32 = 589_996;
// A third-party test tag used by established Windows reparse fixtures. Bit 31
// is clear (non-Microsoft) and the value satisfies the WDK `IsReparseTagValid`
// predicate `((tag & !0xF000FFFF) == 0) && tag > 2`, which native Windows
// enforces at `FSCTL_SET_REPARSE_POINT`.
const VENDOR_REPARSE_TAG: u32 = 42;
const REPARSE_TAG_ALLOWED_MASK: u32 = 0xF000_FFFF;
const VENDOR_GUID: [u8; 16] = [
    0x72, 0x44, 0x53, 0x11, 0x39, 0x17, 0x4f, 0x8b, 0x91, 0x13, 0x65, 0xa2, 0x78, 0xe4, 0xa9, 0x51,
];

const ENV_ROLE: &str = "GODOT_MCP_WINDOWS_TEST_ROLE";
const ENV_ROOT: &str = "GODOT_MCP_WINDOWS_TEST_ROOT";
const ENV_STATE: &str = "GODOT_MCP_WINDOWS_TEST_STATE";
const ENV_READY: &str = "GODOT_MCP_WINDOWS_TEST_READY";
const ENV_STOP: &str = "GODOT_MCP_WINDOWS_TEST_STOP";
#[cfg(feature = "test-hooks")]
const ENV_HANDLES: &str = "GODOT_MCP_WINDOWS_TEST_HANDLES";

struct RawHandle(HANDLE);

impl RawHandle {
    fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for RawHandle {
    fn drop(&mut self) {
        // SAFETY: this wrapper is constructed from one successful CreateFileW
        // result and owns that handle exactly once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

struct Fixture {
    _tmp: tempfile::TempDir,
    base: PathBuf,
    root: PathBuf,
    alias: PathBuf,
    state: PathBuf,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().to_path_buf();
    let root = base.join("game");
    fs::create_dir(&root).expect("create project root");
    fs::write(root.join("project.godot"), b"[application]\n").expect("write project marker");
    let alias = base.join("game-junction");
    create_junction(&alias, &root);
    let state = base.join("state");
    Fixture {
        _tmp: tmp,
        base,
        root,
        alias,
        state,
    }
}

fn wide_nul(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

fn open_reparse(path: &Path) -> RawHandle {
    let path = wide_nul(path.as_os_str());
    // SAFETY: the path is NUL-terminated and all optional pointers are null.
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            SHARE_ALL,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    assert_ne!(
        handle,
        INVALID_HANDLE_VALUE,
        "open reparse point failed: {}",
        std::io::Error::last_os_error()
    );
    assert!(!handle.is_null(), "CreateFileW returned a null handle");
    RawHandle(handle)
}

fn put_u16(buffer: &mut [u8], offset: usize, value: u16) {
    buffer[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(buffer: &mut [u8], offset: usize, value: u32) {
    buffer[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn nt_substitute_name(target: &Path) -> (Vec<u16>, Vec<u16>) {
    let canonical = target.canonicalize().expect("canonical junction target");
    let display: Vec<u16> = canonical.as_os_str().encode_wide().collect();
    let slash = u16::from(b'\\');
    let question = u16::from(b'?');
    let prefix = [slash, slash, question, slash];
    let mut substitute: Vec<u16> = "\\??\\".encode_utf16().collect();
    if display.starts_with(&prefix) {
        substitute.extend_from_slice(&display[prefix.len()..]);
    } else {
        substitute.extend_from_slice(&display);
    }
    (substitute, display)
}

fn set_junction_on(handle: HANDLE, target: &Path) {
    let (substitute, print) = nt_substitute_name(target);
    let substitute_bytes = substitute.len() * size_of::<u16>();
    let print_bytes = print.len() * size_of::<u16>();
    let path_bytes = substitute_bytes + 2 + print_bytes + 2;
    let data_length = 8 + path_bytes;
    let mut buffer = vec![0_u8; 8 + data_length];
    put_u32(&mut buffer, 0, IO_REPARSE_TAG_MOUNT_POINT);
    put_u16(&mut buffer, 4, data_length as u16);
    put_u16(&mut buffer, 8, 0);
    put_u16(&mut buffer, 10, substitute_bytes as u16);
    put_u16(&mut buffer, 12, (substitute_bytes + 2) as u16);
    put_u16(&mut buffer, 14, print_bytes as u16);
    let mut cursor = 16;
    for unit in substitute {
        put_u16(&mut buffer, cursor, unit);
        cursor += 2;
    }
    cursor += 2;
    for unit in print {
        put_u16(&mut buffer, cursor, unit);
        cursor += 2;
    }
    let mut returned = 0_u32;
    // SAFETY: the complete mount-point buffer remains readable for this
    // synchronous call and all output pointers are valid.
    let ok = unsafe {
        DeviceIoControl(
            handle,
            FSCTL_SET_REPARSE_POINT,
            buffer.as_ptr().cast(),
            buffer.len() as u32,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    assert_ne!(
        ok,
        0,
        "set junction failed: {}",
        std::io::Error::last_os_error()
    );
}

fn delete_reparse(handle: HANDLE, tag: u32, guid: Option<[u8; 16]>) {
    let mut buffer = vec![0_u8; if guid.is_some() { 24 } else { 8 }];
    put_u32(&mut buffer, 0, tag);
    if let Some(guid) = guid {
        buffer[8..24].copy_from_slice(&guid);
    }
    let mut returned = 0_u32;
    // SAFETY: the deletion header has the layout required by the tag family.
    let ok = unsafe {
        DeviceIoControl(
            handle,
            FSCTL_DELETE_REPARSE_POINT,
            buffer.as_ptr().cast(),
            buffer.len() as u32,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    assert_ne!(
        ok,
        0,
        "delete reparse point failed: {}",
        std::io::Error::last_os_error()
    );
}

fn object_id(handle: HANDLE) -> (u64, [u8; 16]) {
    let mut info = FILE_ID_INFO::default();
    // SAFETY: info is writable storage matching FileIdInfo.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileIdInfo,
            (&mut info as *mut FILE_ID_INFO).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    };
    assert_ne!(
        ok,
        0,
        "query file id failed: {}",
        std::io::Error::last_os_error()
    );
    (info.VolumeSerialNumber, info.FileId.Identifier)
}

#[cfg(feature = "test-hooks")]
fn encoded_object_id(handle: isize) -> String {
    let (volume, id) = object_id(handle as HANDLE);
    let id = id
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("{}:{volume}:{id}", handle as isize)
}

fn create_junction(alias: &Path, target: &Path) {
    fs::create_dir(alias).expect("create junction object");
    let handle = open_reparse(alias);
    set_junction_on(handle.raw(), target);
}

fn retarget_junction_in_place(alias: &Path, target: &Path) {
    let handle = open_reparse(alias);
    let before = object_id(handle.raw());
    delete_reparse(handle.raw(), IO_REPARSE_TAG_MOUNT_POINT, None);
    set_junction_on(handle.raw(), target);
    assert_eq!(
        object_id(handle.raw()),
        before,
        "retarget must preserve the junction object id"
    );
}

fn create_vendor_reparse(path: &Path) {
    // Third-party fixture: the Microsoft bit must stay clear, and the tag must
    // satisfy the kernel's own validity predicate, otherwise the ioctl fails
    // with ERROR_INVALID_REPARSE_DATA before any product code is exercised.
    assert_eq!(
        VENDOR_REPARSE_TAG & 0x8000_0000,
        0,
        "the fixture tag must not claim a Microsoft-reserved tag"
    );
    assert_eq!(
        VENDOR_REPARSE_TAG & !REPARSE_TAG_ALLOWED_MASK,
        0,
        "the fixture tag must set only bits inside the WDK-allowed mask"
    );
    assert!(
        VENDOR_REPARSE_TAG > 2,
        "the WDK reserves tag values 0..=2 as invalid"
    );
    fs::create_dir(path).expect("create vendor reparse directory");
    let handle = open_reparse(path);
    let mut buffer = vec![0_u8; 24];
    put_u32(&mut buffer, 0, VENDOR_REPARSE_TAG);
    buffer[8..24].copy_from_slice(&VENDOR_GUID);
    let mut returned = 0_u32;
    // SAFETY: this is a complete REPARSE_GUID_DATA_BUFFER with zero payload.
    let ok = unsafe {
        DeviceIoControl(
            handle.raw(),
            FSCTL_SET_REPARSE_POINT,
            buffer.as_ptr().cast(),
            buffer.len() as u32,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    assert_ne!(
        ok,
        0,
        "set vendor reparse tag failed: {}",
        std::io::Error::last_os_error()
    );
}

fn remove_vendor_reparse(path: &Path) {
    let handle = open_reparse(path);
    delete_reparse(handle.raw(), VENDOR_REPARSE_TAG, Some(VENDOR_GUID));
}

fn has_cas_temp(dir: &Path) -> bool {
    fs::read_dir(dir).expect("read directory").any(|entry| {
        entry
            .expect("directory entry")
            .file_name()
            .to_string_lossy()
            .starts_with(".cas-")
    })
}

fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if path.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {}", path.display());
}

fn spawn_helper(role: &str, fx: &Fixture, ready: &Path, stop: &Path, case: &str) -> Child {
    Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            case,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(ENV_ROLE, role)
        .env(ENV_ROOT, &fx.alias)
        .env(ENV_STATE, &fx.state)
        .env(ENV_READY, ready)
        .env(ENV_STOP, stop)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn helper")
}

#[test]
fn junction_alias_converges_with_stable_windows_ids_and_cross_process_conflicts() {
    let fx = fixture();
    let manager = LeaseManager::new(&fx.state);
    let through_alias = manager
        .acquire(&fx.alias)
        .expect("acquire through junction");
    let direct = manager.acquire(&fx.root).expect("same object converges");
    assert!(through_alias.same_lease(&direct));
    assert_eq!(through_alias.lease_key(), direct.lease_key());
    assert_eq!(manager.held_lease_count(), 1);
    let key = through_alias.lease_key();
    match (key.root, key.project_file) {
        (
            StableObjectId::Windows {
                volume_serial: root_volume,
                file_id: root_id,
            },
            StableObjectId::Windows {
                volume_serial: marker_volume,
                file_id: marker_id,
            },
        ) => {
            assert_ne!(root_volume, 0);
            assert_eq!(root_volume, marker_volume);
            assert_ne!(root_id, [0; 16]);
            assert_ne!(marker_id, [0; 16]);
            assert_ne!(root_id, marker_id);
        }
        ids => panic!("expected Windows identities, got {ids:?}"),
    }
    assert_eq!(
        through_alias
            .safe_root()
            .read(Path::new("project.godot"))
            .expect("read through held root"),
        b"[application]\n"
    );

    drop(through_alias);
    drop(direct);
    drop(manager);
    let ready = fx.base.join("holder.ready");
    let stop = fx.base.join("holder.stop");
    let child = spawn_helper(
        "hold",
        &fx,
        &ready,
        &stop,
        "child_helper_holds_junction_lease",
    );
    wait_for(&ready);
    let second = LeaseManager::new(&fx.state);
    let error = second
        .acquire(&fx.root)
        .expect_err("LockFileEx must reject a second process");
    assert!(matches!(error, LeaseError::Conflict { .. }));
    assert_eq!(error.error_kind(), ErrorKind::Conflict);
    fs::write(&stop, b"stop").expect("stop child");
    let output = child.wait_with_output().expect("wait for holder");
    assert!(
        output.status.success(),
        "holder failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn in_place_junction_retarget_is_rejected_before_read_or_write() {
    let fx = fixture();
    let evil = fx.base.join("evil");
    fs::create_dir(&evil).expect("create replacement target");
    fs::write(evil.join("project.godot"), b"[application]\n; evil\n").expect("evil marker");
    let manager = LeaseManager::new(&fx.state);
    let lease = manager.acquire(&fx.alias).expect("acquire junction lease");
    let safe_root = lease.safe_root();
    retarget_junction_in_place(&fx.alias, &evil);

    assert!(matches!(
        safe_root.read(Path::new("project.godot")),
        Err(SafeRootError::IdentityChanged)
    ));
    assert!(matches!(
        safe_root.cas_write(Path::new("pwned.tres"), None, b"x"),
        Err(SafeRootError::IdentityChanged)
    ));
    assert!(!evil.join("pwned.tres").exists());
    assert!(!has_cas_temp(&evil));
}

#[cfg(feature = "test-hooks")]
#[test]
fn aba_junction_retarget_during_acquisition_fails_before_state_creation() {
    let fx = fixture();
    let alternate = fx.base.join("alternate");
    fs::create_dir(&alternate).expect("create alternate junction target");
    fs::write(
        alternate.join("project.godot"),
        b"[application]\n; alternate\n",
    )
    .expect("write alternate marker");

    // Keep the no-follow alias object alive while deriving the hook key, so the
    // test targets the exact junction object whose in-place retargets preserve ID.
    let alias_handle = open_reparse(&fx.alias);
    let (volume_serial, file_id) = object_id(alias_handle.raw());
    let alias_object = StableObjectId::Windows {
        volume_serial,
        file_id,
    };
    let after_initial_reached = Arc::new(Barrier::new(2));
    let after_initial_resume = Arc::new(Barrier::new(2));
    let after_first_follow_reached = Arc::new(Barrier::new(2));
    let after_first_follow_resume = Arc::new(Barrier::new(2));
    let _hook = install_junction_follow_hook(
        alias_object,
        Arc::clone(&after_initial_reached),
        Arc::clone(&after_initial_resume),
        Arc::clone(&after_first_follow_reached),
        Arc::clone(&after_first_follow_resume),
    );

    let alias = fx.alias.clone();
    let state = fx.state.clone();
    let worker = std::thread::spawn(move || LeaseManager::new(&state).acquire(&alias));

    // Initial evidence records A. Retarget to B before follow #1, then restore A
    // after that held handle has opened B. Both subsequent evidence reads see A,
    // so only the two followed-handle identity comparison detects this schedule.
    after_initial_reached.wait();
    retarget_junction_in_place(&fx.alias, &alternate);
    after_initial_resume.wait();
    after_first_follow_reached.wait();
    retarget_junction_in_place(&fx.alias, &fx.root);
    after_first_follow_resume.wait();

    let error = worker
        .join()
        .expect("acquisition worker must not panic")
        .expect_err("A→B→A retarget must fail closed");
    assert!(
        matches!(error, LeaseError::SymlinkComponent { .. }),
        "unexpected acquisition error: {error:?}"
    );
    assert!(
        !fx.state.exists(),
        "junction stabilization must fail before the lease state directory is created"
    );
}

#[test]
fn symlink_and_vendor_reparse_components_are_rejected_even_before_parent_traversal() {
    let fx = fixture();
    let manager = LeaseManager::new(&fx.state);

    let symlink = fx.base.join("symlink-detour");
    std::os::windows::fs::symlink_dir(&fx.root, &symlink).expect("create directory symlink");
    let symlink_error = manager
        .acquire(&symlink.join("..").join("game"))
        .expect_err("symlink/.. must not be normalized away");
    assert!(matches!(symlink_error, LeaseError::SymlinkComponent { .. }));
    assert!(!fx.state.exists());

    let vendor = fx.base.join("vendor-detour");
    create_vendor_reparse(&vendor);
    let vendor_error = manager
        .acquire(&vendor.join("..").join("game"))
        .expect_err("unknown reparse/.. must not be normalized away");
    assert!(matches!(vendor_error, LeaseError::SymlinkComponent { .. }));
    assert!(!fx.state.exists());
    remove_vendor_reparse(&vendor);

    let inner_target = fx.base.join("inner-target");
    fs::create_dir(&inner_target).expect("inner symlink target");
    std::os::windows::fs::symlink_dir(&inner_target, fx.root.join("inner-link"))
        .expect("create symlink below junction");
    let nested_error = manager
        .acquire(&fx.alias.join("inner-link").join(".."))
        .expect_err("junction/symlink/.. must reject the symlink component");
    assert!(matches!(nested_error, LeaseError::SymlinkComponent { .. }));
}

#[test]
fn generated_acl_is_accepted_and_an_inherited_acl_is_rejected() {
    let accepted = fixture();
    LeaseManager::new(&accepted.state)
        .acquire(&accepted.root)
        .expect("backend-created protected owner-only ACL is accepted");

    let rejected = fixture();
    fs::create_dir(&rejected.state).expect("create normally inherited state directory");
    let error = LeaseManager::new(&rejected.state)
        .acquire(&rejected.root)
        .expect_err("an inherited/default ACL must not be accepted as owner-private");
    assert!(matches!(error, LeaseError::InsecureStateDir { .. }));
    assert_eq!(error.error_kind(), ErrorKind::PermissionDenied);
    assert_eq!(
        fs::read_dir(&rejected.state)
            .expect("read rejected state")
            .count(),
        0
    );
}

#[cfg(feature = "test-hooks")]
#[test]
fn root_project_state_and_lock_handles_are_not_inherited() {
    let fx = fixture();
    let walked = walk_dir_no_symlinks(&fx.root).expect("walk root");
    let project =
        open_child_regular_file(walked.dir(), OsStr::new("project.godot")).expect("open marker");
    let state = open_private_state_dir(&fx.state).expect("open private state");
    let lock = open_private_lock_file(&state, "inheritance-probe.lock").expect("open lock");
    let handles = [
        raw_dir_handle_for_test(walked.dir()),
        raw_file_handle_for_test(&project),
        raw_dir_handle_for_test(&state),
        raw_file_handle_for_test(&lock),
    ];
    for handle in handles {
        let mut flags = u32::MAX;
        // SAFETY: these values come from live held handles in this process.
        assert_ne!(
            unsafe { GetHandleInformation(handle as HANDLE, &mut flags) },
            0
        );
        assert_eq!(flags & HANDLE_FLAG_INHERIT, 0);
    }

    let output = Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "child_helper_probes_handle_inheritance",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(ENV_ROLE, "probe-handles")
        .env(ENV_HANDLES, handles.map(encoded_object_id).join(","))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run handle probe");
    assert!(
        output.status.success(),
        "probe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(feature = "test-hooks")]
#[test]
fn create_and_replace_races_preserve_external_state_and_clean_staging() {
    let fx = fixture();
    let manager = LeaseManager::new(&fx.state);
    let lease = manager.acquire(&fx.root).expect("acquire for CAS races");

    let create_rel = PathBuf::from("create-race.tres");
    let create_reached = Arc::new(Barrier::new(2));
    let create_resume = Arc::new(Barrier::new(2));
    let _create_hook = install_cas_publish_hook(
        CasPublishKind::Create,
        create_rel.clone(),
        Arc::clone(&create_reached),
        Arc::clone(&create_resume),
    );
    let create_root = lease.safe_root();
    let create_worker_rel = create_rel.clone();
    let create_worker =
        std::thread::spawn(move || create_root.cas_write(&create_worker_rel, None, b"ours"));
    create_reached.wait();
    fs::write(fx.root.join(&create_rel), b"external-create").expect("external create");
    create_resume.wait();
    let create_result = create_worker.join().expect("create worker");
    assert!(
        matches!(create_result, Err(SafeRootError::StalePreimage { .. })),
        "create collision returned {create_result:?}"
    );
    assert_eq!(
        fs::read(fx.root.join(&create_rel)).expect("external create survives"),
        b"external-create"
    );
    assert!(!has_cas_temp(&fx.root));

    let replace_rel = PathBuf::from("replace-race.tres");
    fs::write(fx.root.join(&replace_rel), b"observed").expect("seed replacement");
    let replace_reached = Arc::new(Barrier::new(2));
    let replace_resume = Arc::new(Barrier::new(2));
    let _replace_hook = install_cas_publish_hook(
        CasPublishKind::Replace,
        replace_rel.clone(),
        Arc::clone(&replace_reached),
        Arc::clone(&replace_resume),
    );
    let replace_root = lease.safe_root();
    let replace_worker_rel = replace_rel.clone();
    let replace_worker = std::thread::spawn(move || {
        replace_root.cas_write(&replace_worker_rel, Some(b"observed"), b"ours")
    });
    replace_reached.wait();
    let external = fx.root.join("external.tmp");
    fs::write(&external, b"external-replacement").expect("stage external replacement");
    fs::remove_file(fx.root.join(&replace_rel)).expect("remove observed entry");
    fs::rename(&external, fx.root.join(&replace_rel)).expect("publish external replacement");
    replace_resume.wait();
    assert!(matches!(
        replace_worker.join().expect("replace worker"),
        Err(SafeRootError::StalePreimage { .. })
    ));
    assert_eq!(
        fs::read(fx.root.join(&replace_rel)).expect("external replacement survives"),
        b"external-replacement"
    );
    assert!(!has_cas_temp(&fx.root));
}

#[test]
#[ignore = "helper case: re-executed by the cross-process LockFileEx test"]
fn child_helper_holds_junction_lease() {
    if std::env::var(ENV_ROLE).as_deref() != Ok("hold") {
        return;
    }
    let root = PathBuf::from(std::env::var(ENV_ROOT).expect("helper root"));
    let state = PathBuf::from(std::env::var(ENV_STATE).expect("helper state"));
    let ready = PathBuf::from(std::env::var(ENV_READY).expect("helper ready"));
    let stop = PathBuf::from(std::env::var(ENV_STOP).expect("helper stop"));
    let manager = LeaseManager::new(&state);
    let _lease = manager.acquire(&root).expect("helper acquires lease");
    fs::write(&ready, b"ready").expect("signal ready");
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline && !stop.exists() {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(stop.exists(), "helper timed out");
}

#[cfg(feature = "test-hooks")]
#[test]
#[ignore = "helper case: re-executed by the handle-inheritance test"]
fn child_helper_probes_handle_inheritance() {
    if std::env::var(ENV_ROLE).as_deref() != Ok("probe-handles") {
        return;
    }
    for value in std::env::var(ENV_HANDLES).expect("handle list").split(',') {
        let mut fields = value.split(':');
        let handle: isize = fields
            .next()
            .expect("handle field")
            .parse()
            .expect("numeric handle");
        let expected_volume: u64 = fields
            .next()
            .expect("volume field")
            .parse()
            .expect("numeric volume");
        let expected_id = fields.next().expect("file-id field");
        assert!(fields.next().is_none(), "unexpected handle identity fields");
        let mut flags = 0_u32;
        // SAFETY: probing an arbitrary handle value is a documented query; an
        // invalid/non-inherited value simply returns FALSE.
        if unsafe { GetHandleInformation(handle as *mut c_void, &mut flags) } == 0 {
            continue;
        }
        let mut info = FILE_ID_INFO::default();
        // A child may independently reuse the same numeric handle value. It is
        // inherited only when that value still names the parent's kernel object.
        let queried = unsafe {
            GetFileInformationByHandleEx(
                handle as HANDLE,
                FileIdInfo,
                (&mut info as *mut FILE_ID_INFO).cast(),
                size_of::<FILE_ID_INFO>() as u32,
            )
        };
        if queried == 0 {
            continue;
        }
        let actual_id = info
            .FileId
            .Identifier
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_ne!(
            (info.VolumeSerialNumber, actual_id.as_str()),
            (expected_volume, expected_id),
            "parent object for handle {handle} was inherited with flags {flags:#x}"
        );
    }
}
