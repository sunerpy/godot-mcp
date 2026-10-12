//! Todo 32 acceptance suite: the cross-plane outcome router.
//!
//! Both descriptor-owned tests are exhaustive by design: this file is the
//! mechanical entry point behind `make qa-task TASK=32`, so every acceptance
//! criterion is asserted here rather than spread across ad-hoc tests.
//!
//! ## What is verified, and what is not
//!
//! The router is assembled through the **production** path
//! (`ProjectRuntime::open`), not a bespoke test assembly, so every assertion
//! below is about the code the shipped stdio binary runs. That is deliberate: the
//! recorded Todo 27 gap was precisely that the cross-plane idempotency gate, the
//! durable store and the survivor probe were bound only by a harness and never by
//! the production assembly. The first block of `outcome_router_happy_path`
//! asserts the binding is live on the production path.
//!
//! The editor and runtime planes are **not shipped in this release** (the
//! cancelled Todos 20-25), and the LSP/DAP plane remains unavailable until its
//! adapter is connected to server-owned editor launch receipts. Their branches
//! are kept in the router and refused honestly with `unavailable`. Consequently:
//!
//! * every *routing decision* for those planes is verified here, by injecting a
//!   deterministic [`PlaneObservation`] that claims the boundary exists; and
//! * the router's refusal to *execute* on them is verified here too, because the
//!   router re-derives availability from the effect gateway and therefore refuses
//!   even when the caller's observation is optimistic; but
//! * the real end-to-end behaviour of an `EditorUndoRedoManager` edit, a running
//!   game inspection or a screenshot capture against a live Godot editor is
//!   **NOT verified** — that plane does not ship, so there is nothing to drive.
//!   The same holds for a genuinely multi-plane composed execution: its plan is
//!   asserted, its step classification is asserted as a pure function, but no
//!   two-plane run can complete while three of the five boundaries are
//!   fail-closed.

use std::path::{Path, PathBuf};

use godot_mcp_domain::contracts::OperationPlane;
use godot_mcp_server::idempotency::{RecordedStatus, ReplayDecision};
use godot_mcp_server::router::{
    Degradation, DegradationReason, EnginePresence, ExecutionStrategy, PlaneObservation,
    ProtocolFidelity, RepairIntent, RouteCompletion, RouteRefusal, RuntimePresence,
    SnapshotAuthority, SnapshotPrecedence, StepOutcome, WorkspaceFreshness,
};
use godot_mcp_server::scheduler::{EffectPlane, PlaneAvailability, QueuePriority};
use godot_mcp_server::test_support::OutcomeRouterHarness;
use serde_json::Value;

const FIXTURE: &str = "tests/fixtures/minimal";

/// Fixed transaction IDs, so every assertion names an exact durable record.
const TX_WRITE: &str = "00000000-0000-4000-8000-000000003201";
const TX_REPLAY: &str = "00000000-0000-4000-8000-000000003202";
const TX_EDITOR: &str = "00000000-0000-4000-8000-000000003203";
const TX_PROCESS: &str = "00000000-0000-4000-8000-000000003204";
const TX_RUNTIME: &str = "00000000-0000-4000-8000-000000003205";
const TX_CONFLICT: &str = "00000000-0000-4000-8000-000000003206";
const TX_OPTIMISTIC: &str = "00000000-0000-4000-8000-000000003207";
const TX_QUARANTINE: &str = "00000000-0000-4000-8000-000000003208";
const TX_BLOCKED: &str = "00000000-0000-4000-8000-000000003209";
const TX_SURVIVOR: &str = "00000000-0000-4000-8000-000000003210";
const TX_AFTER_REPAIR: &str = "00000000-0000-4000-8000-000000003211";
const TX_UNKNOWN_TOOL: &str = "00000000-0000-4000-8000-000000003212";

const WRITE_PATH: &str = "scenes/router.tscn";
const CONFLICT_PATH: &str = "scenes/unsaved.tscn";
const QUARANTINE_PATH: &str = "scenes/quarantined.tscn";
const BLOCKED_PATH: &str = "scenes/blocked.tscn";
const SURVIVOR_PATH: &str = "scenes/survivor.tscn";
const AFTER_REPAIR_PATH: &str = "scenes/after-repair.tscn";

/// Every mutating catalog tool, one per family and plane the catalog implements.
const MUTATING_TOOLS: [&str; 4] = [
    "resource.update",
    "scene.save",
    "project.uid.update",
    "project.import.run",
];

/// The frozen version 0.1 editor and runtime mutation shapes, so the
/// write-blocking assertion keeps covering the planes later milestones
/// implement rather than only the shipped ones. They are routed through
/// `route_historical`, never discovered.
const HISTORICAL_MUTATING_SHAPES: [&str; 2] = ["node.create", "runtime.screenshot.capture"];

fn fixture_source() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE)
}

/// Copy the committed fixture into a writable temp root so a test can mutate it
/// without touching the repository copy.
fn writable_project(label: &str) -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::Builder::new()
        .prefix(&format!("godot-mcp-task32-{label}-"))
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

fn open_harness(label: &str) -> (tempfile::TempDir, PathBuf, OutcomeRouterHarness) {
    let (temp, root) = writable_project(label);
    let harness = OutcomeRouterHarness::open(&root).expect("router harness");
    (temp, root, harness)
}

