//! Todo 6 acceptance suite: stdio parity plus the frozen private effect
//! foundation with a fully working durable journal.
//!
//! Both descriptor-owned tests are exhaustive by design: `journal_foundation.rs`
//! is the mechanical entry point behind `make qa-task TASK=6`, so every acceptance
//! criterion is asserted here rather than spread across ad-hoc tests.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

use godot_mcp_domain::CapabilityProfile;
use godot_mcp_domain::config::Limits;
use godot_mcp_domain::contracts::PrincipalId;
use godot_mcp_server::stdio::{ProjectRuntime, StdioOptions};
#[cfg(target_os = "linux")]
use godot_mcp_server::test_support::ObservedTransaction;
use godot_mcp_server::test_support::{
    EffectBoundary, FaultInjection, JournalFoundationHarness, JournalPhase, MockFileEffect,
    MockFileOperation, RecoveryOperation, TerminalDisposition,
};
use serde_json::{Value, json};

const FIXTURE: &str = "tests/fixtures/minimal";
const SCHEMA: &str = include_str!("../../../schemas/transactions/v1.json");

/// Fixed transaction IDs, so every assertion names an exact durable record.
const TX_CREATE: &str = "00000000-0000-4000-8000-000000000601";
const TX_UPDATE: &str = "00000000-0000-4000-8000-000000000602";
const TX_DELETE: &str = "00000000-0000-4000-8000-000000000603";
const TX_REOPEN: &str = "00000000-0000-4000-8000-000000000604";
const TX_COMPOSITE: &str = "00000000-0000-4000-8000-000000000605";
const TX_HELPER: &str = "00000000-0000-4000-8000-000000000606";
const TX_FAULT: &str = "00000000-0000-4000-8000-000000000607";
const TX_PARTIAL: &str = "00000000-0000-4000-8000-000000000608";
const TX_ILLEGAL: &str = "00000000-0000-4000-8000-000000000609";
const TX_STALE: &str = "00000000-0000-4000-8000-00000000060a";
const TX_BLOCKED: &str = "00000000-0000-4000-8000-00000000060b";
const TX_COMPOSITE_BAD: &str = "00000000-0000-4000-8000-00000000060c";
const TX_HELPER_BAD: &str = "00000000-0000-4000-8000-00000000060d";
const TX_SAME_BYTES: &str = "00000000-0000-4000-8000-00000000060e";
const TX_INTENT_CREATE: &str = "00000000-0000-4000-8000-00000000060f";
const TX_INTENT_UPDATE: &str = "00000000-0000-4000-8000-000000000614";
const TX_INTENT_CREATE_MISMATCH: &str = "00000000-0000-4000-8000-000000000615";
const TX_INTENT_UPDATE_MISMATCH: &str = "00000000-0000-4000-8000-000000000616";
/// One transaction per non-`file` boundary probe, so each has its own record.
const TX_BOUNDARY: [&str; 4] = [
    "00000000-0000-4000-8000-000000000610",
    "00000000-0000-4000-8000-000000000611",
    "00000000-0000-4000-8000-000000000612",
    "00000000-0000-4000-8000-000000000613",
];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root is two levels above the crate manifest")
        .to_path_buf()
}

fn fixture_source() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE)
}

/// Copy the committed fixture into a writable temp root so a test can mutate it
/// without touching the repository copy.
fn writable_project(label: &str) -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::Builder::new()
        .prefix(&format!("godot-mcp-task6-{label}-"))
        .tempdir()
        .expect("temp project root");
    let root = temp.path().join("project");
    std::fs::create_dir_all(&root).expect("project root");
    std::fs::copy(
        fixture_source().join("project.godot"),
        root.join("project.godot"),
    )
    .expect("copy project marker");
    // Todo 2's `SafeRoot` publishes files relative to an already-held parent
    // directory handle; it deliberately owns no directory-creation primitive, so
    // the fixture provides the destination directory the mock effects write into.
    std::fs::create_dir_all(root.join("scenes")).expect("scenes directory");
    (temp, root)
}

fn create_effect(transaction_id: &str, path: &str, bytes: &[u8]) -> MockFileEffect {
    MockFileEffect {
        transaction_id: transaction_id.to_owned(),
        path: path.to_owned(),
        operation: MockFileOperation::Create,
        bytes: bytes.to_vec(),
        required_process_transaction: None,
    }
}

// ---------------------------------------------------------------------------
// Happy path
// ---------------------------------------------------------------------------

