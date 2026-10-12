//! `finalize-approval` — NON-SHIPPING qualification tool (implemented by Todo 37).
//!
//! The deterministic serial F5 finalizer. It runs only after F1–F4 have each
//! independently APPROVEd, re-judges none of them, and writes exactly one file.
//!
//! Committed CLI surface (normative, from the plan's F5 body):
//!
//! ```text
//! finalize-approval --input         <qualification/input-identity.json>
//!                   --report        <qualification/qualification-report.json>
//!                   --reviews       <qualification/review-bindings.json>
//!                   --release-gates <external/release-gates.json>
//!                   --receipts-dir  <final/>
//!                   --schema        schemas/final-approval-manifest.schema.json
//!                   --output        <final/final-manifest.json>
//! ```
//!
//! Each of `--input`, `--report` and `--release-gates` is read together with its
//! own `.sha256` sidecar, and each F1–F4 receipt in `--receipts-dir` with its own.
//! The only file written is `--output`.
//!
//! ## Rejection conditions, all fail-closed with no output
//!
//! A missing, reused, mismatched or non-APPROVE receipt; any open Blocker or Major
//! anywhere in the bound receipts; a receipt binding a different identity/report
//! pair; identity/report/schema drift; a `release-gates.json` sidecar that does not
//! verify; any required gate entry that is missing, skipped, unavailable or
//! nonzero; any gate entry whose bound source commit/tree does not match the frozen
//! candidate — a stale or replayed evidence detection distinct from a missing entry;
//! review bindings that disagree with the frozen identity; or a pre-existing output
//! whose bytes differ from what this run would write.
//!
//! ## Byte stability
//!
//! The manifest is a pure function of the bound hashes, so re-running against
//! identical inputs is byte-identical. A pre-existing identical manifest is
//! accepted as already final; a pre-existing different one is a rejection.

use std::path::Path;
use std::process::ExitCode;

use serde_json::Value;

use qualification_tools::gates::{
    GateError, canonical_bytes, hex_sha256, implied_sidecar, read_bytes, report,
    validate_against_schema, verify_artifact,
};
use qualification_tools::qualification::{
    BINDINGS_SCHEMA, CHECKERS, Finding, Flags, IDENTITY_SCHEMA, QUALIFICATION_SCHEMA_VERSION,
    REPORT_SCHEMA, read_canonical, require_dir, require_file, verify_release_gates,
};

const USAGE: &str = "finalize-approval --input <input-identity.json> --report \
     <qualification-report.json> --reviews <review-bindings.json> --release-gates \
     <release-gates.json> --receipts-dir <final/> --schema \
     schemas/final-approval-manifest.schema.json --output <final-manifest.json>";

const FLAGS: [&str; 7] = [
    "--input",
    "--report",
    "--reviews",
    "--release-gates",
    "--receipts-dir",
    "--schema",
    "--output",
];