/// An observation that claims every boundary exists. It is the *decision-table*
/// input: it lets the plan for an unshipped plane be asserted without pretending
/// the plane can execute, because `route` re-derives availability from the
/// gateway.
fn claiming_every_boundary(
    engine: EnginePresence,
    runtime: RuntimePresence,
    protocol: ProtocolFidelity,
    freshness: WorkspaceFreshness,
) -> PlaneObservation {
    PlaneObservation::new(
        PlaneAvailability::new(true, true, true, true, true),
        engine,
        runtime,
        protocol,
        freshness,
    )
}

/// The full-capability environment: an editor is attached and a game is running.
fn everything_present() -> PlaneObservation {
    claiming_every_boundary(
        EnginePresence::EditorConnected,
        RuntimePresence::GameRunning,
        ProtocolFidelity::Full,
        WorkspaceFreshness::Fresh,
    )
}

/// The environment this release actually ships: boundaries as the gateway reports
/// them, a clean workspace and no editor or game attached.
fn as_shipped(harness: &OutcomeRouterHarness, freshness: WorkspaceFreshness) -> PlaneObservation {
    PlaneObservation::new(
        harness.router().plane_availability(),
        EnginePresence::HeadlessOnly,
        RuntimePresence::NoInstance,
        ProtocolFidelity::Full,
        freshness,
    )
}

#[test]
fn engine_mutations_enter_the_outcome_router() {
    let process = include_str!("../src/engine_plane/process.rs");
    let canonical = include_str!("../src/engine_plane/canonical.rs");

    assert!(
        process.contains(".route(&"),
        "engine process mutations must enter OutcomeRouter::route"
    );
    assert!(
        canonical.contains(".route(&"),
        "canonical engine writes must enter OutcomeRouter::route"
    );
    assert!(
        !process.contains(".dispatch_process_effect("),
        "engine process handlers must not bypass the router"
    );
    assert!(
        !canonical.contains(".dispatch_staged_file_effect("),
        "canonical engine handlers must not bypass the router"
    );
}

#[test]
fn engine_mutations_consume_recorded_replays() {
    let process = include_str!("../src/engine_plane/process.rs");
    let canonical = include_str!("../src/engine_plane/canonical.rs");
    let router = include_str!("../src/router/mod.rs");

    for (name, source) in [("process", process), ("canonical", canonical)] {
        assert_eq!(
            source.matches("report.replayed()").count(),
            1,
            "the shipping {name} unpacker must distinguish an exact recorded replay"
        );
        assert_eq!(
            source.matches("report.recorded_status()").count(),
            1,
            "the shipping {name} unpacker must consume the durable recorded status"
        );
        assert_eq!(
            source.matches("report.recorded_result()").count(),
            1,
            "the shipping {name} unpacker must return the durable recorded result"
        );
    }
    assert_eq!(
        router.matches("pub fn recorded_result(&self)").count(),
        1,
        "RouteReport must expose exactly one read-only recorded-result accessor"
    );
    assert_eq!(
        router
            .matches("recorded_result: recorded.result().cloned()")
            .count(),
        1,
        "the replay report must carry the result read from the existing durable journal"
    );
}

/// A launch's containment boundary is named after its transaction, and a name
/// such as a Windows Job Object's is shared by every process on the host: two
/// servers that start a launch in the same millisecond must not mint the same
/// transaction ID.
#[test]
fn two_routers_never_mint_the_same_transaction_id() {
    let (_first_temp, _first_root, first) = open_harness("first-router");
    let (_second_temp, _second_root, second) = open_harness("second-router");
    assert_ne!(
        first.next_transaction_id("supervised-launch", 1_000),
        second.next_transaction_id("supervised-launch", 1_000)
    );
}

