//! `produce-qualification` — NON-SHIPPING qualification tool (implemented by Todo 37).
//!
//! The sole writer of the final release-gate pair and the five qualification
//! paths. It never writes an F1–F5 output and never changes the candidate.
//!
//! Committed CLI surface (from the plan's Todo 37 producer command, normative):
//!
//! ```text
//! produce-qualification
//!   --candidate-worktree <dir>   --candidate-identity <source-identity.json>
//!   --plan <plan.md>             --cargo-lock <Cargo.lock>
//!   --rust-toolchain <rust-toolchain.toml>
//!   --tooling-lock <tooling.lock.json>
//!   --godot-lock <godot-releases.lock.json>
//!   --archives-dir <dir>
//!   --architecture-momus-receipt <json> --architecture-momus-receipt-sha <sha256>
//!   --architecture-codex-receipt <json> --architecture-codex-receipt-sha <sha256>
//!   --b3-approval <json> --b3-approval-sha <sha256>
//!   --b4-approval <json> --b4-approval-sha <sha256>
//!   --red-evidence-index <json>
//!   --release-gates <json> --release-gates-sha <sha256>
//!   --review-bindings <json>
//!   --input-identity <json> --input-identity-sha <sha256>
//!   --qualification-report <json> --qualification-report-sha <sha256>
//! ```
//!
//! The first seventeen paths are read-only inputs. The last seven are **outputs**
//! and must be absent, as must every F1–F4 receipt/sidecar and
//! `final-manifest.json` in the sibling `final/` directory.
//!
//! ## Strict forward stage order
//!
//! 0. Invoke Todo 35's frozen harness with `QUALIFYING=1`, into private staging,
//!    then validate the staged gates document and its sidecar.
//! 1. Create and validate `review-bindings.json` from independently supplied
//!    current architecture/B3/B4 approvals.
//! 2. Create and validate `input-identity.json`, binding the final-gates hash, the
//!    review-bindings content hash, and every plan/source/tree/archive/fixture/
//!    Red/harness/evidence-schema/toolchain/tooling/runner/workflow identity. It
//!    excludes the report, the receipts and the final manifest.
//! 3. Write and validate `input-identity.sha256` over exactly the validated
//!    identity bytes.
//! 4. Run the qualification evidence catalogue and create/validate
//!    `qualification-report.json`, binding exactly the identity sidecar value plus
//!    that new evidence.
//! 5. Write and validate `qualification-report.sha256` over exactly the validated
//!    report bytes.
//!
//! No stage reads a later output, no artifact hashes itself or a later output, and
//! nothing is published until every stage has succeeded. On any failure the
//! staging tree is removed and all seven final output paths remain absent.
//!
//! ## Atomic, journaled publication
//!
//! Publication happens under one producer lock. A journal listing the seven target
//! paths is fsynced before the first rename and removed after the last, so an
//! interrupted publication is detected on the next startup: the listed targets are
//! removed, the journal is removed, and the run is rejected rather than continuing
//! from a partial state. Any rename failure rolls back to all-seven-absent.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde_json::{Map, Value};

use qualification_tools::gates::{
    GateError, PlanDocument, canonical_bytes, hex_sha256, read_bytes, report,
    validate_against_schema,
};
use qualification_tools::qualification::{
    ArchitectureReviewReceipt, BINDINGS_SCHEMA, BoundInput, CandidateIdentity, EVIDENCE_PLAN_ENV,
    FAULT_ENV, Flags, HARNESS_BINARY_ENV, IDENTITY_SCHEMA, PRODUCER_NAME, Provenance,
    QUALIFICATION_SCHEMA_VERSION, REPORT_SCHEMA, REQUIRED_BOUND_INPUTS, RedEvidenceIndex,
    StepOutcome, StepSpec, bound_inputs, cargo_program, echo_child_output, fault, load_step_plan,
    normalize, pinned_channel, read_seam, require_absent, require_dir, require_file, resolve,
    run_step, sidecar_digest, verified_archives, verify_batch_approval, verify_release_gates,
};

const USAGE: &str = "produce-qualification \
     --candidate-worktree <dir> --candidate-identity <json> --plan <md> --cargo-lock <lock> \
     --rust-toolchain <toml> --tooling-lock <json> --godot-lock <json> --archives-dir <dir> \
     --architecture-momus-receipt <json> --architecture-momus-receipt-sha <sha256> \
     --architecture-codex-receipt <json> --architecture-codex-receipt-sha <sha256> \
     --b3-approval <json> --b3-approval-sha <sha256> --b4-approval <json> --b4-approval-sha \
     <sha256> --red-evidence-index <json> --release-gates <json> --release-gates-sha <sha256> \
     --review-bindings <json> --input-identity <json> --input-identity-sha <sha256> \
     --qualification-report <json> --qualification-report-sha <sha256>";

