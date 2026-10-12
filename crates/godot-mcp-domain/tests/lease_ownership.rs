//! Todo 2 integration tests: secure startup, project ownership and handle-safe
//! paths.
//!
//! The two descriptor-owned cases named by `qa/tasks/2.json` are:
//!
//! * [`same_object_alias_converges_to_one_lease`] — one process holds the
//!   object-identity lease and performs a descriptor-relative in-root read/CAS
//!   write through a lexical/case (or junction/bind-mount) alias.
//! * [`second_owner_symlink_and_retarget_reject`] — the production code path
//!   returns `conflict`/`permission_denied` for a second owner (proved with a
//!   real second OS process), a symlink component (even one resolving to the
//!   same already-leased object), a root *or* `project.godot` replacement/
//!   retarget, an outside path, a stale preimage and a direct non-loopback
//!   configuration, each failing before any cache/socket/file effect.
//!
//! The remaining tests cover the ownership properties those two cases depend on
//! but do not themselves observe: user-private state-directory permissions,
//! mount identity as alias evidence (never in the advisory-lock key), same-
//! manager acquisition convergence, and handle non-inheritability
//! across process creation.
//!
//! The whole feature is Unix-object-identity based; on a non-Unix host the
//! ownership primitives are explicitly unsupported (fail-closed) rather than
//! silently weakened, so these tests only assert the real behaviour on Unix.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::config::{CliOverrides, ConfigError, load_config};
use godot_mcp_domain::lease::{LeaseError, LeaseManager, lock_file_name};
use godot_mcp_domain::objid::{AliasEntry, AliasWitness, LeaseKey, StableObjectId};
use godot_mcp_domain::safe_root::SafeRootError;
#[cfg(feature = "test-hooks")]
use godot_mcp_domain::safe_root::{CasPublishKind, install_cas_publish_hook};

/// A fresh, real (symlink-free) project directory containing a `project.godot`
/// file, plus the *path* of a not-yet-created private state directory for
/// advisory locks (the lease layer must create it with private permissions).
struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    state: PathBuf,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().expect("tempdir");
    // Canonicalize so the base is symlink-free (macOS /var -> /private/var etc.);
    // every alias built below is therefore a *deliberate* alias, never an
    // artefact of the temp base.
    let base = tmp.path().canonicalize().expect("canonicalize temp base");
    let root = base.join("game");
    fs::create_dir(&root).expect("mkdir game");
    fs::write(root.join("project.godot"), b"[application]\n").expect("write project.godot");
    let state = base.join("state");
    Fixture {
        _tmp: tmp,
        root,
        state,
    }
}

fn is_conflict_or_denied(kind: ErrorKind) -> bool {
    matches!(kind, ErrorKind::Conflict | ErrorKind::PermissionDenied)
}

fn has_cas_temp(dir: &Path) -> bool {
    fs::read_dir(dir).expect("read dir").any(|e| {
        e.expect("entry")
            .file_name()
            .to_string_lossy()
            .starts_with(".cas-")
    })
}

fn wait_for(path: &Path, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if path.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {what} at {}", path.display());
}

/// Env-var names the re-executed child helper processes read.
const ENV_ROLE: &str = "GODOT_MCP_TEST_CHILD_ROLE";
const ENV_ROOT: &str = "GODOT_MCP_TEST_CHILD_ROOT";
const ENV_STATE: &str = "GODOT_MCP_TEST_CHILD_STATE";
const ENV_READY: &str = "GODOT_MCP_TEST_CHILD_READY";
const ENV_STOP: &str = "GODOT_MCP_TEST_CHILD_STOP";