#[tokio::test]
async fn outcome_router_happy_path() {
    let (_temp, _root, harness) = open_harness("happy");
    let router = harness.router();

    let first_transaction = harness.next_transaction_id("project.run", 1_000);
    let second_transaction = harness.next_transaction_id("project.run", 1_000);
    assert_ne!(first_transaction, second_transaction);
    assert_eq!(first_transaction.len(), 36);
    assert_eq!(first_transaction.as_bytes()[14], b'4');
    assert_eq!(first_transaction.as_bytes()[19], b'8');

    // ---------------------------------------------------------------------
    // 0. The recorded Todo 27 production-wiring gap is closed.
    //
    // This block is the whole reason the router owns the closure: the gate, the
    // durable store and the survivor probe are bound by the *production*
    // assembly, so the shipped stdio binary enforces cross-plane idempotency and
    // survivor write-blocking rather than compiling three `None`s.
    // ---------------------------------------------------------------------
    assert!(
        harness.assembled_through_production_path(),
        "the harness must drive the production assembly, not a bespoke test wiring"
    );
    let binding = harness.scheduler().journal_binding();
    assert!(
        binding.idempotency,
        "the cross-plane idempotency gate must be bound on the production path"
    );
    assert!(
        binding.store,
        "the durable transaction store must be bound on the production path"
    );
    assert!(
        binding.survivors,
        "the launch survivor probe must be bound on the production path"
    );
    assert!(
        binding.is_complete(),
        "a partially bound scheduler must never report itself as ready"
    );
    let production_planes = harness.router().plane_availability();
    assert!(
        production_planes.of(EffectPlane::File),
        "the production runtime must retain the safe-root file boundary"
    );
    assert!(
        production_planes.of(EffectPlane::Process),
        "the production runtime must attach the supervised process boundary"
    );
    for unavailable in [
        EffectPlane::LspDap,
        EffectPlane::Editor,
        EffectPlane::Runtime,
    ] {
        assert!(
            !production_planes.of(unavailable),
            "unwired or cancelled live plane {} must remain fail-closed",
            unavailable.as_str()
        );
    }
    let health = harness
        .call_tool("health.get", serde_json::json!({}))
        .await
        .expect("the production runtime registers the advertised core handler");
    assert!(
        matches!(
            health.get("status").and_then(Value::as_str),
            Some("ready" | "degraded")
        ),
        "health.get must return the real core outcome: {health}"
    );
    for tool in [
        "project.import.run",
        "project.uid.update",
        "resource.update",
        "scene.save",
        "project.run",
        "project.test",
    ] {
        if let Err(error) = harness.call_tool(tool, serde_json::json!({})).await {
            assert_ne!(
                error.to_string(),
                "tool implementation is not available yet",
                "{tool} must reach its production handler rather than the kernel placeholder"
            );
        }
    }
    // Contract 0.2: `debug.session.get` is a catalog tool answered through a
    // managed editor session's launch receipt. With no editor session the
    // lsp_dap plane cannot serve, so the call is refused as unavailable, naming
    // that plane, rather than reaching a placeholder or being absent.
    let refused = harness
        .call_tool("debug.session.get", serde_json::json!({}))
        .await
        .expect_err("no editor session hosts a debug adapter yet")
        .to_string();
    assert!(
        refused.contains("lsp_dap")
            && !refused.contains("tool was not found")
            && refused != "tool implementation is not available yet",
        "debug.session.get must be refused by its unavailable plane: {refused}"
    );
    // Behavioural corroboration: both durable reads answer instead of reporting
    // "no journal is bound to this scheduler".
    assert_eq!(
        harness
            .scheduler()
            .write_block()
            .expect("the write-block state is readable"),
        None,
        "a freshly reconciled project must not be write-blocked"
    );
    let identity = harness.identity_for(
        "resource.update",
        "idem-32-binding",
        &serde_json::json!({ "contents": "todo-32", "path": WRITE_PATH }),
    );
    assert!(
        matches!(
            harness.scheduler().replay_decision(&identity),
            Ok(ReplayDecision::Fresh)
        ),
        "the bound gate must decide a fresh identity instead of refusing as unbound"
    );

    // ---------------------------------------------------------------------
    // 1. Analyze — static analysis plane, no boundary, no effect.
    // ---------------------------------------------------------------------
    let analyze = router
        .plan("gdscript.symbols.get", &everything_present())
        .expect("a canonical analysis tool has a plan");
    assert_eq!(analyze.strategy(), ExecutionStrategy::StaticAnalysis);
    assert!(analyze.required_planes().is_empty());
    assert!(analyze.effects().is_empty());
    assert_eq!(analyze.outcome_plane(), OperationPlane::Static);
    assert_eq!(analyze.degradation(), Degradation::None);
    assert!(!analyze.is_mutation());

    // ---------------------------------------------------------------------
    // 2. Validate — same plane, still no effect.
    // ---------------------------------------------------------------------
    let validate = router
        .plan("project.validate", &everything_present())
        .expect("a canonical validation tool has a plan");
    assert_eq!(validate.strategy(), ExecutionStrategy::StaticAnalysis);
    assert_eq!(validate.outcome_plane(), OperationPlane::Static);
    assert!(validate.effects().is_empty());

    // ---------------------------------------------------------------------
    // 3. Edit via UndoRedo — the editor plane is required and selected.
    //
    // The decision is asserted under an observation that claims the editor
    // boundary exists. Execution is then refused, because the router re-derives
    // availability from the gateway and this release ships no editor bridge.
    // ---------------------------------------------------------------------
    let edit = router
        .plan_historical("node.property.set", &everything_present())
        .expect("an editor mutation has a plan");
    assert_eq!(edit.strategy(), ExecutionStrategy::Editor);
    assert_eq!(edit.required_planes(), [EffectPlane::Editor]);
    assert_eq!(edit.effects(), [EffectPlane::Editor]);
    assert_eq!(edit.outcome_plane(), OperationPlane::Editor);
    assert!(edit.is_mutation());
    assert_eq!(
        edit.degradation(),
        Degradation::None,
        "an editor-safe mutation is never planned as a degraded answer"
    );
    let refused_edit = router
        .route_historical(&harness.request(
            "node.property.set",
            "idem-32-edit",
            Some(TX_EDITOR),
            WRITE_PATH,
            everything_present(),
        ))
        .await
        .expect_err("this release ships no editor bridge");
    assert_eq!(refused_edit.to_outcome().code(), "unavailable");
    assert!(
        refused_edit.to_outcome().message().contains("editor"),
        "the refusal must name the missing plane: {}",
        refused_edit.to_outcome().message()
    );

    // ---------------------------------------------------------------------
    // 4. Run — the subprocess plane is required and selected.
    // ---------------------------------------------------------------------
    let run = router
        .plan("project.run", &everything_present())
        .expect("a runtime execution tool has a plan");
    assert_eq!(run.strategy(), ExecutionStrategy::Subprocess);
    assert_eq!(run.required_planes(), [EffectPlane::Process]);
    assert_eq!(run.effects(), [EffectPlane::Process]);
    assert_eq!(run.outcome_plane(), OperationPlane::Runtime);

    // ---------------------------------------------------------------------
    // 5. Inspect — a receipt-bound channel read on the runtime plane.
    // ---------------------------------------------------------------------
    let inspect = router
        .plan_historical("runtime.tree.get", &everything_present())
        .expect("a runtime inspection tool has a plan");
    assert_eq!(inspect.strategy(), ExecutionStrategy::ReceiptBoundLspDap);
    assert_eq!(inspect.required_planes(), [EffectPlane::Runtime]);
    assert!(
        inspect.effects().is_empty(),
        "an inspection observes and must never be reported as an effect"
    );
    let debug = router
        .plan_historical("debug.session.get", &everything_present())
        .expect("a debug read has a plan");
    assert_eq!(debug.strategy(), ExecutionStrategy::ReceiptBoundLspDap);
    assert_eq!(debug.required_planes(), [EffectPlane::LspDap]);

    // ---------------------------------------------------------------------
    // 6. Capture — the same plane, but this one is an effect.
    // ---------------------------------------------------------------------
    let capture = router
        .plan_historical("runtime.screenshot.capture", &everything_present())
        .expect("a capture tool has a plan");
    assert_eq!(capture.strategy(), ExecutionStrategy::ReceiptBoundLspDap);
    assert_eq!(capture.required_planes(), [EffectPlane::Runtime]);
    assert_eq!(capture.effects(), [EffectPlane::Runtime]);
    assert!(capture.is_mutation());

    // ---------------------------------------------------------------------
    // 7. Composed execution — more than one plane in one outcome.
    // ---------------------------------------------------------------------
    let import = router
        .plan("project.import.run", &everything_present())
        .expect("an import has a plan");
    assert_eq!(import.strategy(), ExecutionStrategy::Composed);
    assert_eq!(
        import.required_planes(),
        [EffectPlane::Process, EffectPlane::File]
    );
    assert_eq!(import.effects(), [EffectPlane::Process, EffectPlane::File]);
    let save = router
        .plan("scene.save", &everything_present())
        .expect("a scene save has a plan");
    assert_eq!(save.strategy(), ExecutionStrategy::Composed);
    assert_eq!(
        save.required_planes(),
        [EffectPlane::Editor, EffectPlane::File],
        "a scene save must consult the live editor buffer before it writes bytes"
    );
    assert!(save.requires_editor_consult());

    // ---------------------------------------------------------------------
    // 8. A real, shipped mutation: the file plane executes, commits, and the
    //    affected snapshot is refreshed *after* the commit is proven.
    // ---------------------------------------------------------------------
    let write_plan = router
        .plan(
            "resource.update",
            &as_shipped(&harness, WorkspaceFreshness::Fresh),
        )
        .expect("a project write has a plan");
    assert_eq!(write_plan.strategy(), ExecutionStrategy::StaticAnalysis);
    assert_eq!(write_plan.required_planes(), [EffectPlane::File]);
    assert_eq!(write_plan.effects(), [EffectPlane::File]);
    assert_eq!(write_plan.outcome_plane(), OperationPlane::Project);

    let before = router.snapshot_generation(EffectPlane::File);
    let report = router
        .route(&harness.request(
            "resource.update",
            "idem-32-write",
            Some(TX_WRITE),
            WRITE_PATH,
            as_shipped(&harness, WorkspaceFreshness::Fresh),
        ))
        .await
        .expect("the file plane is shipped and unblocked");
    assert_eq!(report.completion(), RouteCompletion::Completed);
    assert_eq!(report.required_planes(), [EffectPlane::File]);
    assert_eq!(report.used_planes(), [EffectPlane::File]);
    assert_eq!(report.effects(), [EffectPlane::File]);
    assert_eq!(report.degradation(), Degradation::None);
    assert_eq!(report.recorded_status(), Some(RecordedStatus::Committed));
    assert_eq!(report.transaction_id(), Some(TX_WRITE));
    assert!(!report.replayed());
    assert!(report.failed_steps().is_empty());
    assert_eq!(
        harness.disk_bytes(WRITE_PATH).as_deref(),
        Some(b"todo-32".as_slice()),
        "a committed write must actually be on disk"
    );
    assert!(
        report.refreshed_snapshots().contains(&EffectPlane::File),
        "the file snapshot must be refreshed after a proven committed write"
    );
    assert!(
        router.snapshot_generation(EffectPlane::File) > before,
        "the snapshot generation must advance only after a proven commit"
    );

    // ---------------------------------------------------------------------
    // 9. The same identity from the same context replays instead of executing.
    // ---------------------------------------------------------------------
    let after_write = router.snapshot_generation(EffectPlane::File);
    let replay = router
        .route(&harness.request(
            "resource.update",
            "idem-32-write",
            Some(TX_REPLAY),
            WRITE_PATH,
            as_shipped(&harness, WorkspaceFreshness::Fresh),
        ))
        .await
        .expect("an exact same-context identity replays");
    assert!(replay.replayed());
    assert_eq!(replay.completion(), RouteCompletion::Completed);
    assert_eq!(replay.transaction_id(), Some(TX_WRITE));
    assert!(
        replay.effects().is_empty(),
        "a replay reports no new effect"
    );
    assert_eq!(
        router.snapshot_generation(EffectPlane::File),
        after_write,
        "a replay must not refresh a snapshot; nothing changed"
    );
    assert!(!harness.transaction_exists(TX_REPLAY));

    // ---------------------------------------------------------------------
    // 9b. A read travels the same gates, reports no effect, and a lagging static
    //     index is refreshed before the answer instead of being trusted.
    // ---------------------------------------------------------------------
    let read_report = router
        .route(&harness.request(
            "project.validate",
            "idem-32-read",
            None,
            WRITE_PATH,
            as_shipped(&harness, WorkspaceFreshness::StaleIndex),
        ))
        .await
        .expect("a static read needs no boundary at all");
    assert_eq!(read_report.completion(), RouteCompletion::Completed);
    assert!(read_report.required_planes().is_empty());
    assert!(read_report.used_planes().is_empty());
    assert!(
        read_report.effects().is_empty(),
        "a read must never be reported as an effect"
    );
    assert!(read_report.transaction_id().is_none());
    assert!(read_report.recorded_status().is_none());
    assert_eq!(
        read_report.degradation(),
        Degradation::ReducedFidelity {
            plane: EffectPlane::File,
            reason: DegradationReason::StaleStaticIndex,
        }
    );
    assert!(
        read_report
            .refreshed_snapshots()
            .contains(&EffectPlane::File),
        "a lagging index must be refreshed rather than answered from"
    );
    let fairness = harness.scheduler().fairness();
    assert!(
        fairness.admitted(QueuePriority::Normal) > 0
            && fairness.admitted(QueuePriority::Interactive) > 0,
        "production routes must admit at least two server-assigned priority classes; \
         normal={}, interactive={}",
        fairness.admitted(QueuePriority::Normal),
        fairness.admitted(QueuePriority::Interactive)
    );

    // ---------------------------------------------------------------------
    // 10. Declared degradation. Every representable degradation is a *declared*
    //     reduced fidelity on the same plane; there is deliberately no
    //     "fell back to a weaker plane" state to hide behind.
    // ---------------------------------------------------------------------
    let stale = router
        .plan(
            "scene.tree.get",
            &claiming_every_boundary(
                EnginePresence::EditorConnected,
                RuntimePresence::GameRunning,
                ProtocolFidelity::Full,
                WorkspaceFreshness::StaleIndex,
            ),
        )
        .expect("a static scene-graph read has a plan");
    assert_eq!(
        stale.degradation(),
        Degradation::ReducedFidelity {
            plane: EffectPlane::File,
            reason: DegradationReason::StaleStaticIndex,
        }
    );
    assert!(stale.requires_snapshot_refresh());

    let diverged = router
        .plan(
            "scene.tree.get",
            &claiming_every_boundary(
                EnginePresence::EditorConnected,
                RuntimePresence::GameRunning,
                ProtocolFidelity::Full,
                WorkspaceFreshness::UnsavedEditorBuffer,
            ),
        )
        .expect("a static scene-graph read has a plan");
    assert_eq!(
        diverged.degradation(),
        Degradation::ReducedFidelity {
            plane: EffectPlane::File,
            reason: DegradationReason::UnsavedEditorBufferDivergesFromDisk,
        },
        "a disk answer taken while an editor buffer diverges must say so"
    );

    let subset = router
        .plan_historical(
            "debug.session.get",
            &claiming_every_boundary(
                EnginePresence::EditorConnected,
                RuntimePresence::GameRunning,
                ProtocolFidelity::Degraded,
                WorkspaceFreshness::Fresh,
            ),
        )
        .expect("a debug read has a plan");
    assert_eq!(
        subset.degradation(),
        Degradation::ReducedFidelity {
            plane: EffectPlane::LspDap,
            reason: DegradationReason::NegotiatedProtocolSubset,
        }
    );

    // ---------------------------------------------------------------------
    // 11. Precedence rules: a live editor buffer outranks disk and index.
    // ---------------------------------------------------------------------
    assert_eq!(
        SnapshotPrecedence::authority_for(EffectPlane::File, WorkspaceFreshness::Fresh),
        SnapshotAuthority::Disk
    );
    assert_eq!(
        SnapshotPrecedence::authority_for(EffectPlane::File, WorkspaceFreshness::StaleIndex),
        SnapshotAuthority::Disk,
        "a stale index is behind disk, so disk still wins"
    );
    assert_eq!(
        SnapshotPrecedence::authority_for(
            EffectPlane::File,
            WorkspaceFreshness::UnsavedEditorBuffer
        ),
        SnapshotAuthority::EditorBuffer,
        "an unsaved editor buffer outranks the bytes on disk"
    );
    for plane in [EffectPlane::Editor, EffectPlane::Runtime] {
        assert_eq!(
            SnapshotPrecedence::authority_for(plane, WorkspaceFreshness::Fresh),
            SnapshotAuthority::EditorBuffer,
            "a live plane always answers from live state"
        );
    }

    // ---------------------------------------------------------------------
    // 12. Completion is derived, never assumed. A dispatched-but-unanswered or
    //     unresolved transaction can never be reported as completed.
    // ---------------------------------------------------------------------
    assert_eq!(
        RouteCompletion::of_recorded(RecordedStatus::NotStarted),
        RouteCompletion::Refused
    );
    assert_eq!(
        RouteCompletion::of_recorded(RecordedStatus::Dispatched),
        RouteCompletion::Uncertain
    );
    assert_eq!(
        RouteCompletion::of_recorded(RecordedStatus::Committed),
        RouteCompletion::Completed
    );
    assert_eq!(
        RouteCompletion::of_recorded(RecordedStatus::Compensated),
        RouteCompletion::Refused
    );
    assert_eq!(
        RouteCompletion::of_recorded(RecordedStatus::Unresolved),
        RouteCompletion::Uncertain
    );
    for status in RecordedStatus::ALL {
        assert_eq!(
            RouteCompletion::of_recorded(status) == RouteCompletion::Completed,
            status == RecordedStatus::Committed,
            "{} must not be reported as completed",
            status.as_str()
        );
    }

    // Step aggregation never collapses a mixed result into a success.
    assert_eq!(
        RouteCompletion::of_steps(&[StepOutcome::Succeeded, StepOutcome::Succeeded]),
        RouteCompletion::Completed
    );
    assert_eq!(
        RouteCompletion::of_steps(&[
            StepOutcome::Succeeded,
            StepOutcome::Failed("the second plane refused".to_owned()),
        ]),
        RouteCompletion::Partial,
        "a partial failure must be reported as partial, never aggregated into success"
    );
    assert_eq!(
        RouteCompletion::of_steps(&[StepOutcome::Failed("nothing ran".to_owned())]),
        RouteCompletion::Refused
    );
    assert_eq!(
        RouteCompletion::of_steps(&[
            StepOutcome::Succeeded,
            StepOutcome::Skipped("no editor buffer to consult"),
        ]),
        RouteCompletion::Completed
    );

    // ---------------------------------------------------------------------
    // 13. Health and remediation report only what is real.
    // ---------------------------------------------------------------------
    let health = router.health(1_700_000_000_000);
    assert!(health.mutations_blocked().is_none());
    assert_eq!(health.observed_at_unix_ms(), 1_700_000_000_000);
    assert!(
        health.usable_planes().contains(&EffectPlane::File),
        "the shipped file plane must be reported usable"
    );
    for plane in [
        EffectPlane::Process,
        EffectPlane::LspDap,
        EffectPlane::Editor,
        EffectPlane::Runtime,
    ] {
        assert!(
            !health.usable_planes().contains(&plane),
            "an unshipped plane must never be reported usable"
        );
        let entry = health
            .plane(plane)
            .expect("every plane keeps its health entry");
        assert!(!entry.usable());
        assert!(
            entry.remediation().is_some(),
            "an unusable plane must carry actionable remediation"
        );
    }
    assert_eq!(
        health.planes().len(),
        EffectPlane::ALL.len(),
        "health must cover every plane, including the ones this release cannot run"
    );
}