const FLAGS: [&str; 24] = [
    "--candidate-worktree",
    "--candidate-identity",
    "--plan",
    "--cargo-lock",
    "--rust-toolchain",
    "--tooling-lock",
    "--godot-lock",
    "--archives-dir",
    "--architecture-momus-receipt",
    "--architecture-momus-receipt-sha",
    "--architecture-codex-receipt",
    "--architecture-codex-receipt-sha",
    "--b3-approval",
    "--b3-approval-sha",
    "--b4-approval",
    "--b4-approval-sha",
    "--red-evidence-index",
    "--release-gates",
    "--release-gates-sha",
    "--review-bindings",
    "--input-identity",
    "--input-identity-sha",
    "--qualification-report",
    "--qualification-report-sha",
];

/// The canonical relative gates paths the frozen harness pins for a qualifying
/// emission. The producer runs the harness with its working directory set to
/// private staging, so these resolve inside staging: the harness's own
/// canonical-path rule is satisfied while nothing is written into the evidence
/// tree before every stage has succeeded.
const HARNESS_OUTPUT: &str =
    ".omo/evidence/godot-mcp-server-architecture/external/release-gates.json";
const HARNESS_OUTPUT_SHA: &str =
    ".omo/evidence/godot-mcp-server-architecture/external/release-gates.sha256";

const PRODUCER_CONTEXT_ENV: &str = "RELEASE_GATES_PRODUCER_CONTEXT";
const PRODUCER_CONTEXT_DOMAIN: &str = "godot-mcp release-gates producer context v1";

const F_RECEIPTS: [&str; 4] = [
    "f1-plan-compliance",
    "f2-code-quality",
    "f3-real-qa",
    "f4-scope-fidelity",
];

const JOURNAL_NAME: &str = ".produce-qualification.journal.json";
const LOCK_NAME: &str = ".produce-qualification.lock";

struct Arguments {
    candidate_worktree: PathBuf,
    candidate_identity: PathBuf,
    plan: PathBuf,
    cargo_lock: PathBuf,
    rust_toolchain: PathBuf,
    tooling_lock: PathBuf,
    godot_lock: PathBuf,
    archives_dir: PathBuf,
    momus_receipt: PathBuf,
    momus_receipt_sha: PathBuf,
    codex_receipt: PathBuf,
    codex_receipt_sha: PathBuf,
    b3_approval: PathBuf,
    b3_approval_sha: PathBuf,
    b4_approval: PathBuf,
    b4_approval_sha: PathBuf,
    red_evidence_index: PathBuf,
    release_gates: PathBuf,
    release_gates_sha: PathBuf,
    review_bindings: PathBuf,
    input_identity: PathBuf,
    input_identity_sha: PathBuf,
    qualification_report: PathBuf,
    qualification_report_sha: PathBuf,
}

impl Arguments {
    fn parse() -> Result<Self, GateError> {
        let flags = Flags::parse(USAGE, &FLAGS)?;
        Ok(Self {
            candidate_worktree: flags.path("--candidate-worktree")?,
            candidate_identity: flags.path("--candidate-identity")?,
            plan: flags.path("--plan")?,
            cargo_lock: flags.path("--cargo-lock")?,
            rust_toolchain: flags.path("--rust-toolchain")?,
            tooling_lock: flags.path("--tooling-lock")?,
            godot_lock: flags.path("--godot-lock")?,
            archives_dir: flags.path("--archives-dir")?,
            momus_receipt: flags.path("--architecture-momus-receipt")?,
            momus_receipt_sha: flags.path("--architecture-momus-receipt-sha")?,
            codex_receipt: flags.path("--architecture-codex-receipt")?,
            codex_receipt_sha: flags.path("--architecture-codex-receipt-sha")?,
            b3_approval: flags.path("--b3-approval")?,
            b3_approval_sha: flags.path("--b3-approval-sha")?,
            b4_approval: flags.path("--b4-approval")?,
            b4_approval_sha: flags.path("--b4-approval-sha")?,
            red_evidence_index: flags.path("--red-evidence-index")?,
            release_gates: flags.path("--release-gates")?,
            release_gates_sha: flags.path("--release-gates-sha")?,
            review_bindings: flags.path("--review-bindings")?,
            input_identity: flags.path("--input-identity")?,
            input_identity_sha: flags.path("--input-identity-sha")?,
            qualification_report: flags.path("--qualification-report")?,
            qualification_report_sha: flags.path("--qualification-report-sha")?,
        })
    }

    fn outputs(&self) -> Vec<PathBuf> {
        vec![
            self.release_gates.clone(),
            self.release_gates_sha.clone(),
            self.review_bindings.clone(),
            self.input_identity.clone(),
            self.input_identity_sha.clone(),
            self.qualification_report.clone(),
            self.qualification_report_sha.clone(),
        ]
    }

    fn anchor(&self) -> PathBuf {
        self.input_identity
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
    }

