//! Todo 37's end-to-end qualification suite: exactly the two test functions
//! `qa/tasks/37.json` invokes by name with `--exact`.
//!
//! * [`e2e_qualification_happy_path`] invokes the exact `produce-qualification`
//!   command against a frozen fixture candidate and asserts final-gate-first
//!   staging, the five qualification outputs in order, complete evidence and
//!   parity, producer provenance, and all six Todo-37-owned binaries' documented
//!   success through to one byte-stable final manifest.
//! * [`e2e_qualification_failure_paths`] injects failure before and within each
//!   producer stage and each publication rename, precreates each F1–F5 output in
//!   turn, mutates each identity/review/tool/runner input, supplies non-approving
//!   and reused receipts, and feeds each verifier and the finalizer a named reject
//!   fixture. Every producer case must exit nonzero with all seven final producer
//!   output paths absent, and no case may ever synthesize or modify a review
//!   receipt.
//!
//! The fixture builder is shared with `qualification-tools`' own real-logic suite
//! by path rather than by a dependency edge, so this shipping crate never gains a
//! manifest reference to the non-shipping qualification tooling.
//!
//! Nothing here writes into the real `.omo/evidence/godot-mcp-server-architecture/**`
//! tree: every output lives under a temporary root, and the committed Red/Green
//! artifacts are read only.

#[allow(dead_code)]
mod support {
    include!("../../qualification-tools/tests/support/mod.rs");
}

use std::path::Path;

use serde_json::{Value, json};
use support::{
    CHECKERS, Fixture, Outcome, archive_package, binary, bind_candidate_identity,
    bind_host_archive_observation, canonical, clean_candidate_worktree, commit_candidate_changes,
    failing_step_plan, fake_package, file_sha256, git_revision, host_archive, mutate, read_json,
    rebind_qualification_archive, rebind_qualification_candidate, refresh_archive_checksums,
    sidecar, write, write_pair,
};

/// Build every binary the suite drives, once. The qualification tooling is a
/// separate package with no dependency edge onto this crate, so `cargo test -p
/// godot-mcp-server` does not build it implicitly.
fn build_tools() {
    let status = std::process::Command::new(env!("CARGO"))
        .args(["build", "--locked", "-p", "qualification-tools"])
        .current_dir(support::repo_root())
        .status()
        .expect("cargo build runs");
    assert!(
        status.success(),
        "the six Todo-37-owned qualification binaries must build"
    );
    support::build_release_package();
}

fn temp() -> tempfile::TempDir {
    tempfile::tempdir().expect("temporary fixture root")
}

/// A published round plus the extracted package and gate catalogue its checkers
/// need.
struct Round {
    root: tempfile::TempDir,
    fixture: Fixture,
    package: std::path::PathBuf,
    archive: std::path::PathBuf,
    worktree: std::path::PathBuf,
    gate_plan: std::path::PathBuf,
}

impl Round {
    fn produce() -> Self {
        let root = temp();
        let fixture = Fixture::build(root.path());
        let worktree = clean_candidate_worktree(root.path());
        bind_candidate_identity(&fixture, &worktree);
        let package = fake_package(root.path());
        let archive = fixture.archives_dir.join(host_archive());
        archive_package(&package, &archive);
        bind_host_archive_observation(&fixture, &archive);
        refresh_archive_checksums(&fixture.archives_dir);
        let outcome = fixture.run_producer(None);
        assert!(
            outcome.succeeded(),
            "the exact producer command must succeed against a frozen fixture candidate: {}",
            outcome.stderr
        );
        let gate_plan = root.path().join("gate-plan.json");
        write(&gate_plan, &canonical(&support::passing_step_plan()));
        Self {
            root,
            fixture,
            package,
            archive,
            worktree,
            gate_plan,
        }
    }

    fn receipt(&self, checker: &str) -> std::path::PathBuf {
        self.fixture.final_dir.join(format!("{checker}.json"))
    }

    fn run(&self, checker: &str) -> Outcome {
        let plan = self.fixture.plan.clone();
        let extra: Vec<(&str, &Path)> = match checker {
            "f1-plan-compliance" | "f4-scope-fidelity" => vec![("--plan", plan.as_path())],
            "f2-code-quality" => vec![("--worktree", self.worktree.as_path())],
            "f3-real-qa" => vec![
                ("--archive", self.archive.as_path()),
                ("--godot-lock", self.fixture.godot_lock.as_path()),
            ],
            other => panic!("unknown checker {other}"),
        };
        let output = self.receipt(checker);
        let mut command = std::process::Command::new(binary(checker));
        command
            .arg("--identity")
            .arg(&self.fixture.input_identity)
            .arg("--report")
            .arg(&self.fixture.qualification_report);
        for (flag, value) in &extra {
            command.arg(flag).arg(value);
        }
        command
            .arg("--output")
            .arg(&output)
            .current_dir(&self.worktree)
            .env("GODOT_MCP_QUALIFICATION_GATE_PLAN", &self.gate_plan);
        Outcome::of_command(command)
    }