#[test]
fn journal_foundation_happy_path() {
    // 1. The canonical schema is a real restrictive schema, not a permissive stub.
    let schema: Value = serde_json::from_str(SCHEMA).expect("canonical schema parses");
    assert_eq!(
        schema["additionalProperties"],
        json!(false),
        "the canonical journal schema must reject unknown properties"
    );
    let required = schema["required"]
        .as_array()
        .expect("canonical schema declares required properties");
    for field in [
        "transaction_id",
        "effect_kind",
        "project_object_identity",
        "project_generation",
        "transport_mode",
        "operation_family",
        "idempotency_id",
        "canonical_request_hash",
        "touched_object_identities",
        "pre_hashes",
        "staged_write_set",
        "intended_post_hashes",
        "phase",
        "phase_history",
        "proof",
    ] {
        assert!(
            required.iter().any(|value| value == field),
            "canonical schema must require `{field}`"
        );
    }
    let proof_branches = schema["properties"]["proof"]["oneOf"]
        .as_array()
        .expect("proof is a discriminated union");
    assert_eq!(
        proof_branches.len(),
        4,
        "exactly three typed proof forms plus the absent case; never a fourth kind"
    );

    // 2. `assert_cmd` drives a core MCP request through the real stdio serve path.
    let (_temp, project) = writable_project("stdio");
    let mut session = StdioSession::spawn(&project);
    let initialize = session.request(
        1,
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "task6-acceptance", "version": "0.1.0" }
        }),
    );
    assert_eq!(
        initialize["result"]["serverInfo"]["name"],
        json!("godot-mcp-server"),
        "initialize must be answered by the shared kernel"
    );
    session.notify("notifications/initialized", json!({}));
    let tools = session.request(2, "tools/list", json!({}));
    let listed: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .expect("tools/list returns an array")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(
        listed.contains(&"capabilities.get"),
        "discovery must expose the canonical core profile, got {listed:?}"
    );
    let resources = session.request(3, "resources/list", json!({}));
    let resource_uris: Vec<&str> = resources["result"]["resources"]
        .as_array()
        .expect("resources/list returns an array")
        .iter()
        .filter_map(|resource| resource["uri"].as_str())
        .collect();
    assert!(
        resource_uris.contains(&"godot-mcp://resources/scheduler"),
        "the scheduler resource must be advertised, got {resource_uris:?}"
    );
    let finished = session.finish();
    assert!(
        finished.stderr.contains("write gate open"),
        "startup must report the write gate through stderr diagnostics: {}",
        finished.stderr
    );
    for line in finished
        .stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
    {
        let frame: Value = serde_json::from_str(line)
            .unwrap_or_else(|error| panic!("stdout must be MCP frames only: {line:?} ({error})"));
        assert_eq!(
            frame["jsonrpc"],
            json!("2.0"),
            "every stdout line must be a JSON-RPC frame: {line}"
        );
    }

    // 3. A test permit commits each mock file effect through the durable store.
    let (_temp, project) = writable_project("commit");
    let harness = JournalFoundationHarness::open(&project).expect("harness opens the project");

    let created = create_effect(TX_CREATE, "scenes/main.tscn", b"[gd_scene]\n");
    assert_eq!(
        harness
            .commit_transaction(&created)
            .expect("create commits"),
        TerminalDisposition::ProvenPostState
    );
    let record = harness.observe(TX_CREATE).expect("record is durable");
    assert_eq!(record.phase, JournalPhase::Committed);
    assert_eq!(
        record.phase_history,
        vec![
            JournalPhase::Intent,
            JournalPhase::Staged,
            JournalPhase::EffectStarted,
            JournalPhase::Committed
        ],
        "all four fsynced phases must be journaled in order"
    );
    assert!(
        record.has_proof,
        "a committed record carries its typed proof"
    );
    assert_eq!(
        std::fs::read(project.join("scenes/main.tscn")).expect("published bytes"),
        b"[gd_scene]\n".to_vec()
    );

    let updated = MockFileEffect {
        transaction_id: TX_UPDATE.to_owned(),
        path: "scenes/main.tscn".to_owned(),
        operation: MockFileOperation::Update,
        bytes: b"[gd_scene format=3]\n".to_vec(),
        required_process_transaction: None,
    };
    assert_eq!(
        harness
            .commit_transaction(&updated)
            .expect("update commits"),
        TerminalDisposition::ProvenPostState
    );
    assert_eq!(
        std::fs::read(project.join("scenes/main.tscn")).expect("updated bytes"),
        b"[gd_scene format=3]\n".to_vec()
    );

    // A delete records an existence-tombstone post-state and a hash-bearing
    // pre-state. Todo 6's working boundary publishes create/update only, so a
    // delete whose target still exists settles as a proven *pre*-state
    // (compensated) rather than guessing that the removal happened.
    std::fs::write(project.join("scenes/old.tscn"), b"stale\n").expect("seed delete target");
    let deleted = MockFileEffect {
        transaction_id: TX_DELETE.to_owned(),
        path: "scenes/old.tscn".to_owned(),
        operation: MockFileOperation::Delete,
        bytes: Vec::new(),
        required_process_transaction: None,
    };
    assert_eq!(
        harness
            .commit_transaction(&deleted)
            .expect("delete settles"),
        TerminalDisposition::ProvenPreState,
        "an unpublished delete must prove its pre-state, never assume the removal"
    );
    let delete_record = harness.observe(TX_DELETE).expect("delete record");
    assert_eq!(delete_record.phase, JournalPhase::Compensated);
    assert_eq!(
        delete_record.staged_write_count, 0,
        "a delete has no staged payload"
    );
    assert!(
        project.join("scenes/old.tscn").exists(),
        "a compensated delete must leave the target untouched"
    );

    // 4. Every effect boundary exists; only `file` has a real implementation.
    let availability = harness.boundary_availability();
    assert_eq!(availability.len(), 5, "five frozen effect boundaries");
    for (boundary, available) in &availability {
        let expected = boundary == "file";
        assert_eq!(
            *available, expected,
            "boundary {boundary} availability must be {expected}"
        );
    }
    for (index, boundary) in EffectBoundary::all()
        .into_iter()
        .filter(|boundary| *boundary != EffectBoundary::File)
        .enumerate()
    {
        let probe = create_effect(
            TX_BOUNDARY[index],
            &format!("scenes/probe-{index}.tscn"),
            b"probe\n",
        );
        harness
            .open_transaction(&probe)
            .expect("boundary probe intent journaled");
        harness
            .stage_transaction(&probe)
            .expect("boundary probe staged");
        let error = harness
            .attempt_boundary(&probe, boundary)
            .expect_err("an unimplemented boundary must fail closed");
        assert!(
            error
                .to_string()
                .contains("is not implemented in this build"),
            "boundary {boundary:?} must fail closed, got {error}"
        );
        assert!(
            !project.join(format!("scenes/probe-{index}.tscn")).exists(),
            "a fail-closed boundary must produce no effect"
        );
    }

    // 5. Reopening the store recovers the committed records (durability across
    //    a fresh open of the same on-disk database).
    drop(harness);
    let reopened = JournalFoundationHarness::open(&project).expect("store reopens");
    assert_eq!(
        reopened.observe(TX_CREATE).expect("record survives").phase,
        JournalPhase::Committed
    );
    assert!(
        reopened.quarantined().expect("quarantine query").is_empty(),
        "a clean run leaves nothing quarantined"
    );

    // 6. Todo 17's composite: a `file` transaction linked to a clean helper run
    //    reaches a proven post-state.
    reopened
        .journal_process_transaction(TX_HELPER, true)
        .expect("clean helper proof");
    let composite = MockFileEffect {
        transaction_id: TX_COMPOSITE.to_owned(),
        path: "scenes/composite.tscn".to_owned(),
        operation: MockFileOperation::Create,
        bytes: b"[gd_scene composite]\n".to_vec(),
        required_process_transaction: Some(TX_HELPER.to_owned()),
    };
    assert_eq!(
        reopened
            .commit_transaction(&composite)
            .expect("composite commits"),
        TerminalDisposition::ProvenPostState
    );
    assert_eq!(
        reopened
            .observe(TX_COMPOSITE)
            .expect("composite record")
            .required_process_transaction
            .as_deref(),
        Some(TX_HELPER),
        "the composite requirement is recorded as journal metadata, not a fourth proof kind"
    );

    // 7. The repository schema and the compiled-in schema are the same bytes.
    let on_disk = std::fs::read_to_string(repo_root().join("schemas/transactions/v1.json"))
        .expect("canonical schema is present in the repository");
    assert_eq!(
        on_disk, SCHEMA,
        "the embedded schema must be byte-identical to the repository copy"
    );
}

// ---------------------------------------------------------------------------
// Failure paths
// ---------------------------------------------------------------------------