    fn final_dir(&self) -> PathBuf {
        self.input_identity
            .parent()
            .and_then(Path::parent)
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .join("final")
    }
}

struct Staging {
    root: PathBuf,
    lock: PathBuf,
    journal: PathBuf,
}

impl Staging {
    fn acquire(arguments: &Arguments) -> Result<Self, GateError> {
        let anchor = arguments.anchor();
        std::fs::create_dir_all(&anchor).map_err(|error| io(&anchor, &error))?;
        let lock = anchor.join(LOCK_NAME);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        match options.open(&lock) {
            Ok(file) => file.sync_all().map_err(|error| io(&lock, &error))?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(GateError::Io {
                    path: lock.display().to_string(),
                    reason: "another produce-qualification run holds the producer lock; \
                             publication is serialized so two producers can never interleave \
                             renames"
                        .to_owned(),
                });
            }
            Err(error) => return Err(io(&lock, &error)),
        }
        let root = anchor.join(format!(
            ".produce-qualification.{}.staging",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).map_err(|error| io(&root, &error))?;
        Ok(Self {
            root,
            lock,
            journal: anchor.join(JOURNAL_NAME),
        })
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn release(&self) {
        let _ = std::fs::remove_dir_all(&self.root);
        let _ = std::fs::remove_file(&self.lock);
    }
}

/// Everything the ordered stages share, so no stage signature grows unbounded.
#[derive(Clone, Copy)]
struct Context<'a> {
    arguments: &'a Arguments,
    staging: &'a Staging,
    workspace: &'a Path,
    candidate: &'a CandidateIdentity,
    plan: &'a PlanDocument,
    provenance: &'a Provenance,
    armed: Option<&'a str>,
}

fn io(path: &Path, error: &std::io::Error) -> GateError {
    GateError::Io {
        path: path.display().to_string(),
        reason: error.to_string(),
    }
}

/// Detect, roll back and reject an interrupted publication before anything runs.
fn reject_interrupted_publication(arguments: &Arguments) -> Result<(), GateError> {
    let journal = arguments.anchor().join(JOURNAL_NAME);
    if !journal.exists() {
        return Ok(());
    }
    let mut removed = Vec::new();
    if let Ok(bytes) = std::fs::read(&journal)
        && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
        && let Some(targets) = value.get("targets").and_then(Value::as_array)
    {
        for target in targets.iter().filter_map(Value::as_str) {
            let path = Path::new(target);
            if path.exists() {
                let _ = std::fs::remove_file(path);
                removed.push(target.to_owned());
            }
        }
    }
    let _ = std::fs::remove_file(&journal);
    Err(GateError::Io {
        path: journal.display().to_string(),
        reason: format!(
            "an interrupted publication was detected and rolled back ({} partial output(s) \
             removed: {removed:?}). All seven output paths are absent again; rerun the producer \
             against a freshly frozen candidate",
            removed.len()
        ),
    })
}

fn preflight(arguments: &Arguments) -> Result<(), GateError> {
    require_dir("--candidate-worktree", &arguments.candidate_worktree)?;
    require_dir("--archives-dir", &arguments.archives_dir)?;
    for (label, path) in [
        ("--candidate-identity", &arguments.candidate_identity),
        ("--plan", &arguments.plan),
        ("--cargo-lock", &arguments.cargo_lock),
        ("--rust-toolchain", &arguments.rust_toolchain),
        ("--tooling-lock", &arguments.tooling_lock),
        ("--godot-lock", &arguments.godot_lock),
        ("--architecture-momus-receipt", &arguments.momus_receipt),
        (
            "--architecture-momus-receipt-sha",
            &arguments.momus_receipt_sha,
        ),
        ("--architecture-codex-receipt", &arguments.codex_receipt),
        (
            "--architecture-codex-receipt-sha",
            &arguments.codex_receipt_sha,
        ),
        ("--b3-approval", &arguments.b3_approval),
        ("--b3-approval-sha", &arguments.b3_approval_sha),
        ("--b4-approval", &arguments.b4_approval),
        ("--b4-approval-sha", &arguments.b4_approval_sha),
        ("--red-evidence-index", &arguments.red_evidence_index),
    ] {
        require_file(label, path)?;
    }
    for (label, path) in [
        ("--release-gates", &arguments.release_gates),
        ("--release-gates-sha", &arguments.release_gates_sha),
        ("--review-bindings", &arguments.review_bindings),
        ("--input-identity", &arguments.input_identity),
        ("--input-identity-sha", &arguments.input_identity_sha),
        ("--qualification-report", &arguments.qualification_report),
        (
            "--qualification-report-sha",
            &arguments.qualification_report_sha,
        ),
    ] {
        require_absent(label, path)?;
    }
    let final_dir = arguments.final_dir();
    for checker in F_RECEIPTS {
        require_absent(
            &format!("{checker} receipt"),
            &final_dir.join(format!("{checker}.json")),
        )?;
        require_absent(
            &format!("{checker} sidecar"),
            &final_dir.join(format!("{checker}.sha256")),
        )?;
    }
    require_absent(
        "final-manifest.json",
        &final_dir.join("final-manifest.json"),
    )?;
    Ok(())
}

