//! `tooling.lock.json` population gate for Todo 1.
//!
//! Asserts the committed `tooling.lock.json` records, for the Rust toolchain and
//! for `cargo-deny`/`cargo-cyclonedx`, an exact version, a source URL, a package
//! checksum, an explicit source kind, and the source-kind-appropriate executable
//! identity, all well-formed. A corrupted or unpopulated entry fails this test
//! (fail-closed).
//!
//! The lock pins the `x86_64-unknown-linux-gnu` toolchain and tools the release
//! gates run with, so their live executables exist only on such a host.

#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use godot_mcp_cli::tooling_lock::{SourceKind, ToolingLock, resolve_selected_executable};

const PORTABILITY_CHILD: &str = "GODOT_MCP_TOOLING_PORTABILITY_CHILD";
const MISSING_TOOLS_CHILD: &str = "GODOT_MCP_TOOLING_MISSING_TOOLS_CHILD";

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root above crates/godot-mcp-cli")
        .to_path_buf()
}

#[test]
fn tooling_lock_populated() {
    let path = workspace_root().join("tooling.lock.json");
    let lock = ToolingLock::load(&path).expect("tooling.lock.json must load");
    lock.validate_populated()
        .expect("tooling.lock.json must be fully populated and well-formed");
    lock.validate_live()
        .expect("tooling.lock.json must match the selected live executables");

    // Exact versions and explicit source classifications per the approved decision.
    assert_eq!(lock.tools["rust-toolchain"].version, "1.96.0");
    assert_eq!(lock.tools["cargo-deny"].version, "0.19.9");
    assert_eq!(lock.tools["cargo-cyclonedx"].version, "0.5.7");
    assert_eq!(
        lock.tools["rust-toolchain"].source_kind,
        SourceKind::PrebuiltArtifact
    );
    assert_eq!(
        lock.tools["cargo-deny"].source_kind,
        SourceKind::CargoInstall
    );
    assert_eq!(
        lock.tools["cargo-cyclonedx"].source_kind,
        SourceKind::CargoInstall
    );
    assert!(lock.tools["rust-toolchain"].executable_sha256.is_some());
    assert!(lock.tools["cargo-deny"].executable_sha256.is_none());
    assert!(lock.tools["cargo-cyclonedx"].executable_sha256.is_none());
}

#[test]
fn prebuilt_live_digest_mismatch_is_rejected() {
    // Given a prebuilt artifact whose committed upstream byte anchor is wrong.
    let mut lock = ToolingLock::load(&workspace_root().join("tooling.lock.json")).unwrap();
    lock.tools
        .get_mut("rust-toolchain")
        .unwrap()
        .executable_sha256 = Some("0".repeat(64));

    // When live validation hashes the selected prebuilt executable.
    let error = lock
        .validate_live()
        .expect_err("a lock whose digest disagrees with the live executable must fail");

    // Then the committed executable anchor remains fail-closed.
    assert!(error.contains("live executable SHA-256"), "{error}");
}

#[test]
fn selected_executable_identity_and_source_anchor_fail_closed() {
    // Given the runtime-selected cargo-installed executable.
    let temp = tempfile::tempdir().unwrap();
    let selected = resolve_selected_executable("cargo-deny").unwrap();
    let mut lock = ToolingLock::load(&workspace_root().join("tooling.lock.json")).unwrap();
    let mut selected_paths = BTreeMap::new();
    selected_paths.insert("cargo-deny".to_string(), selected.clone());

    // When the selected path, runtime digest record, version, and package anchor agree.
    lock.validate_selected_executables(&selected_paths, &["cargo-deny"])
        .expect("matching selected executable and live bytes must pass");

    // Then a different path is rejected even when it contains identical bytes.
    let wrong = temp.path().join("wrong-cargo-deny");
    std::fs::copy(&selected, &wrong).unwrap();
    selected_paths.insert("cargo-deny".to_string(), wrong);
    assert!(
        lock.validate_selected_executables(&selected_paths, &["cargo-deny"])
            .is_err(),
        "a different selected executable must fail even when its bytes hash identically"
    );

    // And malformed source-published package metadata makes the same selection fail.
    selected_paths.insert("cargo-deny".to_string(), selected.clone());
    lock.tools.get_mut("cargo-deny").unwrap().package_checksum = "not-a-sha256".to_string();
    assert!(
        lock.validate_selected_executables(&selected_paths, &["cargo-deny"])
            .is_err(),
        "malformed package metadata must toggle the gate from pass to fail"
    );
}

