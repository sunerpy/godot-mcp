//! End-to-end tests for the two Todo-6-owned B3/B4 gate binaries.
//!
//! Every fixture here is test-local: receipts are written into a per-test
//! `tempfile::TempDir` and never into `.omo/evidence/**/gates/`. No receipt,
//! round ID, reviewer identity or verdict produced by these tests is committed
//! evidence — they exist only to exercise the binaries' accept/reject logic.

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use serde_json::{Value, json};

/// The frozen post-Todo-6 base source identity is supplied by the caller as two
/// independent git object ids. These tests pass fixed, obviously test-local
/// 40-hex placeholders that differ from each other, exactly as a real commit id
/// and the tree id it records always differ.
const TEST_BASE_COMMIT: &str = "cccccccccccccccccccccccccccccccccccccccc";
const TEST_BASE_TREE: &str = "dddddddddddddddddddddddddddddddddddddddd";

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root is two levels above the crate manifest")
        .to_path_buf()
}

fn plan_path() -> PathBuf {
    workspace_root().join(".omo/plans/godot-mcp-server-architecture.md")
}

fn schema(name: &str) -> PathBuf {
    workspace_root().join("schemas/gates").join(name)
}

fn bundle_command() -> Command {
    let mut command = Command::cargo_bin("batch-review-bundle").expect("bundle binary exists");
    command.current_dir(workspace_root());
    command
}

fn finalize_command() -> Command {
    let mut command = Command::cargo_bin("finalize-batch-review").expect("finalize binary exists");
    command.current_dir(workspace_root());
    command
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut hex, "{byte:02x}");
    }
    hex
}

/// Write a canonical artifact plus a strict sidecar, exactly as the binaries do.
fn write_canonical(path: &Path, value: &Value) -> String {
    let bytes = serde_json::to_vec(value).expect("canonical bytes");
    write_bytes_with_sidecar(path, &bytes)
}

fn write_bytes_with_sidecar(path: &Path, bytes: &[u8]) -> String {
    std::fs::write(path, bytes).expect("write artifact");
    let digest = sha256_hex(bytes);
    std::fs::write(sidecar_of(path), format!("{digest}\n")).expect("write sidecar");
    digest
}

/// The plan's committed sidecar names replace the artifact extension
/// (`b3-bundle.json` pairs with `b3-bundle.sha256`).
fn sidecar_of(path: &Path) -> PathBuf {
    path.with_extension("sha256")
}

/// Produce a real bundle for `batch` inside `directory`, returning its path and
/// batch SHA-256.
fn produce_bundle(directory: &Path, batch: &str) -> (PathBuf, String) {
    let bundle = directory.join(format!("{batch}-bundle.json"));
    let sidecar = directory.join(format!("{batch}-bundle.sha256"));
    bundle_command()
        .args(bundle_arguments(
            batch,
            TEST_BASE_COMMIT,
            TEST_BASE_TREE,
            &bundle,
            &sidecar,
        ))
        .assert()
        .success();
    let digest = std::fs::read_to_string(&sidecar).expect("sidecar");
    (bundle, digest.trim_end().to_owned())
}

/// The complete committed `batch-review-bundle` argument vector.
fn bundle_arguments(
    batch: &str,
    base_commit: &str,
    base_tree: &str,
    output: &Path,
    output_sha: &Path,
) -> Vec<String> {
    vec![
        "--batch".to_owned(),
        batch.to_owned(),
        "--plan".to_owned(),
        plan_path().display().to_string(),
        "--base-commit".to_owned(),
        base_commit.to_owned(),
        "--base-tree".to_owned(),
        base_tree.to_owned(),
        "--schema".to_owned(),
        schema("batch-bundle.schema.json").display().to_string(),
        "--output".to_owned(),
        output.display().to_string(),
        "--output-sha".to_owned(),
        output_sha.display().to_string(),
    ]
}

fn receipt(batch: &str, reviewer: &str, bundle_sha: &str, findings: Value, verdict: &str) -> Value {
    json!({
        "schema_version": "1",
        "batch": batch,
        "round_id": format!("test-local-round-1-{reviewer}"),
        "reviewer_identity": reviewer,
        "bundle_sha256": bundle_sha,
        "verdict": verdict,
        "findings": findings,
        "before_bundle_sha256": bundle_sha,
        "after_bundle_sha256": bundle_sha,
    })
}