/// The producer-context token the frozen harness requires for `QUALIFYING=1`.
///
/// It is derived from the frozen candidate identity, so it cannot be a literal
/// copied before the freeze.
fn producer_context_token(commit: &str, tree: &str) -> String {
    hex_sha256(format!("{PRODUCER_CONTEXT_DOMAIN}\n{commit}\n{tree}\n").as_bytes())
}

fn harness_command(workspace: &Path) -> std::process::Command {
    match std::env::var(HARNESS_BINARY_ENV) {
        Ok(binary) if !binary.trim().is_empty() => {
            let mut command = std::process::Command::new(binary.trim());
            command.arg("release-gates");
            command
        }
        _ => {
            let mut command = std::process::Command::new(cargo_program());
            command.args(["run", "--locked", "--quiet", "--manifest-path"]);
            command.arg(workspace.join("Cargo.toml"));
            command.args([
                "-p",
                "godot-mcp-cli",
                "--features",
                "test-hooks",
                "--bin",
                "release-package",
                "--",
                "release-gates",
            ]);
            command
        }
    }
}

struct StagedGates {
    document_path: PathBuf,
    sidecar_path: PathBuf,
    digest: String,
    document: Value,
}

fn stage_release_gates(context: &Context<'_>) -> Result<StagedGates, GateError> {
    let Context {
        arguments,
        staging,
        candidate,
        workspace,
        armed,
        ..
    } = *context;
    fault("stage0-before", armed)?;
    let document_path = staging.root.join(HARNESS_OUTPUT);
    let sidecar_path = staging.root.join(HARNESS_OUTPUT_SHA);
    let parent = document_path
        .parent()
        .unwrap_or(&staging.root)
        .to_path_buf();
    std::fs::create_dir_all(&parent).map_err(|error| io(&parent, &error))?;

    let output = harness_command(workspace)
        .arg("--candidate-worktree")
        .arg(absolute(&arguments.candidate_worktree)?)
        .arg("--source-commit")
        .arg(&candidate.commit)
        .arg("--source-tree")
        .arg(&candidate.tree)
        .arg("--archives-dir")
        .arg(absolute(&arguments.archives_dir)?)
        .arg("--output")
        .arg(HARNESS_OUTPUT)
        .arg("--output-sha")
        .arg(HARNESS_OUTPUT_SHA)
        .arg("--qualifying")
        .arg("1")
        .env(
            PRODUCER_CONTEXT_ENV,
            producer_context_token(&candidate.commit, &candidate.tree),
        )
        .current_dir(&staging.root)
        .output()
        .map_err(|error| GateError::Io {
            path: "release-gates harness".to_owned(),
            reason: format!("the frozen Todo 35 harness could not be invoked: {error}"),
        })?;
    echo_child_output("release-gates-harness", &output);
    if !output.status.success() {
        return Err(GateError::Io {
            path: "release-gates harness".to_owned(),
            reason: format!(
                "the frozen Todo 35 harness exited {}; no release-gate evidence was produced, so \
                 qualification stops before any later stage",
                output
                    .status
                    .code()
                    .map_or_else(|| "by signal".to_owned(), |code| code.to_string())
            ),
        });
    }
    fault("stage0-within", armed)?;
    let (digest, document) = verify_release_gates(
        &document_path,
        &sidecar_path,
        workspace,
        &candidate.commit,
        &candidate.tree,
        true,
    )?;
    Ok(StagedGates {
        document_path,
        sidecar_path,
        digest,
        document,
    })
}

struct StagedBindings {
    path: PathBuf,
    digest: String,
    document: Value,
}