fn run() -> Result<(), GateError> {
    let flags = Flags::parse(USAGE, &FLAGS)?;
    let input = flags.path("--input")?;
    let report_path = flags.path("--report")?;
    let reviews = flags.path("--reviews")?;
    let release_gates = flags.path("--release-gates")?;
    let receipts_dir = flags.path("--receipts-dir")?;
    let schema = flags.path("--schema")?;
    let output = flags.path("--output")?;

    for (label, path) in [
        ("--input", &input),
        ("--report", &report_path),
        ("--reviews", &reviews),
        ("--release-gates", &release_gates),
        ("--schema", &schema),
    ] {
        require_file(label, path)?;
    }
    require_dir("--receipts-dir", &receipts_dir)?;

    let workspace = std::env::current_dir().map_err(|error| GateError::Io {
        path: "<cwd>".to_owned(),
        reason: error.to_string(),
    })?;

    let (identity_sha256, identity) = verify_artifact(&input, &implied_sidecar(&input))?;
    validate_against_schema(
        &input.display().to_string(),
        &workspace.join(IDENTITY_SCHEMA),
        &identity,
    )?;
    let (report_sha256, report_document) =
        verify_artifact(&report_path, &implied_sidecar(&report_path))?;
    validate_against_schema(
        &report_path.display().to_string(),
        &workspace.join(REPORT_SCHEMA),
        &report_document,
    )?;
    let reject = |path: &Path, reason: String| GateError::ReceiptRejected {
        path: path.display().to_string(),
        reason,
    };
    if report_document
        .get("input_identity_sha256")
        .and_then(Value::as_str)
        != Some(identity_sha256.as_str())
    {
        return Err(reject(
            &report_path,
            format!("the report does not bind input identity {identity_sha256}"),
        ));
    }
    if report_document.get("result") != Some(&Value::String("PASS".to_owned())) {
        return Err(reject(
            &report_path,
            "the qualification report does not record a PASS result".to_owned(),
        ));
    }
    for gate in report_document
        .get("gates")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        let name = gate
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("<unnamed>");
        if gate.get("exit_code").and_then(Value::as_i64) != Some(0) {
            return Err(reject(
                &report_path,
                format!("bound qualification evidence step `{name}` did not exit 0"),
            ));
        }
    }

    let (_, bindings) = read_canonical(&reviews)?;
    validate_against_schema(
        &reviews.display().to_string(),
        &workspace.join(BINDINGS_SCHEMA),
        &bindings,
    )?;
    let architecture_review = bindings
        .get("architecture_review")
        .cloned()
        .ok_or_else(|| reject(&reviews, "no architecture review is bound".to_owned()))?;
    if identity.get("architecture_review") != Some(&architecture_review) {
        return Err(reject(
            &reviews,
            "the review bindings and the frozen identity disagree about the current \
             architecture-review record; a historical round can never satisfy the current review"
                .to_owned(),
        ));
    }
    let batch_reviews = bindings
        .get("batch_reviews")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| reject(&reviews, "no batch reviews are bound".to_owned()))?;
    if batch_reviews.len() != 2 {
        return Err(reject(
            &reviews,
            format!(
                "exactly the B3 and B4 batch records are required, found {}",
                batch_reviews.len()
            ),
        ));
    }
    let mut batch_records = Vec::new();
    for record in &batch_reviews {
        let batch = record
            .get("batch")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let batch_sha256 = record
            .get("batch_sha256")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let round_id = record
            .get("round_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let hashes = record
            .get("receipt_hashes")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        if hashes.len() != 2 || hashes[0] == hashes[1] {
            return Err(reject(
                &reviews,
                format!(
                    "batch {batch} does not bind two independent receipt hashes, so one receipt was \
                     reused"
                ),
            ));
        }
        batch_records.push(serde_json::json!({
            "batch": batch,
            "batch_sha256": batch_sha256,
            "round_id": round_id,
        }));
    }

    let candidate_commit = identity
        .get("candidate_commit")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let candidate_tree = identity
        .get("candidate_tree")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let (gates_sha256, _) = verify_release_gates(
        &release_gates,
        &implied_sidecar(&release_gates),
        &workspace,
        candidate_commit,
        candidate_tree,
        true,
    )?;
    if identity.get("release_gates_sha256").and_then(Value::as_str) != Some(gates_sha256.as_str()) {
        return Err(reject(
            &release_gates,
            format!(
                "the frozen identity binds release-gate evidence {} but this document hashes to \
                 {gates_sha256}",
                identity
                    .get("release_gates_sha256")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
            ),
        ));
    }

    let mut reviewer_receipts = Vec::new();
    let mut seen_hashes: Vec<String> = Vec::new();
    for checker in CHECKERS {
        let receipt_path = receipts_dir.join(format!("{checker}.json"));
        require_file(&format!("{checker} receipt"), &receipt_path)?;
        let (receipt_sha256, receipt) =
            verify_artifact(&receipt_path, &implied_sidecar(&receipt_path))?;
        validate_against_schema(
            &receipt_path.display().to_string(),
            &workspace.join(qualification_tools::qualification::RECEIPT_SCHEMA),
            &receipt,
        )?;
        if receipt.get("verifier_identity").and_then(Value::as_str) != Some(checker) {
            return Err(reject(
                &receipt_path,
                format!("the receipt does not identify itself as {checker}"),
            ));
        }
        if receipt.get("verdict") != Some(&Value::String("APPROVE".to_owned())) {
            return Err(reject(
                &receipt_path,
                format!(
                    "{checker} recorded verdict {}; finalization requires four independent APPROVEs",
                    receipt
                        .get("verdict")
                        .and_then(Value::as_str)
                        .unwrap_or("<absent>")
                ),
            ));
        }
        if receipt.get("input_identity_sha256").and_then(Value::as_str)
            != Some(identity_sha256.as_str())
            || receipt
                .get("qualification_report_sha256")
                .and_then(Value::as_str)
                != Some(report_sha256.as_str())
        {
            return Err(reject(
                &receipt_path,
                format!(
                    "{checker} binds a different identity/report pair, so it does not belong to \
                     this qualification round"
                ),
            ));
        }
        if receipt.get("before_bound_hash") != receipt.get("after_bound_hash") {
            return Err(reject(
                &receipt_path,
                format!("{checker} recorded evidence mutation while it was running"),
            ));
        }
        let findings: Vec<Finding> = serde_json::from_value(
            receipt
                .get("findings")
                .cloned()
                .unwrap_or_else(|| Value::Array(Vec::new())),
        )
        .map_err(|error| GateError::Schema {
            path: receipt_path.display().to_string(),
            reason: format!("findings are malformed: {error}"),
        })?;
        let open = findings.iter().filter(|finding| finding.blocks()).count();
        if open > 0 {
            return Err(reject(
                &receipt_path,
                format!("{checker} leaves {open} open Blocker/Major finding(s)"),
            ));
        }
        if seen_hashes.contains(&receipt_sha256) {
            return Err(reject(
                &receipt_path,
                format!(
                    "{checker}'s receipt bytes are identical to another checker's, so one \
                         receipt was reused"
                ),
            ));
        }
        seen_hashes.push(receipt_sha256.clone());
        reviewer_receipts.push(serde_json::json!({
            "checker": checker,
            "receipt_sha256": receipt_sha256,
        }));
    }

    let manifest = serde_json::json!({
        "approved": true,
        "architecture_review": architecture_review,
        "batch_records": batch_records,
        "input_identity_sha256": identity_sha256,
        "qualification_report_sha256": report_sha256,
        "reviewer_receipts": reviewer_receipts,
        "schema_version": QUALIFICATION_SCHEMA_VERSION,
    });
    validate_against_schema(&output.display().to_string(), &schema, &manifest)?;
    let bytes = canonical_bytes(&manifest)?;
    let digest = hex_sha256(&bytes);

    if output.exists() {
        let existing = read_bytes(&output)?;
        if existing == bytes {
            println!("{{\"final_manifest_sha256\":\"{digest}\",\"written\":false}}");
            eprintln!(
                "finalize-approval: {} already holds the identical final manifest",
                output.display()
            );
            return Ok(());
        }
        return Err(reject(
            &output,
            "a different final manifest already exists at this path; finalization never \
             overwrites a mismatched output"
                .to_owned(),
        ));
    }
    write_only_output(&output, &bytes)?;
    println!("{{\"final_manifest_sha256\":\"{digest}\",\"written\":true}}");
    eprintln!("finalize-approval: wrote {}", output.display());
    Ok(())
}

/// Write exactly one file, atomically, and nothing else.
fn write_only_output(output: &Path, bytes: &[u8]) -> Result<(), GateError> {
    let io = |path: &Path, error: &std::io::Error| GateError::Io {
        path: path.display().to_string(),
        reason: error.to_string(),
    };
    if let Some(parent) = output.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|error| io(parent, &error))?;
    }
    let temporary = output.with_file_name(format!(
        ".{}.{}.tmp",
        output.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let result = (|| {
        use std::io::Write as _;
        let mut file = options
            .open(&temporary)
            .map_err(|error| io(&temporary, &error))?;
        file.write_all(bytes)
            .map_err(|error| io(&temporary, &error))?;
        file.sync_all().map_err(|error| io(&temporary, &error))?;
        std::fs::rename(&temporary, output).map_err(|error| io(output, &error))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => report(&error),
    }
}
