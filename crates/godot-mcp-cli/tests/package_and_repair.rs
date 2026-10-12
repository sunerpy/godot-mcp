//! Todo 34 acceptance: the packaged CLI plus the local-only operator-repair
//! command.
//!
//! These are the two exact cases `qa/tasks/34.json` names, and their names are
//! part of that descriptor's frozen command surface:
//! `package_and_repair_happy_path` and `package_and_repair_failure_paths`.
//!
//! ## What is proven here, and what is proven elsewhere
//!
//! The end-to-end proof-driven repairs — the seven happy transitions and the six
//! Todo 17 composite refusals — are driven by the descriptor against fixture state
//! that only `qa-fixture-34` may create, because that binary is the sole owner of
//! `tests/fixtures/quarantined` and `tests/fixtures/repair`. These tests therefore
//! deliberately do **not** build journal records: they prove the properties that
//! hold with no fixture at all, and which no fixture could establish:
//!
//! * the exact command interface, including that `--force` is not a recognized
//!   flag and that no other bypass exists;
//! * that `transaction repair` has no networked path — asserted structurally
//!   against the sources and against the live tool/resource registry snapshot for
//!   every capability profile, not by reading a comment;
//! * that every input gate fails closed with a nonzero exit and no partial change;
//!   and
//! * that the committed release graph is free of every forbidden gdext artifact.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use assert_fs::prelude::*;
use godot_mcp_cli::audit::{
    ArchiveUnderAudit, ReleaseAuditInputs, audit_full_release_graph, audit_release_graph,
};
use godot_mcp_cli::corrupt::{self, CorruptFixture};
use godot_mcp_cli::package;
use godot_mcp_domain::CapabilityProfile;
use godot_mcp_server::kernel::ServiceKernel;
use predicates::prelude::*;
use serde_json::{Value, json};

// The Task-34 fixture and its helpers serve the one test that observes live
// process identity, which this build does on Linux and macOS only.
#[cfg(all(feature = "test-hooks", any(target_os = "linux", target_os = "macos")))]
use godot_mcp_cli::fixture34::{SCENARIO, ids, state::FixtureState, tree::FixturePaths};
#[cfg(all(feature = "test-hooks", any(target_os = "linux", target_os = "macos")))]
use godot_mcp_cli::repair::{RepairError, RepairRequest};
#[cfg(all(feature = "test-hooks", any(target_os = "linux", target_os = "macos")))]
use godot_mcp_server::router::RepairIntent;

/// Every capability profile, so a registry assertion cannot silently skip one.
const EVERY_PROFILE: [CapabilityProfile; 4] = [
    CapabilityProfile::Core,
    CapabilityProfile::Editor,
    CapabilityProfile::Runtime,
    CapabilityProfile::Full,
];

/// The four proof-driven operations, and the complete set of them.
const EVERY_OPERATION: [&str; 4] = [
    "reconcile",
    "compensate",
    "terminate",
    "acknowledge-safe-state",
];

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root above crates/godot-mcp-cli")
        .to_path_buf()
}

fn cli() -> Command {
    let mut command = Command::cargo_bin("godot-mcp").expect("godot-mcp binary");
    for (name, _) in std::env::vars().filter(|(name, _)| name.starts_with("GODOT_MCP_")) {
        command.env_remove(name);
    }
    command
}

/// A minimal project object the operator lane can take a lease on. Its journal is
/// created empty, so every transaction lookup is a `not found` refusal.
fn empty_project() -> assert_fs::TempDir {
    let project = assert_fs::TempDir::new().expect("temp project");
    project
        .child("project.godot")
        .write_str("config_version=5\n")
        .expect("project.godot");
    project
}

fn write_proof(directory: &assert_fs::TempDir, name: &str, value: &Value) -> PathBuf {
    let file = directory.child(name);
    file.write_str(&serde_json::to_string_pretty(value).expect("proof json"))
        .expect("write proof");
    file.path().to_path_buf()
}

#[cfg(all(feature = "test-hooks", any(target_os = "linux", target_os = "macos")))]
fn fixture_cli() -> Command {
    let mut command = Command::cargo_bin("qa-fixture-34").expect("qa-fixture-34 binary");
    command
        .current_dir(workspace_root())
        .env_remove(godot_mcp_cli::fixture34::bridge::BRIDGE_CONFIG_VARIABLE);
    command
}

#[cfg(all(feature = "test-hooks", any(target_os = "linux", target_os = "macos")))]
struct Task34Fixture {
    paths: FixturePaths,
    active: bool,
}

#[cfg(all(feature = "test-hooks", any(target_os = "linux", target_os = "macos")))]
impl Task34Fixture {
    fn setup() -> Self {
        fixture_cli()
            .args(["setup", "--scenario", SCENARIO])
            .assert()
            .success();
        let paths = FixturePaths::resolve(&workspace_root()).expect("fixture paths");
        assert!(paths.project_root.join("project.godot").is_file());
        assert!(paths.repair_dir.is_dir());
        Self {
            paths,
            active: true,
        }
    }

    fn proof(&self, name: &str) -> PathBuf {
        self.paths.repair_dir.join(name)
    }

    fn request(
        &self,
        transaction_id: &str,
        operation: RepairIntent,
        proof_file: PathBuf,
        dry_run: bool,
    ) -> RepairRequest {
        RepairRequest {
            project: self.paths.project_root.clone(),
            transaction_id: transaction_id.to_owned(),
            operation,
            proof_file,
            dry_run,
        }
    }

    fn command(&self, transaction_id: &str, operation: &str, proof_file: &Path) -> Command {
        let mut command = cli();
        command
            .args(["transaction", "repair", "--project"])
            .arg(&self.paths.project_root)
            .args(["--transaction-id", transaction_id, "--operation", operation])
            .arg("--proof-file")
            .arg(proof_file);
        command
    }

    fn cleanup(mut self) {
        fixture_cli()
            .args(["cleanup", "--scenario", SCENARIO])
            .assert()
            .success();
        self.active = false;
        assert!(
            !self.paths.any_present(),
            "fixture cleanup must remove every owned path"
        );
    }
}

#[cfg(all(feature = "test-hooks", any(target_os = "linux", target_os = "macos")))]
impl Drop for Task34Fixture {
    fn drop(&mut self) {
        if self.active {
            let _ = fixture_cli().args(["cleanup", "--scenario", SCENARIO]).ok();
        }
    }
}

#[cfg(all(feature = "test-hooks", any(target_os = "linux", target_os = "macos")))]
fn success_stdout(assertion: assert_cmd::assert::Assert) -> String {
    let assertion = assertion.success();
    String::from_utf8(assertion.get_output().stdout.clone()).expect("utf-8 repair report")
}

#[cfg(all(feature = "test-hooks", any(target_os = "linux", target_os = "macos")))]
fn report_value<'a>(report: &'a str, key: &str) -> &'a str {
    report
        .lines()
        .find_map(|line| {
            let (candidate, value) = line.split_once(' ')?;
            (candidate == key).then_some(value)
        })
        .unwrap_or_else(|| panic!("repair report has no {key:?} field: {report}"))
}

