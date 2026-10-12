//! Real-logic tests for the six Todo-37-owned `qualification-tools` binaries.
//!
//! Every case runs a real built binary as a real process against a fixture tree in
//! a temporary directory, and asserts on its real exit code, its real stdout and
//! the real files it left behind. Nothing is inferred from source text, because a
//! declaration is not verified until the binary runs.
//!
//! No test writes into the real `.omo/evidence/godot-mcp-server-architecture/**`
//! tree; the committed Red/Green artifacts there are read only.

#[allow(dead_code)]
mod support;

use std::path::Path;

/// One fixture mutation applied before a binary runs.
type Mutation = Box<dyn Fn(&Fixture)>;

/// One published-round mutation applied before the finalizer runs.
type RoundMutation = Box<dyn Fn(&Round)>;

use serde_json::{Value, json};
use support::{
    CHECKERS, Fixture, HOST_ARCHIVE, archive_package, binary, bind_candidate_identity,
    bind_host_archive_observation, build_release_package, canonical, clean_candidate_worktree,
    commit_candidate_changes, failing_step_plan, fake_fixture_cargo, fake_package, file_sha256,
    mutate, read_json, rebind_qualification_candidate, refresh_archive_checksums, sidecar, write,
};

/// Build the frozen Todo 35 harness, then a complete fixture rooted at `root`.
fn fixture(root: &Path) -> Fixture {
    build_release_package();
    Fixture::build(root)
}

fn temp() -> tempfile::TempDir {
    tempfile::tempdir().expect("temporary fixture root")
}