/// Spawn this same test binary as a *real separate OS process* running one of
/// the `#[ignore]`d helper cases below. This is genuine cross-process
/// contention: the child has its own address space, its own `LeaseManager` and
/// its own `flock` owner, so no result can be explained by in-process
/// bookkeeping.
fn spawn_helper(role: &str, fx: &Fixture, ready: &Path, stop: &Path, case: &str) -> Child {
    let exe = std::env::current_exe().expect("test binary path");
    Command::new(exe)
        .args([
            "--exact",
            case,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(ENV_ROLE, role)
        .env(ENV_ROOT, &fx.root)
        .env(ENV_STATE, &fx.state)
        .env(ENV_READY, ready)
        .env(ENV_STOP, stop)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn helper child process")
}

// ===========================================================================
// Descriptor-owned happy case
// ===========================================================================

#[test]
fn same_object_alias_converges_to_one_lease() {
    let fx = fixture();

    // A real subdirectory used to build purely lexical aliases: `<root>/sub/..`
    // and `<root>/./sub/../.` name the same OS object as `<root>` with no
    // symlink component anywhere on either path.
    fs::create_dir(fx.root.join("sub")).expect("mkdir sub");
    let alias: PathBuf = fx.root.join("sub").join("..");
    let alias2: PathBuf = fx.root.join(".").join("sub").join("..").join(".");

    let manager = LeaseManager::new(&fx.state);

    // First acquisition through the canonical spelling.
    let lease_a = manager
        .acquire(&fx.root)
        .expect("first acquisition succeeds");

    // Further acquisitions through differently spelled paths to the *same*
    // opened root/project objects must converge onto the one existing lease
    // rather than conflicting — this is the single allowed re-entry.
    let lease_b = manager
        .acquire(&alias)
        .expect("lexical alias converges rather than conflicting");
    let lease_c = manager
        .acquire(&alias2)
        .expect("second lexical alias also converges");
    // Upward discovery from inside the project reaches the same root object.
    let lease_d = manager
        .acquire(&fx.root.join("sub"))
        .expect("upward discovery from a subdirectory converges");

    assert_eq!(
        lease_a.lease_key(),
        lease_b.lease_key(),
        "aliases of the same object must produce one lease key"
    );
    assert_eq!(lease_a.lease_key(), lease_c.lease_key());
    assert_eq!(lease_a.lease_key(), lease_d.lease_key());
    assert!(
        lease_a.same_lease(&lease_b)
            && lease_a.same_lease(&lease_c)
            && lease_a.same_lease(&lease_d),
        "every alias must converge onto the identical in-process lease"
    );
    assert_eq!(
        manager.held_lease_count(),
        1,
        "only one advisory lock/lease exists for the converged aliases"
    );
    // Exactly one advisory lock file exists for the converged aliases.
    assert_eq!(
        fs::read_dir(&fx.state)
            .expect("read state dir")
            .filter(|e| {
                e.as_ref()
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .starts_with("lease-")
            })
            .count(),
        1,
        "converged aliases must share one advisory lock file"
    );

    // Descriptor-relative in-root effects through the *alias* handle: create,
    // read back, then a compare-and-swap overwrite against the correct
    // preimage.
    let sr = lease_b.safe_root();

    sr.cas_write(Path::new("save.tres"), None, b"v1")
        .expect("create via descriptor-relative CAS");
    assert_eq!(
        sr.read(Path::new("save.tres")).expect("read back"),
        b"v1",
        "descriptor-relative read returns the freshly written bytes"
    );
    sr.cas_write(Path::new("save.tres"), Some(b"v1"), b"v2")
        .expect("CAS overwrite with the correct preimage");
    assert_eq!(
        sr.read(Path::new("save.tres")).expect("read back v2"),
        b"v2"
    );
    // The bytes really landed inside the real root, proving the alias resolved
    // to the same object.
    assert_eq!(
        fs::read(fx.root.join("save.tres")).expect("real file present"),
        b"v2"
    );

    // A nested in-root path works through the same held root descriptor.
    fs::create_dir(fx.root.join("scenes")).expect("mkdir scenes");
    sr.cas_write(Path::new("scenes/main.tscn"), None, b"[gd_scene]\n")
        .expect("nested create");
    assert_eq!(
        sr.read(Path::new("scenes/main.tscn")).expect("nested read"),
        b"[gd_scene]\n"
    );

    // No staging temp file survives a successful publish, in either directory.
    assert!(
        !has_cas_temp(&fx.root),
        "no CAS staging temp file may survive a successful publish"
    );
    assert!(
        !has_cas_temp(&fx.root.join("scenes")),
        "no CAS staging temp file may survive a successful nested publish"
    );
}

#[test]
fn same_manager_concurrent_same_key_acquisitions_converge() {
    let fx = fixture();
    let manager = Arc::new(LeaseManager::new(&fx.state));
    let start = Arc::new(Barrier::new(9));
    let mut workers = Vec::new();

    for _ in 0..8 {
        let manager = Arc::clone(&manager);
        let start = Arc::clone(&start);
        let root = fx.root.clone();
        workers.push(std::thread::spawn(move || {
            start.wait();
            manager
                .acquire(&root)
                .expect("same-manager contenders must converge, not self-conflict")
        }));
    }
    start.wait();

    let leases: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().expect("acquisition worker"))
        .collect();
    assert!(
        leases
            .iter()
            .skip(1)
            .all(|lease| leases[0].same_lease(lease)),
        "all same-key acquisitions must return the identical in-process lease"
    );
    assert_eq!(manager.held_lease_count(), 1);
    assert_eq!(
        fs::read_dir(&fx.state)
            .expect("read state dir")
            .filter(|entry| {
                entry
                    .as_ref()
                    .expect("state entry")
                    .file_name()
                    .to_string_lossy()
                    .starts_with("lease-")
            })
            .count(),
        1,
        "same-manager convergence must create exactly one lock file"
    );
}

#[test]
fn symlink_component_cancelled_by_parent_is_rejected_before_state_effects() {
    let fx = fixture();
    let parent = fx.root.parent().expect("fixture parent");
    let detour = parent.join("detour");
    fs::create_dir(&detour).expect("mkdir detour");
    let elided_link = parent.join("elided-link");
    symlink(&detour, &elided_link).expect("create symlink cancelled by parent traversal");

    // The kernel resolves `elided-link/..` back to `parent`, so a lexical
    // collapse would erase the fact that a symlink component was encountered
    // before reaching the real project. Security traversal must inspect the
    // symlink first and reject before creating the state directory or lock.
    let candidate = elided_link.join("..").join("game");
    let manager = LeaseManager::new(&fx.state);
    let error = manager
        .acquire(&candidate)
        .expect_err("a symlink remains forbidden even when a later `..` cancels its name");
    assert!(matches!(error, LeaseError::SymlinkComponent { .. }));
    assert_eq!(error.error_kind(), ErrorKind::PermissionDenied);
    assert!(!fx.state.exists(), "rejection must precede state effects");
    assert_eq!(manager.held_lease_count(), 0);

    // A genuine lexical alias remains valid: the component cancelled by `..`
    // is a real directory that was opened with nofollow semantics.
    fs::create_dir(fx.root.join("sub")).expect("mkdir real subdirectory");
    manager
        .acquire(&fx.root.join("sub").join(".."))
        .expect("real-directory sub/.. alias must continue to converge");
}

#[test]
fn project_marker_must_be_a_regular_file_before_state_effects() {
    let fx = fixture();
    fs::remove_file(fx.root.join("project.godot")).expect("remove marker file");
    fs::create_dir(fx.root.join("project.godot")).expect("replace marker with directory");

    let manager = LeaseManager::new(&fx.state);
    manager
        .acquire(&fx.root)
        .expect_err("a directory named project.godot is not a valid marker file");
    assert!(
        !fx.state.exists(),
        "marker type validation must happen before state or lock creation"
    );
    assert_eq!(manager.held_lease_count(), 0);
}

// ===========================================================================
// Descriptor-owned failure case
// ===========================================================================

#[test]
fn second_owner_symlink_and_retarget_reject() {
    // --- (1) a second OS *process* gets a conflict --------------------------
    {
        let fx = fixture();
        let parent = fx.root.parent().expect("fixture parent").to_path_buf();
        let ready = parent.join("child-holding.ready");
        let stop = parent.join("child.stop");

        // The child process acquires and *holds* the lease.
        let child = spawn_helper("hold", &fx, &ready, &stop, "child_helper_holds_lease");
        wait_for(&ready, "the child process to hold the lease");
        assert_eq!(
            fs::read(fx.root.join("child-owned.tres")).expect("child wrote in-root"),
            b"held",
            "the child process really owns and writes through its lease"
        );

        // This process is now genuinely a second owner: a different PID, a
        // different `flock` owner, an empty in-process registry.
        let ours = LeaseManager::new(&fx.state);
        let err = ours
            .acquire(&fx.root)
            .expect_err("a second OS process must not acquire the held project");
        assert!(
            matches!(err, LeaseError::Conflict { .. }),
            "second process => conflict, got {err:?}"
        );
        assert_eq!(err.error_kind(), ErrorKind::Conflict);

        // An *alias* spelling from the second process is still a conflict, not a
        // convergence: convergence is only ever within one owning process.
        fs::create_dir(fx.root.join("sub")).expect("mkdir sub");
        let err_alias = ours
            .acquire(&fx.root.join("sub").join(".."))
            .expect_err("an alias from a second process is still a conflict");
        assert!(matches!(err_alias, LeaseError::Conflict { .. }));
        assert_eq!(
            ours.held_lease_count(),
            0,
            "a conflicted acquisition must register no lease"
        );

        fs::write(&stop, b"stop").expect("signal child to exit");
        let out = child.wait_with_output().expect("child exits");
        assert!(
            out.status.success(),
            "child helper failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        // Once the child exits its advisory lock is released and we can acquire.
        ours.acquire(&fx.root)
            .expect("the lease is acquirable after the holder exits");
    }

    // --- (2) symlink component resolving to the SAME object still rejected --
    {
        let fx = fixture();
        let parent = fx.root.parent().expect("fixture parent").to_path_buf();
        let manager = LeaseManager::new(&fx.state);
        // First, legitimately hold the real object.
        let _held = manager.acquire(&fx.root).expect("hold real root");

        // `linkroot` is a symlink resolving to the very same object already
        // leased. It must still be rejected before lease acquisition — an alias
        // through a symlink component never converges.
        let linkroot = parent.join("linkroot");
        symlink(&fx.root, &linkroot).expect("create symlink alias");
        let err = manager
            .acquire(&linkroot)
            .expect_err("symlink component must be rejected even for the same object");
        assert!(
            is_conflict_or_denied(err.error_kind()),
            "symlink component => conflict/permission_denied, got {:?}",
            err.error_kind()
        );
        assert!(matches!(err, LeaseError::SymlinkComponent { .. }));

        // A symlinked intermediate *ancestor* is equally rejected.
        let linkdir = parent.join("linkdir");
        symlink(&parent, &linkdir).expect("symlink ancestor");
        let err_mid = manager
            .acquire(&linkdir.join("game"))
            .expect_err("a symlinked ancestor component must be rejected");
        assert!(matches!(err_mid, LeaseError::SymlinkComponent { .. }));
    }

    // --- (3) symlinked `project.godot` rejected (fresh project) ------------
    {
        let fx = fixture();
        // Replace the regular project file with a symlink to an out-of-root
        // file of the same name.
        let outside = fx
            .root
            .parent()
            .expect("fixture parent")
            .join("project.godot");
        fs::write(&outside, b"[application]\n").expect("outside project file");
        fs::remove_file(fx.root.join("project.godot")).expect("rm real project file");
        symlink(&outside, fx.root.join("project.godot")).expect("symlink project.godot");

        let manager = LeaseManager::new(&fx.state);
        let err = manager
            .acquire(&fx.root)
            .expect_err("symlinked project.godot must be rejected");
        assert!(matches!(err, LeaseError::SymlinkComponent { .. }));
        assert!(is_conflict_or_denied(err.error_kind()));
        assert!(
            !fx.state.exists(),
            "a pre-lock rejection must not create the lock state directory"
        );
    }

    // --- (4) root replacement/retarget detected before any effect ----------
    {
        let fx = fixture();
        let parent = fx.root.parent().expect("fixture parent").to_path_buf();
        let manager = LeaseManager::new(&fx.state);
        let lease = manager.acquire(&fx.root).expect("acquire before retarget");
        let sr = lease.safe_root();

        // Atomically swap the display path to name a *different* directory
        // object (with its own project.godot) — the classic retarget attack.
        let evil = parent.join("evil");
        fs::create_dir(&evil).expect("mkdir evil");
        fs::write(evil.join("project.godot"), b"[application]\n").expect("evil project");
        let old = parent.join("game-old");
        fs::rename(&fx.root, &old).expect("move real root aside");
        fs::rename(&evil, &fx.root).expect("retarget display path to evil object");

        // A pre-effect identity re-check must reject: the display path no longer
        // names the held root object.
        let err = sr
            .cas_write(Path::new("pwned.tres"), None, b"x")
            .expect_err("retargeted root must be rejected before the write");
        assert!(matches!(err, SafeRootError::IdentityChanged));
        assert!(is_conflict_or_denied(err.error_kind()));
        // Reads are guarded identically.
        let err_read = sr
            .read(Path::new("project.godot"))
            .expect_err("a read after a retarget must be rejected too");
        assert!(matches!(err_read, SafeRootError::IdentityChanged));
        // No effect happened inside the swapped-in object.
        assert!(
            !fx.root.join("pwned.tres").exists(),
            "no file may be written after a retarget is detected"
        );
        assert!(
            !has_cas_temp(&fx.root),
            "not even a staging temp file may appear in the swapped-in object"
        );
    }

    // --- (4b) `project.godot` replacement after acquisition detected --------
    {
        let fx = fixture();
        let manager = LeaseManager::new(&fx.state);
        let lease = manager
            .acquire(&fx.root)
            .expect("acquire before project-file swap");
        let sr = lease.safe_root();

        // The root object is untouched; only the marker file is replaced by a
        // *different* object with the same name (unlink + create ⇒ new inode).
        fs::remove_file(fx.root.join("project.godot")).expect("rm marker");
        fs::write(fx.root.join("project.godot"), b"[application]\n; swapped\n")
            .expect("write replacement marker");

        let err = sr
            .cas_write(Path::new("after-swap.tres"), None, b"x")
            .expect_err("a replaced project.godot must be rejected before the write");
        assert!(matches!(err, SafeRootError::IdentityChanged));
        assert!(is_conflict_or_denied(err.error_kind()));
        assert!(
            !fx.root.join("after-swap.tres").exists(),
            "no write may land after a project-file replacement"
        );
        assert!(!has_cas_temp(&fx.root));
    }

    // --- (5) outside path rejected -----------------------------------------
    {
        let fx = fixture();
        let parent = fx.root.parent().expect("fixture parent").to_path_buf();
        let manager = LeaseManager::new(&fx.state);
        let lease = manager.acquire(&fx.root).expect("acquire for outside test");
        let sr = lease.safe_root();

        let err = sr
            .read(Path::new("../escape.txt"))
            .expect_err("traversal outside the root must be rejected");
        assert!(matches!(
            err,
            SafeRootError::OutsideRoot { .. } | SafeRootError::Traversal { .. }
        ));
        assert!(is_conflict_or_denied(err.error_kind()));

        // A traversal that would still land inside the root is rejected too: the
        // API is lexically strict, never "resolve then check".
        fs::create_dir(fx.root.join("sub")).expect("mkdir sub");
        let err_inner = sr
            .read(Path::new("sub/../project.godot"))
            .expect_err("any `..` component is rejected");
        assert!(matches!(err_inner, SafeRootError::Traversal { .. }));

        let err_abs = sr
            .read(Path::new("/etc/hostname"))
            .expect_err("absolute path must be rejected");
        assert!(matches!(err_abs, SafeRootError::OutsideRoot { .. }));

        // A symlink inside the root that points outside must not be followed.
        let secret = parent.join("secret.txt");
        fs::write(&secret, b"top secret").expect("write outside secret");
        symlink(&secret, fx.root.join("leak.txt")).expect("symlink leak.txt inside root");
        let err_link = sr
            .read(Path::new("leak.txt"))
            .expect_err("in-root symlink escaping the root must be rejected");
        assert!(matches!(
            err_link,
            SafeRootError::Symlink { .. } | SafeRootError::OutsideRoot { .. }
        ));
        assert!(is_conflict_or_denied(err_link.error_kind()));

        // Writing *through* an in-root symlink is rejected too, and the
        // out-of-root target is left untouched.
        let err_write = sr
            .cas_write(Path::new("leak.txt"), Some(b"top secret"), b"tampered")
            .expect_err("CAS through an in-root symlink must be rejected");
        assert!(matches!(err_write, SafeRootError::Symlink { .. }));
        assert_eq!(
            fs::read(&secret).expect("outside file still readable"),
            b"top secret",
            "an out-of-root target must never be modified"
        );
        // A create-CAS naming an in-root symlink is rejected as well.
        let err_create_link = sr
            .cas_write(Path::new("leak.txt"), None, b"tampered")
            .expect_err("create-CAS onto an in-root symlink must be rejected");
        assert!(matches!(
            err_create_link,
            SafeRootError::Symlink { .. } | SafeRootError::StalePreimage { .. }
        ));
        assert_eq!(fs::read(&secret).expect("still readable"), b"top secret");

        // A symlinked *directory* component inside the root is rejected too.
        let outdir = parent.join("outdir");
        fs::create_dir(&outdir).expect("mkdir outdir");
        fs::write(outdir.join("f.txt"), b"outside").expect("write outside file");
        symlink(&outdir, fx.root.join("linkdir")).expect("symlink dir inside root");
        let err_dir = sr
            .read(Path::new("linkdir/f.txt"))
            .expect_err("a symlinked directory component must be rejected");
        assert!(matches!(err_dir, SafeRootError::Symlink { .. }));
    }

    // --- (6) stale preimage rejected, no write ------------------------------
    {
        let fx = fixture();
        let manager = LeaseManager::new(&fx.state);
        let lease = manager.acquire(&fx.root).expect("acquire for CAS test");
        let sr = lease.safe_root();

        sr.cas_write(Path::new("data.tres"), None, b"real")
            .expect("seed data");
        let err = sr
            .cas_write(Path::new("data.tres"), Some(b"WRONG-PREIMAGE"), b"tampered")
            .expect_err("CAS against a stale preimage must be rejected");
        assert!(matches!(err, SafeRootError::StalePreimage { .. }));
        assert!(is_conflict_or_denied(err.error_kind()));
        assert_eq!(
            sr.read(Path::new("data.tres")).expect("still original"),
            b"real",
            "a rejected CAS must not mutate the file"
        );

        // A create-CAS against an existing target is a stale preimage and must
        // leave both the target and the directory clean.
        let err_create = sr
            .cas_write(Path::new("data.tres"), None, b"clobber")
            .expect_err("create-CAS over an existing file must be rejected");
        assert!(matches!(err_create, SafeRootError::StalePreimage { .. }));
        assert_eq!(
            sr.read(Path::new("data.tres")).expect("unchanged"),
            b"real",
            "a rejected create must not clobber the existing file"
        );
        assert!(
            !has_cas_temp(&fx.root),
            "a failed create must not leave a staging temp file behind"
        );

        // A replace-CAS against an absent target is a stale preimage too.
        let err_missing = sr
            .cas_write(Path::new("nope.tres"), Some(b"whatever"), b"x")
            .expect_err("replace-CAS of an absent file must be rejected");
        assert!(matches!(err_missing, SafeRootError::StalePreimage { .. }));
        assert!(!fx.root.join("nope.tres").exists());
        assert!(!has_cas_temp(&fx.root));
    }

    // --- (7) direct non-loopback configuration rejected before listeners ---
    {
        let toml = r#"
transport = "http"

[http]
bind = "0.0.0.0:8080"
mode = "trusted_local"
host = "127.0.0.1:8080"
origin = "http://127.0.0.1:8080"

[http.local_secret.env]
var = "GODOT_MCP_LAUNCH_SECRET"
"#;
        let err = load_config(Some(toml), &BTreeMap::new(), &CliOverrides::default())
            .expect_err("a public (non-loopback) bind must be rejected");
        assert!(matches!(err, ConfigError::NonLoopbackBind { .. }));
        assert!(
            is_conflict_or_denied(err.error_kind()),
            "non-loopback bind => permission_denied, got {:?}",
            err.error_kind()
        );

        // A non-loopback editor/LSP/DAP bridge endpoint is equally rejected.
        let bridge = "[bridge]\nendpoint = \"0.0.0.0:6009\"\n";
        let err_bridge = load_config(Some(bridge), &BTreeMap::new(), &CliOverrides::default())
            .expect_err("a public bridge endpoint must be rejected");
        assert!(matches!(err_bridge, ConfigError::NonLoopbackBridge { .. }));
        assert!(is_conflict_or_denied(err_bridge.error_kind()));
    }
}

// ===========================================================================
// Supporting ownership properties
// ===========================================================================

#[test]
fn state_directory_and_lock_file_are_user_private() {
    let fx = fixture();
    assert!(!fx.state.exists(), "the fixture starts without a state dir");
    let manager = LeaseManager::new(&fx.state);
    let _lease = manager
        .acquire(&fx.root)
        .expect("acquire creates state dir");

    let mode = fs::metadata(&fx.state)
        .expect("state dir exists")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        mode, 0o700,
        "the advisory-lock state directory must be user-private, got {mode:o}"
    );

    let lock = fs::read_dir(&fx.state)
        .expect("read state dir")
        .map(|e| e.expect("entry").path())
        .find(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("lease-"))
        })
        .expect("a lease lock file exists");
    let lock_mode = fs::metadata(&lock)
        .expect("lock metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        lock_mode, 0o600,
        "the advisory lock file must be user-private, got {lock_mode:o}"
    );
}