    fn rearchive_and_rebind(&self) {
        archive_package(&self.package, &self.archive);
        rebind_qualification_archive(&self.fixture, &self.archive);
    }

    fn approve_all(&self) {
        for checker in CHECKERS {
            let outcome = self.run(checker);
            assert!(
                outcome.succeeded(),
                "{checker} must APPROVE a clean round: {}\n{}\n{:#}",
                outcome.stdout,
                outcome.stderr,
                read_json(&self.receipt(checker))
            );
        }
    }

    /// A snapshot of every receipt's bytes, so a later case can prove no verifier
    /// receipt was synthesized or modified.
    fn receipt_snapshot(&self) -> Vec<(String, Option<Vec<u8>>)> {
        CHECKERS
            .iter()
            .flat_map(|checker| [self.receipt(checker), sidecar(&self.receipt(checker))])
            .map(|path| (path.display().to_string(), std::fs::read(&path).ok()))
            .collect()
    }
}

#[test]
#[ignore = "needs the maintainer's .omo workspace; qa/tasks/37.json runs it"]
fn e2e_qualification_happy_path() {
    build_tools();
    let round = Round::produce();
    let fixture = &round.fixture;

    // Final-gate-first staging: the gates pair is published, is the qualifying
    // emission, and every required check executed at exit 0 bound to the frozen
    // candidate.
    let gates = read_json(&fixture.release_gates);
    assert_eq!(gates.get("qualifying"), Some(&Value::Bool(true)));
    let checks = gates
        .get("checks")
        .and_then(Value::as_array)
        .expect("release-gate checks");
    let required: Vec<&Value> = checks
        .iter()
        .filter(|check| check.get("required") == Some(&Value::Bool(true)))
        .collect();
    assert_eq!(
        required.len(),
        checks.len(),
        "every check emitted by the frozen harness is required"
    );
    for check in &required {
        assert_eq!(
            check.get("observation").and_then(Value::as_str),
            Some("executed")
        );
        assert_eq!(check.get("exit_code").and_then(Value::as_i64), Some(0));
        assert_eq!(
            check.get("source_commit").and_then(Value::as_str),
            Some(git_revision(&round.worktree, "HEAD").as_str())
        );
        assert_eq!(
            check.get("source_tree").and_then(Value::as_str),
            Some(git_revision(&round.worktree, "HEAD^{tree}").as_str())
        );
    }

    // The five qualification outputs exist in the plan's order, and each hash edge
    // points strictly forward.
    let gates_digest = file_sha256(&fixture.release_gates);
    let bindings = read_json(&fixture.review_bindings);
    let identity = read_json(&fixture.input_identity);
    let report = read_json(&fixture.qualification_report);
    let identity_digest = file_sha256(&fixture.input_identity);
    let report_digest = file_sha256(&fixture.qualification_report);

    assert_eq!(
        identity.get("release_gates_sha256").and_then(Value::as_str),
        Some(gates_digest.as_str()),
        "the identity binds the gates emitted before it"
    );
    assert_eq!(
        identity
            .pointer("/bound_inputs/review-bindings/sha256")
            .and_then(Value::as_str),
        Some(file_sha256(&fixture.review_bindings).as_str()),
        "the identity binds the review bindings content hash"
    );
    assert_eq!(
        identity.get("architecture_review"),
        bindings.get("architecture_review"),
        "the identity and the bindings agree on the current architecture review"
    );
    assert_eq!(
        report.get("input_identity_sha256").and_then(Value::as_str),
        Some(identity_digest.as_str()),
        "the report binds exactly the identity sidecar value"
    );
    let identity_text = identity.to_string();
    assert!(!identity_text.contains(&identity_digest));
    assert!(!identity_text.contains(&report_digest));
    assert!(
        identity.get("bound_inputs").is_some()
            && !identity_text.contains("reviewer_receipts")
            && !identity_text.contains("final_manifest"),
        "the identity excludes the report, the receipts and the final manifest"
    );

    // Producer provenance is recorded on every qualification path, and nothing
    // attributes the final emission to Todo 35.
    for document in [&bindings, &identity, &report] {
        assert_eq!(
            document.pointer("/producer/name").and_then(Value::as_str),
            Some("produce-qualification")
        );
        assert_eq!(
            document
                .pointer("/producer/binary_sha256")
                .and_then(Value::as_str)
                .map(str::len),
            Some(64)
        );
        assert!(
            !document
                .to_string()
                .to_ascii_lowercase()
                .contains("todo 35"),
            "no field attributes final emission to Todo 35"
        );
    }

    // Complete evidence and parity: every named category is proven at exit 0.
    let names: Vec<String> = report
        .get("gates")
        .and_then(Value::as_array)
        .expect("evidence array")
        .iter()
        .map(|gate| {
            assert_eq!(gate.get("exit_code").and_then(Value::as_i64), Some(0));
            gate.get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        })
        .collect();
    for category in support::REQUIRED_EVIDENCE {
        assert!(
            names.iter().any(|name| name == category),
            "the report must prove `{category}`, got {names:?}"
        );
    }
    assert_eq!(
        report.get("result"),
        Some(&Value::String("PASS".to_owned()))
    );

    // Both sidecars are exactly 64 lowercase hex digits plus one LF.
    for artifact in [
        &fixture.release_gates,
        &fixture.input_identity,
        &fixture.qualification_report,
    ] {
        let bytes = std::fs::read(sidecar(artifact)).expect("sidecar");
        assert_eq!(bytes.len(), 65);
        assert_eq!(bytes[64], b'\n');
        assert_eq!(
            String::from_utf8_lossy(&bytes[..64]),
            file_sha256(artifact),
            "{} sidecar records its own bytes",
            artifact.display()
        );
    }

    // The producer wrote no F1-F5 output.
    for checker in CHECKERS {
        assert!(!round.receipt(checker).exists());
    }
    assert!(!fixture.final_dir.join("final-manifest.json").exists());

    // All four verifiers document success: exit 0, APPROVE, hash-only receipt.
    round.approve_all();
    let digests = support::bound_digests(fixture);
    for checker in CHECKERS {
        let receipt = read_json(&round.receipt(checker));
        assert_eq!(
            receipt.get("verdict").and_then(Value::as_str),
            Some("APPROVE")
        );
        assert_eq!(
            receipt.get("input_identity_sha256").and_then(Value::as_str),
            Some(digests["identity"].as_str())
        );
        assert_eq!(
            receipt
                .get("qualification_report_sha256")
                .and_then(Value::as_str),
            Some(digests["report"].as_str())
        );
        assert_eq!(
            receipt.get("before_bound_hash"),
            receipt.get("after_bound_hash")
        );
        let text = receipt.to_string();
        for forbidden in ["bound_inputs", "candidate_commit", "runner_identity"] {
            assert!(
                !text.contains(forbidden),
                "{checker}'s receipt never re-embeds `{forbidden}`"
            );
        }
    }

    // The sixth binary documents success: one schema-valid, byte-stable manifest.
    let manifest = fixture.final_dir.join("final-manifest.json");
    let first = fixture.run_finalizer();
    assert!(
        first.succeeded(),
        "finalize-approval must succeed on four APPROVEs: {}",
        first.stderr
    );
    let bytes = std::fs::read(&manifest).expect("the final manifest");
    let document: Value = serde_json::from_slice(&bytes).expect("manifest JSON");
    assert_eq!(document.get("approved"), Some(&Value::Bool(true)));
    assert_eq!(
        document
            .get("reviewer_receipts")
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(4)
    );
    let second = fixture.run_finalizer();
    assert!(second.succeeded());
    assert_eq!(
        std::fs::read(&manifest).expect("the manifest survives a rerun"),
        bytes,
        "re-running against identical inputs is byte-identical"
    );
}

