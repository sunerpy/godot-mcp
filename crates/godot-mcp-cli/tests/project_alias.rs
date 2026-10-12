//! M0: one project object has one owner, however a second server names it.
//!
//! The first `godot-mcp serve` holds the project at its real path. A second
//! one starts inside a private mount namespace where another directory is a
//! bind mount of the same project, and is pointed at a subdirectory of that
//! alias. It converges on the same project object, is refused as a second
//! owner with the startup exit code, and creates no state of its own.
//!
//! An unprivileged process can make such a namespace only through bubblewrap,
//! so the test needs `/usr/bin/bwrap`. Without it the test says so and checks
//! nothing, unless `QA_REQUIRE_BWRAP` is set, which turns the absence into a
//! failure.
#![cfg(target_os = "linux")]

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::Path;
use std::process::{Command, Stdio};

const BWRAP: &str = "/usr/bin/bwrap";

#[test]
fn a_second_server_reaching_a_held_project_through_a_bind_mount_is_refused() {
    if !Path::new(BWRAP).is_file() {
        assert!(
            std::env::var_os("QA_REQUIRE_BWRAP").is_none(),
            "QA_REQUIRE_BWRAP is set, but {BWRAP} is absent"
        );
        eprintln!("skipped: {BWRAP} is absent, so no unprivileged mount namespace can be made");
        return;
    }
    let temp = tempfile::tempdir().expect("temp dir");
    let base = temp.path().canonicalize().expect("canonical temp dir");
    let game = base.join("game");
    std::fs::create_dir_all(game.join("sub")).expect("project dirs");
    std::fs::write(
        game.join("project.godot"),
        "config_version=5\n\n[application]\n\nconfig/name=\"alias\"\n",
    )
    .expect("manifest");
    let alias = base.join("alias");
    std::fs::create_dir(&alias).expect("the alias mount point");
    let binary = env!("CARGO_BIN_EXE_godot-mcp");

    let mut owner = Command::new(binary)
        .args(["serve", "--stdio", "--project"])
        .arg(&game)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the owner starts");
    // The owner holds the project once it has answered `initialize`.
    let mut stdin = owner.stdin.take().expect("owner stdin");
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2025-06-18","capabilities":{{}},"clientInfo":{{"name":"alias-test","version":"1"}}}}}}"#
    )
    .expect("send initialize");
    let mut answer = String::new();
    BufReader::new(owner.stdout.take().expect("owner stdout"))
        .read_line(&mut answer)
        .expect("the owner answers");
    assert!(
        answer.contains("\"id\":1") && answer.contains("\"result\""),
        "{answer}"
    );

    let second = Command::new(BWRAP)
        .args(["--unshare-user", "--dev-bind", "/", "/", "--bind"])
        .arg(&game)
        .arg(&alias)
        .arg("--")
        .arg(binary)
        .args(["serve", "--stdio", "--project"])
        .arg(alias.join("sub"))
        .stdin(Stdio::null())
        .output()
        .expect("the second server runs");
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert_eq!(second.status.code(), Some(69), "{stderr}");
    assert!(
        stderr.contains("already owned by another process"),
        "{stderr}"
    );

    // Outside the namespace the alias was only ever an empty directory, and
    // the project has exactly one private state root: the owner's.
    assert_eq!(
        std::fs::read_dir(&alias).expect("the alias").count(),
        0,
        "nothing was created at the alias"
    );
    let state_roots: Vec<String> = std::fs::read_dir(&game)
        .expect("the project")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".godot-mcp"))
        .collect();
    assert_eq!(state_roots, [".godot-mcp"]);
    assert!(!game.join("sub/.godot-mcp").exists());

    drop(stdin);
    let status = owner.wait().expect("the owner exits");
    assert_eq!(status.code(), Some(0), "the owner exits cleanly");
}