#[test]
fn safe_root_listing_is_bounded_and_never_follows_symlinks() {
    let fx = fixture();
    fs::create_dir_all(fx.root.join("scripts/deep")).expect("create source directory");
    fs::write(fx.root.join("scripts/player.gd"), b"extends Node\n").expect("write source");
    fs::write(
        fx.root.join("scripts/deep/helper.gd"),
        b"class_name Helper\n",
    )
    .expect("write nested source");
    fs::create_dir(fx.root.join(".godot")).expect("create engine cache");
    fs::write(fx.root.join(".godot/cache.bin"), b"cache").expect("write cache");
    let outside = fx.root.parent().expect("fixture parent").join("outside");
    fs::create_dir(&outside).expect("create outside directory");
    fs::write(outside.join("secret.gd"), b"secret").expect("write outside file");
    symlink(&outside, fx.root.join("linked")).expect("create in-root symlink");

    let manager = LeaseManager::new(&fx.state);
    let lease = manager.acquire(&fx.root).expect("acquire listing lease");
    let listing = lease
        .safe_root()
        .list_regular_files(32, 8, &[OsStr::new(".godot")])
        .expect("enumerate through held handles");
    assert!(listing.is_complete());
    assert_eq!(
        listing.files(),
        [
            PathBuf::from("project.godot"),
            PathBuf::from("scripts/deep/helper.gd"),
            PathBuf::from("scripts/player.gd"),
        ]
    );

    let bounded = lease
        .safe_root()
        .list_regular_files(1, 8, &[OsStr::new(".godot")])
        .expect("bounded enumeration still succeeds safely");
    assert!(!bounded.is_complete());
}