#[test]
#[ignore = "needs the maintainer's .omo workspace; qa/tasks/37.json runs it"]
fn e2e_qualification_failure_paths() {
    build_tools();

    producer_stage_and_publication_faults();
    producer_precreated_f_outputs();
    producer_mutated_inputs();
    verifier_and_finalizer_reject_fixtures();
}

/// A failure injected before and within each producer stage, and at each
/// publication rename, exits nonzero with all seven output paths absent.
fn producer_stage_and_publication_faults() {
    let root = temp();
    let fixture = Fixture::build(root.path());
    let points = [
        "pre-flight",
        "stage0-before",
        "stage0-within",
        "stage1-before",
        "stage1-within",
        "stage2-before",
        "stage2-within",
        "stage3-before",
        "stage3-within",
        "stage4-before",
        "stage4-within",
        "stage5-before",
        "stage5-within",
        "publish-0",
        "publish-1",
        "publish-2",
        "publish-3",
        "publish-4",
        "publish-5",
        "publish-6",
        "publish-7",
    ];
    for point in points {
        let outcome = fixture.run_producer(Some(point));
        assert!(
            outcome.failed(),
            "an injected failure at {point} must abort the producer"
        );
        let published: Vec<String> = fixture
            .outputs()
            .iter()
            .filter(|path| path.exists())
            .map(|path| path.display().to_string())
            .collect();
        if published.is_empty() {
            continue;
        }
        assert_eq!(
            point, "publish-7",
            "only a fault after the last rename may leave output behind, found {published:?}"
        );
        let recovery = fixture.run_producer(None);
        assert!(
            recovery.failed() && recovery.stderr.contains("interrupted publication"),
            "an interrupted publication is detected and rejected, not resumed: {}",
            recovery.stderr
        );
        assert!(
            fixture.all_outputs_absent(),
            "the interrupted publication rolls back to all-seven-absent"
        );
    }
    assert!(fixture.all_outputs_absent());
}

