//! Todo 27 acceptance suite: scheduler fairness, backpressure, deadlines,
//! cancellation and cross-plane idempotency over the *existing* Todo 6 journal.
//!
//! Both descriptor-owned tests are exhaustive by design: this file is the
//! mechanical entry point behind `make qa-task TASK=27`, so every acceptance
//! criterion is asserted here rather than spread across ad-hoc tests.
//!
//! Time is deterministic. Every wait is driven by `tokio::test(start_paused =
//! true)`, so no assertion depends on a real sleep winning a race.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use godot_mcp_server::idempotency::{
    OperationToolFamily, RecordedStatus, TransportAuthMode, TupleDimension,
};
use godot_mcp_server::scheduler::{
    CancellationOutcome, EffectPlane, OperationDeadline, QueueDeadline, QueuePriority,
    SchedulerLimits, ShutdownOutcome, StarvationBound, WriteBlock, priority_for,
};
use godot_mcp_server::test_support::{RequestSpec, SchedulerIdempotencyHarness};
use tokio_util::sync::CancellationToken;

const FIXTURE: &str = "tests/fixtures/minimal";

/// Fixed transaction IDs, so every assertion names an exact durable record.
const TX_RECORDED: &str = "00000000-0000-4000-8000-000000002701";
const TX_SECOND: &str = "00000000-0000-4000-8000-000000002702";
const TX_CANCELLED: &str = "00000000-0000-4000-8000-000000002703";
const TX_QUARANTINE: &str = "00000000-0000-4000-8000-000000002704";
const TX_SURVIVOR_BLOCKED: &str = "00000000-0000-4000-8000-000000002705";
const TX_AFTER_REPAIR: &str = "00000000-0000-4000-8000-000000002706";
/// One transaction per tuple dimension, so each mismatch has its own record.
const TX_PER_DIMENSION: [&str; 8] = [
    "00000000-0000-4000-8000-000000002710",
    "00000000-0000-4000-8000-000000002711",
    "00000000-0000-4000-8000-000000002712",
    "00000000-0000-4000-8000-000000002713",
    "00000000-0000-4000-8000-000000002714",
    "00000000-0000-4000-8000-000000002715",
    "00000000-0000-4000-8000-000000002716",
    "00000000-0000-4000-8000-000000002717",
];

const TX_CROSS_TOOL: &str = "00000000-0000-4000-8000-000000002720";
const TX_DISTINCT_IDEM: &str = "00000000-0000-4000-8000-000000002721";

const IDEM_RECORDED: &str = "idem-27-recorded";
const IDEM_CANCELLED: &str = "idem-27-cancelled";
const IDEM_AFTER_REPAIR: &str = "idem-27-after-repair";
const WRITE_PATH: &str = "scenes/todo27.tscn";

fn fixture_source() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE)
}

/// Copy the committed fixture into a writable temp root so a test can mutate it
/// without touching the repository copy.
fn writable_project(label: &str) -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::Builder::new()
        .prefix(&format!("godot-mcp-task27-{label}-"))
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
    // directory handle and owns no directory-creation primitive, so the fixture
    // provides the destination directory the permitted write lands in.
    std::fs::create_dir_all(root.join("scenes")).expect("scenes directory");
    (temp, root)
}

/// Small, deterministic limits: a read lane wide enough to prove overlap, a queue
/// small enough that saturation is reachable, and finite deadlines.
fn tight_limits() -> SchedulerLimits {
    SchedulerLimits {
        read_concurrency: 4,
        queue_limit: 8,
        starvation_bound: StarvationBound::new(3).expect("positive starvation bound"),
        queue_deadline: QueueDeadline::new(5_000).expect("positive queue deadline"),
        operation_deadline: OperationDeadline::new(10_000).expect("positive operation deadline"),
        shutdown_budget_ms: 5_000,
    }
}

fn open_harness(label: &str) -> (tempfile::TempDir, PathBuf, SchedulerIdempotencyHarness) {
    let (temp, root) = writable_project(label);
    let harness = SchedulerIdempotencyHarness::open(&root, tight_limits())
        .expect("scheduler harness opens over the existing journal");
    (temp, root, harness)
}