#[test]
fn group_or_world_accessible_state_directory_is_rejected() {
    for mode in [0o755u32, 0o770, 0o707, 0o777, 0o750] {
        let fx = fixture();
        fs::create_dir(&fx.state).expect("pre-create state dir");
        fs::set_permissions(&fx.state, fs::Permissions::from_mode(mode))
            .expect("loosen state dir permissions");
        let manager = LeaseManager::new(&fx.state);
        let err = match manager.acquire(&fx.root) {
            Ok(_) => panic!("mode {mode:o} state directory must be rejected"),
            Err(e) => e,
        };
        assert!(
            matches!(err, LeaseError::InsecureStateDir { .. }),
            "mode {mode:o} => {err:?}"
        );
        assert_eq!(err.error_kind(), ErrorKind::PermissionDenied);
        assert_eq!(
            fs::read_dir(&fx.state).expect("read state dir").count(),
            0,
            "no lock file may be created in an insecure state directory"
        );
    }
}

#[test]
fn symlinked_state_directory_is_rejected() {
    let fx = fixture();
    let real = fx
        .root
        .parent()
        .expect("fixture parent")
        .join("real-state-dir");
    fs::create_dir(&real).expect("mkdir real state dir");
    fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).expect("chmod");
    symlink(&real, &fx.state).expect("symlink the state dir path");

    let manager = LeaseManager::new(&fx.state);
    let err = manager
        .acquire(&fx.root)
        .expect_err("a symlinked state directory must be rejected");
    assert!(
        matches!(
            err,
            LeaseError::SymlinkComponent { .. } | LeaseError::InsecureStateDir { .. }
        ),
        "got {err:?}"
    );
    assert_eq!(
        fs::read_dir(&real).expect("read real dir").count(),
        0,
        "no lock file may be created through a symlinked state directory"
    );
}