/// Precreating each F1–F5 output in turn stops the producer before it publishes
/// anything, and never modifies the precreated file.
fn producer_precreated_f_outputs() {
    let root = temp();
    let fixture = Fixture::build(root.path());
    let mut slots: Vec<std::path::PathBuf> = Vec::new();
    for checker in CHECKERS {
        slots.push(fixture.final_dir.join(format!("{checker}.json")));
        slots.push(fixture.final_dir.join(format!("{checker}.sha256")));
    }
    slots.push(fixture.final_dir.join("final-manifest.json"));
    for slot in &slots {
        write(slot, b"precreated\n");
        let outcome = fixture.run_producer(None);
        assert!(
            outcome.failed(),
            "a preexisting {} must stop the producer",
            slot.display()
        );
        assert!(fixture.all_outputs_absent());
        assert_eq!(
            std::fs::read(slot).expect("the precreated file survives"),
            b"precreated\n",
            "the producer never synthesizes or modifies a review receipt"
        );
        std::fs::remove_file(slot).expect("clear the precreated slot");
    }
    // The producer's own seven slots are equally refused.
    for slot in fixture.outputs() {
        write(&slot, b"precreated\n");
        assert!(fixture.run_producer(None).failed());
        std::fs::remove_file(&slot).expect("clear the precreated slot");
    }
}

