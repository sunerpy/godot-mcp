//! M2 filesystem effects: handle-relative compare-and-swap delete and missing
//! parent-directory creation.
//!
//! A delete removes exactly the object whose bytes were proven, never a
//! replacement that appeared inside the window, and never anything reached
//! through a symlink. Directory creation walks from the held root with symlink
//! components forbidden, so a planted link cannot redirect a later write.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use godot_mcp_domain::lease::LeaseManager;
use godot_mcp_domain::safe_root::SafeRootError;

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
    fs::create_dir(&root).expect("mkdir game");
    fs::write(root.join("project.godot"), b"config_version=5\n").expect("write project.godot");
    let state = base.join("state");
    Fixture {
        _tmp: tmp,
        base,
        root,
        state,
    }
}

fn has_cas_temp(dir: &Path) -> bool {
    fs::read_dir(dir).expect("read dir").any(|entry| {
        entry
            .expect("entry")
            .file_name()
            .to_string_lossy()
            .starts_with(".cas-")
    })
}

#[test]
fn cas_delete_removes_exactly_the_expected_bytes() {
    let fx = fixture();
    let lease = LeaseManager::new(&fx.state)
        .acquire(&fx.root)
        .expect("acquire");
    let sr = lease.safe_root();
    fs::create_dir(fx.root.join("scripts")).expect("scripts dir");
    fs::write(fx.root.join("scripts/doomed.gd"), b"extends Node\n").expect("seed");

    sr.cas_delete(Path::new("scripts/doomed.gd"), b"extends Node\n")
        .expect("a delete with the exact preimage succeeds");

    assert!(!fx.root.join("scripts/doomed.gd").exists());
    assert!(fx.root.join("scripts").is_dir(), "the parent stays");
    assert!(!has_cas_temp(&fx.root.join("scripts")));
    let again = sr
        .cas_delete(Path::new("scripts/doomed.gd"), b"extends Node\n")
        .expect_err("deleting an absent file is a stale preimage");
    assert!(matches!(again, SafeRootError::StalePreimage { .. }));
}

#[test]
fn cas_delete_with_a_stale_preimage_changes_nothing() {
    let fx = fixture();
    let lease = LeaseManager::new(&fx.state)
        .acquire(&fx.root)
        .expect("acquire");
    let sr = lease.safe_root();
    fs::write(fx.root.join("keep.gd"), b"extends Node\n").expect("seed");

    let error = sr
        .cas_delete(Path::new("keep.gd"), b"something else")
        .expect_err("a mismatched preimage refuses the delete");

    assert!(matches!(error, SafeRootError::StalePreimage { .. }));
    assert_eq!(
        fs::read(fx.root.join("keep.gd")).expect("kept"),
        b"extends Node\n"
    );
    assert!(!has_cas_temp(&fx.root));
}

#[test]
fn cas_delete_refuses_a_symlink_and_never_touches_its_target() {
    let fx = fixture();
    let lease = LeaseManager::new(&fx.state)
        .acquire(&fx.root)
        .expect("acquire");
    let sr = lease.safe_root();
    let outside = fx.base.join("outside.gd");
    fs::write(&outside, b"outside").expect("outside file");
    symlink(&outside, fx.root.join("link.gd")).expect("in-root symlink");
    let outside_dir = fx.base.join("outdir");
    fs::create_dir(&outside_dir).expect("outside dir");
    fs::write(outside_dir.join("f.gd"), b"outside").expect("outside dir file");
    symlink(&outside_dir, fx.root.join("linkdir")).expect("in-root dir symlink");

    let error = sr
        .cas_delete(Path::new("link.gd"), b"outside")
        .expect_err("a symlink is never deleted through");
    assert!(matches!(error, SafeRootError::Symlink { .. }));
    let error = sr
        .cas_delete(Path::new("linkdir/f.gd"), b"outside")
        .expect_err("a symlinked directory component is never traversed");
    assert!(matches!(error, SafeRootError::Symlink { .. }));

    assert!(
        fs::symlink_metadata(fx.root.join("link.gd"))
            .expect("link remains")
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read(&outside).expect("target intact"), b"outside");
    assert_eq!(
        fs::read(outside_dir.join("f.gd")).expect("dir target intact"),
        b"outside"
    );
}

#[test]
fn cas_delete_refuses_a_directory() {
    let fx = fixture();
    let lease = LeaseManager::new(&fx.state)
        .acquire(&fx.root)
        .expect("acquire");
    let sr = lease.safe_root();
    fs::create_dir(fx.root.join("assets")).expect("assets dir");

    sr.cas_delete(Path::new("assets"), b"")
        .expect_err("a directory is not a file to delete");

    assert!(fx.root.join("assets").is_dir());
}

#[test]
fn create_parent_dirs_builds_every_missing_directory() {
    let fx = fixture();
    let lease = LeaseManager::new(&fx.state)
        .acquire(&fx.root)
        .expect("acquire");
    let sr = lease.safe_root();

    sr.create_parent_dirs(Path::new("scenes/levels/one/level.tscn"))
        .expect("missing parents are created");
    assert!(fx.root.join("scenes/levels/one").is_dir());
    assert!(
        !fx.root.join("scenes/levels/one/level.tscn").exists(),
        "only directories are created"
    );
    sr.create_parent_dirs(Path::new("scenes/levels/one/level.tscn"))
        .expect("existing parents are reused");
    sr.cas_write(
        Path::new("scenes/levels/one/level.tscn"),
        None,
        b"[gd_scene]\n",
    )
    .expect("the file can then be created");
    sr.create_parent_dirs(Path::new("top.gd"))
        .expect("a top-level path needs no directory");
}