#[test]
fn lock_file_name_binds_only_stable_identity_and_excludes_alias_witness() {
    let base = LeaseKey {
        root: StableObjectId::Unix { dev: 1, ino: 2 },
        project_file: StableObjectId::Unix { dev: 1, ino: 3 },
    };
    let name = lock_file_name(&base);
    assert!(
        name.starts_with("lease-") && name.ends_with(".lock"),
        "unexpected lock file name {name}"
    );
    assert_eq!(
        lock_file_name(&base),
        name,
        "naming must be deterministic for an identical key"
    );

    // Mount ids belong to the acquisition witness, not the ownership key. Two
    // supported aliases may therefore carry different witnesses while hashing
    // to the same deterministic lock name.
    let mut acquisition_witness = AliasWitness::new();
    acquisition_witness.push(AliasEntry {
        mount_id: Some(10),
        followed_reparse_tag: None,
        followed_reparse_object: None,
        followed_reparse_target: None,
    });
    let mut alias_witness = AliasWitness::new();
    alias_witness.push(AliasEntry {
        mount_id: Some(99),
        followed_reparse_tag: None,
        followed_reparse_object: None,
        followed_reparse_target: None,
    });
    assert_ne!(acquisition_witness, alias_witness);
    assert_eq!(lock_file_name(&base), name);

    // Every stable field of the composite identity changes the derived name.
    let variants = [
        (
            "root.dev",
            LeaseKey {
                root: StableObjectId::Unix { dev: 9, ino: 2 },
                ..base
            },
        ),
        (
            "root.ino",
            LeaseKey {
                root: StableObjectId::Unix { dev: 1, ino: 9 },
                ..base
            },
        ),
        (
            "project.dev",
            LeaseKey {
                project_file: StableObjectId::Unix { dev: 9, ino: 3 },
                ..base
            },
        ),
        (
            "project.ino",
            LeaseKey {
                project_file: StableObjectId::Unix { dev: 1, ino: 9 },
                ..base
            },
        ),
        (
            "platform tag",
            LeaseKey {
                root: StableObjectId::Windows {
                    volume_serial: 1,
                    file_id: 2_u128.to_le_bytes(),
                },
                ..base
            },
        ),
    ];

    for (field, variant) in variants {
        assert_ne!(
            lock_file_name(&variant),
            name,
            "changing {field} must change the lock file name"
        );
    }
}