/// Mutating each identity, review, tool or runner input exits nonzero with all
/// seven output paths absent.
fn producer_mutated_inputs() {
    type Mutation = Box<dyn Fn(&Fixture)>;
    let cases: Vec<(&str, Mutation)> = vec![
        (
            "candidate commit",
            Box::new(|fixture: &Fixture| {
                mutate(&fixture.candidate_identity, |document| {
                    document["commit"] = json!("3333333333333333333333333333333333333333");
                });
            }),
        ),
        (
            "candidate tree copied into the commit field",
            Box::new(|fixture: &Fixture| {
                mutate(&fixture.candidate_identity, |document| {
                    document["commit"] = json!(support::CANDIDATE_TREE);
                });
            }),
        ),
        (
            "a non-approving architecture receipt",
            Box::new(|fixture: &Fixture| {
                let mut value =
                    support::architecture_receipt("momus", "round-fixture-1", &fixture.plan_sha256);
                value["verdict"] = json!("REJECT");
                write_pair(&fixture.momus_receipt, &value);
            }),
        ),
        (
            "an architecture receipt with an open Blocker",
            Box::new(|fixture: &Fixture| {
                let mut value =
                    support::architecture_receipt("momus", "round-fixture-1", &fixture.plan_sha256);
                value["findings"] = json!([{
                    "severity": "Blocker",
                    "status": "open",
                    "summary": "an unresolved defect",
                }]);
                write_pair(&fixture.momus_receipt, &value);
            }),
        ),
        (
            "a rejected historical round bound as current",
            Box::new(|fixture: &Fixture| {
                let value =
                    support::architecture_receipt("momus", "round-2-rejected", &"a".repeat(64));
                write_pair(&fixture.momus_receipt, &value);
            }),
        ),
        (
            "a reused architecture reviewer",
            Box::new(|fixture: &Fixture| {
                let value =
                    support::architecture_receipt("momus", "round-fixture-1", &fixture.plan_sha256);
                write_pair(&fixture.codex_receipt, &value);
            }),
        ),
        (
            "a stale review sidecar",
            Box::new(|fixture: &Fixture| {
                write(
                    &sidecar(&fixture.codex_receipt),
                    format!("{}\n", "b".repeat(64)).as_bytes(),
                );
            }),
        ),
        (
            "a B3 approval for the wrong plan",
            Box::new(|fixture: &Fixture| {
                write_pair(
                    &fixture.b3_approval,
                    &support::batch_approval("b3", &"c".repeat(64)),
                );
            }),
        ),
        (
            "a B4 approval that reuses B3's bundle",
            Box::new(|fixture: &Fixture| {
                let mut value = support::batch_approval("b4", &fixture.plan_sha256);
                value["bundle_sha256"] = json!(support::sha256_hex(b"bundle-b3"));
                write_pair(&fixture.b4_approval, &value);
            }),
        ),
        (
            "a B3 approval that is not approved",
            Box::new(|fixture: &Fixture| {
                let mut value = support::batch_approval("b3", &fixture.plan_sha256);
                value["approved"] = json!(false);
                write_pair(&fixture.b3_approval, &value);
            }),
        ),
        (
            "a Red proof that never failed",
            Box::new(|fixture: &Fixture| {
                mutate(&fixture.red_evidence_index, |document| {
                    document["entries"][0]["red_exit_code"] = json!(0);
                });
            }),
        ),
        (
            "a Red proof claimed for a cancelled todo",
            Box::new(|fixture: &Fixture| {
                mutate(&fixture.red_evidence_index, |document| {
                    document["entries"][0]["todo"] = json!(22);
                });
            }),
        ),
        (
            "a tool lock digest that no longer matches",
            Box::new(|fixture: &Fixture| {
                let manifest = fixture.archives_dir.join("SHA256SUMS.txt");
                let text = std::fs::read_to_string(&manifest).expect("checksum manifest");
                let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
                let mut parts: Vec<String> =
                    lines[0].split_whitespace().map(str::to_owned).collect();
                parts[0] = "d".repeat(64);
                lines[0] = parts.join("  ");
                write(&manifest, format!("{}\n", lines.join("\n")).as_bytes());
            }),
        ),
        (
            "a runner identity that is not bound to an environment",
            Box::new(|fixture: &Fixture| {
                let observations = fixture
                    .candidate_worktree
                    .join("ci")
                    .join("observations")
                    .join("all-required-checks-passed.json");
                let mut document = read_json(&observations);
                if let Some(records) = document.as_array_mut() {
                    records[0]["runner_identity"] =
                        json!({ "arch": "", "os": "", "os_version": "" });
                }
                write(&observations, &canonical(&document));
            }),
        ),
        (
            "a required check reported as unavailable",
            Box::new(|fixture: &Fixture| {
                let observations = fixture
                    .candidate_worktree
                    .join("ci")
                    .join("observations")
                    .join("all-required-checks-passed.json");
                let mut document = read_json(&observations);
                if let Some(records) = document.as_array_mut() {
                    records[0]["observation"] = json!("unavailable");
                }
                write(&observations, &canonical(&document));
            }),
        ),
        (
            "a required check that exited nonzero",
            Box::new(|fixture: &Fixture| {
                let observations = fixture
                    .candidate_worktree
                    .join("ci")
                    .join("observations")
                    .join("all-required-checks-passed.json");
                let mut document = read_json(&observations);
                if let Some(records) = document.as_array_mut() {
                    records[0]["exit_code"] = json!(1);
                }
                write(&observations, &canonical(&document));
            }),
        ),
        (
            "a failing qualification evidence step",
            Box::new(|fixture: &Fixture| {
                write(
                    &fixture.evidence_plan,
                    &canonical(&failing_step_plan("auth")),
                );
            }),
        ),
    ];

    for (label, mutation) in cases {
        let root = temp();
        let fixture = Fixture::build(root.path());
        mutation(&fixture);
        let outcome = fixture.run_producer(None);
        assert!(
            outcome.failed(),
            "the producer must reject {label}: {}",
            outcome.stdout
        );
        assert!(
            fixture.all_outputs_absent(),
            "all seven final producer output paths stay absent after rejecting {label}"
        );
        assert!(
            outcome.stdout.trim().is_empty(),
            "a rejected producer writes no stdout for {label}"
        );
        for checker in CHECKERS {
            assert!(
                !fixture.final_dir.join(format!("{checker}.json")).exists(),
                "the producer never synthesizes a review receipt ({label})"
            );
        }
    }

    // An unknown flag, a repeated flag and a positional argument are usage errors.
    let root = temp();
    let fixture = Fixture::build(root.path());
    for extra in [
        vec!["--allow-unavailable".to_owned(), "1".to_owned()],
        vec![
            "--plan".to_owned(),
            fixture.plan.to_string_lossy().to_string(),
        ],
        vec!["positional".to_owned()],
    ] {
        let mut arguments = fixture.producer_args();
        arguments.extend(extra.clone());
        let outcome = std::process::Command::new(binary("produce-qualification"))
            .args(&arguments)
            .current_dir(&fixture.workspace)
            .output()
            .expect("the producer runs");
        assert_eq!(outcome.status.code(), Some(2), "{extra:?} is a usage error");
        assert!(fixture.all_outputs_absent());
    }
}