// ---------------------------------------------------------------------------
// Happy path
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true, flavor = "current_thread")]
async fn scheduler_idempotency_happy_path() {
    let (_temp, _root, harness) = open_harness("happy");

    // 1. Concurrent reads overlap. The bounded read semaphore admits up to its
    //    limit at once, so more than one read is genuinely in flight.
    let overlap = harness
        .concurrent_reads(4, QueuePriority::Interactive)
        .await;
    assert_eq!(overlap.admitted, 4, "every requested read must be admitted");
    assert!(
        overlap.overlapped(),
        "reads must run concurrently, observed peak {}",
        overlap.max_concurrent
    );
    assert!(
        overlap.max_concurrent <= tight_limits().read_concurrency,
        "read concurrency must stay inside its bound, observed {}",
        overlap.max_concurrent
    );

    // 2. One journaled write executes exclusively through the existing Todo 6
    //    store, and it really touched the boundary exactly once.
    let spec = RequestSpec::stdio_project_mutation(TX_RECORDED, IDEM_RECORDED, WRITE_PATH);
    let first = harness
        .execute_mutation(&spec)
        .await
        .expect("the first mutation is authorized and executes");
    assert_eq!(first.transaction_id, TX_RECORDED);
    assert!(first.executed, "the first attempt must reach the boundary");
    assert!(
        !first.replayed,
        "the first attempt cannot be a replay of anything"
    );
    assert_eq!(
        first.status, "committed",
        "the write must settle as committed in the Todo 6 journal"
    );
    assert_eq!(
        harness.effect_touches(),
        1,
        "exactly one boundary invocation may have happened"
    );
    assert_eq!(
        harness
            .phase_of(TX_RECORDED)
            .expect("the journaled phase is readable"),
        "committed_fsynced",
        "the durable phase must be the Todo 6 terminal commit record"
    );
    assert_eq!(
        harness
            .recorded_status(TX_RECORDED)
            .expect("the recorded status is readable"),
        RecordedStatus::Committed,
        "a committed record must not be replayable"
    );
    assert!(
        !RecordedStatus::Committed.may_execute_again(),
        "a committed mutation needs no replay"
    );
    assert_eq!(
        harness
            .read_path(WRITE_PATH)
            .expect("the published path is readable")
            .as_deref(),
        Some(b"todo-27".as_slice()),
        "the permitted write must really have landed on disk"
    );
    assert_eq!(
        harness.project_generation(),
        harness.project().generation(),
        "the harness and the held project must agree on the generation"
    );

    // 3. The exact same-context retry receives the recorded result *without*
    //    replaying the effect. The boundary count is the proof: it does not move.
    let retry = harness
        .execute_mutation(&spec)
        .await
        .expect("an exact same-context retry is not a conflict");
    assert!(retry.replayed, "the retry must be answered from the record");
    assert!(
        !retry.executed,
        "the retry must not reach the boundary again"
    );
    assert_eq!(
        retry.transaction_id, first.transaction_id,
        "the retry must return its own prior transaction"
    );
    assert_eq!(
        retry.status, first.status,
        "the retry must return its own prior status"
    );
    assert_eq!(
        retry.result, first.result,
        "the retry must return its own prior result"
    );
    assert_eq!(
        harness.effect_touches(),
        1,
        "a replay must produce no additional side effect"
    );

    // 4. The identity the retry presented is byte-identical in all eight
    //    elements, which is the *only* condition that permits a replay.
    let recorded_identity = harness.identity(&spec).expect("identity is representable");
    let retry_identity = harness.identity(&spec).expect("identity is representable");
    assert!(
        recorded_identity.matches_exactly(&retry_identity),
        "an exact same-context retry must match on every tuple element"
    );
    let (mode, principal, session) = harness.bound_context();
    assert_eq!(
        mode,
        TransportAuthMode::Stdio,
        "this harness binds the stdio transport"
    );
    assert_eq!(
        principal.required_mode(),
        TransportAuthMode::Stdio,
        "stdio must carry the local OS principal shape"
    );
    assert!(
        !session.as_str().is_empty(),
        "stdio must carry its connection session"
    );
    assert_eq!(
        recorded_identity.family(),
        OperationToolFamily::ProjectMutation,
        "a resource write belongs to the project-mutation family"
    );

    // 5. A second, genuinely different write in the same context proceeds: the
    //    idempotency gate blocks reuse, not ordinary work.
    let second = RequestSpec::stdio_project_mutation(TX_SECOND, "idem-27-second", WRITE_PATH);
    let second = harness
        .execute_mutation(&second)
        .await
        .expect("a distinct idempotency identifier is not a conflict");
    assert!(second.executed, "distinct work must actually execute");
    assert_eq!(
        harness.effect_touches(),
        2,
        "the second distinct write is the second boundary invocation"
    );

    // 6. Shutdown drains cleanly with no deadlock and nothing left in flight.
    let outcome = harness.shutdown().await;
    assert_eq!(
        outcome,
        ShutdownOutcome::Drained,
        "settled work must allow a clean drain"
    );
    assert_eq!(
        harness.scheduler().running(),
        0,
        "nothing may still be running after a clean drain"
    );
    assert_eq!(
        harness.scheduler().queued(),
        0,
        "nothing may still be queued after a clean drain"
    );
}