#[test]
fn journal_foundation_failure_paths() {
    let (_temp, project) = writable_project("failure");
    let harness = JournalFoundationHarness::open(&project).expect("harness opens the project");

    // 1. Fault injection before and after every phase fsync. Before the barrier
    //    the phase must NOT be durable; after it, the phase must be durable and
    //    the transaction must remain reconcilable rather than half-applied.
    for phase in [
        JournalPhase::Intent,
        JournalPhase::Staged,
        JournalPhase::EffectStarted,
        JournalPhase::Committed,
    ] {
        let (_temp, project) = writable_project("fault-before");
        let harness = JournalFoundationHarness::open(&project).expect("harness");
        let effect = create_effect(TX_FAULT, "scenes/fault.tscn", b"fault\n");
        harness.arm_fault(Some(FaultInjection::BeforeFsync(phase)));
        let outcome = drive_until_fault(&harness, &effect);
        harness.arm_fault(None);
        assert!(
            outcome.is_err(),
            "a fault before the {phase:?} fsync must fail the operation"
        );
        match harness.observe(TX_FAULT) {
            Err(_) => assert_eq!(
                phase,
                JournalPhase::Intent,
                "only an intent-phase fault can leave no record at all"
            ),
            Ok(record) => assert_ne!(
                record.phase, phase,
                "a fault before the {phase:?} fsync must not leave {phase:?} durable"
            ),
        }

        let (_temp, project) = writable_project("fault-after");
        let harness = JournalFoundationHarness::open(&project).expect("harness");
        let effect = create_effect(TX_FAULT, "scenes/fault.tscn", b"fault\n");
        harness.arm_fault(Some(FaultInjection::AfterFsync(phase)));
        let outcome = drive_until_fault(&harness, &effect);
        harness.arm_fault(None);
        assert!(
            outcome.is_err(),
            "a fault after the {phase:?} fsync must still surface to the caller"
        );
        let record = harness
            .observe(TX_FAULT)
            .expect("a fault after the fsync leaves the phase durable");
        assert_eq!(
            record.phase, phase,
            "a fault after the {phase:?} fsync must leave exactly {phase:?} durable"
        );
        // Startup reconciliation must reach exactly one of the three conclusions.
        let settled = harness.reconcile().expect("reconciliation runs");
        for (transaction, conclusion) in &settled {
            assert!(
                matches!(
                    conclusion.as_str(),
                    "proven_pre_state" | "proven_post_state" | "quarantine"
                ),
                "transaction {transaction} reconciled to an unexpected conclusion {conclusion}"
            );
        }
        if phase != JournalPhase::Committed {
            assert_eq!(
                settled.len(),
                1,
                "an open transaction must be settled by reconciliation"
            );
        }
    }

    // 2. Partial staged bytes make neither state provable: quarantine, never a
    //    guessed replay.
    let (_temp, partial_project) = writable_project("partial");
    let partial_harness = JournalFoundationHarness::open(&partial_project).expect("harness");
    let partial = create_effect(TX_PARTIAL, "scenes/partial.tscn", b"complete payload\n");
    partial_harness
        .open_transaction(&partial)
        .expect("intent journaled");
    partial_harness
        .stage_transaction(&partial)
        .expect("staged durably");
    partial_harness
        .corrupt_staged_bytes(TX_PARTIAL, b"compl")
        .expect("inject partial staged bytes");
    let partial_error = partial_harness
        .execute_transaction(&partial)
        .expect_err("partial staged bytes must refuse the effect");
    assert!(
        partial_error.to_string().contains("not intact"),
        "partial staged bytes must be reported as such, got {partial_error}"
    );
    assert!(
        !partial_project.join("scenes/partial.tscn").exists(),
        "no partial write may reach the project"
    );
    let settled = partial_harness.reconcile().expect("reconciliation runs");
    assert_eq!(
        settled,
        vec![(TX_PARTIAL.to_owned(), "quarantine".to_owned())],
        "a partial staged write set must reconcile to write-blocking quarantine"
    );

    // 3. Quarantine blocks every ordinary permit, and the write path stays shut.
    let blocked = create_effect(TX_BLOCKED, "scenes/blocked.tscn", b"blocked\n");
    let blocked_error = partial_harness
        .open_transaction(&blocked)
        .and_then(|()| partial_harness.stage_transaction(&blocked))
        .and_then(|()| {
            partial_harness
                .execute_transaction(&blocked)
                .map(|_disposition| ())
        })
        .expect_err("a quarantined project must refuse further writes");
    assert!(
        blocked_error.to_string().contains("quarantined"),
        "quarantine must be the stated reason, got {blocked_error}"
    );
    let permit_error = partial_harness
        .try_issue_ordinary_permit(TX_BLOCKED)
        .expect_err("no ordinary permit exists while quarantined");
    assert!(
        permit_error.to_string().contains("quarantined"),
        "the permit authority must refuse on quarantine, got {permit_error}"
    );
    assert!(
        !partial_project.join("scenes/blocked.tscn").exists(),
        "a quarantined project must not be written"
    );

    // 4. The quarantine-exempt recovery authority cannot commit without proof, and
    //    always audits before transitioning.
    let refused = partial_harness
        .operator_recovery(TX_PARTIAL, RecoveryOperation::AcknowledgeSafeState)
        .expect("the recovery request itself is accepted");
    assert_eq!(
        refused,
        TerminalDisposition::Quarantine,
        "acknowledge-safe-state without a post-state proof must stay quarantined"
    );
    assert_eq!(
        partial_harness.observe(TX_PARTIAL).expect("record").phase,
        JournalPhase::Quarantined,
        "an unproven repair must leave the record quarantined"
    );
    let audit = partial_harness
        .audit_entries(TX_PARTIAL)
        .expect("audit entries");
    let issued = audit
        .iter()
        .position(|(event, _)| event == "repair_permit_issued")
        .expect("the repair permit issue is audited");
    let outcome = audit
        .iter()
        .position(|(event, _)| event == "repair_proof_outcome")
        .expect("the repair proof outcome is audited");
    assert!(
        issued < outcome,
        "the audit record must precede the proof outcome and any transition"
    );
    // A proof-backed compensate is accepted, because the pre-state still holds.
    let compensated = partial_harness
        .operator_recovery(TX_PARTIAL, RecoveryOperation::Compensate)
        .expect("proof-backed compensate is accepted");
    assert_eq!(compensated, TerminalDisposition::ProvenPreState);
    assert_eq!(
        partial_harness.observe(TX_PARTIAL).expect("record").phase,
        JournalPhase::Compensated
    );

    // 5. An illegal transition is rejected and the record is left untouched.
    let illegal = create_effect(TX_ILLEGAL, "scenes/illegal.tscn", b"illegal\n");
    harness
        .open_transaction(&illegal)
        .expect("intent journaled");
    let before = harness.observe(TX_ILLEGAL).expect("record before");
    let illegal_error = harness
        .try_transition(TX_ILLEGAL, JournalPhase::Committed)
        .expect_err("intent_fsynced cannot jump straight to committed_fsynced");
    assert!(
        illegal_error
            .to_string()
            .contains("illegal phase transition"),
        "an illegal transition must be named as such, got {illegal_error}"
    );
    assert_eq!(
        harness.observe(TX_ILLEGAL).expect("record after"),
        before,
        "a rejected transition must leave the durable record byte-identical"
    );
    // A terminal record has no successor at all.
    let terminal = create_effect(TX_REOPEN, "scenes/terminal.tscn", b"terminal\n");
    harness
        .commit_transaction(&terminal)
        .expect("terminal record commits");
    assert!(
        harness
            .try_transition(TX_REOPEN, JournalPhase::EffectStarted)
            .is_err(),
        "a committed transaction can never transition again"
    );

    // 6. A missing/stale permit is rejected before any effect runs.
    let stale = create_effect(TX_STALE, "scenes/stale.tscn", b"stale\n");
    harness.open_transaction(&stale).expect("intent journaled");
    harness.stage_transaction(&stale).expect("staged durably");
    let stale_error = harness
        .execute_with_stale_permit(&stale, 99)
        .expect_err("a permit from a superseded epoch must be rejected");
    assert!(
        stale_error
            .to_string()
            .contains("superseded authority epoch"),
        "a stale permit must be named as such, got {stale_error}"
    );
    assert_eq!(
        stale_error.code(),
        Some("permission_denied"),
        "a stale permit maps to the permission_denied taxonomy entry"
    );
    let generation_error = harness
        .execute_with_wrong_generation(&stale)
        .expect_err("a permit naming another generation must be rejected");
    assert!(
        generation_error.to_string().contains("generation"),
        "a generation mismatch must be named, got {generation_error}"
    );
    assert!(
        !project.join("scenes/stale.tscn").exists(),
        "a rejected permit must produce no effect"
    );
    assert_eq!(
        harness.observe(TX_STALE).expect("record").phase,
        JournalPhase::Staged,
        "a rejected permit must not advance the phase"
    );

    // 7. Reconciliation against a different held project identity/generation can
    //    only quarantine: a hash comparison against another project proves nothing.
    let foreign = harness
        .reconcile_with_foreign_project()
        .expect("reconciliation runs");
    assert!(
        !foreign.is_empty(),
        "the open staged transaction must be classified"
    );
    for (transaction, conclusion) in &foreign {
        assert_eq!(
            conclusion, "quarantine",
            "transaction {transaction} must quarantine when the held project differs"
        );
    }

    // 8. Todo 17's composite gate: a `file` transaction whose linked helper run is
    //    not a clean terminal run stays quarantined even though the bytes match.
    let (_temp, composite_project) = writable_project("composite");
    let composite_harness =
        JournalFoundationHarness::open(&composite_project).expect("harness opens");
    composite_harness
        .journal_process_transaction(TX_HELPER_BAD, false)
        .expect("live-survivor helper proof");
    let composite = MockFileEffect {
        transaction_id: TX_COMPOSITE_BAD.to_owned(),
        path: "scenes/composite.tscn".to_owned(),
        operation: MockFileOperation::Create,
        bytes: b"[gd_scene composite]\n".to_vec(),
        required_process_transaction: Some(TX_HELPER_BAD.to_owned()),
    };
    assert_eq!(
        composite_harness
            .commit_transaction(&composite)
            .expect("composite settles"),
        TerminalDisposition::Quarantine,
        "matching file bytes with a live-survivor helper proof must quarantine"
    );
    let record = composite_harness
        .observe(TX_COMPOSITE_BAD)
        .expect("composite record");
    assert_eq!(record.phase, JournalPhase::Quarantined);
    assert!(
        record
            .terminal_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("process_launch")),
        "the quarantine reason must name the unsatisfied sub-proof: {record:?}"
    );

    // 9. A missing linked helper transaction also quarantines.
    let (_temp, orphan_project) = writable_project("orphan");
    let orphan_harness = JournalFoundationHarness::open(&orphan_project).expect("harness opens");
    let orphan = MockFileEffect {
        transaction_id: TX_COMPOSITE.to_owned(),
        path: "scenes/orphan.tscn".to_owned(),
        operation: MockFileOperation::Create,
        bytes: b"[gd_scene orphan]\n".to_vec(),
        required_process_transaction: Some(TX_HELPER.to_owned()),
    };
    assert_eq!(
        orphan_harness
            .commit_transaction(&orphan)
            .expect("orphan settles"),
        TerminalDisposition::Quarantine,
        "an absent linked process transaction can never satisfy the composite"
    );

    // 10. Release inspection: test permit/fault hooks are absent without
    //     `test-hooks`, and the harness binary is not built either.
    let output = Command::new(env!("CARGO"))
        .args([
            "build",
            "--package",
            "godot-mcp-server",
            "--bin",
            "qa-journal-foundation",
        ])
        .current_dir(repo_root())
        .output()
        .expect("cargo build runs");
    assert!(
        !output.status.success(),
        "the test-hooks-only harness binary must not build with default features"
    );
    let message = String::from_utf8_lossy(&output.stderr);
    assert!(
        message.contains("requires the features: `test-hooks`"),
        "the harness binary must be excluded by its required-features guard: {message}"
    );
    let default_check = Command::new(env!("CARGO"))
        .args([
            "check",
            "--package",
            "godot-mcp-server",
            "--no-default-features",
            "--features",
            "stdio",
        ])
        .current_dir(repo_root())
        .output()
        .expect("cargo check runs");
    assert!(
        default_check.status.success(),
        "the release feature set must compile without the test hooks: {}",
        String::from_utf8_lossy(&default_check.stderr)
    );
    let harness_source = repo_root().join("crates/godot-mcp-server/src/test_support.rs");
    let harness_text = std::fs::read_to_string(&harness_source).expect("test_support source");
    assert!(
        harness_text.contains("mod journal_harness"),
        "the permit/fault-hook factory must live in the test-hooks module"
    );
    let lib_text = std::fs::read_to_string(repo_root().join("crates/godot-mcp-server/src/lib.rs"))
        .expect("lib source");
    assert!(
        lib_text.contains("#[cfg(feature = \"test-hooks\")]\npub mod test_support;"),
        "test_support must be compiled only under the test-hooks feature"
    );
    assert!(
        lib_text.contains("pub(crate) mod effect;") && lib_text.contains("pub(crate) mod journal;"),
        "the effect gateway and journal must be crate-private"
    );
}

