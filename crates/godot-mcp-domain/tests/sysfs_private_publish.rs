//! A create-only publish into a private state directory, step by step: the
//! directory made private beside a project, a publish into it, a lock taken in
//! it, then a second publish while that lock is held — the order a project-local
//! lease that follows its marker uses on first acquisition. Each step has its own
//! expectation, so a platform that fails names the step.

use std::ffi::OsStr;
use std::fs;

use godot_mcp_domain::sysfs::{
    CasOutcome, cas_create, open_private_child_state_dir, open_private_lock_file, read_child_file,
    try_lock_exclusive, walk_dir_no_symlinks,
};

#[test]
fn a_create_only_publish_lands_in_a_private_state_directory_with_and_without_a_held_lock() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp
        .path()
        .canonicalize()
        .expect("canonicalize temp base")
        .join("game");
    fs::create_dir_all(&root).expect("create the project root");
    fs::write(root.join("project.godot"), b"[application]\n").expect("write project.godot");

    let walked = walk_dir_no_symlinks(&root).expect("step 1: walk to the project root");
    let state = open_private_child_state_dir(walked.dir(), OsStr::new(".godot-mcp"))
        .expect("step 2: create the private state directory");
    let first = cas_create(&state, OsStr::new("first.json"), b"{}", &|| true)
        .expect("step 3: publish into the fresh private directory");
    assert_eq!(first, CasOutcome::Published);
    assert_eq!(
        read_child_file(&state, OsStr::new("first.json")).expect("step 4: read it back"),
        b"{}"
    );

    let lock = open_private_lock_file(&state, "owner.lock").expect("step 5: create a lock file");
    try_lock_exclusive(&lock).expect("step 6: lock it");
    let second = cas_create(&state, OsStr::new("second.json"), b"[]", &|| true)
        .expect("step 7: publish while the lock is held");
    assert_eq!(second, CasOutcome::Published);

    let reopened = open_private_child_state_dir(walked.dir(), OsStr::new(".godot-mcp"))
        .expect("step 8: reopen the existing private directory");
    let third = cas_create(&reopened, OsStr::new("third.json"), b"0", &|| true)
        .expect("step 9: publish through a second handle onto the directory");
    assert_eq!(third, CasOutcome::Published);
}
