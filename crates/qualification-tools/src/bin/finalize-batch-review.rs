//! `finalize-batch-review` — NON-SHIPPING qualification tool (implemented by Todo 6).
//!
//! Verifies both independent review receipts for a B3/B4 gate and emits the
//! canonical approval manifest **only** when both receipts independently record
//! an unambiguous `APPROVE` with zero open Blocker/Major findings.
//!
//! Committed CLI surface (from the plan's B3/B4 dual-review gates section):
//!
//! ```text
//! finalize-batch-review --batch b3|b4 --bundle <path>
//!                       --receipts <a.json> <b.json>
//!                       --receipt-sidecars <a.sha256> <b.sha256>
//!                       --schema schemas/gates/batch-approval-manifest.schema.json
//!                       --output <manifest.json> --output-sha <manifest.sha256>
//! ```
//!
//! Order of verification is fail-closed and deliberate:
//!
//! 1. both receipt sidecars are verified first (strict 64-lowercase-hex + one LF,
//!    present, and matching the receipt bytes);
//! 2. the bundle is verified against its own sidecar and re-hashed, giving the
//!    authoritative batch SHA;
//! 3. the bundle's recorded whole-plan SHA-256 is compared against the live plan —
//!    a stale plan SHA invalidates the gate, because any plan byte change
//!    invalidates both bundles, both receipt pairs and both manifests;
//! 4. each receipt is validated against
//!    `schemas/gates/batch-review-receipt.schema.json`, bound to this batch and
//!    this exact bundle SHA-256, and required to be `APPROVE` with zero open
//!    Blocker/Major;
//! 5. the two receipts must be from two *distinct* reviewer identities, since the
//!    gate requires two independent reviews;
//! 6. only then is the manifest validated against its canonical schema and
//!    written together with its sidecar.
//!
//! Any failure exits nonzero and writes **no** manifest and **no** sidecar.

use std::path::PathBuf;
use std::process::ExitCode;

use qualification_tools::gates::{
    ApprovalReceiptBinding, Batch, BatchApprovalManifest, BatchBundle, DEFAULT_PLAN_PATH,
    GATE_SCHEMA_VERSION, GateError, PlanDocument, canonical_bytes, hex_sha256, implied_sidecar,
    parse_canonical_json, read_bytes, report, validate_against_schema, validate_receipt,
    verify_artifact, verify_artifact_bytes, write_artifact,
};

struct Arguments {
    batch: Batch,
    bundle: PathBuf,
    receipts: [PathBuf; 2],
    receipt_sidecars: [PathBuf; 2],
    manifest_schema: PathBuf,
    output: PathBuf,
    output_sha: PathBuf,
}

const USAGE: &str = "finalize-batch-review --batch b3|b4 --bundle <bundle.json> \
     --receipts <a.json> <b.json> \
     --receipt-sidecars <a.sha256> <b.sha256> --schema <batch-approval-manifest.schema.json> \
     --output <manifest.json> --output-sha <manifest.sha256>";

const DEFAULT_RECEIPT_SCHEMA: &str = "schemas/gates/batch-review-receipt.schema.json";
const DEFAULT_BUNDLE_SCHEMA: &str = "schemas/gates/batch-bundle.schema.json";

fn parse_arguments() -> Result<Arguments, GateError> {
    let mut batch = None;
    let mut bundle = None;
    let mut receipts: Vec<PathBuf> = Vec::new();
    let mut receipt_sidecars: Vec<PathBuf> = Vec::new();
    let mut manifest_schema = None;
    let mut output = None;
    let mut output_sha = None;

    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let mut index = 0_usize;
    while index < arguments.len() {
        let flag = arguments[index].clone();
        let value = |index: &mut usize| -> Result<String, GateError> {
            *index += 1;
            arguments
                .get(*index)
                .cloned()
                .ok_or_else(|| GateError::Usage(format!("{flag} requires a value\n{USAGE}")))
        };
        match arguments[index].as_str() {
            "--batch" => batch = Some(Batch::parse(&value(&mut index)?)?),
            "--bundle" => bundle = Some(PathBuf::from(value(&mut index)?)),
            "--schema" => manifest_schema = Some(PathBuf::from(value(&mut index)?)),
            "--output" => output = Some(PathBuf::from(value(&mut index)?)),
            "--output-sha" => output_sha = Some(PathBuf::from(value(&mut index)?)),
            "--receipts" | "--receipt-sidecars" => {
                let is_receipts = arguments[index] == "--receipts";
                index += 1;
                let mut collected = Vec::new();
                while index < arguments.len() && !arguments[index].starts_with("--") {
                    collected.push(PathBuf::from(arguments[index].clone()));
                    index += 1;
                }
                if collected.len() != 2 {
                    return Err(GateError::Usage(format!(
                        "{flag} requires exactly two paths (the gate needs two independent \
                         reviews), got {}\n{USAGE}",
                        collected.len()
                    )));
                }
                if is_receipts {
                    receipts = collected;
                } else {
                    receipt_sidecars = collected;
                }
                continue;
            }
            other => {
                return Err(GateError::Usage(format!(
                    "unknown argument {other:?}\n{USAGE}"
                )));
            }
        }
        index += 1;
    }

    let require = |name: &str, value: Option<PathBuf>| {
        value.ok_or_else(|| GateError::Usage(format!("{name} is required\n{USAGE}")))
    };
    let batch = batch.ok_or_else(|| GateError::Usage(format!("--batch is required\n{USAGE}")))?;
    if receipts.len() != 2 {
        return Err(GateError::Usage(format!(
            "--receipts requires exactly two paths\n{USAGE}"
        )));
    }
    if receipt_sidecars.len() != 2 {
        return Err(GateError::Usage(format!(
            "--receipt-sidecars requires exactly two paths\n{USAGE}"
        )));
    }
    let bundle = require("--bundle", bundle)?;
    Ok(Arguments {
        batch,
        bundle,
        receipts: [receipts[0].clone(), receipts[1].clone()],
        receipt_sidecars: [receipt_sidecars[0].clone(), receipt_sidecars[1].clone()],
        manifest_schema: require("--schema", manifest_schema)?,
        output: require("--output", output)?,
        output_sha: require("--output-sha", output_sha)?,
    })
}