// ---------------------------------------------------------------------------
// Failure paths
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true, flavor = "current_thread")]
async fn scheduler_idempotency_failure_paths() {
    // --- Queue starvation is bounded -------------------------------------
    {
        let (_temp, _root, harness) = open_harness("starvation");
        let snapshot = harness.saturate(16, 8).await;
        let bound = harness.scheduler().starvation_bound();
        let bulk_priority = priority_for(OperationToolFamily::ProjectImport);
        let interactive_priority = priority_for(OperationToolFamily::StaticAnalysis);
        assert!(
            snapshot.max_observed_overtakes() <= bound.overtakes(),
            "no waiter may be overtaken more than {} times; observed {}",
            bound.overtakes(),
            snapshot.max_observed_overtakes()
        );
        assert!(
            snapshot.promotions() > 0,
            "a saturating burst must promote at least one starved waiter"
        );
        assert!(
            snapshot.admitted(bulk_priority) > 0,
            "the mapped project-import class must still make progress"
        );
        assert!(
            snapshot.admitted(interactive_priority) > 0,
            "the mapped static-analysis class must be served"
        );
    }

    // --- Cancellation race: queued work never starts ----------------------
    {
        let (_temp, _root, harness) = open_harness("cancel");
        let spec = RequestSpec::stdio_project_mutation(TX_CANCELLED, IDEM_CANCELLED, WRITE_PATH);
        let token = CancellationToken::new();
        let before = harness.effect_touches();
        let outcome = harness.cancel_while_queued(&spec, &token).await;
        match outcome {
            Ok(CancellationOutcome::CancelledBeforeStart) => {}
            Ok(other) => panic!("queued work must not report {other:?}"),
            Err(refusal) => assert_eq!(
                refusal.code, "cancelled",
                "cancelled queued work must be reported as cancelled, got {}: {}",
                refusal.code, refusal.message
            ),
        }
        assert_eq!(
            harness.effect_touches(),
            before,
            "cancelled queued work must never reach the boundary"
        );
        assert!(
            harness.phase_of(TX_CANCELLED).is_err(),
            "cancelled queued work must leave no durable transaction behind"
        );
        assert!(
            CancellationOutcome::CancelledBeforeStart.proves_no_effect(),
            "a before-start cancellation is a proof that nothing happened"
        );
        assert!(
            !CancellationOutcome::CancelledDuringExecution.proves_no_effect(),
            "a during-execution cancellation proves nothing about the effect"
        );
        assert_eq!(
            CancellationOutcome::CancelledDuringExecution
                .to_outcome()
                .code(),
            "uncertain",
            "a cancellation after the boundary was touched must not claim the effect did not happen"
        );
    }

    // --- A PID survivor blocks every mutation -----------------------------
    {
        let (_temp, _root, harness) = open_harness("survivor");
        harness.declare_survivor(Some("supervised process 4242 is still contained"));
        let block = harness
            .write_block()
            .expect("the block state is readable")
            .expect("a declared survivor must block writes");
        assert!(
            matches!(block, WriteBlock::UnresolvedSurvivor(_)),
            "the block must name the survivor case, got {block:?}"
        );
        let spec = RequestSpec::stdio_project_mutation(
            TX_SURVIVOR_BLOCKED,
            "idem-27-survivor",
            WRITE_PATH,
        );
        let before = harness.effect_touches();
        let refusal = harness
            .execute_mutation(&spec)
            .await
            .expect_err("a survivor-blocked mutation must be refused");
        assert_eq!(
            refusal.code, "conflict",
            "a survivor block is a conflict, got {}: {}",
            refusal.code, refusal.message
        );
        assert_eq!(
            harness.effect_touches(),
            before,
            "a survivor-blocked mutation must never reach the boundary"
        );

        // Clearing the survivor is not an operator repair, but it is the same
        // proof-producing path Todo 17 owns: once nothing is unresolved, writes
        // resume. The point asserted here is that the *block* is what gates the
        // write, and that it cannot be waved away while it is live.
        harness.declare_survivor(None);
        assert!(
            harness
                .write_block()
                .expect("the block state is readable")
                .is_none(),
            "a resolved survivor must stop blocking writes"
        );
    }

    // --- Quarantine denies every mutation until a proof-producing repair ---
    {
        let (_temp, _root, harness) = open_harness("quarantine");
        harness
            .quarantine(TX_QUARANTINE, WRITE_PATH)
            .expect("the harness can drive a transaction to quarantined");
        let block = harness
            .write_block()
            .expect("the block state is readable")
            .expect("a quarantined transaction must block writes");
        assert!(
            matches!(block, WriteBlock::Quarantined(_)),
            "the block must name the quarantine case, got {block:?}"
        );

        let blocked =
            RequestSpec::stdio_project_mutation(TX_AFTER_REPAIR, IDEM_AFTER_REPAIR, WRITE_PATH);
        let before = harness.effect_touches();
        let refusal = harness
            .execute_mutation(&blocked)
            .await
            .expect_err("a quarantined project must refuse every mutation");
        assert_eq!(
            refusal.code, "conflict",
            "a quarantine block is a conflict, got {}: {}",
            refusal.code, refusal.message
        );
        assert_eq!(
            harness.effect_touches(),
            before,
            "a quarantined mutation must never reach the boundary"
        );

        // Only a proof-producing operator repair unblocks writes. The harness
        // cannot mint a proof itself; it must run the real recovery path.
        let proof = harness
            .repair(TX_QUARANTINE)
            .expect("the operator repair re-derives a typed proof");
        assert_eq!(
            proof.transaction_id(),
            TX_QUARANTINE,
            "the proof must name the transaction it resolved"
        );
        harness
            .scheduler()
            .resolve_write_block(&proof)
            .expect("a proof for the blocking transaction lifts the block");
        assert!(
            harness
                .write_block()
                .expect("the block state is readable")
                .is_none(),
            "a repaired quarantine must stop blocking writes"
        );
        let after = harness
            .execute_mutation(&blocked)
            .await
            .expect("writes resume once the block is proven resolved");
        assert!(
            after.executed,
            "the previously blocked mutation must now execute"
        );
    }

    // --- The editor plane is refused honestly, never faked ----------------
    {
        let (_temp, _root, harness) = open_harness("planes");
        let availability = harness.scheduler().plane_availability();
        assert!(
            availability.of(EffectPlane::File),
            "the file boundary ships in this release"
        );
        for plane in [EffectPlane::Editor, EffectPlane::Runtime] {
            assert!(
                !availability.of(plane),
                "the {} bridge is not shipped in this release",
                plane.as_str()
            );
            let before = harness.effect_touches();
            let refusal = harness
                .probe_plane(plane, QueuePriority::Normal)
                .await
                .expect_err("an unavailable plane must be refused");
            assert_eq!(
                refusal.code,
                "unavailable",
                "an absent {} bridge must be reported as unavailable, got {}: {}",
                plane.as_str(),
                refusal.code,
                refusal.message
            );
            assert_eq!(
                harness.effect_touches(),
                before,
                "an unavailable plane must not touch any boundary"
            );
        }
        // Identity is not availability: every family, editor included, is still
        // nameable, because the frozen manifests still advertise those tools.
        let families: BTreeSet<&str> = OperationToolFamily::ALL
            .iter()
            .map(|family| family.as_str())
            .collect();
        for editor_family in [
            OperationToolFamily::EditorSceneMutation,
            OperationToolFamily::EditorInspection,
        ] {
            assert!(
                families.contains(editor_family.as_str()),
                "the {} family must remain part of mutation identity",
                editor_family.as_str()
            );
        }
    }

    // --- Reuse across every tuple dimension is one byte-stable conflict ----
    {
        let (_temp, _root, harness) = open_harness("tuple");
        let recorded = RequestSpec::stdio_project_mutation(TX_RECORDED, IDEM_RECORDED, WRITE_PATH);
        let first = harness
            .execute_mutation(&recorded)
            .await
            .expect("the recorded write executes");
        assert!(first.executed);
        let touches_after_record = harness.effect_touches();

        assert_eq!(
            TupleDimension::ALL.len(),
            8,
            "mutation identity is an eight-element tuple"
        );

        // Reuse means: the recorded idempotency identifier, presented from a
        // context that differs somewhere else. Every one of the seven other
        // dimensions is that kind of reuse, and every one must be refused with the
        // same bytes. The identifier itself is deliberately excluded here — a
        // *different* identifier is not reuse at all, and is asserted separately
        // below.
        let reuse_dimensions: Vec<TupleDimension> = TupleDimension::ALL
            .into_iter()
            .filter(|dimension| *dimension != TupleDimension::IdempotencyId)
            .collect();
        assert_eq!(
            reuse_dimensions.len(),
            7,
            "seven dimensions can differ while the identifier is reused"
        );

        let mut envelopes: Vec<Vec<u8>> = Vec::new();
        let mut messages: BTreeSet<String> = BTreeSet::new();
        for (index, dimension) in reuse_dimensions.iter().copied().enumerate() {
            let mut varied = harness.vary(&recorded, dimension);
            varied.transaction_id = TX_PER_DIMENSION[index].to_owned();
            let refusal = harness.execute_mutation(&varied).await.expect_err(
                "reuse of a recorded idempotency identifier from a different context must be \
                 refused",
            );
            assert_eq!(
                refusal.code,
                "conflict",
                "a {} mismatch must be a conflict, got {}: {}",
                dimension.as_str(),
                refusal.code,
                refusal.message
            );
            let bytes = refusal.conflict_bytes.clone().unwrap_or_else(|| {
                panic!(
                    "a {} mismatch must carry the canonical conflict envelope",
                    dimension.as_str()
                )
            });
            messages.insert(refusal.message.clone());
            envelopes.push(bytes);

            // No duplicate effect, no guessed outcome, no leaked prior record.
            assert_eq!(
                harness.effect_touches(),
                touches_after_record,
                "a refused reuse must not reach the boundary"
            );
            assert!(
                harness.phase_of(TX_PER_DIMENSION[index]).is_err(),
                "a refused reuse must journal no transaction of its own"
            );
            for leak in [
                TX_RECORDED,
                IDEM_RECORDED,
                WRITE_PATH,
                "committed",
                "principal_",
                "session_",
                "generation",
                "resource.update",
            ] {
                assert!(
                    !refusal.message.contains(leak),
                    "a conflict must not disclose {leak}; message was {}",
                    refusal.message
                );
            }
        }
        assert_eq!(
            messages.len(),
            1,
            "every tuple mismatch must produce exactly one message, saw {messages:?}"
        );
        let canonical = envelopes
            .first()
            .cloned()
            .expect("seven envelopes were collected");
        for (index, bytes) in envelopes.iter().enumerate() {
            assert_eq!(
                bytes,
                &canonical,
                "the {} mismatch envelope must be byte-identical to every other",
                reuse_dimensions[index].as_str()
            );
        }

        // A cross-tool reuse — the same identifier naming a different canonical
        // operation — is refused with those same bytes, and never answered from the
        // other tool's record.
        let mut cross_tool = recorded.clone();
        cross_tool.transaction_id = TX_CROSS_TOOL.to_owned();
        cross_tool.tool = "scene.save".to_owned();
        let cross_refusal = harness
            .execute_mutation(&cross_tool)
            .await
            .expect_err("a cross-tool reuse must be refused");
        assert_eq!(cross_refusal.code, "conflict");
        assert_eq!(
            cross_refusal
                .conflict_bytes
                .as_deref()
                .expect("a cross-tool reuse carries the canonical envelope"),
            canonical.as_slice(),
            "a cross-tool reuse must be indistinguishable from every other reuse"
        );

        // A *different* identifier for an otherwise identical request is not reuse:
        // it is a second deliberate write, and it must proceed. Refusing it would
        // make byte-identical work unrepeatable rather than idempotent.
        let mut distinct = recorded.clone();
        distinct.transaction_id = TX_DISTINCT_IDEM.to_owned();
        distinct.idempotency_id = format!("{IDEM_RECORDED}-distinct");
        let distinct_report = harness
            .execute_mutation(&distinct)
            .await
            .expect("a fresh identifier is not reuse");
        assert!(
            distinct_report.executed && !distinct_report.replayed,
            "a fresh identifier must execute rather than replay: {distinct_report:?}"
        );
        assert_eq!(
            harness.effect_touches(),
            touches_after_record + 1,
            "the second deliberate write is exactly one further boundary invocation"
        );
    }

    // --- Queue full, both deadlines, and the in-flight cancellation --------
    {
        let (_temp, _root, harness) = open_harness("backpressure");
        let limits = tight_limits();

        // The queue is bounded: the one request past the limit is refused as
        // retryable `unavailable`, because it never started.
        let before = harness.effect_touches();
        let overflow = harness.queue_full().await;
        assert_eq!(
            overflow.code, "unavailable",
            "queue overflow must be refused as unavailable, got {}: {}",
            overflow.code, overflow.message
        );
        assert_eq!(
            harness.effect_touches(),
            before,
            "a queue-full refusal must reach no boundary"
        );
        assert!(
            harness.scheduler().queued() <= limits.queue_limit,
            "the queue must never exceed its declared limit, saw {}",
            harness.scheduler().queued()
        );

        // The queue deadline elapses while a mutation waits behind a running one.
        // Paused time makes this exact: the timer only fires because nothing else
        // is runnable, never because a real sleep happened to win.
        let expired = harness.queue_deadline_race().await;
        assert_eq!(
            expired.code, "unavailable",
            "queue-deadline expiry means the work never started, got {}: {}",
            expired.code, expired.message
        );
        assert!(
            expired.message.contains("never started"),
            "the refusal must say the work never started: {}",
            expired.message
        );
        assert_eq!(
            harness.effect_touches(),
            before,
            "deadline-expired queued work must reach no boundary"
        );

        // The two deadline classes are never collapsed: waiting too long is
        // retryable `unavailable`, running too long is non-retryable `timeout`.
        let queue_error = QueueDeadline::expired_error();
        assert_eq!(queue_error.code(), "unavailable");
        assert!(queue_error.retryable());
        let run_error = OperationDeadline::expired_error();
        assert_eq!(
            run_error.code(),
            "timeout",
            "work that started and did not finish is a timeout, not unavailable"
        );
        assert!(
            !run_error.retryable(),
            "an unproven effect must never be advertised as safe to retry"
        );
        assert!(
            limits.queue_deadline.wait_budget_ms() > 0
                && limits.operation_deadline.run_budget_ms() > 0,
            "both deadlines must be finite and positive"
        );

        // Cancellation *after* the boundary was marked reached is `uncertain`, and
        // the admission itself reports the during-execution classification.
        let during = harness
            .cancel_during_execution()
            .await
            .expect("a token tripped after the start mark is classified");
        assert_eq!(
            during,
            CancellationOutcome::CancelledDuringExecution,
            "a cancellation after the start mark must not be reported as before-start"
        );

        // An unreadable write-block state is a refusal, never an assumed "clear".
        harness.set_survivor_state_unreadable(true);
        assert!(
            matches!(
                harness
                    .write_block()
                    .expect("the probe answers rather than panicking"),
                Some(WriteBlock::Unreadable(_))
            ),
            "an unreadable block state must refuse rather than assume nothing is blocked"
        );
        harness.set_survivor_state_unreadable(false);

        let outcome = harness.shutdown().await;
        assert!(
            outcome.is_clean() || matches!(outcome, ShutdownOutcome::DeadlineExpired { .. }),
            "shutdown must report what it observed rather than assuming a drain"
        );
    }
}