fn stage_review_bindings(context: &Context<'_>) -> Result<StagedBindings, GateError> {
    let Context {
        arguments,
        staging,
        workspace,
        plan,
        provenance,
        armed,
        ..
    } = *context;
    fault("stage1-before", armed)?;
    let (momus_hash, momus) = ArchitectureReviewReceipt::verify(
        &arguments.momus_receipt,
        &arguments.momus_receipt_sha,
        plan.sha256(),
    )?;
    let (codex_hash, codex) = ArchitectureReviewReceipt::verify(
        &arguments.codex_receipt,
        &arguments.codex_receipt_sha,
        plan.sha256(),
    )?;
    if momus.round_id != codex.round_id {
        return Err(GateError::ReceiptRejected {
            path: arguments.codex_receipt.display().to_string(),
            reason: format!(
                "the two architecture receipts belong to different rounds ({} and {}), so they do \
                 not form one current dual review",
                momus.round_id, codex.round_id
            ),
        });
    }
    if momus.reviewer_identity == codex.reviewer_identity || momus_hash == codex_hash {
        return Err(GateError::ReceiptRejected {
            path: arguments.codex_receipt.display().to_string(),
            reason: "the two architecture receipts are not independent: one reviewer identity or \
                     one receipt was reused for both"
                .to_owned(),
        });
    }
    let b3 = verify_batch_approval(
        &arguments.b3_approval,
        &arguments.b3_approval_sha,
        workspace,
        "b3",
        plan.sha256(),
    )?;
    let b4 = verify_batch_approval(
        &arguments.b4_approval,
        &arguments.b4_approval_sha,
        workspace,
        "b4",
        plan.sha256(),
    )?;
    if b3.bundle_sha256 == b4.bundle_sha256 {
        return Err(GateError::ReceiptRejected {
            path: arguments.b4_approval.display().to_string(),
            reason: "the B3 and B4 approvals bind the same review bundle, so one gate's approval \
                     was reused for the other"
                .to_owned(),
        });
    }

    let mut batch_reviews = Vec::new();
    for manifest in [&b3, &b4] {
        let hashes: Vec<String> = manifest
            .receipts
            .iter()
            .map(|receipt| receipt.receipt_sha256.clone())
            .collect();
        batch_reviews.push(serde_json::json!({
            "batch": manifest.batch,
            "batch_sha256": manifest.bundle_sha256,
            "receipt_hashes": hashes,
            "round_id": hex_sha256(
                format!("{}\n{}\n", manifest.batch, manifest.bundle_sha256).as_bytes(),
            ),
        }));
    }
    let document = serde_json::json!({
        "architecture_review": {
            "codex_receipt_hash": codex_hash,
            "momus_receipt_hash": momus_hash,
            "plan_sha256": plan.sha256(),
            "round_id": momus.round_id,
        },
        "batch_reviews": batch_reviews,
        "producer": provenance.to_value()?,
        "schema_version": QUALIFICATION_SCHEMA_VERSION,
    });
    validate_against_schema(
        &arguments.review_bindings.display().to_string(),
        &workspace.join(BINDINGS_SCHEMA),
        &document,
    )?;
    let bytes = canonical_bytes(&document)?;
    let path = staging.path("review-bindings.json");
    fault("stage1-within", armed)?;
    write_bytes(&path, &bytes)?;
    Ok(StagedBindings {
        path,
        digest: hex_sha256(&bytes),
        document,
    })
}

struct StagedIdentity {
    path: PathBuf,
    bytes: Vec<u8>,
    document: Value,
}

