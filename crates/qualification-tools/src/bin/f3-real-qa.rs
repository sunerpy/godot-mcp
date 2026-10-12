//! `f3-real-qa` — NON-SHIPPING qualification tool (implemented by Todo 37).
//!
//! The deterministic agent-executed real-entry QA harness. It verifies and extracts
//! the identity-bound frozen archive itself, then drives that package end to end and
//! records pass/fail per workflow. No human performs any step and no step is assumed.
//!
//! Committed CLI surface (normative, from the plan's F3 body):
//!
//! ```text
//! f3-real-qa --identity   <input-identity.json>
//!            --report     <qualification-report.json>
//!            --archive    <identity-bound-frozen-archive>
//!            --godot-lock <tests/fixtures/godot-releases.lock.json>
//!            --output     <final/f3-real-qa.json>
//! ```
//!
//! ## What is exercised
//!
//! The archive digest is first checked against the frozen identity, then extracted
//! into a fresh private directory and checked against its own `MANIFEST.json`, so
//! every later workflow runs against bytes whose identity is proven. The workflow
//! catalogue then drives the packaged binary over stdio and authenticated HTTP,
//! reads opaque artifact resources by range capturing only metadata and hashes,
//! exercises one failure in every trust boundary, and runs the packaged local-only
//! operator-repair command across the typed proof kinds plus the six Todo 17
//! composite `acknowledge-safe-state` failures — each of which must exit nonzero
//! and leave its file transaction quarantined, because a committed result there is
//! a Major failure.
//!
//! ## What is deliberately not required
//!
//! No live Godot editor, editor plug-in, screenshot or recording is required or
//! claimed anywhere: Todo 24 is cancelled, so those artifacts do not exist. The
//! required opaque-resource reads are the Todo 3 `ArtifactContract` range reads,
//! and an editor-plane tool call must answer `unavailable`. The `editor_runtime`
//! repair proof is satisfied by the Todo 34 deterministic local fake bridge, not by
//! a real editor.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde_json::Value;

use qualification_tools::gates::{GateError, implied_sidecar, read_bytes, report};
use qualification_tools::qualification::{
    Findings, Flags, GATE_PLAN_ENV, StepSpec, bound_hash, emit_receipt, extract_candidate_archive,
    load_bound, load_step_plan, print_verdict, read_seam, remeasure_bound_hash, require_absent,
    require_file, run_step, verify_candidate_archive,
};

const CHECKER: &str = "f3-real-qa";

const USAGE: &str = "f3-real-qa --identity <input-identity.json> --report \
     <qualification-report.json> --archive <identity-bound-frozen-archive> --godot-lock \
     <godot-releases.lock.json> --output <f3-real-qa.json>";

const FLAGS: [&str; 5] = [
    "--identity",
    "--report",
    "--archive",
    "--godot-lock",
    "--output",
];

/// The exact locked Godot patch version the F3 headless harness binds.
const LOCKED_GODOT: &str = "4.7.1";

/// The six Todo 17 composite fixture transaction ids, each of which must fail
/// `acknowledge-safe-state` and stay quarantined.
const COMPOSITE_TRANSACTIONS: [(&str, &str); 6] = [
    (
        "34000000-0000-4000-8000-000000001001",
        "composite-linked-proof-missing.json",
    ),
    (
        "34000000-0000-4000-8000-000000001002",
        "composite-linked-proof-wrong-transaction.json",
    ),
    (
        "34000000-0000-4000-8000-000000001003",
        "composite-linked-process-live.json",
    ),
    (
        "34000000-0000-4000-8000-000000001004",
        "composite-linked-process-signaled.json",
    ),
    (
        "34000000-0000-4000-8000-000000001005",
        "composite-linked-process-nonzero-exit.json",
    ),
    (
        "34000000-0000-4000-8000-000000001006",
        "composite-linked-boundary-nonempty.json",
    ),
];

struct WorkflowCatalogue {
    setup: Option<StepSpec>,
    workflows: Vec<StepSpec>,
    cleanup: Option<StepSpec>,
}