#[test]
fn create_parent_dirs_refuses_a_symlinked_component() {
    let fx = fixture();
    let lease = LeaseManager::new(&fx.state)
        .acquire(&fx.root)
        .expect("acquire");
    let sr = lease.safe_root();
    let outside = fx.base.join("elsewhere");
    fs::create_dir(&outside).expect("outside dir");
    symlink(&outside, fx.root.join("linked")).expect("in-root dir symlink");

    let error = sr
        .create_parent_dirs(Path::new("linked/sub/file.gd"))
        .expect_err("a symlinked component is refused");

    assert!(matches!(error, SafeRootError::Symlink { .. }));
    assert!(
        !outside.join("sub").exists(),
        "nothing is created through the link"
    );
}

#[test]
fn create_parent_dirs_refuses_traversal_and_absolute_paths() {
    let fx = fixture();
    let lease = LeaseManager::new(&fx.state)
        .acquire(&fx.root)
        .expect("acquire");
    let sr = lease.safe_root();

    let traversal = sr
        .create_parent_dirs(Path::new("../escape/file.gd"))
        .expect_err("traversal is refused");
    assert!(matches!(traversal, SafeRootError::Traversal { .. }));
    let absolute = sr
        .create_parent_dirs(&fx.base.join("abs/file.gd"))
        .expect_err("an absolute path is refused");
    assert!(matches!(absolute, SafeRootError::OutsideRoot { .. }));
    assert!(!fx.base.join("escape").exists());
    assert!(!fx.base.join("abs").exists());
}

#[test]
fn the_project_marker_is_rewritten_in_place_and_the_lease_survives() {
    use std::os::unix::fs::MetadataExt;

    let fx = fixture();
    let lease = LeaseManager::new(&fx.state)
        .acquire(&fx.root)
        .expect("acquire");
    let sr = lease.safe_root();
    let marker = fx.root.join("project.godot");
    let before = fs::metadata(&marker).expect("marker").ino();

    let stale = sr
        .cas_overwrite(Path::new("project.godot"), b"not the bytes", b"x")
        .expect_err("a stale preimage refuses the rewrite");
    assert!(matches!(stale, SafeRootError::StalePreimage { .. }));
    assert_eq!(fs::read(&marker).expect("kept"), b"config_version=5\n");

    let longer = b"config_version=5\n\n[application]\n\nconfig/name=\"In place\"\n";
    sr.cas_overwrite(Path::new("project.godot"), b"config_version=5\n", longer)
        .expect("an in-place rewrite with the exact preimage");
    assert_eq!(fs::read(&marker).expect("rewritten"), longer);
    assert_eq!(
        fs::metadata(&marker).expect("marker").ino(),
        before,
        "the marker keeps its object identity"
    );
    sr.cas_overwrite(Path::new("project.godot"), longer, b"config_version=5\n")
        .expect("a shorter rewrite truncates");
    assert_eq!(fs::read(&marker).expect("shorter"), b"config_version=5\n");
    sr.read(Path::new("project.godot"))
        .expect("the lease still proves the held objects afterwards");
    assert!(!has_cas_temp(&fx.root));

    let link_target = fx.base.join("outside.godot");
    fs::write(&link_target, b"outside").expect("outside");
    symlink(&link_target, fx.root.join("linked.cfg")).expect("symlink");
    let linked = sr
        .cas_overwrite(Path::new("linked.cfg"), b"outside", b"tampered")
        .expect_err("a symlink is never written through");
    assert!(matches!(linked, SafeRootError::Symlink { .. }));
    assert_eq!(fs::read(&link_target).expect("outside"), b"outside");
}

#[cfg(feature = "test-hooks")]
#[test]
fn delete_race_preserves_an_external_replacement() {
    use std::sync::{Arc, Barrier};

    use godot_mcp_domain::safe_root::{CasPublishKind, install_cas_publish_hook};

    let fx = fixture();
    let lease = LeaseManager::new(&fx.state)
        .acquire(&fx.root)
        .expect("acquire");
    let safe_root = lease.safe_root();
    let rel = PathBuf::from("delete-race.gd");
    fs::write(fx.root.join(&rel), b"observed-preimage").expect("seed");

    let reached = Arc::new(Barrier::new(2));
    let resume = Arc::new(Barrier::new(2));
    let _hook = install_cas_publish_hook(
        CasPublishKind::Delete,
        rel.clone(),
        Arc::clone(&reached),
        Arc::clone(&resume),
    );
    let worker_rel = rel.clone();
    let worker =
        std::thread::spawn(move || safe_root.cas_delete(&worker_rel, b"observed-preimage"));
    reached.wait();

    // A different object takes the name inside the window.
    let external = fx.root.join("external.tmp");
    fs::write(&external, b"external-replacement").expect("stage replacement");
    fs::rename(&external, fx.root.join(&rel)).expect("external replacement wins");
    resume.wait();

    let error = worker
        .join()
        .expect("delete worker did not panic")
        .expect_err("a raced delete reports a stale preimage");
    assert!(matches!(error, SafeRootError::StalePreimage { .. }));
    assert_eq!(
        fs::read(fx.root.join(&rel)).expect("replacement survives"),
        b"external-replacement"
    );
    assert!(
        !has_cas_temp(&fx.root),
        "the raced delete leaked a temp name"
    );
}