#[cfg(feature = "test-hooks")]
#[test]
fn create_publish_race_preserves_external_state_and_cleans_staging() {
    let fx = fixture();
    let manager = LeaseManager::new(&fx.state);
    let lease = manager.acquire(&fx.root).expect("acquire for create race");
    let rel = PathBuf::from("create-race.tres");
    let reached = Arc::new(Barrier::new(2));
    let resume = Arc::new(Barrier::new(2));
    let _hook = install_cas_publish_hook(
        CasPublishKind::Create,
        rel.clone(),
        Arc::clone(&reached),
        Arc::clone(&resume),
    );

    let safe_root = lease.safe_root();
    let worker_rel = rel.clone();
    let worker = std::thread::spawn(move || {
        safe_root.cas_write(&worker_rel, None, b"ours-must-not-publish")
    });
    reached.wait();

    // The staging descriptor is live while the worker is paused. Re-exec this
    // test binary and mechanically observe that neither it nor any held lease
    // descriptor crossed exec.
    let ready = fx.root.parent().expect("fixture parent").join("race.ready");
    let stop = fx.root.parent().expect("fixture parent").join("race.stop");
    let child = spawn_helper(
        "probe-fds",
        &fx,
        &ready,
        &stop,
        "child_helper_reports_inherited_fds",
    );
    let out = child.wait_with_output().expect("probe child exits");
    assert!(out.status.success(), "probe child failed");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains(&fx.root.display().to_string()),
        "a held root/project/staging descriptor crossed exec: {stdout}"
    );
    assert!(
        !stdout.contains(&fx.state.display().to_string()),
        "a held state/lock descriptor crossed exec: {stdout}"
    );

    fs::write(fx.root.join(&rel), b"external-create").expect("external creator wins race");
    resume.wait();
    let err = worker
        .join()
        .expect("CAS worker did not panic")
        .expect_err("raced create must report a stale preimage");
    assert!(matches!(err, SafeRootError::StalePreimage { .. }));
    assert_eq!(
        fs::read(fx.root.join(&rel)).expect("external file survives"),
        b"external-create"
    );
    assert!(!has_cas_temp(&fx.root), "create race leaked a temp file");
}