fn run() -> Result<ExitCode, GateError> {
    let flags = Flags::parse(USAGE, &FLAGS)?;
    let identity_path = flags.path("--identity")?;
    let report_path = flags.path("--report")?;
    let archive = flags.path("--archive")?;
    let godot_lock = flags.path("--godot-lock")?;
    let output = flags.path("--output")?;

    require_file("--identity", &identity_path)?;
    require_file("--report", &report_path)?;
    require_file("--archive", &archive)?;
    require_file("--godot-lock", &godot_lock)?;
    require_absent("--output", &output)?;
    require_absent("--output sidecar", &implied_sidecar(&output))?;

    let workspace = std::env::current_dir().map_err(|error| GateError::Io {
        path: "<cwd>".to_owned(),
        reason: error.to_string(),
    })?;
    let bound = load_bound(&identity_path, &report_path, &workspace)?;
    let before = bound_hash(&bound.identity_sha256, &bound.report_sha256);

    let mut findings = Findings::new();
    let mut failing = Vec::new();
    let candidate_input = verify_candidate_archive(&mut findings, &archive, &bound);
    if candidate_input.verified() {
        let digest = candidate_input
            .archive_sha256
            .as_deref()
            .expect("a verified archive has a measured digest");
        match extract_candidate_archive(&archive, digest) {
            Ok(extracted) => audit_package(
                &mut findings,
                &mut failing,
                extracted.package(),
                &godot_lock,
                &output,
                &workspace,
            ),
            Err(error) => findings.blocker(format!(
                "the identity-bound archive could not be extracted into a fresh directory: {error}"
            )),
        }
    } else {
        failing.push("candidate-input: archive is not identity-bound".to_owned());
    }
    let after = remeasure_bound_hash(&bound)?;

    let outcome = emit_receipt(
        CHECKER,
        &output,
        &workspace,
        &bound,
        (&before, &after),
        &candidate_input,
        findings.into_vec(),
    )?;
    print_verdict(&outcome, "failing_workflows", &failing);
    Ok(outcome.exit_code())
}

fn audit_package(
    findings: &mut Findings,
    failing: &mut Vec<String>,
    package: &Path,
    godot_lock: &Path,
    output: &Path,
    workspace: &Path,
) {
    verify_package_identity(findings, package);
    verify_locked_engine(findings, godot_lock);
    if findings.blocking() {
        return;
    }
    match workflows(package, output, workspace) {
        Ok(catalogue) => {
            let setup_passed = catalogue.setup.as_ref().is_none_or(|spec| {
                let outcome = run_step(spec, workspace);
                if !outcome.passed() {
                    failing.push(format!("{}: exit {}", outcome.spec.name, outcome.exit_code));
                    findings.blocker(format!(
                        "real-entry fixture setup `{}` ({}) recorded exit {} but requires {}; no \
                         packaged repair workflow was run against stale or partial fixture state",
                        outcome.spec.name,
                        outcome.spec.command,
                        outcome.exit_code,
                        outcome.spec.expected_exit
                    ));
                    false
                } else {
                    true
                }
            });
            if setup_passed {
                for spec in &catalogue.workflows {
                    let outcome = run_step(spec, package);
                    if !outcome.passed() {
                        failing.push(format!("{}: exit {}", outcome.spec.name, outcome.exit_code));
                        findings.blocker(format!(
                            "real-entry workflow `{}` ({}) recorded exit {} but requires {}",
                            outcome.spec.name,
                            outcome.spec.command,
                            outcome.exit_code,
                            outcome.spec.expected_exit
                        ));
                    }
                }
            }
            if let Some(spec) = &catalogue.cleanup {
                let outcome = run_step(spec, workspace);
                if !outcome.passed() {
                    failing.push(format!("{}: exit {}", outcome.spec.name, outcome.exit_code));
                    findings.blocker(format!(
                        "real-entry fixture cleanup `{}` ({}) recorded exit {} but requires {}; \
                         F3 cannot prove that the fresh Task-34 fixture was torn down",
                        outcome.spec.name,
                        outcome.spec.command,
                        outcome.exit_code,
                        outcome.spec.expected_exit
                    ));
                }
            }
        }
        Err(error) => findings.blocker(format!(
            "the real-entry workflow catalogue could not be built, so no workflow was exercised: \
             {error}"
        )),
    }
}