fn stage_input_identity(
    context: &Context<'_>,
    gates: &StagedGates,
    bindings: &StagedBindings,
) -> Result<StagedIdentity, GateError> {
    let Context {
        arguments,
        staging,
        workspace,
        plan,
        candidate,
        provenance,
        armed,
    } = *context;
    fault("stage2-before", armed)?;
    RedEvidenceIndex::verify(&arguments.red_evidence_index, workspace)?;
    let channel = pinned_channel(&arguments.rust_toolchain)?;
    let archives = verified_archives(&arguments.archives_dir)?;

    let mut bound: BTreeMap<String, BoundInput> = BTreeMap::new();
    for (key, path) in [
        ("plan", &arguments.plan),
        ("cargo-lock", &arguments.cargo_lock),
        ("rust-toolchain", &arguments.rust_toolchain),
        ("tooling-lock", &arguments.tooling_lock),
        ("godot-lock", &arguments.godot_lock),
        ("candidate-identity", &arguments.candidate_identity),
        ("red-evidence-index", &arguments.red_evidence_index),
        ("architecture-momus-receipt", &arguments.momus_receipt),
        ("architecture-codex-receipt", &arguments.codex_receipt),
        ("b3-approval", &arguments.b3_approval),
        ("b4-approval", &arguments.b4_approval),
    ] {
        bound.insert(
            key.to_owned(),
            BoundInput {
                path: normalize(path),
                sha256: hex_sha256(&read_bytes(path)?),
            },
        );
    }
    let checksums = arguments.archives_dir.join("SHA256SUMS.txt");
    bound.insert(
        "archive-checksums".to_owned(),
        BoundInput {
            path: normalize(&checksums),
            sha256: hex_sha256(&read_bytes(&checksums)?),
        },
    );
    for (name, digest) in &archives {
        bound.insert(
            format!("archive:{name}"),
            BoundInput {
                path: normalize(&arguments.archives_dir.join(name)),
                sha256: digest.clone(),
            },
        );
    }
    for relative in canonical_schema_files(workspace)? {
        bound.insert(
            format!("schema:{}", normalize(&relative)),
            BoundInput {
                path: normalize(&relative),
                sha256: hex_sha256(&read_bytes(&workspace.join(&relative))?),
            },
        );
    }
    // Stage 0's gates and Stage 1's bindings are bound *by* the identity that
    // follows them, never the other way round: this is the only direction the
    // acyclic hash graph allows.
    bound.insert(
        "release-gates".to_owned(),
        BoundInput {
            path: normalize(&arguments.release_gates),
            sha256: gates.digest.clone(),
        },
    );
    bound.insert(
        "review-bindings".to_owned(),
        BoundInput {
            path: normalize(&arguments.review_bindings),
            sha256: bindings.digest.clone(),
        },
    );
    for key in REQUIRED_BOUND_INPUTS {
        if !bound.contains_key(key) {
            return Err(GateError::Schema {
                path: arguments.input_identity.display().to_string(),
                reason: format!("the frozen identity is missing required bound input `{key}`"),
            });
        }
    }

    let runner_identity = gates
        .document
        .get("runner_identity")
        .cloned()
        .ok_or_else(|| GateError::Schema {
            path: arguments.release_gates.display().to_string(),
            reason: "the release-gate evidence records no runner identity for the producer to bind"
                .to_owned(),
        })?;
    let architecture_review = bindings
        .document
        .get("architecture_review")
        .cloned()
        .ok_or_else(|| GateError::Schema {
            path: arguments.review_bindings.display().to_string(),
            reason: "the review bindings record no architecture review".to_owned(),
        })?;
    if architecture_review
        .get("plan_sha256")
        .and_then(Value::as_str)
        != Some(plan.sha256())
    {
        return Err(GateError::StalePlanSha {
            plan: plan.path().display().to_string(),
            recorded: architecture_review
                .get("plan_sha256")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            actual: plan.sha256().to_owned(),
        });
    }

    let document = serde_json::json!({
        "architecture_review": architecture_review,
        "bound_inputs": bound,
        "candidate_commit": candidate.commit,
        "candidate_tree": candidate.tree,
        "producer": provenance.to_value()?,
        "release_gates_sha256": gates.digest,
        "runner_identity": runner_identity,
        "rust_toolchain": {
            "channel": channel,
            "content_hash": hex_sha256(&read_bytes(&arguments.rust_toolchain)?),
        },
        "schema_version": QUALIFICATION_SCHEMA_VERSION,
        "tooling_lock_sha256": hex_sha256(&read_bytes(&arguments.tooling_lock)?),
    });
    validate_against_schema(
        &arguments.input_identity.display().to_string(),
        &workspace.join(IDENTITY_SCHEMA),
        &document,
    )?;
    let bytes = canonical_bytes(&document)?;
    let path = staging.path("input-identity.json");
    fault("stage2-within", armed)?;
    write_bytes(&path, &bytes)?;
    Ok(StagedIdentity {
        path,
        bytes,
        document,
    })
}

/// Every canonical manifest and evidence schema, as workspace-relative paths.
fn canonical_schema_files(workspace: &Path) -> Result<Vec<PathBuf>, GateError> {
    let mut files = Vec::new();
    collect_json(&workspace.join("schemas"), workspace, &mut files)?;
    files.sort();
    if files.is_empty() {
        return Err(GateError::Io {
            path: workspace.join("schemas").display().to_string(),
            reason: "no canonical manifest or evidence schema was found to bind".to_owned(),
        });
    }
    Ok(files)
}

fn collect_json(
    directory: &Path,
    workspace: &Path,
    out: &mut Vec<PathBuf>,
) -> Result<(), GateError> {
    let entries = std::fs::read_dir(directory).map_err(|error| io(directory, &error))?;
    let mut children: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    children.sort();
    for child in children {
        if child.is_dir() {
            collect_json(&child, workspace, out)?;
        } else if child
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            out.push(
                child
                    .strip_prefix(workspace)
                    .unwrap_or(&child)
                    .to_path_buf(),
            );
        }
    }
    Ok(())
}

fn qualification_evidence(
    arguments: &Arguments,
    workspace: &Path,
    identity: &Value,
    outputs: &[PathBuf],
) -> Result<Vec<StepOutcome>, GateError> {
    let catalogue = match read_seam(EVIDENCE_PLAN_ENV, outputs)? {
        Some(path) => load_step_plan(Path::new(&path))?,
        None => default_evidence_catalogue(),
    };
    let mut outcomes: Vec<StepOutcome> = catalogue
        .iter()
        .map(|spec| run_step(spec, workspace))
        .collect();
    outcomes.push(identity_mutation_probe(arguments, workspace, identity)?);
    Ok(outcomes)
}