#[test]
fn startup_reconciliation_same_byte_staged_update_converges_to_compensated() {
    let (_temp, project) = writable_project("same-byte-reconciliation");
    let path = "scenes/same-byte.tscn";
    let bytes = b"[gd_scene same_bytes]\n";
    std::fs::write(project.join(path), bytes).expect("seed the update pre-state");

    let harness = JournalFoundationHarness::open(&project).expect("harness opens the project");
    let effect = MockFileEffect {
        transaction_id: TX_SAME_BYTES.to_owned(),
        path: path.to_owned(),
        operation: MockFileOperation::Update,
        bytes: bytes.to_vec(),
        required_process_transaction: None,
    };
    harness
        .open_transaction(&effect)
        .expect("same-byte update intent is durable");
    harness
        .stage_transaction(&effect)
        .expect("same-byte update is staged durably");
    assert_eq!(
        harness
            .observe(TX_SAME_BYTES)
            .expect("staged transaction")
            .phase,
        JournalPhase::Staged
    );
    drop(harness);

    let runtime = ProjectRuntime::open(&StdioOptions {
        project: project.clone(),
        godot_binary: None,
        profile: CapabilityProfile::Core,
        principal: PrincipalId::parse("principal_0123456789abcdef").expect("principal"),
        observed_at_unix_ms: 1_700_000_000_000,
        limits: Limits::default(),
        process: Default::default(),
    })
    .expect("real startup reconciliation must converge for a staged same-byte update");
    drop(runtime);

    let reopened = JournalFoundationHarness::open(&project).expect("store reopens after startup");
    let record = reopened
        .observe(TX_SAME_BYTES)
        .expect("reconciled transaction is durable");
    assert_eq!(
        record.phase,
        JournalPhase::Compensated,
        "a same-byte update that never entered the effect boundary must not replay as success"
    );
    assert_eq!(
        record.terminal_outcome,
        Some("unavailable"),
        "startup reconciliation must retain a failed pre-state verdict, not a success replay"
    );
}