#[cfg(feature = "test-hooks")]
#[test]
fn replace_publish_race_preserves_external_state_and_cleans_staging() {
    let fx = fixture();
    let manager = LeaseManager::new(&fx.state);
    let lease = manager.acquire(&fx.root).expect("acquire for replace race");
    let safe_root = lease.safe_root();
    let rel = PathBuf::from("replace-race.tres");
    safe_root
        .cas_write(&rel, None, b"observed-preimage")
        .expect("seed replace target");

    let reached = Arc::new(Barrier::new(2));
    let resume = Arc::new(Barrier::new(2));
    let _hook = install_cas_publish_hook(
        CasPublishKind::Replace,
        rel.clone(),
        Arc::clone(&reached),
        Arc::clone(&resume),
    );
    let worker_rel = rel.clone();
    let worker = std::thread::spawn(move || {
        safe_root.cas_write(
            &worker_rel,
            Some(b"observed-preimage"),
            b"ours-must-not-publish",
        )
    });
    reached.wait();

    // Replace the directory entry with a different object, rather than merely
    // changing bytes in place, so the CAS must bind both identity and content.
    let external_tmp = fx.root.join("external-replacement.tmp");
    fs::write(&external_tmp, b"external-replacement").expect("stage external replacement");
    fs::rename(&external_tmp, fx.root.join(&rel)).expect("external replacement wins race");
    resume.wait();

    let err = worker
        .join()
        .expect("CAS worker did not panic")
        .expect_err("raced replacement must report a stale preimage");
    assert!(matches!(err, SafeRootError::StalePreimage { .. }));
    assert_eq!(
        fs::read(fx.root.join(&rel)).expect("external replacement survives"),
        b"external-replacement"
    );
    assert!(!has_cas_temp(&fx.root), "replace race leaked a temp file");
}