/// The committed qualification-evidence catalogue: stdio/authenticated-HTTP
/// parity, profile/transport E2E, and every named trust-boundary, durable
/// transaction, operator-repair, recovery, auth, queue/cancellation and artifact
/// scenario.
fn default_evidence_catalogue() -> Vec<StepSpec> {
    const SUITES: [(&str, &str, &str); 11] = [
        (
            "parity-stdio-http",
            "godot-mcp-server",
            "kernel_rmcp_parity",
        ),
        (
            "profile-transport-e2e",
            "godot-mcp-server",
            "http_transport",
        ),
        (
            "artifact-parity",
            "godot-mcp-server",
            "http_session_binding",
        ),
        ("trust-boundary", "godot-mcp-server", "capability_policy"),
        (
            "durable-transaction",
            "godot-mcp-server",
            "journal_foundation",
        ),
        (
            "queue-cancellation",
            "godot-mcp-server",
            "scheduler_idempotency",
        ),
        ("auth", "godot-mcp-server", "oidc_resource_server"),
        ("artifact-outcomes", "godot-mcp-server", "outcome_router"),
        ("audit-limits", "godot-mcp-server", "audit_limits"),
        ("operator-repair", "godot-mcp-cli", "package_and_repair"),
        ("recovery", "godot-mcp-engine", "process_launch_protocol"),
    ];
    SUITES
        .iter()
        .map(|(name, package, suite)| {
            StepSpec::cargo(
                name,
                &[
                    "test",
                    "--locked",
                    "-p",
                    package,
                    "--all-features",
                    "--test",
                    suite,
                ],
            )
        })
        .collect()
}

/// Recompute every bound input's digest and prove none of them drifted.
///
/// This is the plan's "mutating any bound identity field invalidates the report"
/// evidence, computed rather than asserted. The gates pair and the review bindings
/// are excluded because they are still staged under their staging names: their
/// digests were measured from the exact bytes this identity binds, one stage
/// earlier, and re-reading them from an unpublished final path would prove nothing.
fn identity_mutation_probe(
    arguments: &Arguments,
    workspace: &Path,
    identity: &Value,
) -> Result<StepOutcome, GateError> {
    let bound = bound_inputs(identity)?;
    let mut drifted = Vec::new();
    for (key, input) in &bound {
        if key == "release-gates" || key == "review-bindings" {
            continue;
        }
        let path = resolve(workspace, Path::new(&input.path));
        match read_bytes(&path) {
            Ok(bytes) if hex_sha256(&bytes) == input.sha256 => {}
            _ => drifted.push(key.clone()),
        }
    }
    Ok(StepOutcome {
        spec: StepSpec {
            name: "identity-mutation".to_owned(),
            command: format!(
                "produce-qualification: recompute every bound input digest for {}",
                normalize(&arguments.input_identity)
            ),
            program: PRODUCER_NAME.to_owned(),
            args: Vec::new(),
            env: BTreeMap::new(),
            expected_exit: 0,
        },
        exit_code: i64::from(!drifted.is_empty()),
    })
}

fn write_bytes(path: &Path, bytes: &[u8]) -> Result<(), GateError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|error| io(parent, &error))?;
    }
    std::fs::write(path, bytes).map_err(|error| io(path, &error))
}

fn write_sidecar(path: &Path, digest: &str) -> Result<(), GateError> {
    write_bytes(path, format!("{digest}\n").as_bytes())
}

fn absolute(path: &Path) -> Result<PathBuf, GateError> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    let cwd = std::env::current_dir().map_err(|error| GateError::Io {
        path: "<cwd>".to_owned(),
        reason: error.to_string(),
    })?;
    Ok(cwd.join(path))
}

fn publish(
    staging: &Staging,
    pairs: &[(PathBuf, PathBuf)],
    armed: Option<&str>,
) -> Result<(), GateError> {
    let targets: Vec<String> = pairs.iter().map(|(_, target)| normalize(target)).collect();
    let journal = serde_json::json!({
        "process_id": std::process::id(),
        "producer": PRODUCER_NAME,
        "targets": targets,
    });
    write_bytes(&staging.journal, &canonical_bytes(&journal)?)?;
    if let Ok(file) = std::fs::File::open(&staging.journal) {
        let _ = file.sync_all();
    }

    let mut published: Vec<PathBuf> = Vec::new();
    let mut result = fault("publish-0", armed);
    if result.is_ok() {
        for (index, (staged, target)) in pairs.iter().enumerate() {
            if let Some(parent) = target.parent()
                && !parent.as_os_str().is_empty()
                && let Err(error) = std::fs::create_dir_all(parent)
            {
                result = Err(io(parent, &error));
                break;
            }
            if let Err(error) = std::fs::rename(staged, target) {
                result = Err(io(target, &error));
                break;
            }
            published.push(target.clone());
            result = fault(&format!("publish-{}", index + 1), armed);
            if result.is_err() {
                break;
            }
        }
    }
    if result.is_err() {
        for target in &published {
            let _ = std::fs::remove_file(target);
        }
    }
    let _ = std::fs::remove_file(&staging.journal);
    result
}