#[test]
fn startup_reconciliation_intent_only_create_and_update_prove_pre_state() {
    let (_temp, project) = writable_project("intent-only-reconciliation");
    let update_path = "scenes/intent-update.tscn";
    std::fs::write(project.join(update_path), b"before\n").expect("seed update pre-state");

    let create = create_effect(TX_INTENT_CREATE, "scenes/intent-create.tscn", b"created\n");
    let update = MockFileEffect {
        transaction_id: TX_INTENT_UPDATE.to_owned(),
        path: update_path.to_owned(),
        operation: MockFileOperation::Update,
        bytes: b"after\n".to_vec(),
        required_process_transaction: None,
    };
    let harness = JournalFoundationHarness::open(&project).expect("harness opens the project");
    harness
        .open_transaction(&create)
        .expect("create intent is durable");
    harness
        .open_transaction(&update)
        .expect("update intent is durable");
    drop(harness);

    let runtime = ProjectRuntime::open(&StdioOptions {
        project: project.clone(),
        godot_binary: None,
        profile: CapabilityProfile::Core,
        principal: PrincipalId::parse("principal_0123456789abcdef").expect("principal"),
        observed_at_unix_ms: 1_700_000_000_000,
        limits: Limits::default(),
        process: Default::default(),
    })
    .expect("real startup reconciliation opens the project");
    let ordinary_permit = runtime.write_gate_status();
    drop(runtime);

    let reopened = JournalFoundationHarness::open(&project).expect("store reopens after startup");
    let create_record = reopened
        .observe(TX_INTENT_CREATE)
        .expect("create record is durable");
    let update_record = reopened
        .observe(TX_INTENT_UPDATE)
        .expect("update record is durable");
    assert!(
        create_record.phase == JournalPhase::Compensated
            && create_record.terminal_outcome == Some("unavailable")
            && update_record.phase == JournalPhase::Compensated
            && update_record.terminal_outcome == Some("unavailable")
            && ordinary_permit.is_ok(),
        "intent-only pre-states must compensate and leave the ordinary write gate open: \
         create={create_record:?}, update={update_record:?}, ordinary permit={ordinary_permit:?}"
    );
}