/// Each verifier and the finalizer, fed a named reject fixture, exits nonzero and
/// never synthesizes or modifies a review receipt.
fn verifier_and_finalizer_reject_fixtures() {
    verifier_bound_input_rejections();
    verifier_named_reject_fixtures();
    finalizer_rejections();
}

fn verifier_bound_input_rejections() {
    type Mutation = Box<dyn Fn(&Fixture)>;
    let cases: Vec<(&str, Mutation)> = vec![
        (
            "a mutated identity body",
            Box::new(|fixture: &Fixture| {
                mutate(&fixture.input_identity, |document| {
                    document["candidate_commit"] =
                        json!("5555555555555555555555555555555555555555");
                });
            }),
        ),
        (
            "a mutated report body",
            Box::new(|fixture: &Fixture| {
                mutate(&fixture.qualification_report, |document| {
                    document["result"] = json!("FAIL");
                });
            }),
        ),
        (
            "an identity with no producer provenance",
            Box::new(|fixture: &Fixture| {
                let mut document = read_json(&fixture.input_identity);
                if let Some(object) = document.as_object_mut() {
                    object.remove("producer");
                }
                write_pair(&fixture.input_identity, &document);
            }),
        ),
        (
            "a removed report sidecar",
            Box::new(|fixture: &Fixture| {
                std::fs::remove_file(sidecar(&fixture.qualification_report))
                    .expect("remove the sidecar");
            }),
        ),
    ];
    for checker in CHECKERS {
        for (label, mutation) in &cases {
            let round = Round::produce();
            mutation(&round.fixture);
            let outcome = round.run(checker);
            assert!(
                outcome.failed(),
                "{checker} must reject {label}: {}\n{}",
                outcome.stdout,
                outcome.stderr
            );
            assert!(
                !round.receipt(checker).exists(),
                "{checker} writes no receipt for {label}"
            );
        }
        // A preexisting output is refused and left byte-identical.
        let round = Round::produce();
        write(&round.receipt(checker), b"precreated\n");
        assert!(round.run(checker).failed());
        assert_eq!(
            std::fs::read(round.receipt(checker)).expect("untouched"),
            b"precreated\n"
        );
        assert!(!sidecar(&round.receipt(checker)).exists());
    }
}