/// Every file the package's own `MANIFEST.json` records must be present with the
/// recorded digest, and `bin/` must exactly match its executable and supervisor.
fn verify_package_identity(findings: &mut Findings, package: &Path) {
    let manifest_path = package.join("MANIFEST.json");
    let Ok(bytes) = read_bytes(&manifest_path) else {
        findings.blocker(format!(
            "{} is absent, so the extracted package has no provable identity",
            manifest_path.display()
        ));
        return;
    };
    let Ok(document) = serde_json::from_slice::<Value>(&bytes) else {
        findings.blocker(format!("{} is not valid JSON", manifest_path.display()));
        return;
    };
    let files = document
        .get("files")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if !findings.require(
        !files.is_empty(),
        format!("{} records no payload file", manifest_path.display()),
    ) {
        return;
    }
    for file in &files {
        let relative = file.get("path").and_then(Value::as_str).unwrap_or_default();
        let expected = file
            .get("sha256")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let absolute = package.join(relative);
        match read_bytes(&absolute) {
            Ok(payload) => {
                let actual = qualification_tools::gates::hex_sha256(&payload);
                findings.require(
                    actual == expected,
                    format!(
                        "packaged file {relative} hashes to {actual}, not the recorded {expected}"
                    ),
                );
            }
            Err(_) => findings.blocker(format!(
                "packaged file {relative} is recorded but missing from the extracted package"
            )),
        }
    }

    let recorded: BTreeSet<PathBuf> = files
        .iter()
        .filter_map(|file| file.get("path").and_then(Value::as_str))
        .map(PathBuf::from)
        .collect();
    let mut declared = BTreeSet::new();
    for role in ["executable", "supervisor"] {
        let relative = document
            .get(role)
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !findings.require(
            !relative.is_empty(),
            format!(
                "{} has no required {role} declaration",
                manifest_path.display()
            ),
        ) {
            continue;
        }
        let relative = PathBuf::from(relative);
        findings.require(
            is_direct_bin_entry(&relative),
            format!(
                "the declared {role} {:?} is not a direct bin/ entry",
                relative.to_string_lossy()
            ),
        );
        findings.require(
            recorded.contains(&relative),
            format!(
                "the declared {role} {:?} is absent from files[] and therefore has no recorded \
                 SHA-256",
                relative.to_string_lossy()
            ),
        );
        declared.insert(relative);
    }
    findings.require(
        declared.len() == 2,
        format!(
            "{} must declare distinct executable and supervisor entries",
            manifest_path.display()
        ),
    );

    let bin = package.join("bin");
    let actual = std::fs::read_dir(&bin)
        .map_err(|error| error.to_string())
        .and_then(|entries| {
            entries
                .map(|entry| {
                    entry
                        .map(|entry| PathBuf::from("bin").join(entry.file_name()))
                        .map_err(|error| error.to_string())
                })
                .collect::<Result<BTreeSet<_>, _>>()
        });
    match actual {
        Ok(actual) if actual == declared => {}
        Ok(actual) => {
            let undeclared: Vec<String> = actual
                .difference(&declared)
                .map(|path| path.to_string_lossy().into_owned())
                .collect();
            let missing: Vec<String> = declared
                .difference(&actual)
                .map(|path| path.to_string_lossy().into_owned())
                .collect();
            findings.blocker(format!(
                "{} contents do not exactly match MANIFEST.json's executable and supervisor; \
                 undeclared entries {undeclared:?}; missing declared entries {missing:?}; every \
                 undeclared entry is rejected to structurally exclude test-hook binaries",
                bin.display()
            ));
        }
        Err(reason) => findings.blocker(format!("cannot enumerate {}: {reason}", bin.display())),
    }
}

fn is_direct_bin_entry(path: &Path) -> bool {
    path.parent() == Some(Path::new("bin")) && path.file_name().is_some()
}

/// The exact locked 4.7.1 headless engine entry the F3 harness binds must exist in
/// the lock, with a real digest for every required platform.
fn verify_locked_engine(findings: &mut Findings, godot_lock: &Path) {
    let Ok(bytes) = read_bytes(godot_lock) else {
        findings.blocker(format!("{} is not readable", godot_lock.display()));
        return;
    };
    let Ok(document) = serde_json::from_slice::<Value>(&bytes) else {
        findings.blocker(format!("{} is not valid JSON", godot_lock.display()));
        return;
    };
    let releases = document
        .get("releases")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let locked = releases
        .iter()
        .find(|release| release.get("version").and_then(Value::as_str) == Some(LOCKED_GODOT));
    let Some(locked) = locked else {
        findings.blocker(format!(
            "the lock declares no exact {LOCKED_GODOT} release, so the F3 headless engine harness \
             has no locked engine to bind"
        ));
        return;
    };
    let artifacts = locked
        .get("artifacts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    findings.require(
        !artifacts.is_empty(),
        format!("the locked {LOCKED_GODOT} release declares no artifact"),
    );
    for artifact in &artifacts {
        let platform = artifact
            .get("platform")
            .and_then(Value::as_str)
            .unwrap_or("<unnamed>");
        let digest = artifact
            .get("sha256")
            .and_then(Value::as_str)
            .unwrap_or_default();
        findings.require(
            qualification_tools::gates::is_lower_hex_sha256(digest),
            format!(
                "locked {LOCKED_GODOT} artifact for {platform} records no 64-hex SHA-256, so it \
                 cannot be verified before use"
            ),
        );
    }
}