#[cfg(all(feature = "test-hooks", any(target_os = "linux", target_os = "macos")))]
fn proof_json(path: &Path) -> Value {
    let bytes = std::fs::read(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    serde_json::from_slice(&bytes).expect("fixture proof is json")
}

/// Source text with every comment region removed.
///
/// A self-audit that scanned raw source would flag this file's own module header,
/// and every module that legitimately *documents* the rule it enforces. Only code
/// may be scanned, so this strips line comments (including doc comments) and
/// nested block comments, while leaving string literals intact.
///
/// A `'` is only treated as opening a character literal when it actually is one:
/// `'a'`, `'\n'` and friends. A Rust lifetime such as `&'static str` uses the same
/// character and must not swallow the rest of the file.
fn code_only(source: &str) -> String {
    let characters: Vec<char> = source.chars().collect();
    let mut code = String::with_capacity(source.len());
    let mut index = 0_usize;
    while index < characters.len() {
        let current = characters[index];
        let next = characters.get(index + 1).copied();
        match current {
            '/' if next == Some('/') => {
                while index < characters.len() && characters[index] != '\n' {
                    index += 1;
                }
            }
            '/' if next == Some('*') => {
                let mut depth = 1_usize;
                index += 2;
                while index < characters.len() && depth > 0 {
                    if characters[index] == '/' && characters.get(index + 1) == Some(&'*') {
                        depth += 1;
                        index += 2;
                    } else if characters[index] == '*' && characters.get(index + 1) == Some(&'/') {
                        depth -= 1;
                        index += 2;
                    } else {
                        index += 1;
                    }
                }
                code.push(' ');
            }
            '"' => {
                code.push(current);
                index += 1;
                while index < characters.len() {
                    let inside = characters[index];
                    code.push(inside);
                    index += 1;
                    if inside == '\\' {
                        if let Some(escaped) = characters.get(index) {
                            code.push(*escaped);
                            index += 1;
                        }
                    } else if inside == '"' {
                        break;
                    }
                }
            }
            '\'' if is_char_literal(&characters, index) => {
                code.push(current);
                index += 1;
                while index < characters.len() {
                    let inside = characters[index];
                    code.push(inside);
                    index += 1;
                    if inside == '\\' {
                        if let Some(escaped) = characters.get(index) {
                            code.push(*escaped);
                            index += 1;
                        }
                    } else if inside == '\'' {
                        break;
                    }
                }
            }
            other => {
                code.push(other);
                index += 1;
            }
        }
    }
    code
}

/// Whether the `'` at `index` opens a character literal rather than a lifetime.
fn is_char_literal(characters: &[char], index: usize) -> bool {
    match characters.get(index + 1) {
        Some('\\') => true,
        Some(_) => characters.get(index + 2) == Some(&'\''),
        None => false,
    }
}

/// Every `.rs` file under `crates/`, with its code-only text.
fn every_source() -> Vec<(PathBuf, String)> {
    let mut sources = Vec::new();
    let mut frontier = vec![workspace_root().join("crates")];
    while let Some(directory) = frontier.pop() {
        let entries = std::fs::read_dir(&directory)
            .unwrap_or_else(|error| panic!("{} is listable: {error}", directory.display()));
        for entry in entries {
            let entry = entry.expect("directory entry");
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path).expect("entry metadata");
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                frontier.push(path);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                let text = std::fs::read_to_string(&path)
                    .unwrap_or_else(|error| panic!("{} is readable: {error}", path.display()));
                sources.push((path, code_only(&text)));
            }
        }
    }
    sources.sort_by(|left, right| left.0.cmp(&right.0));
    sources
}