#[tokio::test]
async fn outcome_router_failure_paths() {
    let (_temp, _root, harness) = open_harness("failure");
    let router = harness.router();

    // ---------------------------------------------------------------------
    // 1. An unsaved editor buffer refuses the engine/text overwrite and returns
    //    an actionable conflict instead of any fallback.
    // ---------------------------------------------------------------------
    let unsaved = PlaneObservation::new(
        harness.router().plane_availability(),
        EnginePresence::EditorConnected,
        RuntimePresence::NoInstance,
        ProtocolFidelity::Full,
        WorkspaceFreshness::UnsavedEditorBuffer,
    );
    let refusal = router
        .route(&harness.request(
            "resource.update",
            "idem-32-conflict",
            Some(TX_CONFLICT),
            CONFLICT_PATH,
            unsaved,
        ))
        .await
        .expect_err("a diverging editor buffer must not be overwritten");
    let RouteRefusal::Conflict(conflict) = &refusal else {
        panic!("expected an actionable precedence conflict, got {refusal:?}");
    };
    assert_eq!(refusal.to_outcome().code(), "conflict");
    assert!(!refusal.to_outcome().retryable());
    assert_eq!(conflict.holder(), SnapshotAuthority::EditorBuffer);
    assert_eq!(conflict.path(), CONFLICT_PATH);
    assert_eq!(conflict.required_plane(), EffectPlane::File);
    let steps = conflict.remediation().steps().join(" | ");
    assert!(
        !conflict.remediation().steps().is_empty(),
        "a conflict must be actionable, not merely a refusal"
    );
    assert!(
        steps.contains(CONFLICT_PATH),
        "the remediation must name the resource in conflict: {steps}"
    );
    assert!(
        steps.contains("editor"),
        "the remediation must point at the editor plane that holds the buffer: {steps}"
    );
    assert!(
        steps.contains("scene.save"),
        "the remediation must name the editor-safe route: {steps}"
    );
    assert!(
        harness.disk_bytes(CONFLICT_PATH).is_none(),
        "neither the engine nor a raw-text write may land while the buffer diverges"
    );
    assert!(
        !harness.transaction_exists(TX_CONFLICT),
        "a refused route must not journal a transaction"
    );

    // ---------------------------------------------------------------------
    // 2. An editor-safe mutation never degrades to a raw-text write, and an
    //    optimistic caller observation cannot conjure the plane into existence:
    //    the router re-derives availability from the effect gateway.
    // ---------------------------------------------------------------------
    let optimistic = router
        .route_historical(&harness.request(
            "node.create",
            "idem-32-optimistic",
            Some(TX_OPTIMISTIC),
            CONFLICT_PATH,
            claiming_every_boundary(
                EnginePresence::EditorConnected,
                RuntimePresence::GameRunning,
                ProtocolFidelity::Full,
                WorkspaceFreshness::Fresh,
            ),
        ))
        .await
        .expect_err("an absent boundary cannot be claimed into existence");
    assert!(matches!(optimistic, RouteRefusal::Unavailable(_)));
    assert_eq!(optimistic.to_outcome().code(), "unavailable");
    assert!(optimistic.to_outcome().message().contains("editor"));
    assert!(
        harness.disk_bytes(CONFLICT_PATH).is_none(),
        "an editor mutation must never silently become a file write"
    );
    assert!(!harness.transaction_exists(TX_OPTIMISTIC));

    // ---------------------------------------------------------------------
    // 3. No Godot at all: the subprocess plane is planned and then refused.
    // ---------------------------------------------------------------------
    let no_godot = claiming_every_boundary(
        EnginePresence::Absent,
        RuntimePresence::NoInstance,
        ProtocolFidelity::Full,
        WorkspaceFreshness::Fresh,
    );
    let plan = router
        .plan("project.run", &no_godot)
        .expect("the plan exists even where it cannot run");
    assert_eq!(plan.strategy(), ExecutionStrategy::Subprocess);
    assert!(
        !no_godot.effective(EffectPlane::Process),
        "no Godot means no subprocess plane"
    );
    let refused = router
        .route(&harness.request(
            "project.run",
            "idem-32-no-godot",
            Some(TX_PROCESS),
            WRITE_PATH,
            no_godot,
        ))
        .await
        .expect_err("no engine, no run");
    assert_eq!(refused.to_outcome().code(), "unavailable");
    assert!(refused.to_outcome().message().contains("process"));

    // ---------------------------------------------------------------------
    // 4. Headless only: the editor plane is absent even though the engine is not.
    // ---------------------------------------------------------------------
    let headless = claiming_every_boundary(
        EnginePresence::HeadlessOnly,
        RuntimePresence::NoInstance,
        ProtocolFidelity::Full,
        WorkspaceFreshness::Fresh,
    );
    assert!(headless.effective(EffectPlane::Process));
    assert!(!headless.effective(EffectPlane::Editor));
    assert!(!headless.effective(EffectPlane::LspDap));
    assert!(!headless.effective(EffectPlane::Runtime));
    assert!(
        headless.effective(EffectPlane::File),
        "descriptor-relative project writes never needed Godot"
    );

    // ---------------------------------------------------------------------
    // 5. No running game: a capture is planned and refused.
    // ---------------------------------------------------------------------
    let editor_only = claiming_every_boundary(
        EnginePresence::EditorConnected,
        RuntimePresence::NoInstance,
        ProtocolFidelity::Full,
        WorkspaceFreshness::Fresh,
    );
    assert!(editor_only.effective(EffectPlane::Editor));
    assert!(!editor_only.effective(EffectPlane::Runtime));
    let refused = router
        .route_historical(&harness.request(
            "runtime.screenshot.capture",
            "idem-32-no-game",
            Some(TX_RUNTIME),
            WRITE_PATH,
            editor_only,
        ))
        .await
        .expect_err("no game, no capture");
    assert_eq!(refused.to_outcome().code(), "unavailable");
    assert!(refused.to_outcome().message().contains("runtime"));

    // ---------------------------------------------------------------------
    // 6. Stale disk is never returned as live editor state.
    // ---------------------------------------------------------------------
    let unsaved_claimed = claiming_every_boundary(
        EnginePresence::HeadlessOnly,
        RuntimePresence::NoInstance,
        ProtocolFidelity::Full,
        WorkspaceFreshness::UnsavedEditorBuffer,
    );
    let editor_read = router
        .plan_historical("editor.state.get", &unsaved_claimed)
        .expect("an editor read has a plan");
    assert_eq!(editor_read.required_planes(), [EffectPlane::Editor]);
    assert_eq!(
        editor_read.degradation(),
        Degradation::None,
        "an editor read is answered live or not at all; it is never a degraded disk answer"
    );
    let refused = router
        .route_historical(&harness.request(
            "editor.state.get",
            "idem-32-editor-read",
            None,
            CONFLICT_PATH,
            unsaved_claimed,
        ))
        .await
        .expect_err("a live editor read cannot be served from disk");
    assert_eq!(refused.to_outcome().code(), "unavailable");
    assert!(refused.to_outcome().message().contains("editor"));

    // ---------------------------------------------------------------------
    // 7. Quarantine blocks every mutation route, on every plane.
    // ---------------------------------------------------------------------
    harness
        .quarantine(TX_QUARANTINE, QUARANTINE_PATH)
        .expect("the harness can drive a transaction to quarantine");
    assert_eq!(
        harness
            .scheduler()
            .write_block()
            .expect("readable")
            .map(|block| block.to_outcome().code()),
        Some("conflict")
    );
    for (index, (tool, historical)) in MUTATING_TOOLS
        .iter()
        .map(|tool| (tool, false))
        .chain(HISTORICAL_MUTATING_SHAPES.iter().map(|tool| (tool, true)))
        .enumerate()
    {
        let request = harness.request(
            tool,
            &format!("idem-32-quarantine-{index}"),
            Some(TX_BLOCKED),
            BLOCKED_PATH,
            as_shipped(&harness, WorkspaceFreshness::Fresh),
        );
        let refusal = if historical {
            router.route_historical(&request).await
        } else {
            router.route(&request).await
        }
        .expect_err("a quarantined project refuses every mutation");
        assert!(
            matches!(refusal, RouteRefusal::WriteBlocked(_)),
            "{tool} must be refused by the write-block gate, got {refusal:?}"
        );
        assert_eq!(refusal.to_outcome().code(), "conflict");
        assert!(!refusal.to_outcome().retryable());
    }
    assert!(
        harness.disk_bytes(BLOCKED_PATH).is_none(),
        "no write may land while a transaction is quarantined"
    );

    const BRIDGE_QUARANTINE: &str = "32000000-0000-4000-8000-000000000099";
    let (_bridge_temp, _bridge_root, bridge_harness) = open_harness("bridge-quarantine");
    bridge_harness
        .quarantine_unresolved_bridge(BRIDGE_QUARANTINE)
        .expect("the production bridge path persists into the real transaction store");
    assert_eq!(
        bridge_harness
            .transaction_phase(BRIDGE_QUARANTINE)
            .as_deref(),
        Some("quarantined")
    );
    assert!(
        bridge_harness
            .scheduler()
            .write_block()
            .expect("real store remains readable")
            .is_some(),
        "the persisted bridge quarantine blocks subsequent mutations"
    );
    assert!(!harness.transaction_exists(TX_BLOCKED));
    let blocked_health = router.health(1_700_000_000_001);
    assert!(
        blocked_health.mutations_blocked().is_some(),
        "health must surface the write block"
    );

    // ---------------------------------------------------------------------
    // 8. The in-crate operator recovery path mints a real repair proof and
    //    resolves the block. There is no force and no clear.
    // ---------------------------------------------------------------------
    let repair = router
        .repair(TX_QUARANTINE, RepairIntent::Compensate)
        .expect("a proven pre-state compensates");
    assert_eq!(repair.transaction_id(), TX_QUARANTINE);
    assert_eq!(repair.operation(), "compensate");
    assert_eq!(repair.conclusion(), "proven_pre_state");
    assert!(repair.write_block_cleared());
    assert_eq!(harness.scheduler().write_block().expect("readable"), None);

    // A repair that resolves nothing is refused rather than reported as success.
    let unproven = router.repair(TX_QUARANTINE, RepairIntent::Compensate);
    assert!(
        unproven.is_err(),
        "a transaction that is no longer quarantined has nothing to repair"
    );

    // ---------------------------------------------------------------------
    // 9. Mutations resume only after the block is genuinely gone.
    // ---------------------------------------------------------------------
    let resumed = router
        .route(&harness.request(
            "resource.update",
            "idem-32-after-repair",
            Some(TX_AFTER_REPAIR),
            AFTER_REPAIR_PATH,
            as_shipped(&harness, WorkspaceFreshness::Fresh),
        ))
        .await
        .expect("the block is lifted by a proof, so the write proceeds");
    assert_eq!(resumed.completion(), RouteCompletion::Completed);
    assert_eq!(resumed.recorded_status(), Some(RecordedStatus::Committed));

    // ---------------------------------------------------------------------
    // 10. An unresolved launch survivor blocks mutations the same way, and an
    //     unreadable survivor state is a refusal rather than an assumed "clear".
    // ---------------------------------------------------------------------
    harness.block_survivor("an unresolved launch survivor blocks writes: pid 4242");
    let survivor_refusal = router
        .route(&harness.request(
            "resource.update",
            "idem-32-survivor",
            Some(TX_SURVIVOR),
            SURVIVOR_PATH,
            as_shipped(&harness, WorkspaceFreshness::Fresh),
        ))
        .await
        .expect_err("an unresolved survivor blocks every mutation");
    assert!(matches!(survivor_refusal, RouteRefusal::WriteBlocked(_)));
    assert_eq!(survivor_refusal.to_outcome().code(), "conflict");
    assert!(harness.disk_bytes(SURVIVOR_PATH).is_none());

    harness.make_survivor_unreadable();
    let unreadable = router
        .route(&harness.request(
            "resource.update",
            "idem-32-unreadable",
            Some(TX_SURVIVOR),
            SURVIVOR_PATH,
            as_shipped(&harness, WorkspaceFreshness::Fresh),
        ))
        .await
        .expect_err("an unreadable block state is a refusal, never an assumed clear");
    assert!(matches!(unreadable, RouteRefusal::WriteBlocked(_)));
    harness.clear_survivor();

    // ---------------------------------------------------------------------
    // 11. A snapshot is never refreshed by a route that did not commit.
    // ---------------------------------------------------------------------
    let before = router.snapshot_generation(EffectPlane::File);
    let _ = router
        .route_historical(&harness.request(
            "node.create",
            "idem-32-no-refresh",
            Some(TX_EDITOR),
            CONFLICT_PATH,
            as_shipped(&harness, WorkspaceFreshness::Fresh),
        ))
        .await
        .expect_err("the editor plane is not shipped");
    assert_eq!(
        router.snapshot_generation(EffectPlane::File),
        before,
        "a refused route must not advance any snapshot"
    );

    // ---------------------------------------------------------------------
    // 12. A tool with no canonical matrix row is denied by default.
    // ---------------------------------------------------------------------
    let unknown = router
        .plan("not.a.tool", &everything_present())
        .expect_err("an unknown tool has no plan");
    assert_eq!(unknown.to_outcome().code(), "not_found");
    let unknown = router
        .route(&harness.request(
            "not.a.tool",
            "idem-32-unknown",
            Some(TX_UNKNOWN_TOOL),
            WRITE_PATH,
            everything_present(),
        ))
        .await
        .expect_err("an unknown tool is never routed");
    assert_eq!(unknown.to_outcome().code(), "not_found");
    assert!(!harness.transaction_exists(TX_UNKNOWN_TOOL));
}