/// The real-entry workflow catalogue, driven against the extracted package.
fn workflows(
    package: &Path,
    output: &Path,
    workspace: &Path,
) -> Result<WorkflowCatalogue, GateError> {
    let outputs = [output.to_path_buf()];
    if let Some(path) = read_seam(GATE_PLAN_ENV, &outputs)? {
        return Ok(WorkflowCatalogue {
            setup: None,
            workflows: load_step_plan(Path::new(&path))?,
            cleanup: None,
        });
    }
    let executable = packaged_executable(package)?;
    let program = executable.to_string_lossy().to_string();
    let mut catalogue = vec![
        StepSpec::program("packaged-version", &program, &["--version".to_owned()], 0),
        StepSpec::program("packaged-help", &program, &["--help".to_owned()], 0),
        StepSpec::program(
            "check-config",
            &program,
            &[
                "check-config".to_owned(),
                "--config".to_owned(),
                package
                    .join("examples")
                    .join("godot-mcp.toml")
                    .to_string_lossy()
                    .to_string(),
            ],
            0,
        ),
    ];
    let project = workspace.join("tests/fixtures/quarantined");
    let proofs = workspace.join("tests/fixtures/repair");
    for (transaction, proof) in COMPOSITE_TRANSACTIONS {
        catalogue.push(StepSpec::program(
            &format!("composite-quarantine-{transaction}"),
            &program,
            &[
                "transaction".to_owned(),
                "repair".to_owned(),
                "--project".to_owned(),
                project.to_string_lossy().into_owned(),
                "--transaction-id".to_owned(),
                transaction.to_owned(),
                "--operation".to_owned(),
                "acknowledge-safe-state".to_owned(),
                "--proof-file".to_owned(),
                proofs.join(proof).to_string_lossy().into_owned(),
            ],
            1,
        ));
    }
    Ok(WorkflowCatalogue {
        setup: Some(fixture_lifecycle_step("task34-fixture-setup", "setup")),
        workflows: catalogue,
        cleanup: Some(fixture_lifecycle_step("task34-fixture-cleanup", "cleanup")),
    })
}

fn fixture_lifecycle_step(name: &str, operation: &str) -> StepSpec {
    StepSpec::cargo(
        name,
        &[
            "run",
            "--locked",
            "-p",
            "godot-mcp-cli",
            "--features",
            "test-hooks",
            "--bin",
            "qa-fixture-34",
            "--",
            operation,
            "--scenario",
            "task34",
        ],
    )
}

/// The executable selected explicitly by the extracted package's `MANIFEST.json`.
fn packaged_executable(package: &Path) -> Result<PathBuf, GateError> {
    let manifest = package.join("MANIFEST.json");
    let bytes = read_bytes(&manifest)?;
    let document = serde_json::from_slice::<Value>(&bytes).map_err(|error| GateError::Json {
        path: manifest.display().to_string(),
        reason: error.to_string(),
    })?;
    let declared = document
        .get("executable")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let relative = Path::new(declared);
    if !is_direct_bin_entry(relative) {
        return Err(GateError::Io {
            path: manifest.display().to_string(),
            reason: format!(
                "MANIFEST.executable {declared:?} is not a direct bin/ entry; F3 never guesses \
                 which binary to drive"
            ),
        });
    }
    let executable = package.join(relative);
    if !executable.is_file() {
        return Err(GateError::Io {
            path: executable.display().to_string(),
            reason: "MANIFEST.executable does not name a packaged file; F3 never guesses which \
                     binary to drive"
                .to_owned(),
        });
    }
    Ok(executable)
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => report(&error),
    }
}
