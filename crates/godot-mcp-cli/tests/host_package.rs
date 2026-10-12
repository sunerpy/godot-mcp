//! Integration coverage for the test-only Task-34 host-package launcher.
//!
//! The launcher is intentionally Linux-only: Linux can execute the already
//! verified open file through `/proc/self/fd`, closing the verify-to-spawn path
//! replacement race. Other platforms fail closed until an equivalent
//! handle-based launcher is implemented.

#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;

use godot_mcp_cli::host_package::HostPackageManifest;
use serde_json::json;
fn sha256(path: &Path) -> String {
    godot_mcp_cli::tooling_lock::sha256_hex_file(path).expect("read executable for test digest")
}

fn write_manifest(dir: &Path, executable: &Path, digest: &str) -> PathBuf {
    let path = dir.join("manifest.json");
    fs::write(
        &path,
        serde_json::to_vec_pretty(&json!({
            "schema_version": "1.0.0",
            "executable_path": executable,
            "executable_sha256": digest,
        }))
        .unwrap(),
    )
    .unwrap();
    path
}

fn launcher(manifest: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_qa-host-invoke"));
    command.arg("--manifest").arg(manifest).arg("--");
    command
}

fn copy_shell(dir: &Path, name: &str) -> PathBuf {
    let destination = dir.join(name);
    fs::copy("/bin/sh", &destination).unwrap();
    fs::set_permissions(&destination, fs::Permissions::from_mode(0o755)).unwrap();
    destination
}

#[test]
fn missing_manifest_fails_closed() {
    let temp = tempfile::tempdir().unwrap();
    assert_eq!(
        launcher(&temp.path().join("missing.json"))
            .status()
            .unwrap()
            .code(),
        Some(69)
    );
}

#[test]
fn malformed_manifest_fails_closed() {
    let temp = tempfile::tempdir().unwrap();
    let manifest = temp.path().join("manifest.json");
    fs::write(&manifest, b"{not json").unwrap();
    assert_eq!(launcher(&manifest).status().unwrap().code(), Some(69));
}

#[test]
fn empty_schema_version_fails_closed() {
    let temp = tempfile::tempdir().unwrap();
    let executable = copy_shell(temp.path(), "packaged-sh");
    let manifest = write_manifest(temp.path(), &executable, &sha256(&executable));
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    value["schema_version"] = json!("   ");
    fs::write(&manifest, serde_json::to_vec(&value).unwrap()).unwrap();
    assert_eq!(launcher(&manifest).status().unwrap().code(), Some(69));
}

#[test]
fn relative_executable_path_fails_closed() {
    let temp = tempfile::tempdir().unwrap();
    let manifest = write_manifest(temp.path(), Path::new("packaged-sh"), &"0".repeat(64));
    assert_eq!(launcher(&manifest).status().unwrap().code(), Some(69));
}

#[test]
fn directory_executable_path_fails_closed() {
    let temp = tempfile::tempdir().unwrap();
    let manifest = write_manifest(temp.path(), temp.path(), &"0".repeat(64));
    assert_eq!(launcher(&manifest).status().unwrap().code(), Some(69));
}

#[test]
fn malformed_and_uppercase_digests_fail_closed() {
    let temp = tempfile::tempdir().unwrap();
    let executable = copy_shell(temp.path(), "packaged-sh");
    for digest in ["not-a-digest".to_string(), "A".repeat(64)] {
        let manifest = write_manifest(temp.path(), &executable, &digest);
        assert_eq!(launcher(&manifest).status().unwrap().code(), Some(69));
    }
}

#[test]
fn digest_mismatch_fails_closed() {
    let temp = tempfile::tempdir().unwrap();
    let executable = copy_shell(temp.path(), "packaged-sh");
    let manifest = write_manifest(temp.path(), &executable, &"0".repeat(64));
    assert_eq!(launcher(&manifest).status().unwrap().code(), Some(69));
}

#[test]
fn symlink_component_is_rejected_even_when_target_hash_matches() {
    let temp = tempfile::tempdir().unwrap();
    let real = copy_shell(temp.path(), "packaged-sh");
    let alias = temp.path().join("packaged-sh-link");
    symlink(&real, &alias).unwrap();
    let manifest = write_manifest(temp.path(), &alias, &sha256(&real));
    assert_eq!(
        launcher(&manifest)
            .arg("-c")
            .arg("exit 0")
            .status()
            .unwrap()
            .code(),
        Some(69)
    );
}

#[test]
fn valid_executable_receives_arguments_and_is_invoked() {
    let temp = tempfile::tempdir().unwrap();
    let executable = copy_shell(temp.path(), "packaged-sh");
    let marker = temp.path().join("invoked.txt");
    let manifest = write_manifest(temp.path(), &executable, &sha256(&executable));
    let status = launcher(&manifest)
        .arg("-c")
        .arg("printf '%s' \"$1\" > \"$2\"")
        .arg("packaged-sh")
        .arg("forwarded-verbatim")
        .arg(&marker)
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(0));
    assert_eq!(fs::read_to_string(marker).unwrap(), "forwarded-verbatim");
}

#[test]
fn ordinary_child_exit_code_is_propagated() {
    let temp = tempfile::tempdir().unwrap();
    let executable = copy_shell(temp.path(), "packaged-sh");
    let manifest = write_manifest(temp.path(), &executable, &sha256(&executable));
    assert_eq!(
        launcher(&manifest)
            .arg("-c")
            .arg("exit 42")
            .status()
            .unwrap()
            .code(),
        Some(42)
    );
}

#[test]
fn verified_open_file_survives_path_replacement_without_running_replacement() {
    let temp = tempfile::tempdir().unwrap();
    let executable = copy_shell(temp.path(), "packaged-sh");
    let manifest = write_manifest(temp.path(), &executable, &sha256(&executable));
    let verified = HostPackageManifest::load_and_verify(&manifest).unwrap();

    fs::rename(&executable, temp.path().join("verified-original")).unwrap();
    fs::write(&executable, b"#!/bin/sh\nexit 99\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();

    let status = verified.status(["-c", "exit 23"]).unwrap();
    assert_eq!(
        status.code(),
        Some(23),
        "must execute the verified open file, not its path replacement"
    );
}