#[test]
fn startup_reconciliation_intent_only_pre_state_mismatch_quarantines() {
    let (_temp, project) = writable_project("intent-only-mismatch");
    let create_path = "scenes/intent-create-mismatch.tscn";
    let update_path = "scenes/intent-update-mismatch.tscn";
    std::fs::write(project.join(update_path), b"before\n").expect("seed update pre-state");

    let create = create_effect(TX_INTENT_CREATE_MISMATCH, create_path, b"created\n");
    let update = MockFileEffect {
        transaction_id: TX_INTENT_UPDATE_MISMATCH.to_owned(),
        path: update_path.to_owned(),
        operation: MockFileOperation::Update,
        bytes: b"after\n".to_vec(),
        required_process_transaction: None,
    };
    let harness = JournalFoundationHarness::open(&project).expect("harness opens the project");
    harness
        .open_transaction(&create)
        .expect("create intent is durable");
    harness
        .open_transaction(&update)
        .expect("update intent is durable");
    std::fs::write(project.join(create_path), b"unexpected create\n")
        .expect("invalidate create pre-state");
    std::fs::write(project.join(update_path), b"unexpected update\n")
        .expect("invalidate update pre-state");
    drop(harness);

    let runtime = ProjectRuntime::open(&StdioOptions {
        project: project.clone(),
        godot_binary: None,
        profile: CapabilityProfile::Core,
        principal: PrincipalId::parse("principal_0123456789abcdef").expect("principal"),
        observed_at_unix_ms: 1_700_000_000_000,
        limits: Limits::default(),
        process: Default::default(),
    })
    .expect("real startup reconciliation opens the project");
    let ordinary_permit = runtime.write_gate_status();
    drop(runtime);

    let reopened = JournalFoundationHarness::open(&project).expect("store reopens after startup");
    let create_record = reopened
        .observe(TX_INTENT_CREATE_MISMATCH)
        .expect("create record is durable");
    let update_record = reopened
        .observe(TX_INTENT_UPDATE_MISMATCH)
        .expect("update record is durable");
    assert!(
        create_record.phase == JournalPhase::Quarantined
            && create_record.terminal_outcome == Some("uncertain")
            && update_record.phase == JournalPhase::Quarantined
            && update_record.terminal_outcome == Some("uncertain")
            && ordinary_permit.is_err(),
        "intent-only mismatched pre-states must quarantine and close the ordinary write gate: \
         create={create_record:?}, update={update_record:?}, ordinary permit={ordinary_permit:?}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn production_startup_compensates_every_durable_pre_release_process_window() {
    let cases = [
        ("after_intent_before_child_creation", JournalPhase::Intent),
        (
            "after_child_creation_before_staged_fsync",
            JournalPhase::Intent,
        ),
        ("after_staged_before_receipt_fsync", JournalPhase::Staged),
        ("after_receipt_before_release", JournalPhase::EffectStarted),
    ];

    for (stage, durable_inner_phase) in cases {
        let (temp, project) = writable_project(stage);
        let (godot, _pid_file) = fake_godot(temp.path(), FakeGodotMode::Live);
        let exit = crash_project_run(&project, &godot, stage);
        assert_eq!(exit, 86, "the production launch must crash at {stage}");

        let before = JournalFoundationHarness::open(&project).expect("journal opens after crash");
        let (outer, inner) = linked_process_records(&before);
        assert_eq!(inner.phase, durable_inner_phase, "inner phase at {stage}");
        assert_eq!(
            outer.phase,
            JournalPhase::EffectStarted,
            "outer phase at {stage}"
        );
        assert_eq!(
            outer.required_process_transaction.as_deref(),
            Some(inner.transaction_id.as_str()),
            "the outer transaction must durably identify its canonical launch at {stage}"
        );
        drop(before);

        let restarted = restart_process_server(&project, &godot);
        assert!(
            restarted.stderr.contains("write gate open"),
            "a proven pre-release crash must leave the write gate open at {stage}: {}",
            restarted.stderr
        );
        let after = JournalFoundationHarness::open(&project).expect("journal opens after restart");
        let (outer, inner) = linked_process_records(&after);
        for record in [outer, inner] {
            assert_eq!(
                record.phase,
                JournalPhase::Compensated,
                "{stage} must compensate both linked transactions: {record:?}"
            );
            assert_eq!(record.terminal_outcome, Some("unavailable"));
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn engine_plane_project_test_reports_the_framework_verdict_table() {
    let cases = [
        (ValidationGodotMode::TestsPassed, true, "passed"),
        (ValidationGodotMode::TestsFailed, true, "failed"),
        (ValidationGodotMode::TestsInconclusive, true, "inconclusive"),
        (ValidationGodotMode::TestsPassed, false, "unsupported"),
    ];

    for (index, (mode, install_framework, expected)) in cases.into_iter().enumerate() {
        let (temp, project) = writable_project(expected);
        if install_framework {
            install_gut(&project);
        }
        let (godot, _) = validation_godot(temp.path(), mode, true);
        let mut session = initialized_engine_session(&project, &godot);

        let response = session.call_tool(10 + index as u64, "project.test", json!({}));
        let result = response
            .pointer("/result/structuredContent")
            .unwrap_or_else(|| panic!("project.test must return a structured report: {response}"));

        assert_eq!(
            result["classification"],
            json!(expected),
            "project.test must expose the framework-derived {expected} verdict: {response}"
        );
        if install_framework {
            assert_eq!(result["framework"], json!("gut"));
        } else {
            assert!(
                result["reason"]
                    .as_str()
                    .is_some_and(|reason| reason.contains("no committed test framework")),
                "the unsupported report must state the true detection reason: {response}"
            );
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn engine_plane_script_check_runs_the_canary_and_classifies_real_scripts() {
    let (temp, project) = writable_project("script-check");
    std::fs::create_dir_all(project.join("scripts")).expect("script fixture directory");
    std::fs::write(
        project.join("scripts/valid.gd"),
        "extends Node\nfunc answer() -> int:\n\treturn 42\n",
    )
    .expect("valid script");
    std::fs::write(
        project.join("scripts/broken.gd"),
        "extends Node\nfunc broken(\n",
    )
    .expect("broken script");
    let (godot, invocations) =
        validation_godot(temp.path(), ValidationGodotMode::ScriptChecks, true);
    let mut session = initialized_engine_session(&project, &godot);

    let valid = session.call_tool(20, "script.check", json!({ "script": "scripts/valid.gd" }));
    let valid_result = valid
        .pointer("/result/structuredContent")
        .unwrap_or_else(|| panic!("script.check must return a structured report: {valid}"));
    assert_eq!(
        valid_result["classification"],
        json!("passed"),
        "a valid script must pass only after the canary proves --check-only: {valid}"
    );
    assert_eq!(valid_result["calibration"]["honored"], json!(true));

    let broken = session.call_tool(21, "script.check", json!({ "script": "scripts/broken.gd" }));
    let broken_result = broken
        .pointer("/result/structuredContent")
        .unwrap_or_else(|| panic!("script.check must return a structured report: {broken}"));
    assert_eq!(
        broken_result["classification"],
        json!("failed"),
        "a real parse diagnostic must fail the script check: {broken}"
    );
    assert_eq!(broken_result["diagnostics"][0]["category"], "script_parse");

    let invocation_text = std::fs::read_to_string(&invocations).expect("invocation log");
    assert!(
        invocation_text.matches("check_only_canary.gd").count() >= 2,
        "every script check must run its calibration canary first: {invocation_text}"
    );
    let finished = session.finish();
    assert!(
        finished
            .stdout
            .lines()
            .all(|line| serde_json::from_str::<Value>(line).is_ok()),
        "the production stdio path must emit only JSON frames: {}",
        finished.stdout
    );

    let (unsupported_temp, unsupported_project) = writable_project("script-check-unsupported");
    let (unsupported_godot, unsupported_invocations) = validation_godot(
        unsupported_temp.path(),
        ValidationGodotMode::ScriptChecks,
        false,
    );
    let mut unsupported_session =
        initialized_engine_session(&unsupported_project, &unsupported_godot);
    let unsupported =
        unsupported_session.call_tool(22, "script.check", json!({ "script": "scripts/valid.gd" }));
    let unsupported_result = unsupported
        .pointer("/result/structuredContent")
        .unwrap_or_else(|| panic!("unsupported script.check must be a report: {unsupported}"));
    assert_eq!(
        unsupported_result["classification"],
        json!("unsupported"),
        "a build missing --check-only must return an unsupported report: {unsupported}"
    );
    assert!(
        unsupported_result["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("--check-only"))
    );
    let unsupported_log =
        std::fs::read_to_string(&unsupported_invocations).expect("unsupported invocation log");
    assert!(
        !unsupported_log.contains(" --headless "),
        "a build missing --check-only must be refused before a validation launch: {unsupported_log}"
    );
    drop(unsupported_session.finish());

    let invalid_log = temp.path().join("invalid-path-invocations.log");
    let (invalid_godot, _) = validation_godot_with_log(
        temp.path(),
        ValidationGodotMode::ScriptChecks,
        true,
        &invalid_log,
        "invalid-path-godot",
    );
    let mut invalid_session = initialized_engine_session(&project, &invalid_godot);
    let invalid =
        invalid_session.call_tool(23, "script.check", json!({ "script": "../outside.gd" }));
    assert_eq!(
        invalid.pointer("/error/data/code"),
        Some(&json!("invalid_input")),
        "an escaping script path must be refused by shared request validation: {invalid}"
    );
    assert!(
        !invalid_log.exists(),
        "an invalid path must be refused before engine discovery or launch"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn production_startup_commits_a_released_live_survivor() {
    let (temp, project) = writable_project("released-live-survivor");
    let (godot, pid_file) = fake_godot(temp.path(), FakeGodotMode::Live);
    let exit = crash_project_run(&project, &godot, "after_release_before_commit_fsync");
    assert_eq!(exit, 86, "the production launch must crash after release");
    let child_pid = wait_for_pid(&pid_file);

    let restarted = restart_process_server(&project, &godot);
    assert!(
        restarted.stderr.contains("write gate open"),
        "a released live survivor must be adopted without closing the write gate: {}",
        restarted.stderr
    );
    let after = JournalFoundationHarness::open(&project).expect("journal opens after restart");
    let (outer, inner) = linked_process_records(&after);
    for record in [outer, inner] {
        assert_eq!(
            record.phase,
            JournalPhase::Committed,
            "a released live survivor proves both linked outcomes: {record:?}"
        );
        assert_eq!(record.terminal_outcome, Some("succeeded"));
        assert!(record.has_proof);
    }

    let _ = Command::new("kill")
        .args(["-KILL", &child_pid.to_string()])
        .status();
    wait_for_process_exit(child_pid);
}

#[cfg(target_os = "linux")]
#[test]
fn production_startup_quarantines_a_released_dead_child_without_commit_mark() {
    let (temp, project) = writable_project("released-dead-child");
    let (godot, pid_file) = fake_godot(temp.path(), FakeGodotMode::Exit);
    let exit = crash_project_run(&project, &godot, "after_release_before_commit_fsync");
    assert_eq!(exit, 86, "the production launch must crash after release");
    let child_pid = wait_for_pid(&pid_file);
    wait_for_process_exit(child_pid);

    let restarted = restart_process_server(&project, &godot);
    assert!(
        restarted.stderr.contains("write gate closed"),
        "death alone must not invent release proof: {}",
        restarted.stderr
    );
    let after = JournalFoundationHarness::open(&project).expect("journal opens after restart");
    let (outer, inner) = linked_process_records(&after);
    for record in [outer, inner] {
        assert_eq!(
            record.phase,
            JournalPhase::Quarantined,
            "a dead child with no commit mark must quarantine both linked transactions: {record:?}"
        );
        assert_eq!(record.terminal_outcome, Some("uncertain"));
    }
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
enum FakeGodotMode {
    Live,
    Exit,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
enum ValidationGodotMode {
    TestsPassed,
    TestsFailed,
    TestsInconclusive,
    ScriptChecks,
}

#[cfg(target_os = "linux")]
fn install_gut(project: &Path) {
    let gut = project.join("addons/gut");
    std::fs::create_dir_all(&gut).expect("GUT fixture directory");
    std::fs::write(gut.join("plugin.cfg"), "[plugin]\n").expect("GUT plugin marker");
    std::fs::write(gut.join("gut_cmdln.gd"), "extends SceneTree\n")
        .expect("GUT command-line marker");
}

#[cfg(target_os = "linux")]
fn validation_godot(
    directory: &Path,
    mode: ValidationGodotMode,
    advertise_check_only: bool,
) -> (PathBuf, PathBuf) {
    let log = directory.join("validation-godot-invocations.log");
    validation_godot_with_log(
        directory,
        mode,
        advertise_check_only,
        &log,
        "validation-godot",
    )
}

#[cfg(target_os = "linux")]
fn validation_godot_with_log(
    directory: &Path,
    mode: ValidationGodotMode,
    advertise_check_only: bool,
    invocation_log: &Path,
    name: &str,
) -> (PathBuf, PathBuf) {
    use std::os::unix::fs::PermissionsExt as _;

    let binary = directory.join(name);
    let check_only_capability = if advertise_check_only {
        "--check-only"
    } else {
        ""
    };
    let test_action = match mode {
        ValidationGodotMode::TestsPassed => {
            "printf '%s\\n' 'Totals' '------' 'Tests                 2' 'Passing Tests         2'; exit 0"
        }
        ValidationGodotMode::TestsFailed => {
            "printf '%s\\n' 'Totals' '------' 'Tests                 2' 'Passing Tests         1' 'Failing Tests         1'; exit 1"
        }
        ValidationGodotMode::TestsInconclusive => {
            "printf '%s\\n' 'Godot started but emitted no framework summary'; exit 0"
        }
        ValidationGodotMode::ScriptChecks => "exit 0",
    };
    let script = format!(
        "#!/bin/sh\n\
         printf '%s\\n' \" $* \" >> '{}'\n\
         if [ \"$1\" = \"--version\" ]; then\n\
           printf '%s\\n' '4.7.1.stable.official.b09f793f5'\n\
           exit 0\n\
         fi\n\
         if [ \"$1\" = \"--help\" ]; then\n\
           for flag in --help --version --verbose --headless --path --quit --quit-after --script {check_only_capability} --editor --import --export-release --export-debug --export-pack --doctool --debug-server --main-pack --remote-debug; do\n\
             [ -n \"$flag\" ] && printf '  %s  fixture capability\\n' \"$flag\"\n\
           done\n\
           exit 0\n\
         fi\n\
         case \" $* \" in\n\
           *check_only_canary.gd*)\n\
             canary=''\n\
             previous=''\n\
             for argument in \"$@\"; do\n\
               [ \"$previous\" = \"--script\" ] && canary=\"$argument\"\n\
               previous=\"$argument\"\n\
             done\n\
             printf '%s\\n' 'SCRIPT ERROR: Parse Error: Expected statement.' \"   at: GDScript::reload (${{canary}}:9)\" >&2\n\
             exit 1\n\
             ;;\n\
           *scripts/broken.gd*)\n\
             printf '%s\\n' 'SCRIPT ERROR: Parse Error: Expected closing parenthesis.' '   at: GDScript::reload (res://scripts/broken.gd:2)' >&2\n\
             exit 1\n\
             ;;\n\
           *--check-only*) exit 0 ;;\n\
         esac\n\
         {test_action}\n",
        invocation_log.display()
    );
    std::fs::write(&binary, script).expect("write validation fake Godot");
    let mut permissions = std::fs::metadata(&binary)
        .expect("validation fake Godot metadata")
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&binary, permissions).expect("make validation fake Godot executable");
    (binary, invocation_log.to_path_buf())
}

#[cfg(target_os = "linux")]
fn initialized_engine_session(project: &Path, godot: &Path) -> StdioSession {
    let mut session = StdioSession::spawn_engine(project, godot, None);
    let initialized = session.request(
        1,
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "engine-validation", "version": "0.1.0" }
        }),
    );
    assert_eq!(
        initialized["result"]["serverInfo"]["name"],
        "godot-mcp-server"
    );
    session.notify("notifications/initialized", json!({}));
    session
}

#[cfg(target_os = "linux")]
fn fake_godot(directory: &Path, mode: FakeGodotMode) -> (PathBuf, PathBuf) {
    use std::os::unix::fs::PermissionsExt as _;

    let binary = directory.join("fake-godot");
    let pid_file = directory.join("fake-godot.pid");
    let action = match mode {
        FakeGodotMode::Live => "exec sleep 30",
        FakeGodotMode::Exit => "exit 0",
    };
    let script = format!(
        "#!/bin/sh\n\
         if [ \"$1\" = \"--version\" ]; then\n\
           printf '%s\\n' '4.7.1.stable.official.b09f793f5'\n\
           exit 0\n\
         fi\n\
         if [ \"$1\" = \"--help\" ]; then\n\
           for flag in --help --version --verbose --headless --path --quit --quit-after --script --check-only --editor --import --export-release --export-debug --export-pack --doctool --debug-server --main-pack --remote-debug; do\n\
             printf '  %s  fixture capability\\n' \"$flag\"\n\
           done\n\
           exit 0\n\
         fi\n\
         printf '%s\\n' \"$$\" > '{}'\n\
         {action}\n",
        pid_file.display()
    );
    std::fs::write(&binary, script).expect("write fake Godot");
    let mut permissions = std::fs::metadata(&binary)
        .expect("fake Godot metadata")
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&binary, permissions).expect("make fake Godot executable");
    (binary, pid_file)
}

#[cfg(target_os = "linux")]
fn crash_project_run(project: &Path, godot: &Path, stage: &str) -> i32 {
    let mut session = StdioSession::spawn_engine(project, godot, Some(stage));
    let initialized = session.request(
        1,
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "process-recovery", "version": "0.1.0" }
        }),
    );
    assert_eq!(
        initialized["result"]["serverInfo"]["name"],
        "godot-mcp-server"
    );
    session.notify("notifications/initialized", json!({}));
    session.send(&json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "project.run",
            "arguments": { "headless": true, "deadline_ms": 600_000 }
        }
    }));
    session.wait_for_exit_code(Duration::from_secs(20))
}

#[cfg(target_os = "linux")]
fn restart_process_server(project: &Path, godot: &Path) -> FinishedSession {
    StdioSession::spawn_engine(project, godot, None).finish()
}

#[cfg(target_os = "linux")]
fn linked_process_records(
    harness: &JournalFoundationHarness,
) -> (ObservedTransaction, ObservedTransaction) {
    let records = harness
        .observe_all()
        .expect("read all process transactions");
    let outer = records
        .iter()
        .find(|record| record.required_process_transaction.is_some())
        .cloned()
        .unwrap_or_else(|| panic!("linked outer process transaction is missing: {records:?}"));
    let inner_id = outer
        .required_process_transaction
        .as_deref()
        .expect("outer link exists");
    let inner = records
        .iter()
        .find(|record| record.transaction_id == inner_id)
        .cloned()
        .unwrap_or_else(|| panic!("canonical inner process transaction is missing: {records:?}"));
    (outer, inner)
}

#[cfg(target_os = "linux")]
fn wait_for_pid(path: &Path) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(value) = std::fs::read_to_string(path)
            && let Ok(pid) = value.trim().parse()
        {
            return pid;
        }
        assert!(Instant::now() < deadline, "process PID was not written");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(target_os = "linux")]