fn relative(path: &Path) -> String {
    path.strip_prefix(workspace_root())
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn host_target() -> package::ReleaseTarget {
    package::ReleaseTarget::host().expect("this host is a published release target")
}

/// Build a real archive for the host platform from the shipping binary these tests
/// already run, so the packaging assertions are made against a genuine artifact
/// rather than a synthetic one.
fn host_archive() -> package::ProducedArchive {
    let binary = assert_cmd::cargo::cargo_bin("godot-mcp");
    let executable = std::fs::read(&binary)
        .unwrap_or_else(|error| panic!("{} is readable: {error}", binary.display()));
    package::build_archive(&package::PackageInputs {
        workspace_root: workspace_root(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        target: host_target(),
        executable,
        supervisor: b"supervisor-test-binary".to_vec(),
        profile: "test".to_string(),
    })
    .expect("the host archive is buildable")
}

/// The archive's payload paths with the archive-internal root prefix removed.
fn payload_paths(produced: &package::ProducedArchive) -> Vec<String> {
    let prefix = format!(
        "{}/",
        package::payload_root(env!("CARGO_PKG_VERSION"), produced.target)
    );
    produced
        .entries
        .iter()
        .map(|entry| {
            entry
                .path
                .strip_prefix(&prefix)
                .unwrap_or(&entry.path)
                .to_string()
        })
        .collect()
}

fn payload_bytes(produced: &package::ProducedArchive, payload_path: &str) -> Vec<u8> {
    let prefix = format!(
        "{}/",
        package::payload_root(env!("CARGO_PKG_VERSION"), produced.target)
    );
    produced
        .entries
        .iter()
        .find(|entry| entry.path == format!("{prefix}{payload_path}"))
        .unwrap_or_else(|| panic!("the archive carries {payload_path}"))
        .bytes
        .clone()
}

/// Extract an archive and return its package root.
fn extract(produced: &package::ProducedArchive, into: &Path) -> PathBuf {
    // The host's own archive format: a zip for Windows, a tar.gz elsewhere.
    if produced.file_name.ends_with(".zip") {
        package::archive::extract_zip(&produced.bytes, into)
    } else {
        package::archive::extract_tar_gz(&produced.bytes, into)
    }
    .expect("the archive extracts");
    into.join(package::payload_root(
        env!("CARGO_PKG_VERSION"),
        produced.target,
    ))
}

fn package_payload(package_root: &Path) -> package::installer::InstallPayload {
    let extracted = package_root
        .parent()
        .expect("the package root has a parent");
    package::install_payload_from_package(extracted, env!("CARGO_PKG_VERSION"), host_target())
        .expect("the extracted package carries an installable payload")
}

/// The comment stripper is itself load-bearing for the self-audit, so it is
/// verified directly rather than trusted.
#[test]
fn the_self_audit_scanner_covers_only_code() {
    let source = "//! forbidden in a module doc\n\
                  /// forbidden in an item doc\n\
                  fn kept() -> &'static str { \"forbidden in a string\" }\n\
                  fn quoted() -> char { '\"' }\n\
                  // forbidden in a line comment\n\
                  /* forbidden in a block /* nested */ comment */\n\
                  fn also_kept() {}\n";
    let code = code_only(source);
    assert!(code.contains("fn kept()"), "{code:?}");
    assert!(code.contains("fn quoted()"), "{code:?}");
    assert!(code.contains("fn also_kept()"), "{code:?}");
    assert_eq!(
        code.matches("forbidden").count(),
        1,
        "only the string literal survives: {code:?}"
    );
    assert!(code.contains("\"forbidden in a string\""));
    assert!(!code.contains("//"), "{code:?}");
}

/// Contract: each repair proof kind must accept independently re-derived state,
/// refuse a specific contradictory observation, and leave exactly the reported
/// durable and live state after the requested operation.
// The Task-34 fixture records an observed PID and start time, which this build
// observes on Linux and macOS only.
#[cfg(all(feature = "test-hooks", any(target_os = "linux", target_os = "macos")))]
#[test]
fn operator_repair_rederives_every_proof_kind_end_to_end() {
    let fixture = Task34Fixture::setup();
    let altered = assert_fs::TempDir::new().expect("altered proofs");

    let file_pre_proof = fixture.proof("file-pre-state-match.json");
    let file_pre_path = fixture
        .paths
        .project_root
        .join(ids::touched_path(ids::FILE_PRE).expect("file pre path"));
    let file_pre_bytes = std::fs::read(&file_pre_path).expect("file pre bytes");

    let dry_run = godot_mcp_cli::repair::run(&fixture.request(
        ids::FILE_PRE,
        RepairIntent::Compensate,
        file_pre_proof.clone(),
        true,
    ))
    .expect("the matching pre-state supports a compensation dry run");
    assert_eq!(dry_run.transaction_id, ids::FILE_PRE);
    assert_eq!(dry_run.effect_kind, "file");
    assert_eq!(dry_run.operation, "compensate");
    assert_eq!(dry_run.disposition, Some("pre-state"));
    assert_eq!(dry_run.phase_before, "quarantined");
    assert_eq!(dry_run.phase_after, "quarantined");
    assert!(!dry_run.transitioned);
    assert!(dry_run.dry_run);
    assert_eq!(
        dry_run.detail,
        "dry run: would transition to compensated — every touched path matches the recorded \
         pre-state state, and every re-derived object identity matches the record"
    );
    assert_eq!(
        std::fs::read(&file_pre_path).expect("file pre bytes"),
        file_pre_bytes
    );

    let canonical_file = proof_json(&file_pre_proof);
    let touched_path = ids::touched_path(ids::FILE_PRE).expect("file pre path");
    let actual_hash = canonical_file["observed_hashes"][touched_path]
        .as_str()
        .expect("observed file hash")
        .to_owned();
    let claimed_hash = "0".repeat(64);
    assert_ne!(claimed_hash, actual_hash);
    let mut wrong_hash = canonical_file.clone();
    wrong_hash["observed_hashes"][touched_path] = json!(claimed_hash);
    let wrong_hash_path = write_proof(&altered, "wrong-hash.json", &wrong_hash);
    let refused = godot_mcp_cli::repair::run(&fixture.request(
        ids::FILE_PRE,
        RepairIntent::Reconcile,
        wrong_hash_path,
        false,
    ))
    .expect("reconcile reports an unproven state without writing");
    assert_eq!(refused.disposition, None);
    assert_eq!(refused.phase_before, "quarantined");
    assert_eq!(refused.phase_after, "quarantined");
    assert!(!refused.transitioned);
    assert_eq!(
        refused.detail,
        format!(
            "the proof file claims {touched_path} hashes to {} and it hashes to {actual_hash}",
            "0".repeat(64)
        )
    );

    let mut wrong_transaction = canonical_file.clone();
    wrong_transaction["transaction_id"] = json!(ids::FILE_POST);
    let wrong_transaction_path =
        write_proof(&altered, "wrong-transaction.json", &wrong_transaction);
    let error = godot_mcp_cli::repair::run(&fixture.request(
        ids::FILE_PRE,
        RepairIntent::Compensate,
        wrong_transaction_path,
        false,
    ))
    .expect_err("a proof for another transaction is refused");
    assert!(matches!(error, RepairError::WrongBinding(_)));
    assert_eq!(
        error.to_string(),
        format!(
            "the proof file does not bind to this repair: it names transaction {}, not {}",
            ids::FILE_POST,
            ids::FILE_PRE
        )
    );

    let mut wrong_identity = canonical_file.clone();
    let held_identity = canonical_file["project_object_identity"]
        .as_str()
        .expect("held project identity");
    let foreign_identity = format!("project_{}", "b".repeat(64));
    assert_ne!(foreign_identity, held_identity);
    wrong_identity["project_object_identity"] = json!(foreign_identity);
    let wrong_identity_path = write_proof(&altered, "wrong-identity.json", &wrong_identity);
    let error = godot_mcp_cli::repair::run(&fixture.request(
        ids::FILE_PRE,
        RepairIntent::Compensate,
        wrong_identity_path,
        false,
    ))
    .expect_err("a proof for another project object is refused");
    assert!(matches!(error, RepairError::WrongBinding(_)));
    assert_eq!(
        error.to_string(),
        format!(
            "the proof file does not bind to this repair: it names project object project_{}, and \
             the held lease designates {held_identity}",
            "b".repeat(64)
        )
    );

    let mut wrong_generation = canonical_file.clone();
    let held_generation = canonical_file["project_generation"]
        .as_u64()
        .expect("held project generation");
    wrong_generation["project_generation"] = json!(held_generation + 1);
    let wrong_generation_path = write_proof(&altered, "wrong-generation.json", &wrong_generation);
    let error = godot_mcp_cli::repair::run(&fixture.request(
        ids::FILE_PRE,
        RepairIntent::Compensate,
        wrong_generation_path,
        false,
    ))
    .expect_err("a proof for another project generation is refused");
    assert!(matches!(error, RepairError::WrongBinding(_)));
    assert_eq!(
        error.to_string(),
        format!(
            "the proof file does not bind to this repair: it names project generation {}, and the \
             held lease designates {held_generation}",
            held_generation + 1
        )
    );

    let mut wrong_kind = proof_json(&fixture.proof("process-pre-state-match.json"));
    wrong_kind["transaction_id"] = json!(ids::FILE_PRE);
    let wrong_kind_path = write_proof(&altered, "wrong-kind.json", &wrong_kind);
    let error = godot_mcp_cli::repair::run(&fixture.request(
        ids::FILE_PRE,
        RepairIntent::Compensate,
        wrong_kind_path,
        false,
    ))
    .expect_err("a proof of another effect kind is refused");
    assert!(matches!(error, RepairError::WrongBinding(_)));
    assert_eq!(
        error.to_string(),
        format!(
            "the proof file does not bind to this repair: it is a process_launch proof, and \
             transaction {} recorded effect kind file",
            ids::FILE_PRE
        )
    );

    let error = godot_mcp_cli::repair::run(&fixture.request(
        ids::FILE_PRE,
        RepairIntent::AcknowledgeSafeState,
        file_pre_proof.clone(),
        false,
    ))
    .expect_err("a pre-state proof cannot acknowledge a post-state");
    assert!(matches!(error, RepairError::Unproven(_)));
    assert_eq!(
        error.to_string(),
        "the state this operation requires is not proven: acknowledge-safe-state requires a \
         post-state proof, and the re-derivation proved a pre-state state"
    );

    let report = success_stdout(
        fixture
            .command(ids::FILE_PRE, "compensate", &file_pre_proof)
            .assert(),
    );
    assert_eq!(report_value(&report, "transaction"), ids::FILE_PRE);
    assert_eq!(report_value(&report, "effect-kind"), "file");
    assert_eq!(report_value(&report, "operation"), "compensate");
    assert_eq!(report_value(&report, "proven-disposition"), "pre-state");
    assert_eq!(report_value(&report, "phase-before"), "quarantined");
    assert_eq!(report_value(&report, "phase-after"), "compensated");
    assert_eq!(report_value(&report, "transition"), "written");
    assert!(report_value(&report, "detail").contains("recorded pre-state state"));
    assert_eq!(
        std::fs::read(&file_pre_path).expect("file pre bytes"),
        file_pre_bytes
    );

    let error = godot_mcp_cli::repair::run(&fixture.request(
        ids::FILE_PRE,
        RepairIntent::Reconcile,
        file_pre_proof,
        false,
    ))
    .expect_err("a compensated transaction is no longer repairable");
    assert!(matches!(error, RepairError::NotQuarantined { .. }));
    assert_eq!(
        error.to_string(),
        format!(
            "transaction {} is compensated, not quarantined; this command refuses it with no \
             state change",
            ids::FILE_PRE
        )
    );

    let file_post_proof = fixture.proof("file-post-state-match.json");
    let file_post_path = fixture
        .paths
        .project_root
        .join(ids::touched_path(ids::FILE_POST).expect("file post path"));
    let file_post_bytes = std::fs::read(&file_post_path).expect("file post bytes");
    let error = godot_mcp_cli::repair::run(&fixture.request(
        ids::FILE_POST,
        RepairIntent::Terminate,
        file_post_proof.clone(),
        false,
    ))
    .expect_err("a file transaction is not terminable");
    assert!(matches!(error, RepairError::NotTerminable(_)));
    assert_eq!(
        error.to_string(),
        "terminate applies only to a process_launch transaction, not file"
    );
    let report = success_stdout(
        fixture
            .command(ids::FILE_POST, "acknowledge-safe-state", &file_post_proof)
            .assert(),
    );
    assert_eq!(report_value(&report, "proven-disposition"), "post-state");
    assert_eq!(report_value(&report, "phase-after"), "committed_fsynced");
    assert_eq!(report_value(&report, "transition"), "written");
    assert_eq!(
        std::fs::read(&file_post_path).expect("file post bytes"),
        file_post_bytes
    );

    let state = FixtureState::load(&fixture.paths.state_file(), SCENARIO)
        .expect("fixture state is readable")
        .expect("fixture state exists");
    let process_pre = state
        .survivors
        .iter()
        .find(|record| record.transaction_id == ids::PROCESS_PRE)
        .expect("process pre survivor");
    let process_pre_start = process_pre
        .start_time
        .as_deref()
        .expect("process start time");
    assert!(
        godot_mcp_cli::repair::process_kind::is_live(process_pre.pid, process_pre_start)
            .expect("process liveness is observable")
    );

    let process_pre_proof = fixture.proof("process-pre-state-match.json");
    let process_dry_run = godot_mcp_cli::repair::run(&fixture.request(
        ids::PROCESS_PRE,
        RepairIntent::Terminate,
        process_pre_proof.clone(),
        true,
    ))
    .expect("the suspended survivor supports a terminate dry run");
    assert_eq!(process_dry_run.disposition, Some("pre-state"));
    assert_eq!(process_dry_run.phase_after, "quarantined");
    assert!(!process_dry_run.transitioned);
    assert!(
        process_dry_run
            .detail
            .contains("would transition to compensated")
    );

    let mut wrong_pid = proof_json(&process_pre_proof);
    let claimed_pid = u64::from(process_pre.pid) + 1;
    wrong_pid["observed_pid"] = json!(claimed_pid);
    let wrong_pid_path = write_proof(&altered, "wrong-pid.json", &wrong_pid);
    let refused = godot_mcp_cli::repair::run(&fixture.request(
        ids::PROCESS_PRE,
        RepairIntent::Reconcile,
        wrong_pid_path,
        false,
    ))
    .expect("reconcile reports a contradictory PID without writing");
    assert_eq!(
        refused.detail,
        format!(
            "the proof file claims pid {claimed_pid} and the record names pid {}",
            process_pre.pid
        )
    );
    assert_eq!(refused.phase_after, "quarantined");
    assert!(!refused.transitioned);

    let report = success_stdout(
        fixture
            .command(ids::PROCESS_PRE, "terminate", &process_pre_proof)
            .assert(),
    );
    assert_eq!(report_value(&report, "effect-kind"), "process_launch");
    assert_eq!(report_value(&report, "operation"), "terminate");
    assert_eq!(report_value(&report, "proven-disposition"), "pre-state");
    assert_eq!(report_value(&report, "phase-after"), "compensated");
    assert_eq!(report_value(&report, "transition"), "written");
    assert!(
        !godot_mcp_cli::repair::process_kind::is_live(process_pre.pid, process_pre_start)
            .expect("terminated process liveness is observable"),
        "the proven survivor must be gone before the repair is journaled"
    );

    let process_post = state
        .survivors
        .iter()
        .find(|record| record.transaction_id == ids::PROCESS_POST)
        .expect("process post survivor");
    let process_post_start = process_post
        .start_time
        .as_deref()
        .expect("process start time");
    let process_post_proof = fixture.proof("process-post-state-match.json");
    let report = success_stdout(
        fixture
            .command(
                ids::PROCESS_POST,
                "acknowledge-safe-state",
                &process_post_proof,
            )
            .assert(),
    );
    assert_eq!(report_value(&report, "proven-disposition"), "post-state");
    assert_eq!(report_value(&report, "phase-after"), "committed_fsynced");
    assert_eq!(report_value(&report, "transition"), "written");
    assert!(
        godot_mcp_cli::repair::process_kind::is_live(process_post.pid, process_post_start)
            .expect("contained process liveness is observable"),
        "acknowledging a safe post-state must not terminate its contained survivor"
    );

    let editor_pre_proof = fixture.proof("editor-pre-state-match.json");
    let rendezvous = fixture.paths.bridge_rendezvous();
    let offline_rendezvous = fixture.paths.bridge_dir().join("rendezvous.offline");
    std::fs::rename(&rendezvous, &offline_rendezvous).expect("hide bridge rendezvous");
    let unreachable = godot_mcp_cli::repair::run(&fixture.request(
        ids::EDITOR_PRE,
        RepairIntent::Compensate,
        editor_pre_proof.clone(),
        false,
    ));
    std::fs::rename(&offline_rendezvous, &rendezvous).expect("restore bridge rendezvous");
    let error = unreachable.expect_err("an undiscoverable bridge is refused");
    assert!(matches!(error, RepairError::Unproven(_)));
    assert_eq!(
        error.to_string(),
        format!(
            "the state this operation requires is not proven: the editor bridge is unreachable, \
             so no receipt or live state could be observed and no file hash can substitute: no \
             rendezvous.json bound to this project object identity and generation was found under {}",
            fixture.paths.project_root.display()
        )
    );

    let report = success_stdout(
        fixture
            .command(ids::EDITOR_PRE, "compensate", &editor_pre_proof)
            .assert(),
    );
    assert_eq!(report_value(&report, "effect-kind"), "editor_runtime");
    assert_eq!(report_value(&report, "proven-disposition"), "pre-state");
    assert_eq!(report_value(&report, "phase-after"), "compensated");
    assert_eq!(report_value(&report, "transition"), "written");
    assert!(report_value(&report, "detail").contains("live bridge acknowledges action"));

    let editor_post_proof = fixture.proof("editor-post-state-match.json");
    let report = success_stdout(
        fixture
            .command(
                ids::EDITOR_POST,
                "acknowledge-safe-state",
                &editor_post_proof,
            )
            .assert(),
    );
    assert_eq!(report_value(&report, "effect-kind"), "editor_runtime");
    assert_eq!(report_value(&report, "proven-disposition"), "post-state");
    assert_eq!(report_value(&report, "phase-after"), "committed_fsynced");
    assert_eq!(report_value(&report, "transition"), "written");
    assert!(report_value(&report, "detail").contains("live bridge acknowledges action"));

    fixture.cleanup();
}

#[test]
fn package_and_repair_happy_path() {
    // The committed release graph carries no forbidden gdext artifact, so the
    // shipping surface these tests exercise is the pure-Rust one.
    let findings = audit_release_graph(&workspace_root()).expect("the release graph is auditable");
    assert!(
        findings.is_empty(),
        "the committed release graph must be clean: {findings:?}"
    );

    // The shipping binary answers its own version query on clean stdout, which is
    // the stdio smoke the packaged archive repeats against its extracted copy.
    cli()
        .arg("version")
        .assert()
        .success()
        .stdout(predicate::str::contains("godot-mcp "))
        .stdout(predicate::str::contains("server-contract "));

    // The exact interface: `transaction repair` exists, takes exactly the five
    // documented arguments, and accepts exactly the four proof-driven operations.
    let help = cli()
        .args(["transaction", "repair", "--help"])
        .assert()
        .success();
    let help = String::from_utf8(help.get_output().stdout.clone()).expect("utf-8 help");
    for flag in [
        "--project",
        "--transaction-id",
        "--operation",
        "--proof-file",
        "--dry-run",
    ] {
        assert!(
            help.contains(flag),
            "the interface must expose {flag}: {help}"
        );
    }
    let flags: BTreeSet<&str> = help
        .split_whitespace()
        .filter(|token| token.starts_with("--"))
        .map(|token| token.trim_end_matches(['>', ',', '.']))
        .collect();
    let expected: BTreeSet<&str> = [
        "--project",
        "--transaction-id",
        "--operation",
        "--proof-file",
        "--dry-run",
        "--help",
    ]
    .into_iter()
    .collect();
    assert_eq!(
        flags, expected,
        "the repair interface must expose exactly the documented arguments"
    );

    // Every documented operation parses, and each is refused on its own merits
    // (there is no journaled transaction here) rather than at the parser.
    let project = empty_project();
    let proofs = assert_fs::TempDir::new().expect("temp proofs");
    let proof = write_proof(
        &proofs,
        "file.json",
        &json!({
            "effect_kind": "file",
            "transaction_id": "34000000-0000-4000-8000-000000000101",
            "project_object_identity": format!("project_{}", "a".repeat(64)),
            "project_generation": 0,
            "observed_touched_object_identities": ["unix:dev=1,ino=2"],
            "observed_existence": { "scenes/base_pre.tscn": "present" },
            "observed_hashes": { "scenes/base_pre.tscn": "0".repeat(64) },
            "claimed_disposition": "pre-state",
        }),
    );
    for operation in EVERY_OPERATION {
        let assertion = cli()
            .args(["transaction", "repair", "--project"])
            .arg(project.path())
            .args([
                "--transaction-id",
                "34000000-0000-4000-8000-000000000101",
                "--operation",
                operation,
                "--proof-file",
            ])
            .arg(&proof)
            .assert()
            .code(1);
        let stderr =
            String::from_utf8(assertion.get_output().stderr.clone()).expect("utf-8 diagnostics");
        assert!(
            !stderr.contains("unknown operation"),
            "{operation} must be a recognized operation: {stderr}"
        );
        assert!(
            stderr.contains("is not journaled"),
            "{operation} must be refused because no such transaction exists: {stderr}"
        );
    }

    // `--dry-run` parses on every operation and is likewise refused on the merits.
    for operation in EVERY_OPERATION {
        cli()
            .args(["transaction", "repair", "--project"])
            .arg(project.path())
            .args([
                "--transaction-id",
                "34000000-0000-4000-8000-000000000101",
                "--operation",
                operation,
                "--dry-run",
                "--proof-file",
            ])
            .arg(&proof)
            .assert()
            .code(1)
            .stderr(predicate::str::contains("is not journaled"));
    }

    packaging_happy_path();
}

/// The packaging half of the happy path: a real archive is produced, audited,
/// extracted, self-checked, executed, and used to install project assets.
fn packaging_happy_path() {
    let produced = host_archive();

    // Every required payload is present: the Rust binary, the shipping GDScript,
    // the schemas, both licence notices, the CycloneDX SBOM and an example config.
    let paths = payload_paths(&produced);
    for required in package::REQUIRED_PAYLOAD_PATHS {
        assert!(
            paths.iter().any(|path| path == required),
            "the archive must carry {required}: {paths:?}"
        );
    }
    let server = format!("bin/{}", host_target().executable_name());
    let supervisor = format!("bin/{}", host_target().supervisor_name());
    assert!(
        paths.contains(&server),
        "the archive must carry the Rust binary: {paths:?}"
    );

    // The server and its required supervisor ship, and neither is a test-hooks binary. The
    // test-hook list is read from the crate manifest, so a newly added test-only
    // binary is covered without editing this test.
    let binaries: Vec<&String> = paths
        .iter()
        .filter(|path| path.starts_with("bin/"))
        .collect();
    // In the archive's own (sorted) order: `.exe` sorts the server after its
    // supervisor on Windows.
    let mut expected = vec![&server, &supervisor];
    expected.sort();
    assert_eq!(
        binaries, expected,
        "an archive must carry the server and its required supervisor"
    );
    let hooks = package::test_hook_binary_names(&workspace_root())
        .expect("the crate manifest declares its test-hook binaries");
    assert!(
        hooks.contains(&"qa-fixture-34".to_owned())
            && hooks.contains(&"qa-host-invoke".to_owned())
            && hooks.contains(&"release-package".to_owned()),
        "the test-hook exclusion list must be derived, not assumed: {hooks:?}"
    );
    for hook in &hooks {
        assert!(
            !paths.iter().any(|path| path == &format!("bin/{hook}")),
            "the archive must not carry the test-hooks binary {hook}"
        );
    }

    // Godot itself is never bundled, and no gdext artifact rides along.
    for path in &paths {
        assert!(
            !path.ends_with(".gdextension"),
            "no .gdextension descriptor may ship: {path}"
        );
        for suffix in [".so", ".dylib", ".dll"] {
            assert!(
                !path.ends_with(suffix),
                "no native library may ship: {path}"
            );
        }
    }

    // The SBOM is a real CycloneDX document describing the linked closure, and the
    // shipping crate itself is one of its components.
    let sbom_bytes = payload_bytes(&produced, "sbom/godot-mcp.cdx.json");
    let document: Value = serde_json::from_slice(&sbom_bytes).expect("the SBOM is valid json");
    assert_eq!(document["bomFormat"], json!("CycloneDX"));
    assert_eq!(document["specVersion"], json!("1.5"));
    let components = godot_mcp_cli::package::sbom::component_names(&document);
    assert!(
        components.contains(&"godot-mcp-cli".to_owned()),
        "the SBOM must describe the shipping closure: {components:?}"
    );
    assert!(
        components.iter().any(|name| name == "serde"),
        "the SBOM must list third-party components: {components:?}"
    );

    // The example config enables no listener and carries no credential.
    let example = String::from_utf8(payload_bytes(&produced, "examples/godot-mcp.toml"))
        .expect("utf-8 example config");
    assert!(example.contains("transport = \"stdio\""), "{example}");
    assert!(!example.contains("0.0.0.0"), "{example}");

    // Archive bytes are a pure function of the payload, which is what makes the
    // two-clean-build reproducibility check meaningful.
    let again = package::build_archive(&package::PackageInputs {
        workspace_root: workspace_root(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        target: host_target(),
        executable: payload_bytes(&produced, &server),
        supervisor: payload_bytes(&produced, &supervisor),
        profile: "test".to_owned(),
    })
    .expect("the same payload is buildable twice");
    assert_eq!(
        produced.sha256, again.sha256,
        "two packaging runs over the same inputs must produce identical archives"
    );
    assert_eq!(produced.bytes, again.bytes);

    // The published checksum manifest names the archive with its real digest.
    let checksums = package::checksum_manifest(std::slice::from_ref(&produced));
    assert_eq!(
        checksums,
        format!("{}  {}\n", produced.sha256, produced.file_name)
    );

    // The provenance attestation is honest about what this host built: it records
    // the produced subject and names every target it did not build, unsigned.
    let attestation = package::provenance(
        &workspace_root(),
        env!("CARGO_PKG_VERSION"),
        std::slice::from_ref(&produced),
    );
    assert_eq!(attestation["signed"], json!(false));
    assert_eq!(
        attestation["subject"][0]["digest"]["sha256"],
        json!(produced.sha256)
    );
    let unbuilt: Vec<String> = attestation["not_built_here"]
        .as_array()
        .expect("the attestation lists unbuilt targets")
        .iter()
        .filter_map(|entry| entry["target"].as_str())
        .map(str::to_string)
        .collect();
    for target in package::ReleaseTarget::ALL.iter().copied() {
        if target == produced.target {
            assert!(!unbuilt.contains(&target.triple().to_owned()));
        } else {
            assert!(
                unbuilt.contains(&target.triple().to_owned()),
                "{} must be recorded as not built here: {unbuilt:?}",
                target.triple()
            );
        }
    }

    // The whole first-release native-dependency guard passes over the committed
    // workspace, the SBOM that ships and the archive that was produced.
    let findings = audit_full_release_graph(&ReleaseAuditInputs {
        workspace_root: &workspace_root(),
        sbom: Some(&document),
        archives: &[ArchiveUnderAudit {
            file_name: &produced.file_name,
            entries: &produced.entries,
        }],
    })
    .expect("the full release graph is auditable");
    assert!(
        findings.is_empty(),
        "the produced release must be free of every native-bridge artifact: {findings:?}"
    );

    // The archive extracts, verifies itself, and the extracted binary runs.
    let extracted = assert_fs::TempDir::new().expect("temp extraction");
    let root = extract(&produced, extracted.path());
    let report = package::self_check(&root).expect("the extracted package self-checks");
    assert!(
        report.is_clean(),
        "the extracted package must verify cleanly: {:?}",
        report.problems
    );
    assert!(report.files_verified > 0);

    let extracted_binary = root.join("bin").join("godot-mcp");
    Command::new(&extracted_binary)
        .arg("version")
        .assert()
        .success()
        .stdout(predicate::str::contains("godot-mcp "))
        .stdout(predicate::str::contains("server-contract "));
    Command::new(&extracted_binary)
        .arg("self-check")
        .assert()
        .success()
        .stdout(predicate::str::contains("result clean"));

    installer_happy_path(&root);
}

/// Install, idempotent update, backup-on-drift and removal against a real
/// project, proving no user-owned file is ever touched.
fn installer_happy_path(package_root: &Path) {
    use godot_mcp_cli::package::installer::{self, WriteMode};

    let payload = package_payload(package_root);
    let project = empty_project();
    let user_file = project.child("scripts/user_written.gd");
    user_file
        .write_str("# the user's own script\n")
        .expect("write the user's file");
    let user_bytes = std::fs::read(user_file.path()).expect("read the user's file");

    let installed = installer::apply(project.path(), &payload, "0.1.0", WriteMode::Install)
        .expect("a first install into a clean project succeeds");
    assert_eq!(installed.created.len(), payload.files.len());
    assert!(installed.backed_up.is_empty());

    // Idempotency: a second update over unchanged files writes nothing.
    let again = installer::apply(project.path(), &payload, "0.1.0", WriteMode::Update)
        .expect("an update over an unchanged install succeeds");
    assert_eq!(
        again.changed(),
        0,
        "an update must be idempotent: {again:?}"
    );
    assert_eq!(again.unchanged.len(), payload.files.len());

    // A user edit to an owned file is preserved as a backup before the update
    // overwrites it, so the edit is never simply discarded.
    let owned = project.path().join(&payload.files[0].path);
    let edited = {
        let mut bytes = std::fs::read(&owned).expect("read the owned file");
        bytes.extend_from_slice(b"\n# the user's edit\n");
        bytes
    };
    std::fs::write(&owned, &edited).expect("apply the user's edit");
    let after_drift = installer::apply(project.path(), &payload, "0.1.0", WriteMode::Update)
        .expect("an update backs up a drifted owned file");
    assert_eq!(after_drift.backed_up.len(), 1);
    let backup = project.path().join(&after_drift.backed_up[0].backup);
    assert_eq!(
        std::fs::read(&backup).expect("read the backup"),
        edited,
        "the user's edit must survive verbatim in the backup"
    );

    // Removal takes back only what it still owns, and leaves the backup behind.
    let removed = installer::remove(project.path()).expect("removal succeeds");
    assert_eq!(removed.removed.len(), payload.files.len());
    assert!(backup.is_file(), "a backup must survive removal");
    assert!(
        installer::InstallManifest::load(project.path())
            .expect("the manifest is loadable")
            .is_none(),
        "removal must clear the ownership manifest"
    );

    // Removing twice is a no-op rather than an error.
    let twice = installer::remove(project.path()).expect("a second removal is a no-op");
    assert_eq!(twice.changed(), 0);

    // Throughout all of that, the user's own files were never touched.
    assert_eq!(
        std::fs::read(user_file.path()).expect("the user's file survives"),
        user_bytes
    );
    project.child("project.godot").assert("config_version=5\n");
}

#[test]
fn package_and_repair_failure_paths() {
    let project = empty_project();
    let proofs = assert_fs::TempDir::new().expect("temp proofs");
    let identity = format!("project_{}", "a".repeat(64));
    let transaction = "34000000-0000-4000-8000-000000000101";
    let canonical = json!({
        "effect_kind": "file",
        "transaction_id": transaction,
        "project_object_identity": identity,
        "project_generation": 0,
        "observed_touched_object_identities": ["unix:dev=1,ino=2"],
        "observed_existence": { "scenes/base_pre.tscn": "present" },
        "observed_hashes": { "scenes/base_pre.tscn": "0".repeat(64) },
        "claimed_disposition": "pre-state",
    });

    let repair = |proof: &Path, operation: &str| {
        cli()
            .args(["transaction", "repair", "--project"])
            .arg(project.path())
            .args(["--transaction-id", transaction, "--operation", operation])
            .arg("--proof-file")
            .arg(proof)
            .assert()
    };

    // A bypass flag does not exist. This is a parser refusal, not a policy check.
    cli()
        .args(["transaction", "repair", "--project"])
        .arg(project.path())
        .args([
            "--transaction-id",
            transaction,
            "--operation",
            "acknowledge-safe-state",
            "--proof-file",
            "unused.json",
            "--force",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unexpected argument '--force'"));

    // Neither does any other spelling of a bypass.
    for bypass in ["--yes", "--assume-proven", "--skip-proof", "--no-verify"] {
        cli()
            .args(["transaction", "repair", "--project"])
            .arg(project.path())
            .args([
                "--transaction-id",
                transaction,
                "--operation",
                "acknowledge-safe-state",
                "--proof-file",
                "unused.json",
            ])
            .arg(bypass)
            .assert()
            .failure()
            .stderr(predicate::str::contains("unexpected argument"));
    }

    // An operation outside the four proof-driven ones is refused at the parser.
    for operation in ["force", "commit", "clear-quarantine", "override"] {
        cli()
            .args(["transaction", "repair", "--project"])
            .arg(project.path())
            .args([
                "--transaction-id",
                transaction,
                "--operation",
                operation,
                "--proof-file",
                "unused.json",
            ])
            .assert()
            .failure()
            .stderr(predicate::str::contains("unknown operation"));
    }

    // Every argument is required; none has a silent default.
    for omitted in [
        "--project",
        "--transaction-id",
        "--operation",
        "--proof-file",
    ] {
        let mut command = cli();
        command.args(["transaction", "repair"]);
        if omitted != "--project" {
            command.arg("--project").arg(project.path());
        }
        if omitted != "--transaction-id" {
            command.args(["--transaction-id", transaction]);
        }
        if omitted != "--operation" {
            command.args(["--operation", "compensate"]);
        }
        if omitted != "--proof-file" {
            command.args(["--proof-file", "unused.json"]);
        }
        command
            .assert()
            .failure()
            .stderr(predicate::str::contains("required"));
    }

    // The normative authorization order loads the journaled record *before* the
    // proof file, so against this journal-free project every one of the following
    // proof files reaches the same fail-closed refusal with an empty stdout and no
    // partial change. Each individual proof gate is therefore asserted against the
    // proof parser itself, which is the component that owns it.
    use godot_mcp_cli::repair::supplied;

    // An unreadable proof file is refused, and the command still fails closed.
    repair(Path::new("does-not-exist.json"), "compensate")
        .code(1)
        .stdout(predicate::str::is_empty());
    let missing = supplied::load(Path::new("does-not-exist.json"))
        .expect_err("an unreadable proof file is refused");
    assert!(
        missing.to_string().contains("could not be read"),
        "{missing} must name the unreadable file"
    );

    // A proof file carrying an unknown field is proving something this command
    // does not evaluate, and the canonical schema refuses it.
    let mut unknown = canonical.clone();
    unknown
        .as_object_mut()
        .expect("object")
        .insert("force".to_owned(), json!(true));
    let path = write_proof(&proofs, "unknown.json", &unknown);
    repair(&path, "compensate")
        .code(1)
        .stdout(predicate::str::is_empty());
    let refusal = supplied::load(&path).expect_err("an unknown field is refused");
    assert!(
        refusal
            .to_string()
            .contains("violates the canonical schema"),
        "{refusal} must be a schema refusal"
    );

    // A proof file that names no claimed disposition is refused rather than
    // defaulted, for every effect kind.
    for (name, mut claim) in [
        ("file-no-disposition.json", canonical.clone()),
        (
            "process-no-disposition.json",
            json!({
                "effect_kind": "process_launch",
                "transaction_id": transaction,
                "project_object_identity": identity,
                "project_generation": 0,
                "observed_pid": 4242,
                "observed_start_time": "12345",
                "observed_boundary_membership": "confirmed",
                "observed_release_status": "suspended",
                "claimed_disposition": "pre-state",
            }),
        ),
        (
            "editor-no-disposition.json",
            json!({
                "effect_kind": "editor_runtime",
                "transaction_id": transaction,
                "project_object_identity": identity,
                "project_generation": 0,
                "bridge_reachable": true,
                "bridge_receipt": "action-1",
                "live_state_probe": "node:/root",
                "claimed_disposition": "pre-state",
            }),
        ),
    ] {
        claim
            .as_object_mut()
            .expect("object")
            .remove("claimed_disposition");
        let path = write_proof(&proofs, name, &claim);
        repair(&path, "compensate")
            .code(1)
            .stdout(predicate::str::is_empty());
        let refusal = supplied::load(&path).expect_err("a missing disposition is refused");
        let message = refusal.to_string();
        assert!(
            message.contains("claimed_disposition") || message.contains("oneOf"),
            "{message} must refuse the claim through the field gate or the schema gate"
        );
    }

    // A null bridge receipt or a null live-state probe always fails
    // `editor_runtime` proof: a file hash can never substitute for editor state.
    for absent in ["bridge_receipt", "live_state_probe"] {
        let mut claim = json!({
            "effect_kind": "editor_runtime",
            "transaction_id": transaction,
            "project_object_identity": identity,
            "project_generation": 0,
            "bridge_reachable": true,
            "bridge_receipt": "action-1",
            "live_state_probe": "node:/root",
            "claimed_disposition": "post-state",
        });
        claim
            .as_object_mut()
            .expect("object")
            .insert(absent.to_owned(), Value::Null);
        let path = write_proof(&proofs, &format!("editor-null-{absent}.json"), &claim);
        repair(&path, "acknowledge-safe-state")
            .code(1)
            .stdout(predicate::str::is_empty());
        let refusal = supplied::load(&path).expect_err("a null field is refused");
        assert!(
            refusal.to_string().contains(absent),
            "{refusal} must name {absent}"
        );
    }

    // An unjournaled transaction is refused with no state change, and a project
    // that is not a project at all cannot even be leased.
    repair(
        &write_proof(&proofs, "canonical.json", &canonical),
        "compensate",
    )
    .code(1)
    .stderr(predicate::str::contains("is not journaled"));
    let bare = assert_fs::TempDir::new().expect("temp dir");
    cli()
        .args(["transaction", "repair", "--project"])
        .arg(bare.path())
        .args([
            "--transaction-id",
            transaction,
            "--operation",
            "compensate",
            "--proof-file",
        ])
        .arg(proofs.child("canonical.json").path())
        .assert()
        .code(1)
        .stderr(predicate::str::contains("lease"));

    // A public HTTP bind is rejected outright, with no partial application.
    cli()
        .args(["check-config", "--config"])
        .arg({
            let config = proofs.child("public-http.toml");
            config
                .write_str(
                    "transport = \"http\"\n\n[http]\nbind = \"0.0.0.0:9000\"\nmode = \
                     \"trusted_local\"\nhost = \"0.0.0.0:9000\"\norigin = \
                     \"http://0.0.0.0:9000\"\n",
                )
                .expect("write config");
            config.path().to_path_buf()
        })
        .assert()
        .code(2);

    // Each deliberately corrupted release graph reproduces its own refusal, so a
    // forbidden gdext artifact can never reach an archive unnoticed.
    for fixture in [
        CorruptFixture::ShippingGodot,
        CorruptFixture::ShippingGdextBridge,
        CorruptFixture::ShippingGdextension,
        CorruptFixture::ShippingNativeBridgeLib,
    ] {
        let staged = assert_fs::TempDir::new().expect("temp fixture");
        let root = corrupt::build(fixture, staged.path()).expect("the fixture is buildable");
        let findings = audit_release_graph(&root).expect("the staged graph is auditable");
        assert!(
            !findings.is_empty(),
            "{fixture:?} must reproduce a forbidden-artifact finding"
        );
    }
    // The false-positive guard: an ordinary native dependency library is not
    // addon/bridge shipping content and must not be flagged.
    let benign = assert_fs::TempDir::new().expect("temp fixture");
    let root = corrupt::build(CorruptFixture::OrdinaryDependencyLibrary, benign.path())
        .expect("the fixture is buildable");
    assert!(
        audit_release_graph(&root)
            .expect("the staged graph is auditable")
            .is_empty(),
        "an ordinary dependency library must not be reported as a forbidden artifact"
    );

    packaging_failure_paths();
}

/// The packaging half of the failure paths: every surface the first-release
/// native-dependency guard covers is proven falsifiable, and the installer refuses
/// without touching a byte.
fn packaging_failure_paths() {
    // A deliberately supplied `godot` dependency is rejected on every surface it
    // reaches: the workspace dependency table, the shipping member's own manifest,
    // the lock, and the release binary's linked-crate list. The fixture is
    // self-contained and non-shipping, so this workspace never gains the crate.
    let staged = assert_fs::TempDir::new().expect("temp fixture");
    let root = corrupt::build(CorruptFixture::SuppliedGodotDependency, staged.path())
        .expect("the supplied-godot fixture is buildable");
    let findings = audit_full_release_graph(&ReleaseAuditInputs {
        workspace_root: &root,
        sbom: None,
        archives: &[],
    })
    .expect("the staged graph is auditable");
    assert!(
        findings.iter().any(|found| found.artifact == "godot"),
        "the supplied dependency must be named: {findings:?}"
    );
    for expected in [
        "Cargo.toml [workspace.dependencies]",
        "crates/godot-mcp-cli/Cargo.toml",
        "Cargo.lock",
        "linked-crate list of godot-mcp-cli",
    ] {
        assert!(
            findings
                .iter()
                .any(|found| found.location == expected && found.artifact == "godot"),
            "the guard must reject crate godot at {expected}: {findings:?}"
        );
    }
    // The whole gdext family is caught, not just the exact name `godot`.
    assert!(
        findings.iter().any(|found| found.artifact == "godot-core"),
        "a transitively linked gdext crate must be rejected too: {findings:?}"
    );
    // The workspace's own crates share that prefix and must never be flagged.
    for found in &findings {
        assert_ne!(found.artifact, "godot-mcp-cli");
        assert_ne!(found.artifact, "godot-mcp-domain");
    }

    // An archive carrying a forbidden artifact is rejected by name, for each class
    // separately, so one class can never stand in for the others.
    let clean = host_archive();
    for (planted, expected) in [
        ("addons/godot_mcp/godot_mcp.gdextension", ".gdextension"),
        ("bridge/bin/libgodot_mcp_bridge.so", "native-bridge-library"),
        ("bin/qa-fixture-34", "test-hooks binary qa-fixture-34"),
        ("bin/release-package", "test-hooks binary release-package"),
    ] {
        let mut entries = clean.entries.clone();
        entries.push(package::archive::ArchiveEntry {
            path: format!(
                "{}/{planted}",
                package::payload_root(env!("CARGO_PKG_VERSION"), clean.target)
            ),
            bytes: b"planted".to_vec(),
            mode: 0o644,
        });
        let findings = audit_full_release_graph(&ReleaseAuditInputs {
            workspace_root: &workspace_root(),
            sbom: None,
            archives: &[ArchiveUnderAudit {
                file_name: &clean.file_name,
                entries: &entries,
            }],
        })
        .expect("the archive is auditable");
        assert!(
            findings
                .iter()
                .any(|found| found.artifact == expected && found.location.contains(planted)),
            "an archive carrying {planted} must be rejected as {expected}: {findings:?}"
        );
    }

    // An SBOM that lists a forbidden component is rejected even when everything
    // else is clean.
    let poisoned = json!({
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "components": [
            { "type": "library", "name": "godot", "version": "0.5.4" },
        ],
    });
    let findings = audit_full_release_graph(&ReleaseAuditInputs {
        workspace_root: &workspace_root(),
        sbom: Some(&poisoned),
        archives: &[],
    })
    .expect("the SBOM is auditable");
    assert!(
        findings
            .iter()
            .any(|found| found.artifact == "godot" && found.location == "CycloneDX SBOM component"),
        "a forbidden SBOM component must be rejected: {findings:?}"
    );

    // A package whose payload was altered after packaging fails its own
    // self-check, and a credential planted into it is named as well — so an
    // archive can neither drift nor smuggle a secret past verification.
    let extracted = assert_fs::TempDir::new().expect("temp extraction");
    let package_root = extract(&clean, extracted.path());
    let secret = package::credential_markers()
        .into_iter()
        .next()
        .expect("at least one credential marker");
    std::fs::write(
        package_root.join("examples/godot-mcp.toml"),
        format!("{secret}\nMIIBOgIBAAJB\n"),
    )
    .expect("alter a recorded payload file");
    let report = package::self_check(&package_root).expect("self-check runs");
    assert!(
        !report.is_clean(),
        "a package whose recorded file was altered must not verify"
    );
    assert!(
        report
            .problems
            .iter()
            .any(|problem| problem.contains("does not match its recorded SHA-256")),
        "the self-check must name the digest mismatch: {:?}",
        report.problems
    );
    assert!(
        report
            .problems
            .iter()
            .any(|problem| problem.contains("contains a credential marker")),
        "the self-check must name the planted credential: {:?}",
        report.problems
    );

    // A package missing a required notice, SBOM or example config is rejected.
    let stripped = assert_fs::TempDir::new().expect("temp extraction");
    let stripped_root = extract(&clean, stripped.path());
    std::fs::remove_file(stripped_root.join("licenses/THIRD-PARTY-NOTICES.md"))
        .expect("remove a required notice");
    let report = package::self_check(&stripped_root).expect("self-check runs");
    assert!(
        report
            .problems
            .iter()
            .any(|problem| problem
                .contains("licenses/THIRD-PARTY-NOTICES.md is recorded but missing")),
        "a missing notice must be named: {:?}",
        report.problems
    );

    installer_failure_paths();
}

/// The installer refuses a foreign file and a drifted owned file, and in both
/// cases the project is left byte-for-byte unchanged.
fn installer_failure_paths() {
    use godot_mcp_cli::package::installer::{self, FileAction, WriteMode};

    let extracted = assert_fs::TempDir::new().expect("temp extraction");
    let produced = host_archive();
    let package_root = extract(&produced, extracted.path());
    let payload = package_payload(&package_root);

    // A file the installer does not own is never overwritten, and nothing else is
    // written either: the refusal happens before the first byte.
    let project = empty_project();
    let occupied = project.path().join(&payload.files[0].path);
    std::fs::create_dir_all(occupied.parent().expect("parent")).expect("create the parent");
    std::fs::write(&occupied, b"the user's own file\n").expect("plant a foreign file");
    let before = snapshot(project.path());
    let refusal = installer::apply(project.path(), &payload, "0.1.0", WriteMode::Install)
        .expect_err("a foreign file is a conflict");
    assert!(
        refusal.contains("not owned by this installer"),
        "{refusal} must name the ownership reason"
    );
    assert_eq!(
        snapshot(project.path()),
        before,
        "a refused install must leave the project unchanged"
    );
    let plan = installer::plan(project.path(), &payload, WriteMode::Install)
        .expect("the plan is computable");
    assert!(
        plan.files
            .iter()
            .any(|file| file.action == FileAction::ForeignConflict)
    );

    // An install onto a drifted owned file refuses; only `update` may back it up.
    let clean_project = empty_project();
    installer::apply(clean_project.path(), &payload, "0.1.0", WriteMode::Install)
        .expect("the first install succeeds");
    let owned = clean_project.path().join(&payload.files[0].path);
    std::fs::write(&owned, b"# the user's edit\n").expect("apply the user's edit");
    let before = snapshot(clean_project.path());
    let refusal = installer::apply(clean_project.path(), &payload, "0.1.0", WriteMode::Install)
        .expect_err("a drifted owned file is a conflict for install");
    assert!(
        refusal.contains("was modified since install"),
        "{refusal} must name the drift reason"
    );
    assert_eq!(
        snapshot(clean_project.path()),
        before,
        "a refused install must leave the project unchanged"
    );

    // A payload path outside the install root can never reach a user file.
    let escaping = installer::InstallPayload {
        files: vec![installer::PayloadFile {
            path: "project.godot".to_string(),
            bytes: b"overwritten\n".to_vec(),
            mode: 0o644,
        }],
    };
    let before = snapshot(clean_project.path());
    let refusal = installer::apply(clean_project.path(), &escaping, "0.1.0", WriteMode::Update)
        .expect_err("a payload outside the install root is refused");
    assert!(refusal.contains("install root"), "{refusal}");
    assert_eq!(snapshot(clean_project.path()), before);

    for traversal in ["godot_mcp/../project.godot", "godot_mcp/./x.gd"] {
        let escaping = installer::InstallPayload {
            files: vec![installer::PayloadFile {
                path: traversal.to_string(),
                bytes: b"overwritten\n".to_vec(),
                mode: 0o644,
            }],
        };
        installer::apply(clean_project.path(), &escaping, "0.1.0", WriteMode::Update)
            .expect_err("a traversal path is refused");
        assert_eq!(snapshot(clean_project.path()), before);
    }
}

/// Every file under `root` with its bytes, so "nothing changed" is asserted
/// against real contents rather than against a modification time.
fn snapshot(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut frontier = vec![root.to_path_buf()];
    while let Some(directory) = frontier.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                frontier.push(path);
            } else if let Ok(bytes) = std::fs::read(&path) {
                let key = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .to_string();
                files.insert(key, bytes);
            }
        }
    }
    files
}

/// The tool/resource registry snapshot the transports advertise contains no path
/// to `transaction repair`, for every capability profile.
///
/// This is the runtime half of the no-network guarantee: whatever the sources say,
/// what a peer can actually discover is what these snapshots list.
#[test]
fn no_advertised_tool_or_resource_reaches_the_repair_command() {
    for profile in EVERY_PROFILE {
        let kernel = ServiceKernel::new(profile);
        for tool in kernel.tools() {
            assert!(
                !tool.name.contains("repair") && !tool.name.contains("transaction"),
                "profile {} advertises tool {}, which names an operator-repair path",
                profile.manifest_name(),
                tool.name
            );
        }
        let access = godot_mcp_server::kernel::AccessContext::new(
            godot_mcp_domain::contracts::ProjectIdentity::from_lease_key(
                godot_mcp_domain::objid::LeaseKey {
                    root: godot_mcp_domain::objid::StableObjectId::Unix { dev: 1, ino: 2 },
                    project_file: godot_mcp_domain::objid::StableObjectId::Unix { dev: 1, ino: 3 },
                },
                1,
            ),
            godot_mcp_domain::contracts::PrincipalId::parse("principal_0123456789abcdef")
                .expect("principal"),
            None,
            0,
        );
        for resource in kernel.resources(&access) {
            assert!(
                !resource.uri.contains("repair") && !resource.name.contains("repair"),
                "profile {} advertises resource {}, which names an operator-repair path",
                profile.manifest_name(),
                resource.uri
            );
        }
    }
}

/// `godot_mcp_cli::repair::run` has exactly one call site in the whole workspace,
/// and it is the `transaction repair` subcommand arm of the shipping binary.
///
/// This is the structural half of the no-network guarantee: there is no second
/// assembly point a transport could reach, so the guarantee cannot be broken by
/// adding a handler that happens to call the same function.
#[test]
fn the_repair_command_has_exactly_one_call_site_and_it_is_the_cli_subcommand() {
    // The scanner must not flag the module that defines the entry point, nor this
    // file, whose string literals name the very symbol it is auditing for.
    let excluded = [
        "crates/godot-mcp-cli/src/repair/mod.rs",
        "crates/godot-mcp-cli/tests/package_and_repair.rs",
    ];
    let mut call_sites = Vec::new();
    for (path, code) in every_source() {
        let relative = relative(&path);
        if excluded.contains(&relative.as_str()) {
            continue;
        }
        let occurrences =
            code.matches("run_repair(").count() + code.matches("repair::run(").count();
        if occurrences > 0 {
            call_sites.push((relative, occurrences));
        }
    }
    assert_eq!(
        call_sites,
        vec![("crates/godot-mcp-cli/src/bin/godot-mcp.rs".to_owned(), 1)],
        "the operator-repair entry point must have exactly one call site"
    );

    let entry =
        std::fs::read_to_string(workspace_root().join("crates/godot-mcp-cli/src/bin/godot-mcp.rs"))
            .expect("the shipping entry point is readable");
    let code = code_only(&entry);

    // The one call lives inside the one function, and that function is reached
    // from exactly one place: the `transaction repair` subcommand arm.
    let declaration = code
        .find("fn transaction_repair(")
        .expect("the call site is inside fn transaction_repair");
    let call = code
        .find("run_repair(")
        .expect("the call site is in the entry point");
    assert!(
        declaration < call,
        "the only call must be inside fn transaction_repair"
    );
    let arm = code
        .find("TransactionCommand::Repair")
        .expect("the transaction repair subcommand arm exists");
    let dispatch: Vec<usize> = code
        .match_indices("transaction_repair(&")
        .map(|(index, _)| index)
        .collect();
    assert_eq!(
        dispatch.len(),
        1,
        "fn transaction_repair must be invoked from exactly one place"
    );
    assert!(
        arm < dispatch[0],
        "fn transaction_repair must be invoked from the transaction repair subcommand arm"
    );
}

/// No transport, handler registry or capability-policy source constructs the
/// operator-recovery port.
///
/// The port requires a *held* Todo 2 advisory OS file lock, which a network peer
/// cannot present; this asserts that no server-side module even names it, so the
/// refusal does not depend on that argument alone.
#[test]
fn no_transport_or_policy_source_constructs_the_operator_port() {
    let mut offenders = Vec::new();
    for (path, code) in every_source() {
        let relative = relative(&path);
        let server_side = relative.starts_with("crates/godot-mcp-server/src/");
        let is_the_port = relative == "crates/godot-mcp-server/src/operator.rs";
        if !server_side || is_the_port {
            continue;
        }
        if code.contains("OperatorSession") || code.contains("operator::OperatorSession") {
            offenders.push(relative);
        }
    }
    assert!(
        offenders.is_empty(),
        "no transport, registry or policy source may construct the operator port: {offenders:?}"
    );
}