struct Gate {
    /// RAII guard only: holding it keeps the per-test temp directory alive for the
    /// lifetime of the fixture, so every artifact path below stays valid and is
    /// removed when the test ends. It is intentionally never read.
    _directory: tempfile::TempDir,
    bundle: PathBuf,
    bundle_sha: String,
    receipts: [PathBuf; 2],
    manifest: PathBuf,
    manifest_sidecar: PathBuf,
}

impl Gate {
    /// Build a complete, approving gate fixture for `batch`.
    fn approving(batch: &str) -> Self {
        Self::with_receipts(batch, |bundle_sha| {
            [
                receipt(batch, "test-local-momus", bundle_sha, json!([]), "APPROVE"),
                receipt(batch, "test-local-codex", bundle_sha, json!([]), "APPROVE"),
            ]
        })
    }

    fn with_receipts(batch: &str, build: impl Fn(&str) -> [Value; 2]) -> Self {
        let directory = tempfile::Builder::new()
            .prefix("godot-mcp-gate-")
            .tempdir()
            .expect("temp gate directory");
        let (bundle, bundle_sha) = produce_bundle(directory.path(), batch);
        let values = build(&bundle_sha);
        let momus = directory.path().join(format!("{batch}-momus-receipt.json"));
        let codex = directory.path().join(format!("{batch}-codex-receipt.json"));
        write_canonical(&momus, &values[0]);
        write_canonical(&codex, &values[1]);
        Self {
            manifest: directory
                .path()
                .join(format!("{batch}-approval-manifest.json")),
            manifest_sidecar: directory
                .path()
                .join(format!("{batch}-approval-manifest.sha256")),
            _directory: directory,
            bundle,
            bundle_sha,
            receipts: [momus, codex],
        }
    }

    fn finalize(&self, batch: &str) -> Command {
        let mut command = finalize_command();
        command.args([
            "--batch",
            batch,
            "--bundle",
            &self.bundle.display().to_string(),
            "--receipts",
            &self.receipts[0].display().to_string(),
            &self.receipts[1].display().to_string(),
            "--receipt-sidecars",
            &sidecar_of(&self.receipts[0]).display().to_string(),
            &sidecar_of(&self.receipts[1]).display().to_string(),
            "--schema",
            &schema("batch-approval-manifest.schema.json")
                .display()
                .to_string(),
            "--output",
            &self.manifest.display().to_string(),
            "--output-sha",
            &self.manifest_sidecar.display().to_string(),
        ]);
        command
    }

    /// Assert no manifest and no manifest sidecar exist. Every failure path must
    /// satisfy this.
    fn assert_no_manifest(&self) {
        assert!(
            !self.manifest.exists(),
            "a rejected gate must not write {}",
            self.manifest.display()
        );
        assert!(
            !self.manifest_sidecar.exists(),
            "a rejected gate must not write {}",
            self.manifest_sidecar.display()
        );
    }