fn run() -> Result<(), GateError> {
    let arguments = parse_arguments()?;

    let receipt_schema = PathBuf::from(DEFAULT_RECEIPT_SCHEMA);
    let plan_path = PathBuf::from(DEFAULT_PLAN_PATH);
    let mut receipt_bytes = Vec::with_capacity(2);
    for (receipt, sidecar) in arguments
        .receipts
        .iter()
        .zip(arguments.receipt_sidecars.iter())
    {
        receipt_bytes.push(verify_artifact_bytes(receipt, sidecar)?);
    }
    let mut receipt_inputs = Vec::with_capacity(2);
    for (receipt, (digest, bytes)) in arguments.receipts.iter().zip(receipt_bytes) {
        let value = parse_canonical_json(receipt, &bytes)?;
        validate_against_schema(&receipt.display().to_string(), &receipt_schema, &value)?;
        receipt_inputs.push((digest, value));
    }

    let (bundle_sha256, bundle_value) =
        verify_artifact(&arguments.bundle, &implied_sidecar(&arguments.bundle))?;
    validate_against_schema(
        &arguments.bundle.display().to_string(),
        &PathBuf::from(DEFAULT_BUNDLE_SCHEMA),
        &bundle_value,
    )?;
    let bundle: BatchBundle =
        serde_json::from_value(bundle_value).map_err(|error| GateError::Json {
            path: arguments.bundle.display().to_string(),
            reason: error.to_string(),
        })?;
    if bundle.batch != arguments.batch.as_str() {
        return Err(GateError::ReceiptRejected {
            path: arguments.bundle.display().to_string(),
            reason: format!(
                "bundle is for batch {:?}, not {:?}",
                bundle.batch,
                arguments.batch.as_str()
            ),
        });
    }

    let live_plan_sha = hex_sha256(&read_bytes(&plan_path)?);
    if bundle.plan_sha256 != live_plan_sha {
        return Err(GateError::StalePlanSha {
            plan: plan_path.display().to_string(),
            recorded: bundle.plan_sha256,
            actual: live_plan_sha,
        });
    }
    let plan = PlanDocument::load(&plan_path)?;
    let expected_slice = plan.target_slice(arguments.batch)?;
    if bundle.target_plan_slice != expected_slice {
        return Err(GateError::ReceiptRejected {
            path: arguments.bundle.display().to_string(),
            reason: "bundle's target_plan_slice does not equal the current plan slice for this \
                     batch"
                .to_owned(),
        });
    }
    let expected_rows = plan.dependency_matrix_rows(arguments.batch)?;
    if bundle.dependency_matrix_rows != expected_rows {
        return Err(GateError::ReceiptRejected {
            path: arguments.bundle.display().to_string(),
            reason:
                "bundle's dependency_matrix_rows do not equal the current plan rows for this batch"
                    .to_owned(),
        });
    }

    let mut verified = Vec::with_capacity(2);
    for (receipt, (digest, value)) in arguments.receipts.iter().zip(receipt_inputs) {
        verified.push(validate_receipt(
            receipt,
            digest,
            &value,
            arguments.batch,
            &bundle_sha256,
        )?);
    }

    // Step 5 — two receipts from the same reviewer are not two independent reviews.
    if verified[0].reviewer_identity == verified[1].reviewer_identity {
        return Err(GateError::ReceiptRejected {
            path: arguments.receipts[1].display().to_string(),
            reason: format!(
                "both receipts are from reviewer {:?}; the gate requires two independent reviewers",
                verified[0].reviewer_identity
            ),
        });
    }

    let manifest = BatchApprovalManifest {
        schema_version: GATE_SCHEMA_VERSION.to_owned(),
        batch: arguments.batch.as_str().to_owned(),
        plan_sha256: live_plan_sha,
        bundle_sha256: bundle_sha256.clone(),
        receipts: verified
            .iter()
            .map(|receipt| ApprovalReceiptBinding {
                reviewer_identity: receipt.reviewer_identity.clone(),
                receipt_sha256: receipt.receipt_sha256.clone(),
            })
            .collect(),
        approved: true,
    };
    let manifest = serde_json::to_value(manifest).map_err(|error| GateError::Json {
        path: "<in-memory>".to_owned(),
        reason: error.to_string(),
    })?;
    validate_against_schema(
        &arguments.output.display().to_string(),
        &arguments.manifest_schema,
        &manifest,
    )?;
    let bytes = canonical_bytes(&manifest)?;
    let digest = write_artifact(&arguments.output, &arguments.output_sha, &bytes)?;
    println!(
        "{{\"batch\":\"{}\",\"bundle_sha256\":\"{bundle_sha256}\",\
         \"manifest_sha256\":\"{digest}\",\"round_ids\":[\"{}\",\"{}\"]}}",
        arguments.batch.as_str(),
        verified[0].round_id,
        verified[1].round_id
    );
    eprintln!(
        "finalize-batch-review: wrote {} and {}",
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
