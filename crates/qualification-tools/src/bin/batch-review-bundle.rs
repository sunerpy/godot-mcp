//! `batch-review-bundle` — NON-SHIPPING qualification tool (implemented by Todo 6).
//!
//! Deterministically generates the canonical B3/B4 dual-review input bundle and
//! its strict `.sha256` sidecar, validated against
//! `schemas/gates/batch-bundle.schema.json` **before** anything is written.
//!
//! Committed CLI surface (from the plan's B3/B4 dual-review gates section):
//!
//! ```text
//! batch-review-bundle --batch b3|b4 --plan <path>
//!                     --base-commit <hash> --base-tree <hash>
//!                     --schema schemas/gates/batch-bundle.schema.json
//!                     --output <bundle.json> --output-sha <bundle.sha256>
//! ```
//!
//! Bundle contents are exactly: this plan's whole-file SHA-256, the plan's Todo
//! 19–25 text (B3) or Todo 26–31 text (B4) as a canonicalized substring, the
//! frozen post-Todo-6 base source identity, and that batch's dependency-matrix
//! rows. The bundle's SHA-256 — the "batch SHA" referenced elsewhere in the plan
//! — is computed over the canonical byte serialization (UTF-8, sorted keys, no
//! trailing whitespace) and recorded in the `--output-sha` sidecar.
//!
//! The base source identity is two **independent** git object ids: the frozen
//! commit id in `--base-commit` and the frozen tree id in `--base-tree`. Neither
//! is ever copied into the other field, both must be exactly 40 lowercase hex
//! digits, and the two must differ — a commit object's SHA-1 is computed over a
//! header that already records its tree id, so equal values could only mean one
//! hash was supplied twice, leaving every gate receipt bound to a source identity
//! that proves nothing. This is enforced here rather than in
//! `schemas/gates/batch-bundle.schema.json`, whose `base_source_identity` fields
//! are frozen at `minLength: 1`.
//!
//! Determinism: the bundle is a pure function of the plan bytes, the batch, and
//! the two base object ids. No timestamp, hostname, environment value or ordering
//! nondeterminism enters it, so re-running the command on the same inputs
//! reproduces the same batch SHA byte for byte.

use std::path::PathBuf;
use std::process::ExitCode;

use qualification_tools::gates::{
    BaseSourceIdentity, Batch, BatchBundle, GATE_SCHEMA_VERSION, GateError, PlanDocument,
    canonical_bytes, report, validate_against_schema, write_artifact,
};

struct Arguments {
    batch: Batch,
    plan: PathBuf,
    base_source_identity: BaseSourceIdentity,
    schema: PathBuf,
    output: PathBuf,
    output_sha: PathBuf,
}

const USAGE: &str = "batch-review-bundle --batch b3|b4 --plan <path> --base-commit <hash> --base-tree <hash> \
     --schema <schema.json> --output <bundle.json> --output-sha <bundle.sha256>";

fn parse_arguments() -> Result<Arguments, GateError> {
    let mut batch = None;
    let mut plan = None;
    let mut base_commit = None;
    let mut base_tree = None;
    let mut schema = None;
    let mut output = None;
    let mut output_sha = None;

    let mut arguments = std::env::args().skip(1);
    while let Some(flag) = arguments.next() {
        let mut value = || {
            arguments
                .next()
                .ok_or_else(|| GateError::Usage(format!("{flag} requires a value\n{USAGE}")))
        };
        match flag.as_str() {
            "--batch" => batch = Some(Batch::parse(&value()?)?),
            "--plan" => plan = Some(PathBuf::from(value()?)),
            "--base-commit" => base_commit = Some(value()?),
            "--base-tree" => base_tree = Some(value()?),
            "--schema" => schema = Some(PathBuf::from(value()?)),
            "--output" => output = Some(PathBuf::from(value()?)),
            "--output-sha" => output_sha = Some(PathBuf::from(value()?)),
            other => {
                return Err(GateError::Usage(format!(
                    "unknown argument {other:?}\n{USAGE}"
                )));
            }
        }
    }

    let require = |name: &str, value: Option<PathBuf>| {
        value.ok_or_else(|| GateError::Usage(format!("{name} is required\n{USAGE}")))
    };
    let require_hash = |name: &str, value: Option<String>| {
        value.ok_or_else(|| GateError::Usage(format!("{name} is required\n{USAGE}")))
    };
    let batch = batch.ok_or_else(|| GateError::Usage(format!("--batch is required\n{USAGE}")))?;
    // The two object ids are validated as a pair before any file is touched, so a
    // copied-into-both-fields identity can never reach the evidence directory.
    let base_source_identity = BaseSourceIdentity::new(
        &require_hash("--base-commit", base_commit)?,
        &require_hash("--base-tree", base_tree)?,
    )?;
    Ok(Arguments {
        batch,
        plan: require("--plan", plan)?,
        base_source_identity,
        schema: require("--schema", schema)?,
        output: require("--output", output)?,
        output_sha: require("--output-sha", output_sha)?,
    })
}

fn build(arguments: &Arguments) -> Result<(Vec<u8>, String), GateError> {
    let plan = PlanDocument::load(&arguments.plan)?;
    let slice = plan.target_slice(arguments.batch)?;
    let rows = plan.dependency_matrix_rows(arguments.batch)?;
    let bundle = BatchBundle {
        schema_version: GATE_SCHEMA_VERSION.to_owned(),
        batch: arguments.batch.as_str().to_owned(),
        plan_sha256: plan.sha256().to_owned(),
        target_plan_slice: slice,
        base_source_identity: arguments.base_source_identity.clone(),
        dependency_matrix_rows: rows,
    };
    let bundle = serde_json::to_value(bundle).map_err(|error| GateError::Json {
        path: "<in-memory>".to_owned(),
        reason: error.to_string(),
    })?;
    // The bundle is validated against its canonical schema BEFORE it is written,
    // so an invalid bundle never reaches the evidence directory.
    validate_against_schema(
        &arguments.output.display().to_string(),
        &arguments.schema,
        &bundle,
    )?;
    let bytes = canonical_bytes(&bundle)?;
    Ok((bytes, plan.sha256().to_owned()))
}

fn run() -> Result<(), GateError> {
    let arguments = parse_arguments()?;
    let (bytes, plan_sha256) = build(&arguments)?;
    let digest = write_artifact(&arguments.output, &arguments.output_sha, &bytes)?;
    // stdout carries exactly the machine-readable identity of what was produced.
    println!(
        "{{\"batch\":\"{}\",\"bundle_sha256\":\"{digest}\",\"plan_sha256\":\"{plan_sha256}\"}}",
        arguments.batch.as_str()
    );
    eprintln!(
        "batch-review-bundle: wrote {} and {}",
        arguments.output.display(),
        arguments.output_sha.display()
    );
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => report(&error),
    }
}