    fn rewrite_bundle_and_receipts(&mut self, batch: &str, mutate: impl FnOnce(&mut Value)) {
        let mut bundle: Value =
            serde_json::from_slice(&std::fs::read(&self.bundle).expect("bundle bytes"))
                .expect("bundle json");
        mutate(&mut bundle);
        self.bundle_sha = write_canonical(&self.bundle, &bundle);
        for receipt_path in &self.receipts {
            let mut receipt: Value =
                serde_json::from_slice(&std::fs::read(receipt_path).expect("receipt bytes"))
                    .expect("receipt json");
            receipt["batch"] = json!(batch);
            receipt["bundle_sha256"] = json!(self.bundle_sha);
            receipt["before_bundle_sha256"] = json!(self.bundle_sha);
            receipt["after_bundle_sha256"] = json!(self.bundle_sha);
            write_canonical(receipt_path, &receipt);
        }
    }
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn bundle_is_deterministic_and_schema_valid_for_both_batches() {
    let plan_sha = sha256_hex(&std::fs::read(plan_path()).expect("plan bytes"));
    for batch in ["b3", "b4"] {
        let first = tempfile::tempdir().expect("temp dir");
        let second = tempfile::tempdir().expect("temp dir");
        let (bundle_a, sha_a) = produce_bundle(first.path(), batch);
        let (bundle_b, sha_b) = produce_bundle(second.path(), batch);
        assert_eq!(sha_a, sha_b, "the batch SHA must be reproducible");
        assert_eq!(
            std::fs::read(&bundle_a).expect("bytes"),
            std::fs::read(&bundle_b).expect("bytes"),
            "bundle bytes must be byte-identical across runs"
        );

        let bytes = std::fs::read(&bundle_a).expect("bytes");
        assert_eq!(
            sha256_hex(&bytes),
            sha_a,
            "the sidecar records the bundle SHA"
        );
        let text = String::from_utf8(bytes.clone()).expect("utf-8");
        assert!(
            !text.ends_with('\n') && text.trim_end() == text,
            "canonical bundle bytes carry no trailing whitespace"
        );
        assert!(
            text.starts_with("{\"base_source_identity\":"),
            "canonical bundle keys must be sorted: {}",
            &text[..48.min(text.len())]
        );

        let value: Value = serde_json::from_slice(&bytes).expect("bundle json");
        assert_eq!(value["batch"], json!(batch));
        assert_eq!(value["plan_sha256"], json!(plan_sha));
        assert_eq!(
            value["base_source_identity"]["commit"],
            json!(TEST_BASE_COMMIT)
        );
        assert_eq!(value["base_source_identity"]["tree"], json!(TEST_BASE_TREE));
        assert_ne!(
            value["base_source_identity"]["commit"], value["base_source_identity"]["tree"],
            "the commit id and the tree id are independent objects and never equal"
        );
        let (first_todo, last_todo) = if batch == "b3" { (19, 25) } else { (26, 31) };
        let slice = value["target_plan_slice"].as_str().expect("slice");
        assert!(slice.contains(&format!("] {first_todo}. ")));
        assert!(slice.contains(&format!("] {last_todo}. ")));
        assert!(
            !slice.contains(&format!("] {}. ", last_todo + 1)),
            "the slice must not spill into the next batch"
        );
        let rows = value["dependency_matrix_rows"]
            .as_array()
            .expect("matrix rows");
        assert_eq!(rows.len(), (last_todo - first_todo + 1) as usize);
        assert!(
            rows[0]
                .as_str()
                .expect("row")
                .starts_with(&format!("| {first_todo} |"))
        );

        let sidecar = std::fs::read(sidecar_of(&bundle_a)).expect("sidecar bytes");
        assert_eq!(sidecar.len(), 65, "sidecar is 64 hex digits plus one LF");
        assert_eq!(sidecar[64], b'\n');
        assert!(
            sidecar[..64]
                .iter()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte)),
            "sidecar digest must be lowercase hexadecimal"
        );
    }
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn bundle_rejects_a_missing_or_empty_base_tree() {
    let directory = tempfile::tempdir().expect("temp dir");
    let output = directory.path().join("bundle.json");
    let sidecar = directory.path().join("bundle.sha256");
    for arguments in [
        vec![
            "--batch".to_owned(),
            "b3".to_owned(),
            "--plan".to_owned(),
            plan_path().display().to_string(),
            "--base-commit".to_owned(),
            TEST_BASE_COMMIT.to_owned(),
            "--schema".to_owned(),
            schema("batch-bundle.schema.json").display().to_string(),
            "--output".to_owned(),
            output.display().to_string(),
            "--output-sha".to_owned(),
            sidecar.display().to_string(),
        ],
        bundle_arguments("b3", TEST_BASE_COMMIT, "   ", &output, &sidecar),
    ] {
        bundle_command().args(&arguments).assert().failure();
        assert!(!output.exists(), "a rejected bundle writes nothing");
        assert!(!sidecar.exists(), "a rejected bundle writes no sidecar");
    }
    bundle_command().args(["--batch", "b5"]).assert().failure();
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn bundle_records_the_commit_and_tree_object_ids_independently() {
    // Running the same batch twice with the two ids swapped proves neither field
    // is derived from the other: a generator that copied one value into both
    // fields would produce identical identities for both runs.
    let first_commit = "0123456789abcdef0123456789abcdef01234567";
    let first_tree = "fedcba9876543210fedcba9876543210fedcba98";
    for (base_commit, base_tree) in [(first_commit, first_tree), (first_tree, first_commit)] {
        let directory = tempfile::tempdir().expect("temp dir");
        let bundle = directory.path().join("b3-bundle.json");
        let sidecar = directory.path().join("b3-bundle.sha256");
        bundle_command()
            .args(bundle_arguments(
                "b3",
                base_commit,
                base_tree,
                &bundle,
                &sidecar,
            ))
            .assert()
            .success();
        let value: Value = serde_json::from_slice(&std::fs::read(&bundle).expect("bundle bytes"))
            .expect("bundle json");
        let identity = &value["base_source_identity"];
        assert_eq!(
            identity["commit"],
            json!(base_commit),
            "commit must carry exactly the --base-commit value"
        );
        assert_eq!(
            identity["tree"],
            json!(base_tree),
            "tree must carry exactly the --base-tree value"
        );
    }
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn bundle_rejects_a_commit_equal_to_the_tree() {
    let directory = tempfile::tempdir().expect("temp dir");
    let output = directory.path().join("bundle.json");
    let sidecar = directory.path().join("bundle.sha256");
    // A commit object's SHA-1 covers a header naming its tree, so a commit id can
    // never equal its own tree id. Equal values can only mean one hash was copied
    // into both fields, which is exactly the identity defect this rejects.
    let same = "57b17f4e5b05079634d5083ff156049d64825a78";
    bundle_command()
        .args(bundle_arguments("b3", same, same, &output, &sidecar))
        .assert()
        .failure()
        .stdout(predicates::str::is_empty())
        .stderr(predicates::str::contains("can never equal its own tree id"))
        .stderr(predicates::str::contains("copied into both fields"));
    assert!(!output.exists(), "a rejected bundle writes nothing");
    assert!(!sidecar.exists(), "a rejected bundle writes no sidecar");
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn bundle_rejects_a_missing_base_commit_and_malformed_object_ids() {
    let directory = tempfile::tempdir().expect("temp dir");
    let output = directory.path().join("bundle.json");
    let sidecar = directory.path().join("bundle.sha256");

    // Omitting --base-commit entirely is a usage error naming the missing flag.
    let mut without_base_commit = bundle_arguments("b3", "", TEST_BASE_TREE, &output, &sidecar);
    let commit_flag = without_base_commit
        .iter()
        .position(|argument| argument == "--base-commit")
        .expect("the argument vector carries --base-commit");
    without_base_commit.drain(commit_flag..=commit_flag + 1);
    bundle_command()
        .args(&without_base_commit)
        .assert()
        .failure()
        .code(i32::from(qualification_tools::gates::USAGE_EXIT))
        .stdout(predicates::str::is_empty())
        .stderr(predicates::str::contains("--base-commit is required"));

    // Both ids must be exactly 40 lowercase hexadecimal digits.
    let malformed = [
        "cccccccccccccccccccccccccccccccccccccc",    // 38 digits
        "ccccccccccccccccccccccccccccccccccccccccc", // 41 digits
        "CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC",  // uppercase
        "cccccccccccccccccccccccccccccccccccccccg",  // non-hex digit
        "0123456789abcdef0123456789abcdef0123456 ",  // embedded space
    ];
    for value in malformed {
        for (base_commit, base_tree) in [(value, TEST_BASE_TREE), (TEST_BASE_COMMIT, value)] {
            bundle_command()
                .args(bundle_arguments(
                    "b3",
                    base_commit,
                    base_tree,
                    &output,
                    &sidecar,
                ))
                .assert()
                .failure()
                .code(i32::from(qualification_tools::gates::USAGE_EXIT))
                .stdout(predicates::str::is_empty())
                .stderr(predicates::str::contains(
                    "exactly 40 lowercase hexadecimal digits",
                ));
            assert!(!output.exists(), "a rejected bundle writes nothing");
            assert!(!sidecar.exists(), "a rejected bundle writes no sidecar");
        }
    }
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn finalize_writes_the_manifest_only_when_both_receipts_approve() {
    for batch in ["b3", "b4"] {
        let gate = Gate::approving(batch);
        gate.finalize(batch).assert().success();

        let bytes = std::fs::read(&gate.manifest).expect("manifest bytes");
        let text = String::from_utf8(bytes.clone()).expect("utf-8");
        assert!(
            !text.ends_with('\n') && text.trim_end() == text,
            "canonical manifest bytes carry no trailing whitespace"
        );
        assert!(
            text.starts_with("{\"approved\":true,"),
            "canonical manifest keys must be sorted: {}",
            &text[..32.min(text.len())]
        );
        let manifest: Value = serde_json::from_slice(&bytes).expect("manifest json");
        assert_eq!(manifest["batch"], json!(batch));
        assert_eq!(manifest["approved"], json!(true));
        assert_eq!(manifest["bundle_sha256"], json!(gate.bundle_sha));
        let receipts = manifest["receipts"].as_array().expect("receipts");
        assert_eq!(receipts.len(), 2);
        for (index, entry) in receipts.iter().enumerate() {
            let recorded = entry["receipt_sha256"].as_str().expect("receipt sha");
            let actual = sha256_hex(&std::fs::read(&gate.receipts[index]).expect("receipt bytes"));
            assert_eq!(recorded, actual, "manifest binds each receipt's real hash");
        }
        let sidecar = std::fs::read(&gate.manifest_sidecar).expect("manifest sidecar");
        assert_eq!(sidecar.len(), 65);
        assert_eq!(
            String::from_utf8_lossy(&sidecar[..64]),
            sha256_hex(&bytes),
            "the manifest sidecar records the manifest bytes"
        );
    }
}

/// One named way to corrupt a `.sha256` sidecar's exact 64-hex-plus-LF format.
type SidecarVariant = (&'static str, Box<dyn Fn(&str) -> Vec<u8>>);

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn finalize_rejects_every_malformed_sidecar_variant() {
    let variants: [SidecarVariant; 8] = [
        (
            "uppercase",
            Box::new(|digest: &str| format!("{}\n", digest.to_uppercase()).into_bytes()),
        ),
        (
            "no trailing newline",
            Box::new(|digest: &str| digest.as_bytes().to_vec()),
        ),
        (
            "crlf",
            Box::new(|digest: &str| format!("{digest}\r\n").into_bytes()),
        ),
        (
            "double newline",
            Box::new(|digest: &str| format!("{digest}\n\n").into_bytes()),
        ),
        (
            "leading whitespace",
            Box::new(|digest: &str| format!(" {digest}\n").into_bytes()),
        ),
        (
            "embedded filename",
            Box::new(|digest: &str| format!("{digest}  b3-momus-receipt.json\n").into_bytes()),
        ),
        (
            "truncated digest",
            Box::new(|digest: &str| format!("{}\n", &digest[..63]).into_bytes()),
        ),
        (
            "digest mismatch",
            Box::new(|_digest: &str| format!("{}\n", "0".repeat(64)).into_bytes()),
        ),
    ];
    for (label, mangle) in variants {
        let gate = Gate::approving("b3");
        let sidecar = sidecar_of(&gate.receipts[0]);
        let real = std::fs::read_to_string(&sidecar).expect("sidecar");
        std::fs::write(&sidecar, mangle(real.trim_end())).expect("write mangled sidecar");
        gate.finalize("b3")
            .assert()
            .failure()
            .stdout(predicates::str::is_empty());
        gate.assert_no_manifest();
        assert!(
            !label.is_empty(),
            "every sidecar variant is named for diagnosis"
        );
    }

    // A missing sidecar is rejected too.
    let gate = Gate::approving("b3");
    std::fs::remove_file(sidecar_of(&gate.receipts[1])).expect("remove sidecar");
    gate.finalize("b3").assert().failure();
    gate.assert_no_manifest();
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn finalize_rejects_a_tampered_receipt_body() {
    let gate = Gate::approving("b4");
    // Change the verdict without updating the sidecar: the sidecar check catches
    // it before the verdict is even read.
    let mut value: Value =
        serde_json::from_slice(&std::fs::read(&gate.receipts[0]).expect("bytes")).expect("json");
    value["verdict"] = json!("REJECT");
    std::fs::write(
        &gate.receipts[0],
        serde_json::to_vec(&value).expect("bytes"),
    )
    .expect("write tampered receipt");
    gate.finalize("b4").assert().failure();
    gate.assert_no_manifest();
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn finalize_rejects_a_stale_whole_plan_sha() {
    let mut gate = Gate::approving("b3");
    gate.rewrite_bundle_and_receipts("b3", |bundle| {
        bundle["plan_sha256"] = json!("f".repeat(64));
    });
    gate.finalize("b3")
        .assert()
        .failure()
        .stderr(predicates::str::contains("stale whole-plan SHA-256"));
    gate.assert_no_manifest();
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn finalize_rejects_a_single_approve() {
    let gate = Gate::with_receipts("b3", |bundle_sha| {
        [
            receipt("b3", "test-local-momus", bundle_sha, json!([]), "APPROVE"),
            receipt("b3", "test-local-codex", bundle_sha, json!([]), "REJECT"),
        ]
    });
    gate.finalize("b3")
        .assert()
        .failure()
        .stderr(predicates::str::contains("requires an unambiguous APPROVE"));
    gate.assert_no_manifest();
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn finalize_rejects_approve_with_an_open_major_or_blocker() {
    for severity in ["Major", "Blocker"] {
        let gate = Gate::with_receipts("b4", |bundle_sha| {
            [
                receipt("b4", "test-local-momus", bundle_sha, json!([]), "APPROVE"),
                receipt(
                    "b4",
                    "test-local-codex",
                    bundle_sha,
                    json!([{
                        "severity": severity,
                        "status": "open",
                        "summary": "test-local open finding",
                    }]),
                    "APPROVE",
                ),
            ]
        });
        gate.finalize("b4")
            .assert()
            .failure()
            .stderr(predicates::str::contains("open Blocker/Major"));
        gate.assert_no_manifest();
    }

    // A closed Blocker and an open Follow-up do not block the gate.
    let gate = Gate::with_receipts("b4", |bundle_sha| {
        [
            receipt(
                "b4",
                "test-local-momus",
                bundle_sha,
                json!([{
                    "severity": "Blocker",
                    "status": "closed",
                    "summary": "test-local resolved blocker",
                }]),
                "APPROVE",
            ),
            receipt(
                "b4",
                "test-local-codex",
                bundle_sha,
                json!([{
                    "severity": "Follow-up",
                    "status": "open",
                    "summary": "test-local follow-up",
                }]),
                "APPROVE",
            ),
        ]
    });
    gate.finalize("b4").assert().success();
    assert!(gate.manifest.exists());
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn finalize_rejects_a_receipt_bound_to_another_bundle() {
    let gate = Gate::with_receipts("b3", |_bundle_sha| {
        let foreign = "f".repeat(64);
        [
            receipt("b3", "test-local-momus", &foreign, json!([]), "APPROVE"),
            receipt("b3", "test-local-codex", &foreign, json!([]), "APPROVE"),
        ]
    });
    gate.finalize("b3")
        .assert()
        .failure()
        .stderr(predicates::str::contains("the receipt is stale"));
    gate.assert_no_manifest();
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn finalize_rejects_two_receipts_from_one_reviewer_and_a_batch_mismatch() {
    let same = Gate::with_receipts("b3", |bundle_sha| {
        [
            receipt("b3", "test-local-momus", bundle_sha, json!([]), "APPROVE"),
            receipt("b3", "test-local-momus", bundle_sha, json!([]), "APPROVE"),
        ]
    });
    same.finalize("b3")
        .assert()
        .failure()
        .stderr(predicates::str::contains("two independent reviewers"));
    same.assert_no_manifest();

    // A b4 receipt can never satisfy the b3 gate.
    let crossed = Gate::with_receipts("b3", |bundle_sha| {
        [
            receipt("b3", "test-local-momus", bundle_sha, json!([]), "APPROVE"),
            receipt("b4", "test-local-codex", bundle_sha, json!([]), "APPROVE"),
        ]
    });
    crossed.finalize("b3").assert().failure();
    crossed.assert_no_manifest();

    // Finalizing a b3 bundle as b4 is refused.
    let gate = Gate::approving("b3");
    gate.finalize("b4").assert().failure();
    gate.assert_no_manifest();
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn finalize_rejects_a_receipt_that_violates_its_canonical_schema() {
    let gate = Gate::with_receipts("b3", |bundle_sha| {
        let mut invalid = receipt("b3", "test-local-codex", bundle_sha, json!([]), "APPROVE");
        invalid
            .as_object_mut()
            .expect("object")
            .insert("unexpected_field".to_owned(), json!(true));
        [
            receipt("b3", "test-local-momus", bundle_sha, json!([]), "APPROVE"),
            invalid,
        ]
    });
    gate.finalize("b3")
        .assert()
        .failure()
        .stderr(predicates::str::contains("canonical schema violation"));
    gate.assert_no_manifest();
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn finalize_requires_exactly_two_receipts_and_two_sidecars() {
    let gate = Gate::approving("b3");
    let mut command = finalize_command();
    command.args([
        "--batch",
        "b3",
        "--bundle",
        &gate.bundle.display().to_string(),
        "--receipts",
        &gate.receipts[0].display().to_string(),
        "--receipt-sidecars",
        &sidecar_of(&gate.receipts[0]).display().to_string(),
        "--schema",
        &schema("batch-approval-manifest.schema.json")
            .display()
            .to_string(),
        "--output",
        &gate.manifest.display().to_string(),
        "--output-sha",
        &gate.manifest_sidecar.display().to_string(),
    ]);
    command
        .assert()
        .failure()
        .stderr(predicates::str::contains("exactly two paths"));
    gate.assert_no_manifest();
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn receipt_schema_validation_precedes_bundle_validation() {
    let gate = Gate::with_receipts("b3", |bundle_sha| {
        let mut invalid = receipt("b3", "test-local-momus", bundle_sha, json!([]), "APPROVE");
        invalid
            .as_object_mut()
            .expect("object")
            .insert("unexpected_field".to_owned(), json!(true));
        [
            invalid,
            receipt("b3", "test-local-codex", bundle_sha, json!([]), "APPROVE"),
        ]
    });
    std::fs::remove_file(sidecar_of(&gate.bundle)).expect("remove bundle sidecar");

    gate.finalize("b3")
        .assert()
        .failure()
        .stderr(predicates::str::contains("canonical schema violation"));
    gate.assert_no_manifest();
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn finalize_rejects_a_changed_before_bundle_sha() {
    let gate = Gate::with_receipts("b3", |bundle_sha| {
        let mut changed = receipt("b3", "test-local-codex", bundle_sha, json!([]), "APPROVE");
        changed["before_bundle_sha256"] = json!("f".repeat(64));
        [
            receipt("b3", "test-local-momus", bundle_sha, json!([]), "APPROVE"),
            changed,
        ]
    });

    gate.finalize("b3")
        .assert()
        .failure()
        .stderr(predicates::str::contains("before_bundle_sha256"));
    gate.assert_no_manifest();
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn finalize_rejects_noncanonical_receipt_bytes_even_with_a_matching_sidecar() {
    let gate = Gate::approving("b4");
    let value: Value =
        serde_json::from_slice(&std::fs::read(&gate.receipts[0]).expect("receipt bytes"))
            .expect("receipt json");
    let pretty = serde_json::to_vec_pretty(&value).expect("pretty receipt bytes");
    write_bytes_with_sidecar(&gate.receipts[0], &pretty);

    gate.finalize("b4")
        .assert()
        .failure()
        .stderr(predicates::str::contains("canonical JSON bytes"));
    gate.assert_no_manifest();
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn finalize_rejects_a_bundle_with_changed_dependency_rows() {
    let mut gate = Gate::approving("b3");
    gate.rewrite_bundle_and_receipts("b3", |bundle| {
        bundle["dependency_matrix_rows"][0] =
            json!("| 19 | test-local-tamper | test-local-tamper | — |");
    });

    gate.finalize("b3")
        .assert()
        .failure()
        .stderr(predicates::str::contains("dependency_matrix_rows"));
    gate.assert_no_manifest();
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn producer_write_failures_leave_no_partial_artifact() {
    let directory = tempfile::tempdir().expect("temp dir");
    let bundle = directory.path().join("bundle.json");
    let unusable_bundle_sidecar = directory.path().join("bundle-sidecar-is-a-directory");
    std::fs::create_dir(&unusable_bundle_sidecar).expect("sidecar directory");
    bundle_command()
        .args(bundle_arguments(
            "b3",
            TEST_BASE_COMMIT,
            TEST_BASE_TREE,
            &bundle,
            &unusable_bundle_sidecar,
        ))
        .assert()
        .failure();
    assert!(!bundle.exists(), "failed bundle publication must roll back");

    let gate = Gate::approving("b4");
    std::fs::create_dir(&gate.manifest_sidecar).expect("manifest sidecar directory");
    gate.finalize("b4").assert().failure();
    assert!(
        !gate.manifest.exists(),
        "failed manifest publication must roll back"
    );
}
