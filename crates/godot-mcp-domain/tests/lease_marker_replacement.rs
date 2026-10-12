//! The `project.godot` marker under a live Godot editor.
//!
//! The editor saves `project.godot` on startup and on every settings change by
//! writing a new file and renaming it over the old one, so the marker becomes a
//! different filesystem object while the project stays the same. These tests
//! pin the two consequences:
//!
//! * a replaced marker never admits a second owner of the same project root,
//!   whatever the marker policy — ownership is also locked on the root object;
//! * a lease that follows marker replacement keeps writing through an editor
//!   save, keeps its key across restarts, and still refuses a marker that is a
//!   symlink or a second link to another file, and a retargeted root.
//!
//! The strict default, which refuses any marker replacement, is pinned by
//! `lease_ownership.rs::second_owner_symlink_and_retarget_reject`.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use godot_mcp_domain::lease::{LeaseError, LeaseManager, MarkerPolicy};

struct Fixture {
    _tmp: tempfile::TempDir,
    base: PathBuf,
    root: PathBuf,
    state: PathBuf,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().canonicalize().expect("canonicalize temp base");
    let root = base.join("game");
    fs::create_dir_all(&root).expect("create project root");
    fs::write(root.join("project.godot"), b"[application]\n").expect("write project.godot");
    let state = base.join("state");
    Fixture {
        _tmp: tmp,
        base,
        root,
        state,
    }
}

/// Replace `project.godot` the way the Godot editor saves it.
fn save_like_the_editor(root: &Path, contents: &[u8]) {
    let staged = root.join("project.godot.tmp");
    fs::write(&staged, contents).expect("stage the new marker");
    fs::rename(&staged, root.join("project.godot")).expect("rename it over the marker");
}

fn following(manager: LeaseManager) -> LeaseManager {
    manager.with_marker_policy(MarkerPolicy::FollowReplacement)
}

#[test]
fn a_replaced_marker_admits_no_second_owner() {
    for (label, project_local, follow) in [
        ("fixed state, strict", false, false),
        ("fixed state, following", false, true),
        ("project-local, strict", true, false),
        ("project-local, following", true, true),
    ] {
        let fx = fixture();
        let manager = || {
            let manager = if project_local {
                LeaseManager::project_local()
            } else {
                LeaseManager::new(&fx.state)
            };
            if follow { following(manager) } else { manager }
        };
        let first = manager().acquire(&fx.root).expect("the first owner");
        save_like_the_editor(&fx.root, b"[application]\n; saved by the editor\n");

        let second = manager().acquire(&fx.root);
        assert!(
            matches!(second, Err(LeaseError::Conflict { .. })),
            "{label}: a replaced project.godot must not admit a second owner: {second:?}"
        );

        drop(first);
        manager()
            .acquire(&fx.root)
            .unwrap_or_else(|error| panic!("{label}: a later owner is admitted: {error}"));
    }
}

#[test]
fn a_following_lease_writes_through_editor_saves_and_refuses_unsafe_markers() {
    let fx = fixture();
    let lease = following(LeaseManager::new(&fx.state))
        .acquire(&fx.root)
        .expect("acquire");
    let root = lease.safe_root();
    let marker = fx.root.join("project.godot");

    save_like_the_editor(&fx.root, b"[application]\n; saved\n");
    root.cas_write(Path::new("after-save.tres"), None, b"x")
        .expect("an editor save of the marker is followed");
    assert_eq!(lease.marker_replacements(), 1);
    root.cas_write(Path::new("second-write.tres"), None, b"x")
        .expect("the followed marker stays accepted");
    assert_eq!(lease.marker_replacements(), 1, "one save, one replacement");

    // A marker that is a symlink is never followed.
    let outside = fx.base.join("outside.godot");
    fs::write(&outside, b"[application]\n").expect("outside file");
    fs::remove_file(&marker).expect("remove marker");
    symlink(&outside, &marker).expect("symlink marker");
    root.cas_write(Path::new("after-symlink.tres"), None, b"x")
        .expect_err("a symlinked marker is refused");
    assert!(!fx.root.join("after-symlink.tres").exists());

    // Nor is a marker that is a second link to a file elsewhere.
    fs::remove_file(&marker).expect("remove symlink");
    fs::hard_link(&outside, &marker).expect("hard-link marker");
    root.cas_write(Path::new("after-link.tres"), None, b"x")
        .expect_err("a multiply-linked marker is refused");
    assert!(!fx.root.join("after-link.tres").exists());

    // A regular marker again is followed.
    fs::remove_file(&marker).expect("remove link");
    save_like_the_editor(&fx.root, b"[application]\n; saved again\n");
    root.cas_write(Path::new("after-restore.tres"), None, b"x")
        .expect("a regular single-link marker is followed again");

    // Following the marker never follows a retargeted root.
    let moved = fx.base.join("moved-away");
    fs::rename(&fx.root, &moved).expect("move the project away");
    fs::create_dir(&fx.root).expect("a different root at the same path");
    fs::write(fx.root.join("project.godot"), b"[application]\n").expect("other marker");
    root.cas_write(Path::new("after-retarget.tres"), None, b"x")
        .expect_err("a retargeted root is refused");
    assert!(!fx.root.join("after-retarget.tres").exists());
    assert!(!moved.join("after-retarget.tres").exists());
}

#[test]
fn a_following_lease_keeps_its_key_across_restarts() {
    for project_local in [false, true] {
        let fx = fixture();
        let manager = || {
            following(if project_local {
                LeaseManager::project_local()
            } else {
                LeaseManager::new(&fx.state)
            })
        };
        let key = manager().acquire(&fx.root).expect("first run").lease_key();

        save_like_the_editor(&fx.root, b"[application]\n; saved while stopped\n");
        let restarted = manager().acquire(&fx.root).expect("a restart");
        assert_eq!(
            restarted.lease_key(),
            key,
            "a restart after an editor save is the same project (project_local={project_local})"
        );
        restarted
            .safe_root()
            .cas_write(Path::new("after-restart.tres"), None, b"x")
            .expect("writable after the restart");
    }
}

#[test]
fn a_key_recorded_for_another_root_is_not_trusted() {
    let fx = fixture();
    let key = following(LeaseManager::project_local())
        .acquire(&fx.root)
        .expect("original")
        .lease_key();

    // Copy the project together with its private state directory: the copy is
    // a different project object and must not inherit the original's key.
    let copy = fx.base.join("copy");
    copy_tree(&fx.root, &copy);
    let copied = following(LeaseManager::project_local())
        .acquire(&copy)
        .expect("the copy is its own project");
    assert_ne!(copied.lease_key(), key);
    assert_ne!(copied.lease_key().root, key.root);
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir(to).expect("create copy directory");
    let mode = fs::metadata(from)
        .expect("source mode")
        .permissions()
        .mode();
    for entry in fs::read_dir(from).expect("read source") {
        let entry = entry.expect("entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("copy file");
        }
    }
    fs::set_permissions(to, fs::Permissions::from_mode(mode)).expect("copy mode");
}
