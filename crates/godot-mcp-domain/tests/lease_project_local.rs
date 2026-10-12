//! A project-local lease, taken the way the server takes one: the state
//! directory inside the project root, and the marker followed across editor
//! saves. Every platform must acquire it, prove it again, lock a legacy state
//! directory under the root with it, and refuse a second owner. Each step has
//! its own expectation, so a platform that fails says which step it was.

use std::fs;
use std::path::{Path, PathBuf};

use godot_mcp_domain::lease::{LeaseError, LeaseManager, MarkerPolicy, PROJECT_STATE_DIR_NAME};

fn project() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp
        .path()
        .canonicalize()
        .expect("canonicalize temp base")
        .join("game");
    fs::create_dir_all(&root).expect("create project root");
    fs::write(root.join("project.godot"), b"[application]\n").expect("write project.godot");
    (tmp, root)
}

fn manager(policy: MarkerPolicy) -> LeaseManager {
    LeaseManager::project_local().with_marker_policy(policy)
}

#[test]
fn a_project_local_lease_is_acquired_and_proven_again_under_either_marker_policy() {
    for policy in [MarkerPolicy::Strict, MarkerPolicy::FollowReplacement] {
        let (_tmp, root) = project();
        let lease = manager(policy)
            .acquire(&root)
            .unwrap_or_else(|error| panic!("{policy:?}: the first acquisition: {error:?}"));
        assert!(root.join(PROJECT_STATE_DIR_NAME).is_dir(), "{policy:?}");
        lease
            .revalidate()
            .unwrap_or_else(|error| panic!("{policy:?}: the held lease's re-proof: {error:?}"));
        lease
            .revalidate()
            .unwrap_or_else(|error| panic!("{policy:?}: a second re-proof: {error:?}"));
    }
}

#[test]
fn a_second_owner_of_a_project_local_lease_is_a_conflict_until_the_first_lets_go() {
    let (_tmp, root) = project();
    let first = manager(MarkerPolicy::FollowReplacement)
        .acquire(&root)
        .expect("the first owner is admitted");
    let second = manager(MarkerPolicy::FollowReplacement).acquire(&root);
    assert!(
        matches!(second, Err(LeaseError::Conflict { .. })),
        "a second manager must see the held project as a conflict: {second:?}"
    );
    drop(first);
    let again = manager(MarkerPolicy::FollowReplacement)
        .acquire(&root)
        .expect("the project is admitted again once its owner lets go");
    again
        .revalidate()
        .expect("the re-acquired lease proves itself again");
}

#[test]
fn a_legacy_state_directory_under_the_root_is_locked_with_the_lease() {
    let (_tmp, root) = project();
    let legacy = Path::new("tools").join(PROJECT_STATE_DIR_NAME);
    fs::create_dir_all(root.join(&legacy)).expect("create a legacy state directory");
    let lease = manager(MarkerPolicy::FollowReplacement)
        .acquire(&root)
        .expect("the owner is admitted");
    let guard = lease
        .lock_sibling_state_dir(&legacy)
        .expect("the legacy state directory is locked with the lease's own key");
    lease
        .revalidate()
        .expect("the lease proves itself again while it holds the legacy lock");
    drop(guard);
}