fn verifier_named_reject_fixtures() {
    // F1: a sibling receipt bound to another round cannot belong to this one.
    let round = Round::produce();
    write_pair(
        &round.fixture.final_dir.join("f3-real-qa.json"),
        &json!({
            "after_bound_hash": "a".repeat(64),
            "before_bound_hash": "a".repeat(64),
            "candidate_input": {
                "kind": "archive",
                "path": "/foreign/archive.tar.gz",
                "archive_name": "archive.tar.gz",
                "archive_sha256": "d".repeat(64),
            },
            "findings": [],
            "input_identity_sha256": "b".repeat(64),
            "launch_id": "foreign-launch",
            "qualification_report_sha256": "c".repeat(64),
            "round_id": "foreign-round",
            "schema_version": "1",
            "verdict": "APPROVE",
            "verifier_identity": "f3-real-qa",
        }),
    );
    let outcome = round.run("f1-plan-compliance");
    assert!(outcome.failed(), "F1 rejects a foreign sibling receipt");
    assert_eq!(
        read_json(&round.receipt("f1-plan-compliance"))
            .get("verdict")
            .and_then(Value::as_str),
        Some("REJECT")
    );

    // F2: a nonzero aggregate gate is a REJECT that names the failing gate.
    let round = Round::produce();
    let plan = round.root.path().join("failing-gate-plan.json");
    write(&plan, &canonical(&failing_step_plan("trust-boundary")));
    let output = round.receipt("f2-code-quality");
    let outcome = std::process::Command::new(binary("f2-code-quality"))
        .arg("--identity")
        .arg(&round.fixture.input_identity)
        .arg("--report")
        .arg(&round.fixture.qualification_report)
        .arg("--worktree")
        .arg(&round.worktree)
        .arg("--output")
        .arg(&output)
        .current_dir(&round.worktree)
        .env("GODOT_MCP_QUALIFICATION_GATE_PLAN", &plan)
        .output()
        .expect("f2 runs");
    assert_eq!(outcome.status.code(), Some(1));
    let verdict: Value = serde_json::from_slice(&outcome.stdout).expect("verdict JSON");
    assert_eq!(
        verdict.get("verdict").and_then(Value::as_str),
        Some("REJECT")
    );
    assert!(
        verdict
            .get("failing_gates")
            .and_then(Value::as_array)
            .expect("failing_gates")
            .iter()
            .any(|entry| entry
                .as_str()
                .is_some_and(|text| text.contains("trust-boundary")))
    );

    // F3: a package whose bytes no longer match its own manifest is a REJECT.
    let round = Round::produce();
    write(
        &round.package.join("examples").join("godot-mcp.toml"),
        b"[server]\nprofile = \"full\"\n",
    );
    round.rearchive_and_rebind();
    assert!(round.run("f3-real-qa").failed());
    assert_eq!(
        read_json(&round.receipt("f3-real-qa"))
            .get("verdict")
            .and_then(Value::as_str),
        Some("REJECT")
    );

    // F4: a cancelled capability presented as delivered is a creep match, while a
    // half-exposed reserved configuration surface is the same defect. The
    // cancelled capabilities are the first release's: that candidate is one
    // without the contract 0.2 discovery manifests, which deliver them.
    for (name, body, expected, first_release) in [
        (
            "qa-f4-delivered.md",
            "# Intruder\n\nThis release provides viewport screenshot capture and \
             EditorUndoRedoManager edits today.\n",
            "qa-f4-delivered",
            true,
        ),
        (
            "qa-f4-endpoint.md",
            "# Intruder\n\nSet `bridge.endpoint` to the address the server listens on.\n",
            "no runtime effect",
            false,
        ),
    ] {
        let round = Round::produce();
        let complete = round.worktree.join("schemas/domain/profiles/v0.2");
        if first_release && complete.exists() {
            std::fs::remove_dir_all(&complete).expect("drop the 0.2 manifests");
        }
        write(
            &round.worktree.join("docs").join("operator").join(name),
            body.as_bytes(),
        );
        commit_candidate_changes(&round.worktree);
        rebind_qualification_candidate(&round.fixture, &round.worktree);
        let outcome = round.run("f4-scope-fidelity");
        assert!(outcome.failed(), "F4 must match {name}");
        assert!(
            outcome
                .json()
                .get("creep_matches")
                .and_then(Value::as_array)
                .expect("creep_matches")
                .iter()
                .any(|entry| entry.as_str().is_some_and(|text| text.contains(expected))),
            "the match names {expected}"
        );
    }

    // F4 must NOT demand the absent screenshot capabilities.
    let round = Round::produce();
    let outcome = round.run("f4-scope-fidelity");
    assert!(
        outcome.succeeded(),
        "the absence of cancelled screenshot/recording capabilities is not a match: {}\n{}",
        outcome.stdout,
        outcome.stderr
    );
}