fn produce(context: &Context<'_>) -> Result<String, GateError> {
    let Context {
        arguments,
        staging,
        workspace,
        provenance,
        armed,
        ..
    } = *context;
    let outputs = arguments.outputs();

    let gates = stage_release_gates(context)?;
    let bindings = stage_review_bindings(context)?;
    let identity = stage_input_identity(context, &gates, &bindings)?;

    fault("stage3-before", armed)?;
    let identity_digest = hex_sha256(&identity.bytes);
    let identity_sidecar = staging.path("input-identity.sha256");
    write_sidecar(&identity_sidecar, &identity_digest)?;
    if sidecar_digest(&identity_sidecar)? != identity_digest {
        return Err(GateError::SidecarMismatch {
            artifact: identity.path.display().to_string(),
            sidecar: identity_sidecar.display().to_string(),
        });
    }
    fault("stage3-within", armed)?;

    fault("stage4-before", armed)?;
    let evidence = qualification_evidence(arguments, workspace, &identity.document, &outputs)?;
    let failing: Vec<String> = evidence
        .iter()
        .filter(|outcome| !outcome.passed())
        .map(|outcome| format!("{} exited {}", outcome.spec.name, outcome.exit_code))
        .collect();
    if !failing.is_empty() {
        return Err(GateError::ReceiptRejected {
            path: arguments.qualification_report.display().to_string(),
            reason: format!(
                "{} qualification evidence step(s) did not pass, so no qualification report is \
                 published: {failing:?}",
                failing.len()
            ),
        });
    }
    let report_document = serde_json::json!({
        "gates": evidence
            .iter()
            .map(StepOutcome::to_gate_value)
            .collect::<Vec<_>>(),
        "input_identity_sha256": identity_digest,
        "producer": provenance.to_value()?,
        "result": "PASS",
        "schema_version": QUALIFICATION_SCHEMA_VERSION,
    });
    validate_against_schema(
        &arguments.qualification_report.display().to_string(),
        &workspace.join(REPORT_SCHEMA),
        &report_document,
    )?;
    let report_bytes = canonical_bytes(&report_document)?;
    let report_path = staging.path("qualification-report.json");
    write_bytes(&report_path, &report_bytes)?;
    fault("stage4-within", armed)?;

    fault("stage5-before", armed)?;
    let report_digest = hex_sha256(&report_bytes);
    let report_sidecar = staging.path("qualification-report.sha256");
    write_sidecar(&report_sidecar, &report_digest)?;
    if sidecar_digest(&report_sidecar)? != report_digest {
        return Err(GateError::SidecarMismatch {
            artifact: report_path.display().to_string(),
            sidecar: report_sidecar.display().to_string(),
        });
    }
    fault("stage5-within", armed)?;

    publish(
        staging,
        &[
            (gates.document_path, arguments.release_gates.clone()),
            (gates.sidecar_path, arguments.release_gates_sha.clone()),
            (bindings.path, arguments.review_bindings.clone()),
            (identity.path, arguments.input_identity.clone()),
            (identity_sidecar, arguments.input_identity_sha.clone()),
            (report_path, arguments.qualification_report.clone()),
            (report_sidecar, arguments.qualification_report_sha.clone()),
        ],
        armed,
    )?;

    let mut summary = Map::new();
    summary.insert(
        "input_identity_sha256".to_owned(),
        Value::String(identity_digest),
    );
    summary.insert(
        "producer".to_owned(),
        Value::String(PRODUCER_NAME.to_owned()),
    );
    summary.insert(
        "qualification_report_sha256".to_owned(),
        Value::String(report_digest),
    );
    summary.insert(
        "release_gates_sha256".to_owned(),
        Value::String(gates.digest),
    );
    summary.insert(
        "review_bindings_sha256".to_owned(),
        Value::String(bindings.digest),
    );
    Ok(Value::Object(summary).to_string())
}

fn run() -> Result<(), GateError> {
    let arguments = Arguments::parse()?;
    let outputs = arguments.outputs();
    let armed = read_seam(FAULT_ENV, &outputs)?;
    let armed = armed.as_deref();

    reject_interrupted_publication(&arguments)?;
    preflight(&arguments)?;
    fault("pre-flight", armed)?;

    let workspace = std::env::current_dir().map_err(|error| GateError::Io {
        path: "<cwd>".to_owned(),
        reason: error.to_string(),
    })?;
    let candidate = CandidateIdentity::load(&arguments.candidate_identity)?;
    let plan = PlanDocument::load(&arguments.plan)?;
    let provenance = Provenance::of_current_binary()?;

    let staging = Staging::acquire(&arguments)?;
    let result = produce(&Context {
        arguments: &arguments,
        staging: &staging,
        workspace: &workspace,
        candidate: &candidate,
        plan: &plan,
        provenance: &provenance,
        armed,
    });
    staging.release();
    println!("{}", result?);
    eprintln!(
        "produce-qualification: published {} and the five qualification paths",
        arguments.release_gates.display()
    );
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => report(&error),
    }
}