// ---------------------------------------------------------------------------
// produce-qualification
// ---------------------------------------------------------------------------

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn produce_qualification_publishes_the_seven_paths_in_forward_order() {
    let root = temp();
    let fixture = fixture(root.path());
    assert!(fixture.all_outputs_absent());

    let outcome = fixture.run_producer(None);
    assert!(
        outcome.succeeded(),
        "the producer must succeed against a frozen fixture candidate: {}\n{}",
        outcome.stderr,
        outcome.stdout
    );
    for path in fixture.outputs() {
        assert!(path.is_file(), "{} must be published", path.display());
    }

    let summary = outcome.json();
    let identity = read_json(&fixture.input_identity);
    let report = read_json(&fixture.qualification_report);
    let bindings = read_json(&fixture.review_bindings);

    for document in [&identity, &report, &bindings] {
        assert_eq!(
            document.pointer("/producer/name").and_then(Value::as_str),
            Some("produce-qualification"),
            "every qualification path declares the Todo-37-owned producer"
        );
        assert_eq!(
            document
                .pointer("/producer/binary_sha256")
                .and_then(Value::as_str)
                .map(str::len),
            Some(64),
            "the producer binary hash is bound"
        );
    }

    // Stage order, proved from content hashes rather than timestamps: the gates
    // hash is bound by the identity, and the identity hash is bound by the report.
    let gates_digest = support::file_sha256(&fixture.release_gates);
    assert_eq!(
        identity.get("release_gates_sha256").and_then(Value::as_str),
        Some(gates_digest.as_str())
    );
    assert_eq!(
        identity
            .pointer("/bound_inputs/release-gates/sha256")
            .and_then(Value::as_str),
        Some(gates_digest.as_str())
    );
    assert_eq!(
        identity
            .pointer("/bound_inputs/review-bindings/sha256")
            .and_then(Value::as_str),
        Some(support::file_sha256(&fixture.review_bindings).as_str())
    );
    let identity_digest = support::file_sha256(&fixture.input_identity);
    assert_eq!(
        report.get("input_identity_sha256").and_then(Value::as_str),
        Some(identity_digest.as_str())
    );
    assert_eq!(
        summary.get("input_identity_sha256").and_then(Value::as_str),
        Some(identity_digest.as_str())
    );

    // No artifact hashes itself or a later output.
    let report_digest = support::file_sha256(&fixture.qualification_report);
    let identity_text = identity.to_string();
    assert!(
        !identity_text.contains(&identity_digest),
        "the identity never hashes itself"
    );
    assert!(
        !identity_text.contains(&report_digest),
        "the identity never binds the report that follows it"
    );

    // Both sidecars carry exactly 64 lowercase hex digits plus one LF.
    for artifact in [
        &fixture.release_gates,
        &fixture.input_identity,
        &fixture.qualification_report,
    ] {
        let bytes = std::fs::read(sidecar(artifact)).expect("sidecar bytes");
        assert_eq!(
            bytes.len(),
            65,
            "{} sidecar is 65 bytes",
            artifact.display()
        );
        assert_eq!(bytes[64], b'\n');
        assert_eq!(
            String::from_utf8_lossy(&bytes[..64]),
            support::file_sha256(artifact)
        );
    }

    // The producer never writes an F1-F5 output.
    for checker in CHECKERS {
        assert!(!fixture.final_dir.join(format!("{checker}.json")).exists());
    }
    assert!(!fixture.final_dir.join("final-manifest.json").exists());

    // The report proves every named evidence category, including the computed
    // identity-mutation probe the producer appends.
    let names: Vec<String> = report
        .get("gates")
        .and_then(Value::as_array)
        .expect("gates array")
        .iter()
        .map(|gate| {
            gate.get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        })
        .collect();
    for required in support::REQUIRED_EVIDENCE {
        assert!(
            names.iter().any(|name| name == required),
            "the report must prove `{required}`, got {names:?}"
        );
    }
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn produce_qualification_rejects_every_preexisting_output_slot() {
    let root = temp();
    let fixture = fixture(root.path());
    let outputs = fixture.outputs();
    for (index, slot) in outputs.iter().enumerate() {
        write(slot, b"stale\n");
        let outcome = fixture.run_producer(None);
        assert!(
            outcome.failed(),
            "a preexisting {} must be rejected",
            slot.display()
        );
        for (other, path) in outputs.iter().enumerate() {
            if other != index {
                assert!(
                    !path.exists(),
                    "{} must stay absent when the run is rejected",
                    path.display()
                );
            }
        }
        std::fs::remove_file(slot).expect("clear the precreated slot");
    }
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn produce_qualification_rejects_every_preexisting_f1_to_f5_output() {
    let root = temp();
    let fixture = fixture(root.path());
    let mut slots: Vec<std::path::PathBuf> = Vec::new();
    for checker in CHECKERS {
        slots.push(fixture.final_dir.join(format!("{checker}.json")));
        slots.push(fixture.final_dir.join(format!("{checker}.sha256")));
    }
    slots.push(fixture.final_dir.join("final-manifest.json"));
    for slot in &slots {
        write(slot, b"stale\n");
        let outcome = fixture.run_producer(None);
        assert!(
            outcome.failed(),
            "a preexisting {} must stop the producer before any output is published",
            slot.display()
        );
        assert!(
            fixture.all_outputs_absent(),
            "all seven producer outputs stay absent when an F1-F5 output preexists"
        );
        std::fs::remove_file(slot).expect("clear the precreated receipt");
    }
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn produce_qualification_rolls_back_every_injected_stage_and_publication_fault() {
    let root = temp();
    let fixture = fixture(root.path());
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
        assert!(
            outcome.stderr.contains(point) || outcome.stderr.contains("interrupted publication"),
            "the failure at {point} is reported: {}",
            outcome.stderr
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
        // A fault injected after the final rename cannot un-publish what already
        // landed, so the journal makes the next startup detect and remove it.
        assert_eq!(
            point, "publish-7",
            "only a fault after the last rename may leave output behind, found {published:?} at \
             {point}"
        );
        let recovery = fixture.run_producer(None);
        assert!(
            recovery.failed(),
            "an interrupted publication is rejected, not resumed"
        );
        assert!(
            recovery.stderr.contains("interrupted publication"),
            "the interrupted publication is named: {}",
            recovery.stderr
        );
        assert!(
            fixture.all_outputs_absent(),
            "the interrupted publication is rolled back to all-seven-absent"
        );
    }
    assert!(fixture.all_outputs_absent());
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn produce_qualification_rejects_every_mutated_identity_review_tool_and_runner_input() {
    build_release_package();

    let cases: Vec<(&str, Mutation)> = vec![
        (
            "candidate identity commit",
            Box::new(|fixture: &Fixture| {
                mutate(&fixture.candidate_identity, |document| {
                    document["commit"] = json!("3333333333333333333333333333333333333333");
                });
            }),
        ),
        (
            "candidate identity commit equal to tree",
            Box::new(|fixture: &Fixture| {
                mutate(&fixture.candidate_identity, |document| {
                    document["commit"] = json!(support::CANDIDATE_TREE);
                });
            }),
        ),
        (
            "architecture receipt plan hash",
            Box::new(|fixture: &Fixture| {
                let value =
                    support::architecture_receipt("momus", "round-fixture-1", &"a".repeat(64));
                support::write_pair(&fixture.momus_receipt, &value);
            }),
        ),
        (
            "architecture receipt verdict",
            Box::new(|fixture: &Fixture| {
                let mut value =
                    support::architecture_receipt("momus", "round-fixture-1", &fixture.plan_sha256);
                value["verdict"] = json!("REJECT");
                support::write_pair(&fixture.momus_receipt, &value);
            }),
        ),
        (
            "architecture receipt open Major",
            Box::new(|fixture: &Fixture| {
                let mut value =
                    support::architecture_receipt("momus", "round-fixture-1", &fixture.plan_sha256);
                value["findings"] = json!([{
                    "severity": "Major",
                    "status": "open",
                    "summary": "an unresolved gap",
                }]);
                support::write_pair(&fixture.momus_receipt, &value);
            }),
        ),
        (
            "architecture receipts from different rounds",
            Box::new(|fixture: &Fixture| {
                let value =
                    support::architecture_receipt("codex", "round-fixture-2", &fixture.plan_sha256);
                support::write_pair(&fixture.codex_receipt, &value);
            }),
        ),
        (
            "reused architecture reviewer identity",
            Box::new(|fixture: &Fixture| {
                let value =
                    support::architecture_receipt("momus", "round-fixture-1", &fixture.plan_sha256);
                support::write_pair(&fixture.codex_receipt, &value);
            }),
        ),
        (
            "stale architecture sidecar",
            Box::new(|fixture: &Fixture| {
                write(
                    &sidecar(&fixture.momus_receipt),
                    format!("{}\n", "b".repeat(64)).as_bytes(),
                );
            }),
        ),
        (
            "b3 approval plan hash",
            Box::new(|fixture: &Fixture| {
                support::write_pair(
                    &fixture.b3_approval,
                    &support::batch_approval("b3", &"c".repeat(64)),
                );
            }),
        ),
        (
            "b4 approval wrong batch",
            Box::new(|fixture: &Fixture| {
                support::write_pair(
                    &fixture.b4_approval,
                    &support::batch_approval("b3", &fixture.plan_sha256),
                );
            }),
        ),
        (
            "b3 and b4 approvals reuse one bundle",
            Box::new(|fixture: &Fixture| {
                let mut value = support::batch_approval("b4", &fixture.plan_sha256);
                value["bundle_sha256"] = json!(support::sha256_hex(b"bundle-b3"));
                support::write_pair(&fixture.b4_approval, &value);
            }),
        ),
        (
            "b3 approval reuses one receipt",
            Box::new(|fixture: &Fixture| {
                let mut value = support::batch_approval("b3", &fixture.plan_sha256);
                let reused = value["receipts"][0]["receipt_sha256"].clone();
                value["receipts"][1]["receipt_sha256"] = reused;
                support::write_pair(&fixture.b3_approval, &value);
            }),
        ),
        (
            "b3 approval not approved",
            Box::new(|fixture: &Fixture| {
                let mut value = support::batch_approval("b3", &fixture.plan_sha256);
                value["approved"] = json!(false);
                support::write_pair(&fixture.b3_approval, &value);
            }),
        ),
        (
            "red evidence index zero red exit",
            Box::new(|fixture: &Fixture| {
                mutate(&fixture.red_evidence_index, |document| {
                    document["entries"][0]["red_exit_code"] = json!(0);
                });
            }),
        ),
        (
            "red evidence index nonzero green exit",
            Box::new(|fixture: &Fixture| {
                mutate(&fixture.red_evidence_index, |document| {
                    document["entries"][0]["green_exit_code"] = json!(1);
                });
            }),
        ),
        (
            "red evidence index wrong artifact digest",
            Box::new(|fixture: &Fixture| {
                mutate(&fixture.red_evidence_index, |document| {
                    document["entries"][0]["red_output_sha256"] = json!("d".repeat(64));
                });
            }),
        ),
        (
            "red evidence index claims a cancelled todo",
            Box::new(|fixture: &Fixture| {
                mutate(&fixture.red_evidence_index, |document| {
                    document["entries"][0]["todo"] = json!(24);
                });
            }),
        ),
        (
            "archive checksum manifest digest",
            Box::new(|fixture: &Fixture| {
                let manifest = fixture.archives_dir.join("SHA256SUMS.txt");
                let text = std::fs::read_to_string(&manifest).expect("checksum manifest");
                let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
                let mut parts: Vec<String> =
                    lines[0].split_whitespace().map(str::to_owned).collect();
                parts[0] = "e".repeat(64);
                lines[0] = parts.join("  ");
                write(&manifest, format!("{}\n", lines.join("\n")).as_bytes());
            }),
        ),
        (
            "candidate observation source commit",
            Box::new(|fixture: &Fixture| {
                let observations = fixture
                    .candidate_worktree
                    .join("ci")
                    .join("observations")
                    .join("all-required-checks-passed.json");
                let mut document = read_json(&observations);
                if let Some(records) = document.as_array_mut() {
                    records[0]["source_commit"] = json!("4444444444444444444444444444444444444444");
                }
                write(&observations, &canonical(&document));
            }),
        ),
        (
            "candidate observation is skipped",
            Box::new(|fixture: &Fixture| {
                let observations = fixture
                    .candidate_worktree
                    .join("ci")
                    .join("observations")
                    .join("all-required-checks-passed.json");
                let mut document = read_json(&observations);
                if let Some(records) = document.as_array_mut() {
                    records[0]["observation"] = json!("skipped");
                }
                write(&observations, &canonical(&document));
            }),
        ),
        (
            "candidate observation runner identity",
            Box::new(|fixture: &Fixture| {
                let observations = fixture
                    .candidate_worktree
                    .join("ci")
                    .join("observations")
                    .join("all-required-checks-passed.json");
                let mut document = read_json(&observations);
                if let Some(records) = document.as_array_mut() {
                    records[0]["runner_identity"] =
                        json!({ "os": "", "os_version": "", "arch": "" });
                }
                write(&observations, &canonical(&document));
            }),
        ),
        (
            "one qualification evidence step fails",
            Box::new(|fixture: &Fixture| {
                write(
                    &fixture.evidence_plan,
                    &canonical(&failing_step_plan("trust-boundary")),
                );
            }),
        ),
    ];

    for (label, mutation) in cases {
        let case_root = temp();
        let case = Fixture::build(case_root.path());
        mutation(&case);
        let outcome = case.run_producer(None);
        assert!(
            outcome.failed(),
            "the producer must reject a mutated {label}: {}",
            outcome.stdout
        );
        assert!(
            case.all_outputs_absent(),
            "all seven outputs stay absent after rejecting a mutated {label}"
        );
        assert!(
            outcome.stdout.trim().is_empty(),
            "a rejected producer writes no stdout for {label}: {}",
            outcome.stdout
        );
    }
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn produce_qualification_rejects_an_unknown_or_repeated_flag() {
    let root = temp();
    let fixture = fixture(root.path());
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
        assert_eq!(
            outcome.status.code(),
            Some(2),
            "{extra:?} must be a usage error: {}",
            String::from_utf8_lossy(&outcome.stderr)
        );
        assert!(outcome.stdout.is_empty());
        assert!(fixture.all_outputs_absent());
    }
}

// ---------------------------------------------------------------------------
// F1-F4 and F5
// ---------------------------------------------------------------------------

/// One published qualification round plus the four approving receipts.
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
        let fixture = fixture(root.path());
        let worktree = clean_candidate_worktree(root.path());
        bind_candidate_identity(&fixture, &worktree);
        let package = fake_package(root.path());
        let archive = fixture.archives_dir.join(HOST_ARCHIVE);
        archive_package(&package, &archive);
        bind_host_archive_observation(&fixture, &archive);
        refresh_archive_checksums(&fixture.archives_dir);
        let outcome = fixture.run_producer(None);
        assert!(
            outcome.succeeded(),
            "the round's producer must succeed: {}",
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

    fn run(&self, checker: &str) -> support::Outcome {
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
            .arg(self.fixture.final_dir.join(format!("{checker}.json")))
            .current_dir(&self.worktree)
            .env("GODOT_MCP_QUALIFICATION_GATE_PLAN", &self.gate_plan);
        support::Outcome::of_command(command)
    }

    fn run_f3_with_archive(&self, archive: &Path) -> support::Outcome {
        let output = self.receipt("f3-real-qa");
        let mut command = std::process::Command::new(binary("f3-real-qa"));
        command
            .arg("--identity")
            .arg(&self.fixture.input_identity)
            .arg("--report")
            .arg(&self.fixture.qualification_report)
            .arg("--archive")
            .arg(archive)
            .arg("--godot-lock")
            .arg(&self.fixture.godot_lock)
            .arg("--output")
            .arg(output)
            .current_dir(&self.worktree)
            .env("GODOT_MCP_QUALIFICATION_GATE_PLAN", &self.gate_plan);
        support::Outcome::of_command(command)
    }

    fn rebind_candidate(&self, commit: &str, tree: &str) {
        let mut identity = read_json(&self.fixture.input_identity);
        identity["candidate_commit"] = json!(commit);
        identity["candidate_tree"] = json!(tree);
        let identity_sha = support::write_pair(&self.fixture.input_identity, &identity);
        let mut report = read_json(&self.fixture.qualification_report);
        report["input_identity_sha256"] = json!(identity_sha);
        support::write_pair(&self.fixture.qualification_report, &report);
    }

    fn rearchive_and_rebind(&self) {
        archive_package(&self.package, &self.archive);
        let digest = file_sha256(&self.archive);
        let mut identity = read_json(&self.fixture.input_identity);
        identity["bound_inputs"][format!("archive:{HOST_ARCHIVE}")]["sha256"] = json!(digest);
        let identity_sha = support::write_pair(&self.fixture.input_identity, &identity);
        let mut report = read_json(&self.fixture.qualification_report);
        report["input_identity_sha256"] = json!(identity_sha);
        support::write_pair(&self.fixture.qualification_report, &report);
    }

    fn approve_all(&self) {
        for checker in CHECKERS {
            let outcome = self.run(checker);
            assert!(
                outcome.succeeded(),
                "{checker} must APPROVE this round: {}\n{}\n{:#}",
                outcome.stdout,
                outcome.stderr,
                read_json(&self.receipt(checker))
            );
            assert_eq!(
                outcome.json().get("verdict").and_then(Value::as_str),
                Some("APPROVE")
            );
        }
    }

    fn receipt(&self, checker: &str) -> std::path::PathBuf {
        self.fixture.final_dir.join(format!("{checker}.json"))
    }
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn every_checker_approves_a_clean_round_and_writes_only_its_own_hash_only_receipt() {
    let round = Round::produce();
    let digests = support::bound_digests(&round.fixture);
    for checker in CHECKERS {
        let outcome = round.run(checker);
        assert!(
            outcome.succeeded(),
            "{checker} must exit 0 for APPROVE: {}\n{}\n{:#}",
            outcome.stdout,
            outcome.stderr,
            read_json(&round.receipt(checker))
        );
        let receipt = read_json(&round.receipt(checker));
        assert_eq!(
            receipt.get("verdict").and_then(Value::as_str),
            Some("APPROVE")
        );
        assert_eq!(
            receipt.get("verifier_identity").and_then(Value::as_str),
            Some(checker)
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
            receipt.get("after_bound_hash"),
            "{checker} must not mutate its own bound evidence"
        );
        // The receipt binds hashes only: neither document body is re-embedded.
        let text = receipt.to_string();
        for forbidden in [
            "bound_inputs",
            "candidate_commit",
            "\"gates\"",
            "runner_identity",
        ] {
            assert!(
                !text.contains(forbidden),
                "{checker}'s receipt must never re-embed `{forbidden}`"
            );
        }
        let bytes = std::fs::read(sidecar(&round.receipt(checker))).expect("receipt sidecar");
        assert_eq!(bytes.len(), 65);
        assert_eq!(
            String::from_utf8_lossy(&bytes[..64]),
            support::file_sha256(&round.receipt(checker))
        );
    }
    // Four independent round/launch ids, none reused across checkers.
    let mut ids: Vec<String> = Vec::new();
    for checker in CHECKERS {
        let receipt = read_json(&round.receipt(checker));
        for key in ["round_id", "launch_id"] {
            let id = receipt
                .get(key)
                .and_then(Value::as_str)
                .expect("an id")
                .to_owned();
            assert!(!ids.contains(&id), "{checker}'s {key} is reused");
            ids.push(id);
        }
    }
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn every_checker_refuses_a_preexisting_output_and_writes_nothing() {
    let round = Round::produce();
    for checker in CHECKERS {
        let output = round.receipt(checker);
        write(&output, b"stale\n");
        let outcome = round.run(checker);
        assert!(
            outcome.failed(),
            "{checker} must refuse a preexisting output"
        );
        assert_eq!(
            std::fs::read(&output).expect("the precreated file is untouched"),
            b"stale\n"
        );
        assert!(
            !sidecar(&output).exists(),
            "{checker} writes no sidecar when it refuses"
        );
        std::fs::remove_file(&output).expect("clear the precreated output");
    }
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn every_checker_rejects_a_mutated_or_unverifiable_bound_input() {
    for checker in CHECKERS {
        for (label, mutation) in mutation_cases() {
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
                "{checker} writes no receipt when its bound input is unusable ({label})"
            );
        }
    }
}

fn mutation_cases() -> Vec<(&'static str, Mutation)> {
    vec![
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
            "a stale identity sidecar",
            Box::new(|fixture: &Fixture| {
                write(
                    &sidecar(&fixture.input_identity),
                    format!("{}\n", "f".repeat(64)).as_bytes(),
                );
            }),
        ),
        (
            "a removed report sidecar",
            Box::new(|fixture: &Fixture| {
                std::fs::remove_file(sidecar(&fixture.qualification_report))
                    .expect("remove the report sidecar");
            }),
        ),
        (
            "a report bound to another identity",
            Box::new(|fixture: &Fixture| {
                let mut document = read_json(&fixture.qualification_report);
                document["input_identity_sha256"] = json!("a".repeat(64));
                support::write_pair(&fixture.qualification_report, &document);
            }),
        ),
        (
            "an identity with no producer provenance",
            Box::new(|fixture: &Fixture| {
                let mut document = read_json(&fixture.input_identity);
                if let Some(object) = document.as_object_mut() {
                    object.remove("producer");
                }
                support::write_pair(&fixture.input_identity, &document);
            }),
        ),
    ]
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn f1_rejects_a_receipt_bound_to_another_round_and_an_uncheckable_plan() {
    let round = Round::produce();
    // A sibling receipt from a different round binds different digests, which is
    // exactly how a receipt that predates the qualification artifacts is caught.
    let foreign = round.fixture.final_dir.join("f2-code-quality.json");
    support::write_pair(
        &foreign,
        &json!({
            "after_bound_hash": "a".repeat(64),
            "before_bound_hash": "a".repeat(64),
            "candidate_input": {
                "kind": "worktree",
                "path": "/foreign/worktree",
                "resolved_commit": "d".repeat(40),
                "resolved_tree": "e".repeat(40),
                "tracked_clean": true,
            },
            "findings": [],
            "input_identity_sha256": "b".repeat(64),
            "launch_id": "foreign",
            "qualification_report_sha256": "c".repeat(64),
            "round_id": "foreign",
            "schema_version": "1",
            "verdict": "APPROVE",
            "verifier_identity": "f2-code-quality",
        }),
    );
    let outcome = round.run("f1-plan-compliance");
    assert!(
        outcome.failed(),
        "F1 must reject a sibling receipt from another round"
    );
    let receipt = read_json(&round.receipt("f1-plan-compliance"));
    assert_eq!(
        receipt.get("verdict").and_then(Value::as_str),
        Some("REJECT")
    );
    assert!(
        !receipt
            .get("findings")
            .and_then(Value::as_array)
            .expect("findings")
            .is_empty(),
        "a REJECT records why"
    );
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn f2_aggregates_real_gate_exit_codes_and_never_waives_one() {
    let round = Round::produce();
    for failing in [
        "parity-stdio-http",
        "trust-boundary",
        "durable-transaction",
        "auth",
    ] {
        let plan = round.root.path().join(format!("gate-plan-{failing}.json"));
        write(&plan, &canonical(&failing_step_plan(failing)));
        let output = round.fixture.final_dir.join("f2-code-quality.json");
        let _ = std::fs::remove_file(&output);
        let _ = std::fs::remove_file(sidecar(&output));
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
        assert_eq!(
            outcome.status.code(),
            Some(1),
            "a nonzero aggregate gate must be a REJECT with exit 1"
        );
        let document: Value = serde_json::from_slice(&outcome.stdout).expect("verdict JSON");
        assert_eq!(
            document.get("verdict").and_then(Value::as_str),
            Some("REJECT")
        );
        let failing_gates = document
            .get("failing_gates")
            .and_then(Value::as_array)
            .expect("failing_gates");
        assert!(
            failing_gates
                .iter()
                .any(|entry| entry.as_str().is_some_and(|text| text.contains(failing))),
            "the failing gate is named: {failing_gates:?}"
        );
        assert!(read_json(&output).get("verdict").and_then(Value::as_str) == Some("REJECT"));
    }
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn f2_rejects_a_worktree_that_is_not_the_identity_candidate() {
    let round = Round::produce();
    round.rebind_candidate(&"a".repeat(40), &"b".repeat(40));

    let outcome = round.run("f2-code-quality");

    assert!(
        outcome.failed(),
        "F2 must reject when the audited worktree is not the identity candidate"
    );
    let receipt = read_json(&round.receipt("f2-code-quality"));
    let findings = receipt["findings"].as_array().expect("F2 findings");
    assert!(
        findings.iter().any(|finding| finding["summary"]
            .as_str()
            .is_some_and(|summary| summary.contains("candidate_commit"))),
        "F2 must name the candidate commit mismatch, got {findings:?}"
    );
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn f2_rejects_a_worktree_with_tracked_modifications() {
    let round = Round::produce();
    write(
        &round.worktree.join("README.md"),
        b"tracked candidate drift\n",
    );

    let outcome = round.run("f2-code-quality");

    assert!(
        outcome.failed(),
        "F2 must reject tracked modifications before running any audit gate"
    );
    let receipt = read_json(&round.receipt("f2-code-quality"));
    let findings = receipt["findings"].as_array().expect("F2 findings");
    assert!(
        findings.iter().any(|finding| finding["summary"]
            .as_str()
            .is_some_and(|summary| summary.contains("tracked modifications"))),
        "F2 must name tracked worktree modifications, got {findings:?}"
    );
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn f2_seams_are_refused_for_an_evidence_tree_output() {
    let round = Round::produce();
    let resolved = round.worktree.join(Path::new(
        ".omo/evidence/godot-mcp-server-architecture/final/f2-seam-probe.json",
    ));
    let before = std::fs::read(&resolved).ok();
    let outcome = std::process::Command::new(binary("f2-code-quality"))
        .arg("--identity")
        .arg(&round.fixture.input_identity)
        .arg("--report")
        .arg(&round.fixture.qualification_report)
        .arg("--worktree")
        .arg(&round.worktree)
        .arg("--output")
        .arg(&resolved)
        .current_dir(&round.worktree)
        .env("GODOT_MCP_QUALIFICATION_GATE_PLAN", &round.gate_plan)
        .output()
        .expect("f2 runs");
    assert!(
        !outcome.status.success(),
        "a substituted catalogue is refused when the output is the real evidence path"
    );
    assert!(
        String::from_utf8_lossy(&outcome.stderr).contains(
            "GODOT_MCP_QUALIFICATION_GATE_PLAN is set, but this invocation writes under \
             `.omo/evidence/`"
        ),
        "the refusal identifies the substituted-catalogue evidence-tree seam: {}",
        String::from_utf8_lossy(&outcome.stderr)
    );
    let after = std::fs::read(&resolved).ok();
    let failure = if before.is_some() {
        "f2 overwrote a file that was present in the real evidence tree"
    } else {
        "f2 created a file that was absent in the real evidence tree"
    };
    assert_eq!(after, before, "{failure}");
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn f3_rejects_a_package_whose_bytes_do_not_match_its_manifest() {
    let round = Round::produce();
    let payload = round.package.join("examples").join("godot-mcp.toml");
    write(&payload, b"[server]\nprofile = \"full\"\n");
    round.rearchive_and_rebind();
    let outcome = round.run("f3-real-qa");
    assert!(
        outcome.failed(),
        "F3 must reject a package whose payload no longer matches its manifest"
    );
    let receipt = read_json(&round.receipt("f3-real-qa"));
    assert_eq!(
        receipt.get("verdict").and_then(Value::as_str),
        Some("REJECT")
    );
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn f3_drives_the_manifest_executable_when_the_required_supervisor_is_also_packaged() {
    let round = Round::produce();
    let output = round.receipt("f3-real-qa");
    let cargo = fake_fixture_cargo(round.root.path());
    let outcome = std::process::Command::new(binary("f3-real-qa"))
        .arg("--identity")
        .arg(&round.fixture.input_identity)
        .arg("--report")
        .arg(&round.fixture.qualification_report)
        .arg("--archive")
        .arg(&round.archive)
        .arg("--godot-lock")
        .arg(&round.fixture.godot_lock)
        .arg("--output")
        .arg(&output)
        .current_dir(&round.worktree)
        .env("CARGO", cargo)
        .env("GODOT_MCP_QA_FIXTURE_ROOT", round.root.path())
        .output()
        .expect("f3 runs");

    assert_eq!(
        outcome.status.code(),
        Some(0),
        "F3 must drive MANIFEST.executable rather than reject or run the supervisor: {}\n{}",
        String::from_utf8_lossy(&outcome.stdout),
        String::from_utf8_lossy(&outcome.stderr)
    );
    assert_eq!(
        read_json(&output).get("verdict").and_then(Value::as_str),
        Some("APPROVE")
    );
    assert!(
        !round.root.path().join("task34-fixture.active").exists(),
        "F3 must clean up the fresh Task-34 fixture"
    );
    assert!(
        round.root.path().join("task34-fixture.cleaned").is_file(),
        "the exact Task-34 cleanup command must run"
    );
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn f3_rejects_and_names_an_undeclared_binary_entry() {
    let round = Round::produce();
    let planted = round.package.join("bin").join("qa-fixture-34");
    write(&planted, b"undeclared test hook\n");
    round.rearchive_and_rebind();

    let outcome = round.run("f3-real-qa");
    assert!(
        outcome.failed(),
        "an undeclared binary must remain a REJECT"
    );
    let receipt = read_json(&round.receipt("f3-real-qa"));
    let findings = receipt
        .get("findings")
        .and_then(Value::as_array)
        .expect("F3 findings");
    assert!(
        findings.iter().any(|finding| finding
            .get("summary")
            .and_then(Value::as_str)
            .is_some_and(|summary| summary.contains("bin/qa-fixture-34"))),
        "the undeclared entry must be named, got {findings:?}"
    );
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn f3_requires_the_supervisor_declaration_to_be_digest_recorded() {
    let round = Round::produce();
    let manifest = round.package.join("MANIFEST.json");
    let mut document = read_json(&manifest);
    document
        .as_object_mut()
        .expect("manifest object")
        .remove("supervisor");
    write(&manifest, &canonical(&document));
    round.rearchive_and_rebind();

    let outcome = round.run("f3-real-qa");
    assert!(
        outcome.failed(),
        "a missing supervisor declaration must reject"
    );
    let receipt = read_json(&round.receipt("f3-real-qa"));
    let findings = receipt
        .get("findings")
        .and_then(Value::as_array)
        .expect("F3 findings");
    assert!(
        findings.iter().any(|finding| finding
            .get("summary")
            .and_then(Value::as_str)
            .is_some_and(|summary| summary.contains("required supervisor"))),
        "the required supervisor declaration must be named, got {findings:?}"
    );
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn f3_rejects_a_lock_without_the_exact_locked_engine() {
    let round = Round::produce();
    let lock = round.root.path().join("godot-releases.lock.json");
    let mut document = read_json(&round.fixture.godot_lock);
    if let Some(releases) = document.get_mut("releases").and_then(Value::as_array_mut) {
        releases.retain(|release| release.get("version").and_then(Value::as_str) != Some("4.7.1"));
    }
    write(&lock, &canonical(&document));
    let output = round.fixture.final_dir.join("f3-real-qa.json");
    let outcome = std::process::Command::new(binary("f3-real-qa"))
        .arg("--identity")
        .arg(&round.fixture.input_identity)
        .arg("--report")
        .arg(&round.fixture.qualification_report)
        .arg("--archive")
        .arg(&round.archive)
        .arg("--godot-lock")
        .arg(&lock)
        .arg("--output")
        .arg(&output)
        .current_dir(&round.worktree)
        .env("GODOT_MCP_QUALIFICATION_GATE_PLAN", &round.gate_plan)
        .output()
        .expect("f3 runs");
    assert_eq!(outcome.status.code(), Some(1));
    assert_eq!(
        read_json(&output).get("verdict").and_then(Value::as_str),
        Some("REJECT")
    );
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn f3_rejects_an_archive_whose_digest_is_not_bound_by_the_identity() {
    let round = Round::produce();
    let unrelated = round.root.path().join("unrelated").join(HOST_ARCHIVE);
    std::fs::create_dir_all(unrelated.parent().expect("unrelated archive parent"))
        .expect("create unrelated archive directory");
    std::fs::copy(&round.archive, &unrelated).expect("copy bound archive");
    use std::io::Write as _;
    std::fs::OpenOptions::new()
        .append(true)
        .open(&unrelated)
        .expect("open unrelated archive")
        .write_all(b"unbound bytes")
        .expect("mutate unrelated archive");

    let outcome = round.run_f3_with_archive(&unrelated);

    assert!(
        outcome.failed(),
        "F3 must reject an archive whose digest differs from the identity binding"
    );
    let receipt = read_json(&round.receipt("f3-real-qa"));
    let findings = receipt["findings"].as_array().expect("F3 findings");
    assert!(
        findings.iter().any(|finding| finding["summary"]
            .as_str()
            .is_some_and(|summary| summary.contains("archive SHA-256"))),
        "F3 must name the bound archive digest mismatch, got {findings:?}"
    );
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn f3_approves_the_correctly_bound_archive() {
    let round = Round::produce();

    let outcome = round.run("f3-real-qa");

    assert!(
        outcome.succeeded(),
        "F3 must approve the correctly bound archive: {}\n{}",
        outcome.stdout,
        outcome.stderr
    );
    let receipt = read_json(&round.receipt("f3-real-qa"));
    assert_eq!(receipt["verdict"], json!("APPROVE"));
}

/// Make the round's candidate a first-release one: without the contract 0.2
/// discovery manifests the cancelled-plane rules are the ones that apply.
fn as_first_release(round: &Round) {
    let complete = round.worktree.join("schemas/domain/profiles/v0.2");
    if complete.exists() {
        std::fs::remove_dir_all(&complete).expect("drop the 0.2 manifests");
    }
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn f4_matches_a_cancelled_capability_presented_as_delivered() {
    let round = Round::produce();
    as_first_release(&round);
    support::write(
        &round
            .worktree
            .join("docs")
            .join("operator")
            .join("qa-f4-intruder.md"),
        b"# Fixture intruder\n\nThis release provides viewport screenshot capture and \
          EditorUndoRedoManager edits today.\n",
    );
    commit_candidate_changes(&round.worktree);
    rebind_qualification_candidate(&round.fixture, &round.worktree);
    let outcome = round.run("f4-scope-fidelity");
    assert!(
        outcome.failed(),
        "F4 must match a doc presenting a cancelled capability as delivered"
    );
    let document = outcome.json();
    let matches = document
        .get("creep_matches")
        .and_then(Value::as_array)
        .expect("creep_matches");
    assert!(
        matches.iter().any(|entry| entry
            .as_str()
            .is_some_and(|text| text.contains("qa-f4-intruder"))),
        "the matching document is named: {matches:?}"
    );
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn f4_takes_the_delivered_planes_as_the_complete_releases_scope() {
    let round = Round::produce();
    // A complete-release candidate delivers these capabilities, so saying so
    // is not creep.
    support::write(
        &round
            .worktree
            .join("docs")
            .join("operator")
            .join("qa-f4-delivered.md"),
        b"# Fixture delivered\n\nThis release provides viewport screenshot capture and \
          EditorUndoRedoManager edits today.\n",
    );
    commit_candidate_changes(&round.worktree);
    rebind_qualification_candidate(&round.fixture, &round.worktree);
    let outcome = round.run("f4-scope-fidelity");
    assert!(
        outcome.succeeded(),
        "a complete release names what it delivers: {}\n{}",
        outcome.stdout,
        outcome.stderr
    );

    // Its full profile must discover every delivered plane.
    let manifest = round
        .worktree
        .join("schemas/domain/profiles/v0.2/full.json");
    let mut profile = read_json(&manifest);
    let tools = profile["tools"].as_array_mut().expect("tools");
    tools.retain(|tool| tool != "project.export");
    write(&manifest, &canonical(&profile));
    commit_candidate_changes(&round.worktree);
    rebind_qualification_candidate(&round.fixture, &round.worktree);
    let _ = std::fs::remove_file(round.fixture.final_dir.join("f4-scope-fidelity.json"));
    let _ = std::fs::remove_file(sidecar(
        &round.fixture.final_dir.join("f4-scope-fidelity.json"),
    ));
    let outcome = round.run("f4-scope-fidelity");
    assert!(
        outcome.failed(),
        "a delivered plane that is not discovered is a REJECT"
    );
    let receipt = serde_json::to_string(&read_json(&round.receipt("f4-scope-fidelity")))
        .expect("receipt text");
    assert!(
        receipt.contains("project.export"),
        "the undiscovered tool is named: {receipt}"
    );
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn f2_proves_the_delivered_planes_of_a_complete_candidate() {
    let round = Round::produce();
    std::fs::remove_file(
        round
            .worktree
            .join("crates/godot-mcp-bridge/addon/godot_mcp/plugin.cfg"),
    )
    .expect("drop the add-on's plugin.cfg");
    // A native descriptor inside the isolated lane is allowed; one anywhere
    // else is not.
    support::write(
        &round.worktree.join("gdext-reference/qa/spike.gdextension"),
        b"[configuration]\n",
    );
    support::write(
        &round.worktree.join("native/spike.gdextension"),
        b"[configuration]\n",
    );
    commit_candidate_changes(&round.worktree);
    rebind_qualification_candidate(&round.fixture, &round.worktree);
    let outcome = round.run("f2-code-quality");
    assert!(outcome.failed(), "a missing delivered plane is a REJECT");
    let receipt = read_json(&round.receipt("f2-code-quality"));
    let findings = serde_json::to_string(&receipt).expect("receipt text");
    assert!(
        findings.contains("plugin.cfg"),
        "the missing source is named: {findings}"
    );
    assert!(
        findings.contains("native/spike.gdextension"),
        "a descriptor outside the lane is named: {findings}"
    );
    assert!(
        !findings.contains("gdext-reference/qa/spike.gdextension"),
        "the isolated lane's descriptor is allowed: {findings}"
    );
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn f4_matches_a_half_exposed_reserved_configuration_surface() {
    let round = Round::produce();
    support::write(
        &round
            .worktree
            .join("docs")
            .join("operator")
            .join("qa-f4-endpoint.md"),
        b"# Fixture endpoint\n\nSet `bridge.endpoint` to the address the server listens on.\n",
    );
    commit_candidate_changes(&round.worktree);
    rebind_qualification_candidate(&round.fixture, &round.worktree);
    let outcome = round.run("f4-scope-fidelity");
    assert!(
        outcome.failed(),
        "describing bridge.endpoint as an effective endpoint is a REJECT"
    );
    let document = outcome.json();
    assert!(
        document
            .get("creep_matches")
            .and_then(Value::as_array)
            .expect("creep_matches")
            .iter()
            .any(|entry| entry
                .as_str()
                .is_some_and(|text| text.contains("no runtime effect"))),
        "the reserved-key requirement is named"
    );
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn f4_does_not_demand_the_absent_screenshot_capabilities() {
    let round = Round::produce();
    let outcome = round.run("f4-scope-fidelity");
    assert!(
        outcome.succeeded(),
        "the absence of cancelled screenshot/recording capabilities is not a scope-fidelity \
         match: {}\n{}",
        outcome.stdout,
        outcome.stderr
    );
    assert!(
        outcome
            .json()
            .get("creep_matches")
            .and_then(Value::as_array)
            .expect("creep_matches")
            .is_empty()
    );
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn finalize_approval_writes_one_byte_stable_manifest() {
    let round = Round::produce();
    round.approve_all();
    let manifest = round.fixture.final_dir.join("final-manifest.json");

    let first = round.fixture.run_finalizer();
    assert!(
        first.succeeded(),
        "four APPROVEs plus complete bindings and passing gates must finalize: {}",
        first.stderr
    );
    let bytes = std::fs::read(&manifest).expect("the manifest exists");
    let document: Value = serde_json::from_slice(&bytes).expect("manifest JSON");
    assert_eq!(document.get("approved"), Some(&Value::Bool(true)));
    assert_eq!(
        document
            .get("reviewer_receipts")
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(4)
    );
    assert_eq!(
        document
            .get("batch_records")
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(2)
    );

    // Only final-manifest.json was written.
    let mut written: Vec<String> = std::fs::read_dir(&round.fixture.final_dir)
        .expect("final directory")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect();
    written.sort();
    let mut expected: Vec<String> = vec!["final-manifest.json".to_owned()];
    for checker in CHECKERS {
        expected.push(format!("{checker}.json"));
        expected.push(format!("{checker}.sha256"));
    }
    expected.sort();
    assert_eq!(written, expected);

    let second = round.fixture.run_finalizer();
    assert!(second.succeeded(), "a rerun is accepted: {}", second.stderr);
    assert_eq!(
        std::fs::read(&manifest).expect("the manifest still exists"),
        bytes,
        "re-running against identical inputs is byte-identical"
    );
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn finalize_approval_rejects_every_named_condition_with_no_output() {
    let manifest_name = "final-manifest.json";
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
                support::write_pair(&round.receipt("f1-plan-compliance"), &document);
            }),
        ),
        (
            "a reused receipt",
            Box::new(|round: &Round| {
                let borrowed =
                    std::fs::read(round.receipt("f1-plan-compliance")).expect("read F1's receipt");
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
                support::write_pair(&round.receipt("f2-code-quality"), &document);
            }),
        ),
        (
            "an open Blocker in a receipt",
            Box::new(|round: &Round| {
                let mut document = read_json(&round.receipt("f2-code-quality"));
                document["findings"] = json!([{
                    "severity": "Blocker",
                    "status": "open",
                    "summary": "an unresolved defect",
                }]);
                support::write_pair(&round.receipt("f2-code-quality"), &document);
            }),
        ),
        (
            "an open Major in a receipt",
            Box::new(|round: &Round| {
                let mut document = read_json(&round.receipt("f3-real-qa"));
                document["findings"] = json!([{
                    "severity": "Major",
                    "status": "open",
                    "summary": "an unresolved gap",
                }]);
                support::write_pair(&round.receipt("f3-real-qa"), &document);
            }),
        ),
        (
            "a stale receipt sidecar",
            Box::new(|round: &Round| {
                write(
                    &sidecar(&round.receipt("f4-scope-fidelity")),
                    format!("{}\n", "b".repeat(64)).as_bytes(),
                );
            }),
        ),
        (
            "receipt-time evidence mutation",
            Box::new(|round: &Round| {
                let mut document = read_json(&round.receipt("f1-plan-compliance"));
                document["after_bound_hash"] = json!("c".repeat(64));
                support::write_pair(&round.receipt("f1-plan-compliance"), &document);
            }),
        ),
        (
            "a skipped required release gate",
            Box::new(|round: &Round| {
                let mut document = read_json(&round.fixture.release_gates);
                if let Some(checks) = document.get_mut("checks").and_then(Value::as_array_mut) {
                    checks[0]["observation"] = json!("skipped");
                }
                support::write_pair(&round.fixture.release_gates, &document);
            }),
        ),
        (
            "a nonzero required release gate",
            Box::new(|round: &Round| {
                let mut document = read_json(&round.fixture.release_gates);
                if let Some(checks) = document.get_mut("checks").and_then(Value::as_array_mut) {
                    checks[0]["exit_code"] = json!(1);
                }
                support::write_pair(&round.fixture.release_gates, &document);
            }),
        ),
        (
            "replayed release-gate evidence from another tree",
            Box::new(|round: &Round| {
                let mut document = read_json(&round.fixture.release_gates);
                if let Some(checks) = document.get_mut("checks").and_then(Value::as_array_mut) {
                    checks[0]["source_tree"] = json!("9999999999999999999999999999999999999999");
                }
                support::write_pair(&round.fixture.release_gates, &document);
            }),
        ),
        (
            "a release-gates sidecar that does not verify",
            Box::new(|round: &Round| {
                write(
                    &sidecar(&round.fixture.release_gates),
                    format!("{}\n", "d".repeat(64)).as_bytes(),
                );
            }),
        ),
        (
            "a historical architecture round substituted into the bindings",
            Box::new(|round: &Round| {
                let mut document = read_json(&round.fixture.review_bindings);
                document["architecture_review"]["round_id"] = json!("round-2-rejected");
                write(&round.fixture.review_bindings, &canonical(&document));
            }),
        ),
        (
            "a reused batch receipt hash",
            Box::new(|round: &Round| {
                let mut document = read_json(&round.fixture.review_bindings);
                let reused = document["batch_reviews"][0]["receipt_hashes"][0].clone();
                document["batch_reviews"][0]["receipt_hashes"][1] = reused;
                write(&round.fixture.review_bindings, &canonical(&document));
            }),
        ),
        (
            "toolchain drift in the frozen identity",
            Box::new(|round: &Round| {
                let mut document = read_json(&round.fixture.input_identity);
                document["rust_toolchain"]["content_hash"] = json!("e".repeat(64));
                support::write_pair(&round.fixture.input_identity, &document);
            }),
        ),
        (
            "runner drift in the frozen identity",
            Box::new(|round: &Round| {
                let mut document = read_json(&round.fixture.input_identity);
                document["runner_identity"]["os"] = json!("PlanNine");
                support::write_pair(&round.fixture.input_identity, &document);
            }),
        ),
    ];

    for (label, mutation) in cases {
        let round = Round::produce();
        round.approve_all();
        mutation(&round);
        let manifest = round.fixture.final_dir.join(manifest_name);
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
    }
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn finalize_approval_rejects_a_different_preexisting_manifest() {
    let round = Round::produce();
    round.approve_all();
    let manifest = round.fixture.final_dir.join("final-manifest.json");
    write(&manifest, b"{\"approved\":true}");
    let outcome = round.fixture.run_finalizer();
    assert!(outcome.failed());
    assert_eq!(
        std::fs::read(&manifest).expect("the preexisting file is untouched"),
        b"{\"approved\":true}"
    );
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn every_binary_rejects_an_unknown_flag_with_a_usage_error() {
    for name in [
        "produce-qualification",
        "f1-plan-compliance",
        "f2-code-quality",
        "f3-real-qa",
        "f4-scope-fidelity",
        "finalize-approval",
    ] {
        let outcome = std::process::Command::new(binary(name))
            .args(["--not-a-flag", "value"])
            .output()
            .expect("the binary runs");
        assert_eq!(
            outcome.status.code(),
            Some(2),
            "{name} must report a usage error for an unknown flag"
        );
        assert!(outcome.stdout.is_empty(), "{name} writes no stdout");
        assert!(
            String::from_utf8_lossy(&outcome.stderr).contains("unknown argument"),
            "{name} names the unknown argument"
        );
    }
}