#[cfg(target_os = "linux")]
#[test]
fn held_handles_are_not_inherited_by_a_child_process() {
    let fx = fixture();
    let parent = fx.root.parent().expect("fixture parent").to_path_buf();
    let ready = parent.join("probe.ready");
    let stop = parent.join("probe.stop");

    let manager = LeaseManager::new(&fx.state);
    let _lease = manager
        .acquire(&fx.root)
        .expect("hold the lease while probing a child");

    // The child enumerates its *actual* inherited descriptors from
    // /proc/self/fd and prints each target. Nothing here reads our source or
    // trusts a flag: it observes what the kernel handed the new process.
    let child = spawn_helper(
        "probe-fds",
        &fx,
        &ready,
        &stop,
        "child_helper_reports_inherited_fds",
    );
    let out = child.wait_with_output().expect("probe child exits");
    assert!(
        out.status.success(),
        "probe child failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let targets: Vec<&str> = stdout
        .lines()
        .filter_map(|l| l.strip_prefix("INHERITED-FD-TARGET "))
        .collect();
    assert!(
        !targets.is_empty(),
        "the probe child must report at least its own std descriptors: {stdout}"
    );
    let root_str = fx.root.display().to_string();
    let state_str = fx.state.display().to_string();
    for target in targets {
        assert!(
            !target.starts_with(&root_str),
            "the project root/project.godot handle leaked into a child: {target}"
        );
        assert!(
            !target.starts_with(&state_str),
            "the advisory lock handle leaked into a child: {target}"
        );
    }
}

// ===========================================================================
// Child helper cases (never run as part of the ordinary suite)
// ===========================================================================

/// Acquire the lease in this *separate process* and hold it until signalled.
#[test]
#[ignore = "helper case: re-executed as a child process by the cross-process tests"]
fn child_helper_holds_lease() {
    if std::env::var(ENV_ROLE).as_deref() != Ok("hold") {
        return;
    }
    let root = PathBuf::from(std::env::var(ENV_ROOT).expect("child root"));
    let state = PathBuf::from(std::env::var(ENV_STATE).expect("child state"));
    let ready = PathBuf::from(std::env::var(ENV_READY).expect("child ready path"));
    let stop = PathBuf::from(std::env::var(ENV_STOP).expect("child stop path"));

    let manager = LeaseManager::new(&state);
    let lease = manager.acquire(&root).expect("child acquires the lease");
    // Prove the child really owns it by doing a descriptor-relative effect.
    lease
        .safe_root()
        .cas_write(Path::new("child-owned.tres"), None, b"held")
        .expect("child performs an in-root CAS write");
    fs::write(&ready, b"ready").expect("signal readiness");

    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline && !stop.exists() {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        stop.exists(),
        "child helper timed out waiting for the stop signal"
    );
    drop(lease);
}

/// Report every descriptor this *separate process* actually inherited.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "helper case: re-executed as a child process by the inheritance test"]
fn child_helper_reports_inherited_fds() {
    if std::env::var(ENV_ROLE).as_deref() != Ok("probe-fds") {
        return;
    }
    let entries = fs::read_dir("/proc/self/fd").expect("read /proc/self/fd");
    let mut reported = 0usize;
    for entry in entries {
        let entry = entry.expect("fd entry");
        if let Ok(target) = fs::read_link(entry.path()) {
            println!("INHERITED-FD-TARGET {}", target.display());
            reported += 1;
        }
    }
    assert!(reported > 0, "expected at least the std descriptors");
}