#[test]
fn cargo_install_source_rejects_a_committed_machine_digest() {
    // Given a cargo-installed source entry carrying a machine-local executable digest.
    let json = r#"{
        "schema_version": "2.0.0",
        "tools": {
            "rust-toolchain": {
                "version": "1.96.0",
                "source_kind": "prebuilt-artifact",
                "source_url": "https://static.rust-lang.org/dist/rust-1.96.0-x86_64-unknown-linux-gnu.tar.xz",
                "package_checksum": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "executable_sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            },
            "cargo-deny": {
                "version": "0.19.9",
                "source_kind": "cargo-install",
                "source_url": "https://crates.io/api/v1/crates/cargo-deny/0.19.9/download",
                "package_checksum": "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
                "executable_sha256": "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
            },
            "cargo-cyclonedx": {
                "version": "0.5.7",
                "source_kind": "cargo-install",
                "source_url": "https://crates.io/api/v1/crates/cargo-cyclonedx/0.5.7/download",
                "package_checksum": "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
            }
        }
    }"#;
    let lock: ToolingLock = serde_json::from_str(json).unwrap();

    // When the structural gate classifies each entry.
    let error = lock
        .validate_populated()
        .expect_err("cargo-install entries must not commit machine-local digests");

    // Then the machine-specific value is rejected rather than silently ignored.
    assert!(
        error.contains("must not record executable_sha256"),
        "{error}"
    );
}

#[test]
fn tooling_lock_validates_under_different_home_and_cargo_home() {
    if std::env::var_os(PORTABILITY_CHILD).is_some() {
        let lock = ToolingLock::load(&workspace_root().join("tooling.lock.json")).unwrap();
        lock.validate_live()
            .expect("runtime-resolved executables must validate under the alternate homes");
        return;
    }

    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("alternate-home");
    let cargo_home = temp.path().join("alternate-cargo-home");
    let cargo_bin = cargo_home.join("bin");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cargo_bin).unwrap();
    for tool in ["cargo-deny", "cargo-cyclonedx"] {
        let source = resolve_selected_executable(tool).unwrap();
        std::fs::copy(source, cargo_bin.join(tool)).unwrap();
    }

    let rustup = Command::new("rustup")
        .args(["show", "home"])
        .output()
        .unwrap();
    assert!(rustup.status.success());
    let rustup_home = String::from_utf8(rustup.stdout).unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "tooling_lock_validates_under_different_home_and_cargo_home",
            "--nocapture",
        ])
        .env(PORTABILITY_CHILD, "1")
        .env("HOME", &home)
        .env("CARGO_HOME", &cargo_home)
        .env("RUSTUP_HOME", rustup_home.trim())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "alternate HOME={} CARGO_HOME={} failed\nstdout:\n{}\nstderr:\n{}",
        home.display(),
        cargo_home.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn tooling_lock_rejects_an_empty_cargo_home() {
    if std::env::var_os(MISSING_TOOLS_CHILD).is_some() {
        // Given an empty CARGO_HOME selected by the child process.
        let lock = ToolingLock::load(&workspace_root().join("tooling.lock.json")).unwrap();

        // When live validation resolves the required cargo-installed tools.
        let error = lock
            .validate_live()
            .expect_err("missing cargo-installed tools must fail closed");

        // Then absence is reported as a hard regular-file failure.
        assert!(error.contains("not a regular file"), "{error}");
        return;
    }

    // Given isolated alternate homes with no installed cargo subcommands.
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("empty-home");
    let cargo_home = temp.path().join("empty-cargo-home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cargo_home).unwrap();
    let rustup = Command::new("rustup")
        .args(["show", "home"])
        .output()
        .unwrap();
    assert!(rustup.status.success());
    let rustup_home = String::from_utf8(rustup.stdout).unwrap();

    // When the same test runs under those homes.
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "tooling_lock_rejects_an_empty_cargo_home",
            "--nocapture",
        ])
        .env(MISSING_TOOLS_CHILD, "1")
        .env("HOME", &home)
        .env("CARGO_HOME", &cargo_home)
        .env("RUSTUP_HOME", rustup_home.trim())
        .output()
        .unwrap();

    // Then the child test succeeds only because validation itself failed closed.
    assert!(
        output.status.success(),
        "empty HOME={} CARGO_HOME={} did not fail closed as expected\nstdout:\n{}\nstderr:\n{}",
        home.display(),
        cargo_home.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn corrupted_tooling_lock_entry_fails() {
    // An empty tools map must fail validation (a corrupted entry fails).
    let json = r#"{ "schema_version": "1.0.0", "tools": {} }"#;
    let lock: ToolingLock = serde_json::from_str(json).unwrap();
    assert!(lock.validate_populated().is_err());
}
