//! M2 bootstrap authority: a held workspace directory under which a server may
//! create exactly new projects and resolve one to bind.
//!
//! Every effect is relative to the held workspace handle. A symlinked
//! component, a traversal, an absolute spelling, a non-empty target, an
//! existing project and a project nested inside another are all refused
//! before a byte is written.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::lease::LeaseManager;
use godot_mcp_domain::workspace_root::{WorkspaceError, WorkspaceRoot};

struct Fixture {
    _tmp: tempfile::TempDir,
    base: PathBuf,
    workspace: PathBuf,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().canonicalize().expect("canonical base");
    let workspace = base.join("workspace");
    fs::create_dir(&workspace).expect("workspace dir");
    Fixture {
        _tmp: tmp,
        base,
        workspace,
    }
}

const MANIFEST: &[u8] = b"config_version=5\n\n[application]\n\nconfig/name=\"Created\"\n";

#[test]
fn a_new_project_is_created_under_the_held_workspace() {
    let fx = fixture();
    let workspace = WorkspaceRoot::open(&fx.workspace).expect("hold the workspace");

    let created = workspace
        .create_project(Path::new("games/first"), MANIFEST)
        .expect("create a project");

    assert_eq!(created.relative(), Path::new("games/first"));
    assert_eq!(
        fs::read(fx.workspace.join("games/first/project.godot")).expect("manifest"),
        MANIFEST
    );
    let entries: Vec<_> = fs::read_dir(fx.workspace.join("games/first"))
        .expect("project dir")
        .map(|entry| entry.expect("entry").file_name())
        .collect();
    assert_eq!(entries, vec![std::ffi::OsString::from("project.godot")]);

    let resolved = workspace
        .resolve_project(Path::new("games/first"))
        .expect("resolve the created project");
    assert_eq!(resolved.display_root(), fx.workspace.join("games/first"));
    let lease = LeaseManager::project_local()
        .acquire(resolved.display_root())
        .expect("the resolved project can be leased");
    assert_eq!(
        lease.lease_key().root,
        resolved.root_identity(),
        "binding must lease exactly the object that was resolved"
    );

    let listed = workspace.list_projects(1_000, 8).expect("list projects");
    assert_eq!(listed.projects(), &[PathBuf::from("games/first")]);
    assert!(listed.is_complete());
}

#[test]
fn an_empty_existing_directory_may_become_a_project() {
    let fx = fixture();
    fs::create_dir(fx.workspace.join("empty")).expect("empty dir");
    let workspace = WorkspaceRoot::open(&fx.workspace).expect("hold");
    workspace
        .create_project(Path::new("empty"), MANIFEST)
        .expect("an empty directory is accepted");
    assert!(fx.workspace.join("empty/project.godot").is_file());
}

#[test]
fn creation_never_replaces_or_nests_a_project() {
    let fx = fixture();
    let workspace = WorkspaceRoot::open(&fx.workspace).expect("hold");
    workspace
        .create_project(Path::new("game"), MANIFEST)
        .expect("first project");

    let again = workspace
        .create_project(Path::new("game"), b"config_version=5\n")
        .expect_err("an existing project is never replaced");
    assert_eq!(again.error_kind(), ErrorKind::Conflict);
    assert_eq!(
        fs::read(fx.workspace.join("game/project.godot")).expect("kept"),
        MANIFEST
    );

    let nested = workspace
        .create_project(Path::new("game/inner"), MANIFEST)
        .expect_err("a project inside another project is refused");
    assert_eq!(nested.error_kind(), ErrorKind::Conflict);
    assert!(!fx.workspace.join("game/inner").exists());

    fs::create_dir(fx.workspace.join("busy")).expect("busy dir");
    fs::write(fx.workspace.join("busy/notes.txt"), b"keep").expect("busy file");
    let busy = workspace
        .create_project(Path::new("busy"), MANIFEST)
        .expect_err("a non-empty directory is refused");
    assert_eq!(busy.error_kind(), ErrorKind::Conflict);
    assert!(!fx.workspace.join("busy/project.godot").exists());
}

#[test]
fn creation_refuses_escapes_and_symlinks() {
    let fx = fixture();
    let outside = fx.base.join("outside");
    fs::create_dir(&outside).expect("outside dir");
    symlink(&outside, fx.workspace.join("link")).expect("in-workspace symlink");
    let workspace = WorkspaceRoot::open(&fx.workspace).expect("hold");

    for (relative, expected) in [
        ("../escape", ErrorKind::PermissionDenied),
        ("link/game", ErrorKind::PermissionDenied),
        ("link", ErrorKind::PermissionDenied),
    ] {
        let error = workspace
            .create_project(Path::new(relative), MANIFEST)
            .expect_err(relative);
        assert_eq!(error.error_kind(), expected, "{relative}: {error}");
    }
    let absolute = workspace
        .create_project(&fx.base.join("abs"), MANIFEST)
        .expect_err("absolute");
    assert_eq!(absolute.error_kind(), ErrorKind::PermissionDenied);
    assert!(fs::read_dir(&outside).expect("outside").next().is_none());
    assert!(!fx.base.join("escape").exists());
    assert!(!fx.base.join("abs").exists());
}

#[test]
fn resolution_requires_a_real_project_directory() {
    let fx = fixture();
    fs::create_dir(fx.workspace.join("plain")).expect("plain dir");
    let real = fx.base.join("real-project");
    fs::create_dir(&real).expect("real project");
    fs::write(real.join("project.godot"), MANIFEST).expect("manifest");
    symlink(&real, fx.workspace.join("aliased")).expect("symlinked project");
    let workspace = WorkspaceRoot::open(&fx.workspace).expect("hold");

    let plain = workspace
        .resolve_project(Path::new("plain"))
        .expect_err("a directory without project.godot");
    assert_eq!(plain.error_kind(), ErrorKind::NotFound);
    let aliased = workspace
        .resolve_project(Path::new("aliased"))
        .expect_err("a symlinked project directory");
    assert_eq!(aliased.error_kind(), ErrorKind::PermissionDenied);
    let missing = workspace
        .resolve_project(Path::new("missing"))
        .expect_err("a missing directory");
    assert_eq!(missing.error_kind(), ErrorKind::NotFound);
}

#[test]
fn the_workspace_itself_must_be_a_real_directory() {
    let fx = fixture();
    let file = fx.base.join("file");
    fs::write(&file, b"x").expect("file");
    assert!(matches!(
        WorkspaceRoot::open(&file),
        Err(WorkspaceError::NotADirectory)
    ));
    assert!(WorkspaceRoot::open(&fx.base.join("missing")).is_err());
    symlink(&fx.workspace, fx.base.join("ws-link")).expect("workspace symlink");
    let linked = WorkspaceRoot::open(&fx.base.join("ws-link")).expect_err("symlinked workspace");
    assert_eq!(linked.error_kind(), ErrorKind::PermissionDenied);
}