fn finalizer_rejections() {
    type RoundMutation = Box<dyn Fn(&Round)>;
    let cases: Vec<(&str, RoundMutation)> = vec![
        (
            "a missing F3 receipt",
            Box::new(|round: &Round| {
                std::fs::remove_file(round.receipt("f3-real-qa")).expect("remove F3");
            }),
        ),
        (
            "a mismatched report hash",
            Box::new(|round: &Round| {
                let mut document = read_json(&round.receipt("f1-plan-compliance"));
                document["qualification_report_sha256"] = json!("a".repeat(64));
                write_pair(&round.receipt("f1-plan-compliance"), &document);
            }),
        ),
        (
            "a reused receipt",
            Box::new(|round: &Round| {
                let borrowed = std::fs::read(round.receipt("f1-plan-compliance")).expect("read F1");
                write(&round.receipt("f4-scope-fidelity"), &borrowed);
                write(
                    &sidecar(&round.receipt("f4-scope-fidelity")),
                    format!("{}\n", support::sha256_hex(&borrowed)).as_bytes(),
                );
            }),
        ),
        (
            "a non-APPROVE receipt",
            Box::new(|round: &Round| {
                let mut document = read_json(&round.receipt("f2-code-quality"));
                document["verdict"] = json!("REJECT");
                write_pair(&round.receipt("f2-code-quality"), &document);
            }),
        ),
        (
            "an open Blocker anywhere in the receipts",
            Box::new(|round: &Round| {
                let mut document = read_json(&round.receipt("f2-code-quality"));
                document["findings"] = json!([{
                    "severity": "Blocker",
                    "status": "open",
                    "summary": "an unresolved defect",
                }]);
                write_pair(&round.receipt("f2-code-quality"), &document);
            }),
        ),
        (
            "an open Major anywhere in the receipts",
            Box::new(|round: &Round| {
                let mut document = read_json(&round.receipt("f3-real-qa"));
                document["findings"] = json!([{
                    "severity": "Major",
                    "status": "open",
                    "summary": "an unresolved gap",
                }]);
                write_pair(&round.receipt("f3-real-qa"), &document);
            }),
        ),
        (
            "receipt-time evidence mutation",
            Box::new(|round: &Round| {
                let mut document = read_json(&round.receipt("f4-scope-fidelity"));
                document["after_bound_hash"] = json!("b".repeat(64));
                write_pair(&round.receipt("f4-scope-fidelity"), &document);
            }),
        ),
        (
            "a missing required release gate",
            Box::new(|round: &Round| {
                let mut document = read_json(&round.fixture.release_gates);
                if let Some(checks) = document.get_mut("checks").and_then(Value::as_array_mut) {
                    checks[0]["observation"] = json!("missing");
                }
                write_pair(&round.fixture.release_gates, &document);
            }),
        ),
        (
            "a nonzero required release gate",
            Box::new(|round: &Round| {
                let mut document = read_json(&round.fixture.release_gates);
                if let Some(checks) = document.get_mut("checks").and_then(Value::as_array_mut) {
                    checks[0]["exit_code"] = json!(2);
                }
                write_pair(&round.fixture.release_gates, &document);
            }),
        ),
        (
            "replayed release-gate evidence from another tree",
            Box::new(|round: &Round| {
                let mut document = read_json(&round.fixture.release_gates);
                if let Some(checks) = document.get_mut("checks").and_then(Value::as_array_mut) {
                    checks[0]["source_tree"] = json!("9999999999999999999999999999999999999999");
                }
                write_pair(&round.fixture.release_gates, &document);
            }),
        ),
        (
            "a historical architecture round substituted for the current one",
            Box::new(|round: &Round| {
                let mut document = read_json(&round.fixture.review_bindings);
                document["architecture_review"]["round_id"] = json!("round-3-rejected");
                write(&round.fixture.review_bindings, &canonical(&document));
            }),
        ),
        (
            "toolchain drift in the frozen identity",
            Box::new(|round: &Round| {
                let mut document = read_json(&round.fixture.input_identity);
                document["rust_toolchain"]["content_hash"] = json!("c".repeat(64));
                write_pair(&round.fixture.input_identity, &document);
            }),
        ),
        (
            "runner drift in the frozen identity",
            Box::new(|round: &Round| {
                let mut document = read_json(&round.fixture.input_identity);
                document["runner_identity"]["arch"] = json!("pdp11");
                write_pair(&round.fixture.input_identity, &document);
            }),
        ),
    ];

    for (label, mutation) in cases {
        let round = Round::produce();
        round.approve_all();
        let before = round.receipt_snapshot();
        mutation(&round);
        let after_mutation = round.receipt_snapshot();
        let manifest = round.fixture.final_dir.join("final-manifest.json");
        let outcome = round.fixture.run_finalizer();
        assert!(
            outcome.failed(),
            "finalization must reject {label}: {}",
            outcome.stdout
        );
        assert!(
            !manifest.exists(),
            "no manifest is written when finalization rejects {label}"
        );
        assert_eq!(
            round.receipt_snapshot(),
            after_mutation,
            "the finalizer never synthesizes or modifies a review receipt ({label})"
        );
        assert!(
            before.len() == after_mutation.len(),
            "the receipt set is not resized by the finalizer ({label})"
        );
    }

    // A different preexisting manifest is refused and left untouched.
    let round = Round::produce();
    round.approve_all();
    let manifest = round.fixture.final_dir.join("final-manifest.json");
    write(&manifest, b"{\"approved\":true}");
    assert!(round.fixture.run_finalizer().failed());
    assert_eq!(
        std::fs::read(&manifest).expect("untouched"),
        b"{\"approved\":true}"
    );
}