fn wait_for_process_exit(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Path::new(&format!("/proc/{pid}")).exists() {
        assert!(Instant::now() < deadline, "process {pid} did not exit");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn drive_until_fault(
    harness: &JournalFoundationHarness,
    effect: &MockFileEffect,
) -> Result<(), String> {
    harness
        .open_transaction(effect)
        .map_err(|error| error.to_string())?;
    harness
        .stage_transaction(effect)
        .map_err(|error| error.to_string())?;
    harness
        .execute_transaction(effect)
        .map(|_disposition| ())
        .map_err(|error| error.to_string())
}

// ---------------------------------------------------------------------------
// Out-of-process stdio driver
// ---------------------------------------------------------------------------

struct StdioSession {
    child: Child,
    reader: BufReader<std::process::ChildStdout>,
    transcript: String,
}

struct FinishedSession {
    stdout: String,
    stderr: String,
}

impl StdioSession {
    fn spawn(project: &Path) -> Self {
        Self::spawn_with(project, None, "core", None)
    }

    #[cfg(target_os = "linux")]
    fn spawn_engine(project: &Path, godot: &Path, crash_stage: Option<&str>) -> Self {
        Self::spawn_with(project, Some(godot), "runtime", crash_stage)
    }

    fn spawn_with(
        project: &Path,
        godot: Option<&Path>,
        profile: &str,
        crash_stage: Option<&str>,
    ) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_qa-journal-foundation"));
        command
            .arg("--project")
            .arg(project)
            .arg("--profile")
            .arg(profile);
        if let Some(godot) = godot {
            command.arg("--godot-binary").arg(godot);
        }
        match crash_stage {
            Some(stage) => {
                command.env("GODOT_MCP_TEST_LAUNCH_CRASH_STAGE", stage);
            }
            None => {
                command.env_remove("GODOT_MCP_TEST_LAUNCH_CRASH_STAGE");
            }
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("stdio server starts");
        let reader = BufReader::new(child.stdout.take().expect("stdout is piped"));
        Self {
            child,
            reader,
            transcript: String::new(),
        }
    }

    fn send(&mut self, frame: &Value) {
        let stdin = self.child.stdin.as_mut().expect("stdin is piped");
        let mut line = serde_json::to_string(frame).expect("frame serializes");
        line.push('\n');
        stdin.write_all(line.as_bytes()).expect("write frame");
        stdin.flush().expect("flush frame");
    }

    fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        }));
        loop {
            let mut line = String::new();
            let read = self.reader.read_line(&mut line).expect("read frame");
            assert!(
                read > 0,
                "the server closed stdout before answering {method}"
            );
            self.transcript.push_str(&line);
            let frame: Value = serde_json::from_str(line.trim())
                .unwrap_or_else(|error| panic!("stdout frame must be JSON: {line:?} ({error})"));
            if frame["id"] == json!(id) {
                return frame;
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn call_tool(&mut self, id: u64, name: &str, arguments: Value) -> Value {
        self.request(
            id,
            "tools/call",
            json!({
                "name": name,
                "arguments": arguments,
            }),
        )
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    fn finish(mut self) -> FinishedSession {
        drop(self.child.stdin.take());
        let mut remaining = String::new();
        while self
            .reader
            .read_line(&mut remaining)
            .expect("drain remaining stdout")
            > 0
        {
            self.transcript.push_str(&remaining);
            remaining.clear();
        }
        let output = self.child.wait_with_output().expect("server exits");
        FinishedSession {
            stdout: self.transcript,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    #[cfg(target_os = "linux")]
    fn wait_for_exit_code(mut self, timeout: Duration) -> i32 {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().expect("query server status") {
                return status.code().unwrap_or(-1);
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                panic!("stdio server did not exit within {timeout:?}");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
