use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::contracts::{Capability, ProjectIdentity};
use godot_mcp_domain::lease::{Lease, LeaseManager};
use godot_mcp_domain::safe_root::SafeRootError;
use godot_mcp_engine::supervision::boundary::TrustLevel;
use godot_mcp_engine::supervision::journal::{
    OperationFamily as EngineOperationFamily, ProjectIdentityClaim, TransactionEnvelope,
    TransportMode as EngineTransportMode,
};
use godot_mcp_engine::supervision::protocol::{
    EnvironmentAllowlist, LaunchRequest, OutputCaps, TerminationKind,
};
use godot_mcp_engine::validation::outcome::Observed;

use crate::effect::boundary::{
    BoundaryUnavailable, EditorEffectBoundary, EditorMutation, EffectTicket, FileEffectBoundary,
    FileWrite, LspDapEffectBoundary, LspDapExchange, ProcessEffectBoundary, ProcessEffectOutcome,
    ProcessLaunchRequest, ProcessOutputMetadata, RuntimeEffectBoundary, RuntimeOperation,
    SafeRootFileBoundary, UnavailableBoundaries,
};
use crate::effect::permit::{
    EffectPermit, LocalLeaseEvidence, OperatorRecoveryAuthority, PermitAuthority, PermitRequest,
    RepairOperation, RepairPermit,
};
use crate::effect::{
    DerivedDisposition, DerivedTerminal, EffectBoundaries, EffectError, EffectGateway,
    FileEffectIntent, PlannedWrite, ProcessEffectIntent, WritePrecondition,
};
use crate::journal::{
    BoundaryMembership, Conclusion, EffectBoundaryKind, FileOperation, HeldProject,
    OperationFamily, Phase, ReleaseStatus, SafeRootProofSource, TerminalOutcome, TransactionRecord,
    TransactionStore, TransportMode, TypedProof, encode_object_identity,
};

const EPOCH: u64 = 17;

struct GatewayFixture {
    root: PathBuf,
    store: Arc<TransactionStore>,
    held: HeldProject,
    gateway: EffectGateway,
    _lease: Lease,
    _temp: tempfile::TempDir,
}

impl GatewayFixture {
    fn new(build_boundaries: impl FnOnce(&Lease) -> EffectBoundaries) -> Self {
        let temp = tempfile::Builder::new()
            .prefix("godot-mcp-gateway-")
            .tempdir()
            .expect("temporary project root");
        let root = temp.path().join("project");
        std::fs::create_dir_all(root.join("scenes")).expect("project directories");
        std::fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/minimal/project.godot"),
            root.join("project.godot"),
        )
        .expect("copy fixture project marker");

        let state_dir = crate::stdio::state_dir_for(&root);
        let lease = LeaseManager::new(&state_dir)
            .acquire(&root)
            .expect("hold fixture project");
        let key = lease.lease_key();
        let project =
            ProjectIdentity::from_lease_key(key, crate::stdio::INITIAL_PROJECT_GENERATION);
        let held = HeldProject {
            object_identity: project.object_id().to_string(),
            root_object: encode_object_identity(key.root),
            file_object: encode_object_identity(key.project_file),
            generation: crate::stdio::INITIAL_PROJECT_GENERATION,
        };
        let store = Arc::new(
            TransactionStore::open(&state_dir.join("journal")).expect("open transaction store"),
        );
        let gateway = EffectGateway::new(
            Arc::clone(&store),
            held.clone(),
            EPOCH,
            Box::new(SafeRootProofSource::new(lease.safe_root())),
            build_boundaries(&lease),
        );
        Self {
            root,
            store,
            held,
            gateway,
            _lease: lease,
            _temp: temp,
        }
    }

    fn with_defaults() -> Self {
        Self::new(|lease| EffectBoundaries {
            file: Box::new(SafeRootFileBoundary::new(lease.safe_root())),
            process: Box::new(UnavailableBoundaries),
            lsp_dap: Box::new(UnavailableBoundaries),
            editor: Box::new(UnavailableBoundaries),
            runtime: Box::new(UnavailableBoundaries),
        })
    }

    fn file_intent(
        &self,
        transaction_id: &str,
        path: &str,
        operation: FileOperation,
        bytes: Option<&[u8]>,
    ) -> FileEffectIntent {
        FileEffectIntent {
            transaction_id: transaction_id.to_owned(),
            tool_name: "resource.update".to_owned(),
            operation_family: OperationFamily::ProjectMutation,
            idempotency_id: format!("idem-{transaction_id}"),
            transport_mode: TransportMode::Stdio,
            principal: Some("principal_0123456789abcdef".to_owned()),
            session: None,
            arguments: serde_json::json!({ "path": path }),
            writes: vec![PlannedWrite {
                path: path.to_owned(),
                operation,
                bytes: bytes.map(<[u8]>::to_vec),
                precondition: WritePrecondition::Observed,
            }],
            required_process_transaction: None,
            opened_at_unix_ms: 1_700_000_000_000,
            expected_revision: None,
        }
    }

    fn process_intent(&self, transaction_id: &str) -> ProcessEffectIntent {
        let transaction_suffix = transaction_id
            .strip_prefix("00000000-")
            .expect("fixture process transaction id uses the reserved test UUID prefix");
        ProcessEffectIntent {
            transaction_id: transaction_id.to_owned(),
            tool_name: "project.run".to_owned(),
            operation_family: OperationFamily::RuntimeExecution,
            idempotency_id: format!("idem-{transaction_id}"),
            transport_mode: TransportMode::Stdio,
            principal: None,
            session: Some("stdio".to_owned()),
            arguments: serde_json::json!({ "mode": "headless" }),
            required_process_transaction: format!("10000000-{transaction_suffix}"),
            opened_at_unix_ms: 1_700_000_000_000,
        }
    }

    fn permit(
        &self,
        transaction_id: &str,
        capability: Capability,
        operation_family: OperationFamily,
        boundary: EffectBoundaryKind,
    ) -> EffectPermit {
        PermitAuthority::new(Arc::clone(&self.store), EPOCH)
            .issue(PermitRequest {
                transaction_id: transaction_id.to_owned(),
                capability,
                operation_family,
                boundary,
                project_object_identity: self.held.object_identity.clone(),
                project_generation: self.held.generation,
            })
            .expect("fixture permit")
    }

    fn repair_permit(&self, transaction_id: &str, operation: RepairOperation) -> RepairPermit {
        self.recovery_authority()
            .issue(transaction_id, operation)
            .expect("fixture repair permit")
    }

    fn recovery_authority(&self) -> OperatorRecoveryAuthority {
        OperatorRecoveryAuthority::new(
            Arc::clone(&self.store),
            LocalLeaseEvidence::new(self.held.object_identity.clone(), self.held.generation),
        )
    }

    fn open_staged_file(&self, intent: &FileEffectIntent) -> TransactionRecord {
        self.gateway
            .open_file_transaction(intent)
            .expect("open file transaction");
        self.gateway
            .stage_file_transaction(intent)
            .expect("stage file transaction")
    }

    fn quarantine_process(&self, transaction_id: &str) -> TransactionRecord {
        self.gateway
            .open_process_transaction(&self.process_intent(transaction_id))
            .expect("open process transaction");
        self.store
            .advance(
                transaction_id,
                Phase::EffectStartedFsynced,
                None,
                None,
                None,
            )
            .expect("mark process effect started");
        self.store
            .advance(
                transaction_id,
                Phase::Quarantined,
                None,
                Some("fixture quarantine".to_owned()),
                Some(TerminalOutcome::failed(ErrorKind::Uncertain)),
            )
            .expect("quarantine process transaction")
    }

    fn quarantine_file(
        &self,
        transaction_id: &str,
        path: &str,
        bytes: &[u8],
        materialize_post_state: bool,
        required_process_transaction: Option<&str>,
    ) -> TransactionRecord {
        let mut intent = self.file_intent(transaction_id, path, FileOperation::Create, Some(bytes));
        intent.required_process_transaction = required_process_transaction.map(str::to_owned);
        self.open_staged_file(&intent);
        if materialize_post_state {
            std::fs::write(self.root.join(path), bytes).expect("materialize intended post-state");
        }
        self.store
            .advance(
                transaction_id,
                Phase::EffectStartedFsynced,
                None,
                None,
                None,
            )
            .expect("mark file effect started");
        self.store
            .advance(
                transaction_id,
                Phase::Quarantined,
                None,
                Some("fixture quarantine".to_owned()),
                Some(TerminalOutcome::failed(ErrorKind::Uncertain)),
            )
            .expect("quarantine file transaction")
    }

    fn dummy_process_request(&self, transaction_id: &str) -> ProcessLaunchRequest {
        ProcessLaunchRequest {
            launch: LaunchRequest {
                envelope: TransactionEnvelope {
                    transaction_id: transaction_id.to_owned(),
                    project: ProjectIdentityClaim {
                        object_identity: self.held.object_identity.clone(),
                        root_object: self.held.root_object.clone(),
                        file_object: self.held.file_object.clone(),
                        generation: self.held.generation,
                    },
                    transport_mode: EngineTransportMode::Stdio,
                    principal: None,
                    session: Some("stdio".to_owned()),
                    operation_family: EngineOperationFamily::RuntimeExecution,
                    tool_name: "project.run".to_owned(),
                    idempotency_id: format!("request-{transaction_id}"),
                    canonical_request_hash: "a".repeat(64),
                    canonical_request_bytes: 2,
                    opened_at_unix_ms: 1_700_000_000_000,
                },
                trust: TrustLevel::Trusted,
                program: self.root.join("godot"),
                arguments: vec!["--headless".to_owned()],
                binary_sha256: "b".repeat(64),
                supervisor_binary: self.root.join("godot-mcp-supervisor"),
                project_root_display: self.root.clone(),
                environment: EnvironmentAllowlist::default(),
                caps: OutputCaps {
                    stdout_bytes: 1024,
                    stderr_bytes: 1024,
                },
                deadline: Duration::from_secs(1),
                work_dir: self.root.join("work"),
                evidence_path: None,
            },
        }
    }
}

fn clean_process_proof(label: &str) -> TypedProof {
    TypedProof::ProcessLaunch {
        boundary_identity: format!("boundary:{label}"),
        pid: Some(4242),
        start_time: Some("12345".to_owned()),
        boundary_membership: BoundaryMembership::Confirmed,
        release_status: ReleaseStatus::ExitedClean,
        exit_code: Some(0),
        signal: None,
        boundary_empty_after_exit: Some(true),
    }
}

fn process_outcome(label: &str, termination: TerminationKind) -> ProcessEffectOutcome {
    let timed_out = matches!(termination, TerminationKind::TimedOut);
    let (release_status, exit_code) = match &termination {
        TerminationKind::ExitedClean => (ReleaseStatus::ExitedClean, Some(0)),
        TerminationKind::NonzeroExit { code } => {
            (ReleaseStatus::NonzeroExit, Some(i64::from(*code)))
        }
        TerminationKind::Signaled { .. } | TerminationKind::TimedOut => {
            (ReleaseStatus::Signaled, None)
        }
    };
    ProcessEffectOutcome {
        proof: TypedProof::ProcessLaunch {
            boundary_identity: format!("boundary:{label}"),
            pid: Some(4242),
            start_time: Some("12345".to_owned()),
            boundary_membership: BoundaryMembership::Confirmed,
            release_status,
            exit_code,
            signal: None,
            boundary_empty_after_exit: Some(true),
        },
        termination,
        observed: Observed {
            exit_code: exit_code.and_then(|code| i32::try_from(code).ok()),
            signal: None,
            timed_out,
        },
        diagnostics: Vec::new(),
        output: ProcessOutputMetadata::default(),

        excerpt: crate::effect::boundary::ProcessOutputExcerpt::default(),

        boundary_kind: "unix_locator",

        boundary_strength: "weak",
    }
}

fn gateway_with_process_outcome(outcome: ProcessEffectOutcome) -> GatewayFixture {
    GatewayFixture::new(move |lease| EffectBoundaries {
        file: Box::new(SafeRootFileBoundary::new(lease.safe_root())),
        process: Box::new(RecordingProcessBoundary {
            calls: Arc::new(Mutex::new(Vec::new())),
            result: Ok(outcome),
        }),
        lsp_dap: Box::new(UnavailableBoundaries),
        editor: Box::new(UnavailableBoundaries),
        runtime: Box::new(UnavailableBoundaries),
    })
}

fn execute_fixture_process(
    fixture: &GatewayFixture,
    transaction_id: &str,
) -> Result<ProcessEffectOutcome, EffectError> {
    fixture
        .gateway
        .open_process_transaction(&fixture.process_intent(transaction_id))
        .expect("open process transaction");
    let permit = fixture.permit(
        transaction_id,
        Capability::RuntimeExecute,
        OperationFamily::RuntimeExecution,
        EffectBoundaryKind::Process,
    );
    fixture
        .gateway
        .execute_process_effect(&permit, &fixture.dummy_process_request(transaction_id))
}

#[derive(Clone)]
struct RecordingProcessBoundary {
    calls: Arc<Mutex<Vec<ProcessLaunchRequest>>>,
    result: Result<ProcessEffectOutcome, BoundaryUnavailable>,
}

impl ProcessEffectBoundary for RecordingProcessBoundary {
    fn available(&self) -> bool {
        true
    }

    fn launch(
        &self,
        _ticket: &EffectTicket,
        request: &ProcessLaunchRequest,
    ) -> Result<ProcessEffectOutcome, BoundaryUnavailable> {
        self.calls
            .lock()
            .expect("process call lock")
            .push(request.clone());
        self.result.clone()
    }
}

#[derive(Clone, Default)]
struct BoundaryProbe {
    calls: Arc<Mutex<Vec<String>>>,
}

impl ProcessEffectBoundary for BoundaryProbe {
    fn available(&self) -> bool {
        true
    }

    fn launch(
        &self,
        ticket: &EffectTicket,
        _request: &ProcessLaunchRequest,
    ) -> Result<ProcessEffectOutcome, BoundaryUnavailable> {
        self.calls
            .lock()
            .expect("boundary probe lock")
            .push(format!("process:{}", ticket.transaction_id()));
        Ok(ProcessEffectOutcome {
            proof: clean_process_proof(ticket.transaction_id()),
            termination: TerminationKind::ExitedClean,
            observed: Observed {
                exit_code: Some(0),
                signal: None,
                timed_out: false,
            },
            diagnostics: Vec::new(),
            output: ProcessOutputMetadata::default(),

            excerpt: crate::effect::boundary::ProcessOutputExcerpt::default(),

            boundary_kind: "unix_locator",

            boundary_strength: "weak",
        })
    }
}

impl LspDapEffectBoundary for BoundaryProbe {
    fn available(&self) -> bool {
        true
    }

    fn exchange(
        &self,
        ticket: &EffectTicket,
        exchange: &LspDapExchange,
    ) -> Result<String, BoundaryUnavailable> {
        self.calls
            .lock()
            .expect("boundary probe lock")
            .push(format!(
                "lsp:{}:{}:{}",
                ticket.transaction_id(),
                exchange.method,
                exchange.payload
            ));
        Ok("response".to_owned())
    }
}

impl EditorEffectBoundary for BoundaryProbe {
    fn available(&self) -> bool {
        true
    }

    fn mutate(
        &self,
        ticket: &EffectTicket,
        mutation: &EditorMutation,
    ) -> Result<TypedProof, BoundaryUnavailable> {
        self.calls
            .lock()
            .expect("boundary probe lock")
            .push(format!(
                "editor:{}:{}:{}",
                ticket.transaction_id(),
                mutation.action,
                mutation.payload
            ));
        Ok(TypedProof::EditorRuntime {
            bridge_reachable: true,
            receipt_transaction_id: Some(ticket.transaction_id().to_owned()),
            receipt_action_id: Some("editor-action".to_owned()),
            live_state_probe: Some("clean".to_owned()),
        })
    }
}

impl RuntimeEffectBoundary for BoundaryProbe {
    fn available(&self) -> bool {
        true
    }

    fn invoke(
        &self,
        ticket: &EffectTicket,
        operation: &RuntimeOperation,
    ) -> Result<TypedProof, BoundaryUnavailable> {
        self.calls
            .lock()
            .expect("boundary probe lock")
            .push(format!(
                "runtime:{}:{}:{}",
                ticket.transaction_id(),
                operation.operation,
                operation.payload
            ));
        Ok(TypedProof::EditorRuntime {
            bridge_reachable: true,
            receipt_transaction_id: Some(ticket.transaction_id().to_owned()),
            receipt_action_id: Some("runtime-action".to_owned()),
            live_state_probe: Some("running".to_owned()),
        })
    }
}

struct CountingFileBoundary {
    calls: Arc<AtomicUsize>,
}

impl FileEffectBoundary for CountingFileBoundary {
    fn available(&self) -> bool {
        true
    }

    fn publish(&self, _ticket: &EffectTicket, _write: &FileWrite) -> Result<(), SafeRootError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn delete(
        &self,
        _ticket: &EffectTicket,
        _path: &str,
        _expected: &[u8],
    ) -> Result<(), SafeRootError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[test]
fn diverged_preimage_refuses_before_the_file_boundary() {
    // Contract: an update whose recorded preimage changed must never reach the
    // file boundary, overwrite the concurrent bytes, or escape quarantine.
    let calls = Arc::new(AtomicUsize::new(0));
    let fixture = GatewayFixture::new({
        let calls = Arc::clone(&calls);
        move |_lease| EffectBoundaries {
            file: Box::new(CountingFileBoundary { calls }),
            process: Box::new(UnavailableBoundaries),
            lsp_dap: Box::new(UnavailableBoundaries),
            editor: Box::new(UnavailableBoundaries),
            runtime: Box::new(UnavailableBoundaries),
        }
    });
    let transaction_id = "00000000-0000-4000-8000-00000000a001";
    let path = "scenes/diverged.tscn";
    std::fs::write(fixture.root.join(path), b"recorded preimage\n").expect("seed preimage");
    let intent = fixture.file_intent(
        transaction_id,
        path,
        FileOperation::Update,
        Some(b"intended post-state\n"),
    );
    fixture.open_staged_file(&intent);
    std::fs::write(fixture.root.join(path), b"concurrent edit\n").expect("diverge preimage");
    let permit = fixture.permit(
        transaction_id,
        Capability::ProjectWrite,
        OperationFamily::ProjectMutation,
        EffectBoundaryKind::File,
    );

    let outcome = fixture
        .gateway
        .execute_file_effect(&permit)
        .expect("divergence settles to quarantine");

    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(outcome.phase, Phase::Quarantined);
    assert_eq!(outcome.conclusion, Conclusion::Quarantine);
    assert_eq!(
        std::fs::read(fixture.root.join(path)).expect("concurrent bytes survive"),
        b"concurrent edit\n"
    );
    let record = fixture.store.load(transaction_id).expect("durable record");
    assert_eq!(record.phase, Phase::Quarantined);
    assert_eq!(record.proof, None);
}

fn project_write_permit(fixture: &GatewayFixture, transaction_id: &str) -> EffectPermit {
    fixture.permit(
        transaction_id,
        Capability::ProjectWrite,
        OperationFamily::ProjectMutation,
        EffectBoundaryKind::File,
    )
}

#[test]
fn a_delete_removes_the_file_only_through_its_recorded_preimage() {
    // Contract: a journaled delete is executed (not merely asserted) and is
    // proven by the post-state tombstone.
    let fixture = GatewayFixture::with_defaults();
    let transaction_id = "00000000-0000-4000-8000-00000000a101";
    let path = "scenes/doomed.tscn";
    std::fs::write(fixture.root.join(path), b"doomed\n").expect("seed");
    let mut intent = fixture.file_intent(transaction_id, path, FileOperation::Delete, None);
    intent.writes[0].precondition =
        WritePrecondition::Sha256(crate::journal::hex_sha256(b"doomed\n"));
    fixture.open_staged_file(&intent);

    let outcome = fixture
        .gateway
        .execute_file_effect_removing(
            &project_write_permit(&fixture, transaction_id),
            &[path.to_owned()],
        )
        .expect("the delete settles");

    assert_eq!(
        outcome.conclusion,
        Conclusion::ProvenPostState,
        "{outcome:?}"
    );
    assert_eq!(outcome.phase, Phase::CommittedFsynced);
    assert!(!fixture.root.join(path).exists(), "the file is gone");
}

#[test]
fn a_move_publishes_the_destination_and_removes_the_source_in_one_transaction() {
    let fixture = GatewayFixture::with_defaults();
    let transaction_id = "00000000-0000-4000-8000-00000000a102";
    let source = "scenes/old.tscn";
    let destination = "levels/one/new.tscn";
    std::fs::write(fixture.root.join(source), b"[gd_scene format=3]\n").expect("seed");
    let mut intent = fixture.file_intent(
        transaction_id,
        destination,
        FileOperation::Create,
        Some(b"[gd_scene format=3]\n"),
    );
    intent.writes[0].precondition = WritePrecondition::Absent;
    intent.writes.push(PlannedWrite {
        path: source.to_owned(),
        operation: FileOperation::Delete,
        bytes: None,
        precondition: WritePrecondition::Sha256(crate::journal::hex_sha256(
            b"[gd_scene format=3]\n",
        )),
    });
    fixture.open_staged_file(&intent);

    let outcome = fixture
        .gateway
        .execute_file_effect_removing(
            &project_write_permit(&fixture, transaction_id),
            &[source.to_owned()],
        )
        .expect("the move settles");

    assert_eq!(
        outcome.conclusion,
        Conclusion::ProvenPostState,
        "{outcome:?}"
    );
    assert_eq!(
        std::fs::read(fixture.root.join(destination)).expect("destination"),
        b"[gd_scene format=3]\n",
        "missing destination directories are created"
    );
    assert!(!fixture.root.join(source).exists(), "the source is removed");
}

#[test]
fn a_delete_without_a_proven_removal_is_never_executed() {
    // Contract: only a removal the request proved is executed; a journaled
    // delete without that proof leaves the file and settles to its pre-state.
    let fixture = GatewayFixture::with_defaults();
    let transaction_id = "00000000-0000-4000-8000-00000000a105";
    let path = "scenes/kept.tscn";
    std::fs::write(fixture.root.join(path), b"kept\n").expect("seed");
    let intent = fixture.file_intent(transaction_id, path, FileOperation::Delete, None);
    fixture.open_staged_file(&intent);

    let outcome = fixture
        .gateway
        .execute_file_effect(&project_write_permit(&fixture, transaction_id))
        .expect("the delete settles");

    assert_eq!(
        outcome.conclusion,
        Conclusion::ProvenPreState,
        "{outcome:?}"
    );
    assert_eq!(
        std::fs::read(fixture.root.join(path)).expect("kept"),
        b"kept\n"
    );
}

#[test]
fn a_failed_precondition_opens_no_transaction() {
    let fixture = GatewayFixture::with_defaults();
    let path = "scenes/present.tscn";
    std::fs::write(fixture.root.join(path), b"present\n").expect("seed");

    let mut create = fixture.file_intent(
        "00000000-0000-4000-8000-00000000a103",
        path,
        FileOperation::Create,
        Some(b"clobber\n"),
    );
    create.writes[0].precondition = WritePrecondition::Absent;
    let error = fixture
        .gateway
        .open_file_transaction(&create)
        .expect_err("a create over an existing file is refused");
    assert_eq!(error.to_outcome().kind(), ErrorKind::Conflict, "{error}");

    let mut update = fixture.file_intent(
        "00000000-0000-4000-8000-00000000a104",
        path,
        FileOperation::Update,
        Some(b"new\n"),
    );
    update.writes[0].precondition =
        WritePrecondition::Sha256(crate::journal::hex_sha256(b"not what is there\n"));
    let error = fixture
        .gateway
        .open_file_transaction(&update)
        .expect_err("an update against a stale hash is refused");
    assert_eq!(error.to_outcome().kind(), ErrorKind::Conflict, "{error}");

    for transaction_id in [
        "00000000-0000-4000-8000-00000000a103",
        "00000000-0000-4000-8000-00000000a104",
    ] {
        assert!(
            fixture.store.load(transaction_id).is_err(),
            "a refused precondition journals nothing"
        );
    }
    assert_eq!(
        std::fs::read(fixture.root.join(path)).expect("unchanged"),
        b"present\n"
    );
}

#[test]
fn staging_requires_every_recorded_payload() {
    // Contract: staging must reject a caller that omits bytes recorded in the
    // intent and must leave the durable transaction at intent_fsynced.
    let fixture = GatewayFixture::with_defaults();
    let transaction_id = "00000000-0000-4000-8000-00000000a002";
    let intent = fixture.file_intent(
        transaction_id,
        "scenes/missing.tscn",
        FileOperation::Create,
        Some(b"payload\n"),
    );
    fixture
        .gateway
        .open_file_transaction(&intent)
        .expect("open intent");
    let mut missing = intent.clone();
    missing.writes[0].bytes = None;

    let error = fixture
        .gateway
        .stage_file_transaction(&missing)
        .expect_err("missing staged bytes are refused");

    match error {
        EffectError::Boundary(message) => assert_eq!(
            message,
            "planned bytes for staged path scenes/missing.tscn are missing"
        ),
        other => panic!("expected boundary refusal, got {other}"),
    }
    let record = fixture.store.load(transaction_id).expect("durable record");
    assert_eq!(record.phase, Phase::IntentFsynced);
    assert_eq!(record.staged_write_set.len(), 1);
}

#[test]
fn process_effect_records_success_failure_and_wrong_phase() {
    // Contract: a supervised process request is forwarded unchanged, then its
    // returned proof commits while a boundary refusal compensates with its reason.
    let success_calls = Arc::new(Mutex::new(Vec::new()));
    let success_proof = clean_process_proof("success");
    let fixture = GatewayFixture::new({
        let calls = Arc::clone(&success_calls);
        let proof = success_proof.clone();
        move |lease| EffectBoundaries {
            file: Box::new(SafeRootFileBoundary::new(lease.safe_root())),
            process: Box::new(RecordingProcessBoundary {
                calls,
                result: Ok(ProcessEffectOutcome {
                    proof,
                    termination: TerminationKind::ExitedClean,
                    observed: Observed {
                        exit_code: Some(0),
                        signal: None,
                        timed_out: false,
                    },
                    diagnostics: Vec::new(),
                    output: ProcessOutputMetadata::default(),

                    excerpt: crate::effect::boundary::ProcessOutputExcerpt::default(),

                    boundary_kind: "unix_locator",

                    boundary_strength: "weak",
                }),
            }),
            lsp_dap: Box::new(UnavailableBoundaries),
            editor: Box::new(UnavailableBoundaries),
            runtime: Box::new(UnavailableBoundaries),
        }
    });
    let success_id = "00000000-0000-4000-8000-00000000a003";
    let opened = fixture
        .gateway
        .open_process_transaction(&fixture.process_intent(success_id))
        .expect("open and stage process intent");
    assert_eq!(opened.phase, Phase::StagedFsynced);
    assert_eq!(opened.effect_boundary, EffectBoundaryKind::Process);
    let permit = fixture.permit(
        success_id,
        Capability::RuntimeExecute,
        OperationFamily::RuntimeExecution,
        EffectBoundaryKind::Process,
    );
    let request = fixture.dummy_process_request(success_id);
    let returned = fixture
        .gateway
        .execute_process_effect(&permit, &request)
        .expect("process proof commits");
    assert_eq!(returned.proof, success_proof);
    assert_eq!(
        success_calls.lock().expect("process calls").as_slice(),
        &[request]
    );
    let committed = fixture.store.load(success_id).expect("committed record");
    assert_eq!(committed.phase, Phase::CommittedFsynced);
    assert_eq!(committed.proof, Some(success_proof));
    assert_eq!(
        committed.terminal_reason.as_deref(),
        Some("supervised process launch completed with a durable proof")
    );

    let failure_calls = Arc::new(Mutex::new(Vec::new()));
    let failed = GatewayFixture::new({
        let calls = Arc::clone(&failure_calls);
        move |lease| EffectBoundaries {
            file: Box::new(SafeRootFileBoundary::new(lease.safe_root())),
            process: Box::new(RecordingProcessBoundary {
                calls,
                result: Err(BoundaryUnavailable {
                    boundary: "test_process",
                }),
            }),
            lsp_dap: Box::new(UnavailableBoundaries),
            editor: Box::new(UnavailableBoundaries),
            runtime: Box::new(UnavailableBoundaries),
        }
    });
    let failure_id = "00000000-0000-4000-8000-00000000a004";
    failed
        .gateway
        .open_process_transaction(&failed.process_intent(failure_id))
        .expect("open failed launch transaction");
    let permit = failed.permit(
        failure_id,
        Capability::RuntimeExecute,
        OperationFamily::RuntimeExecution,
        EffectBoundaryKind::Process,
    );
    let request = failed.dummy_process_request(failure_id);
    let error = failed
        .gateway
        .execute_process_effect(&permit, &request)
        .expect_err("boundary refusal is returned");
    match error {
        EffectError::Boundary(message) => assert_eq!(
            message,
            "the test_process effect boundary is not implemented in this build"
        ),
        other => panic!("expected boundary refusal, got {other}"),
    }
    assert_eq!(
        failure_calls.lock().expect("process calls").as_slice(),
        &[request]
    );
    let compensated = failed.store.load(failure_id).expect("compensated record");
    assert_eq!(compensated.phase, Phase::Compensated);
    assert_eq!(compensated.proof, None);
    assert_eq!(
        compensated.terminal_reason.as_deref(),
        Some("the test_process effect boundary is not implemented in this build")
    );

    let wrong_phase = GatewayFixture::with_defaults();
    let wrong_id = "00000000-0000-4000-8000-00000000a005";
    wrong_phase
        .gateway
        .open_process_transaction(&wrong_phase.process_intent(wrong_id))
        .expect("open process transaction");
    wrong_phase
        .store
        .advance(wrong_id, Phase::EffectStartedFsynced, None, None, None)
        .expect("advance beyond staged");
    let permit = wrong_phase.permit(
        wrong_id,
        Capability::RuntimeExecute,
        OperationFamily::RuntimeExecution,
        EffectBoundaryKind::Process,
    );
    let error = wrong_phase
        .gateway
        .execute_process_effect(&permit, &wrong_phase.dummy_process_request(wrong_id))
        .expect_err("only staged process effects can execute");
    assert!(matches!(
        error,
        EffectError::WrongPhase {
            transaction_id,
            phase: Phase::EffectStartedFsynced,
        } if transaction_id == wrong_id
    ));
}

#[test]
fn timed_out_process_launch_surfaces_as_timeout_not_success_or_failure() {
    let transaction_id = "00000000-0000-4000-8000-00000000a060";
    let fixture =
        gateway_with_process_outcome(process_outcome(transaction_id, TerminationKind::TimedOut));

    let error = execute_fixture_process(&fixture, transaction_id)
        .expect_err("a supervisor deadline must surface as timeout, never success");
    let outcome = error.to_outcome();

    assert_eq!(
        outcome.code(),
        "timeout",
        "a supervisor deadline must retain the timeout classification"
    );
    assert!(
        !outcome.retryable(),
        "an interrupted effect is unproven and must not be retried automatically"
    );
    assert_eq!(
        outcome.message(),
        "the operation exceeded its execution deadline; its effect is not yet proven"
    );
    let record = fixture.store.load(transaction_id).expect("timeout record");
    assert_eq!(record.phase, Phase::Compensated);
    assert!(matches!(
        record.proof,
        Some(TypedProof::ProcessLaunch {
            release_status: ReleaseStatus::Signaled,
            exit_code: None,
            boundary_empty_after_exit: Some(true),
            ..
        })
    ));
}

#[test]
fn clean_process_launch_still_commits_successfully() {
    let transaction_id = "00000000-0000-4000-8000-00000000a061";
    let expected = process_outcome(transaction_id, TerminationKind::ExitedClean);
    let expected_proof = expected.proof.clone();
    let fixture = gateway_with_process_outcome(expected);

    let outcome = execute_fixture_process(&fixture, transaction_id)
        .expect("a clean process exit must remain successful");

    assert_eq!(outcome.proof, expected_proof);
    assert_eq!(
        fixture
            .store
            .load(transaction_id)
            .expect("clean exit record")
            .phase,
        Phase::CommittedFsynced,
        "a clean exit must keep its committed terminal state"
    );
}

#[test]
fn nonzero_process_launch_keeps_its_existing_result() {
    let transaction_id = "00000000-0000-4000-8000-00000000a062";
    let expected = process_outcome(transaction_id, TerminationKind::NonzeroExit { code: 17 });
    let expected_proof = expected.proof.clone();
    let fixture = gateway_with_process_outcome(expected);

    let outcome = execute_fixture_process(&fixture, transaction_id)
        .expect("a nonzero child exit must keep its existing process result");

    assert_eq!(outcome.proof, expected_proof);
    assert!(matches!(
        outcome.proof,
        TypedProof::ProcessLaunch {
            release_status: ReleaseStatus::NonzeroExit,
            exit_code: Some(17),
            ..
        }
    ));
    assert_eq!(
        fixture
            .store
            .load(transaction_id)
            .expect("nonzero exit record")
            .phase,
        Phase::CommittedFsynced,
        "a nonzero exit must retain the pre-existing committed terminal state"
    );
}

#[test]
fn operator_process_repairs_require_matching_derived_proofs() {
    // Contract: operator-supplied process observations may only produce the
    // disposition their repair operation names, with audit-before-transition.
    let fixture = GatewayFixture::with_defaults();
    let reconcile_id = "00000000-0000-4000-8000-00000000a010";
    fixture.quarantine_process(reconcile_id);
    let reconcile_permit = fixture.repair_permit(reconcile_id, RepairOperation::Reconcile);
    let reconcile = fixture
        .gateway
        .apply_operator_repair(
            &reconcile_permit,
            Some(DerivedTerminal {
                disposition: DerivedDisposition::PostState,
                proof: clean_process_proof("reconcile"),
            }),
        )
        .expect("reconcile reports without transitioning");
    assert_eq!(reconcile.phase, Phase::Quarantined);
    assert_eq!(reconcile.conclusion, Conclusion::Quarantine);
    assert_eq!(reconcile.reason, "reconcile only: re-derived post-state");
    let audit = fixture
        .store
        .audit_entries(reconcile_id)
        .expect("reconcile audit");
    assert_eq!(audit[audit.len() - 1].0, "repair_proof_outcome");
    assert!(audit[audit.len() - 1].1.contains("operation=reconcile"));

    let missing_id = "00000000-0000-4000-8000-00000000a011";
    fixture.quarantine_process(missing_id);
    let missing_permit = fixture.repair_permit(missing_id, RepairOperation::Compensate);
    let error = fixture
        .gateway
        .apply_operator_repair(&missing_permit, None)
        .expect_err("proofless compensation is refused");
    match error {
        EffectError::Unauthorized(message) => assert_eq!(
            message,
            "compensate carries no re-derived proof, so no transition is authorized"
        ),
        other => panic!("expected unauthorized repair, got {other}"),
    }
    assert_eq!(
        fixture
            .store
            .load(missing_id)
            .expect("missing record")
            .phase,
        Phase::Quarantined
    );

    let kind_id = "00000000-0000-4000-8000-00000000a012";
    fixture.quarantine_process(kind_id);
    let kind_permit = fixture.repair_permit(kind_id, RepairOperation::Compensate);
    let error = fixture
        .gateway
        .apply_operator_repair(
            &kind_permit,
            Some(DerivedTerminal {
                disposition: DerivedDisposition::PreState,
                proof: TypedProof::File {
                    entries: Vec::new(),
                },
            }),
        )
        .expect_err("a foreign proof kind is refused");
    match error {
        EffectError::Unauthorized(message) => assert_eq!(
            message,
            "a file proof cannot settle a process_launch transaction"
        ),
        other => panic!("expected unauthorized repair, got {other}"),
    }

    let disposition_id = "00000000-0000-4000-8000-00000000a013";
    fixture.quarantine_process(disposition_id);
    let disposition_permit = fixture.repair_permit(disposition_id, RepairOperation::Compensate);
    let error = fixture
        .gateway
        .apply_operator_repair(
            &disposition_permit,
            Some(DerivedTerminal {
                disposition: DerivedDisposition::PostState,
                proof: clean_process_proof("wrong-disposition"),
            }),
        )
        .expect_err("post-state cannot authorize compensation");
    match error {
        EffectError::Unauthorized(message) => assert_eq!(
            message,
            "compensate requires a pre-state proof, not a post-state proof"
        ),
        other => panic!("expected unauthorized repair, got {other}"),
    }

    let terminate_id = "00000000-0000-4000-8000-00000000a014";
    fixture.quarantine_process(terminate_id);
    let terminate_proof = clean_process_proof("terminated");
    let terminate_permit = fixture.repair_permit(terminate_id, RepairOperation::Terminate);
    let terminated = fixture
        .gateway
        .apply_operator_repair(
            &terminate_permit,
            Some(DerivedTerminal {
                disposition: DerivedDisposition::PreState,
                proof: terminate_proof.clone(),
            }),
        )
        .expect("proof-backed termination compensates");
    assert_eq!(terminated.phase, Phase::Compensated);
    assert_eq!(terminated.conclusion, Conclusion::ProvenPreState);
    let record = fixture.store.load(terminate_id).expect("terminated record");
    assert_eq!(record.phase, Phase::Compensated);
    assert_eq!(record.proof, Some(terminate_proof));
    let audit = fixture
        .store
        .audit_entries(terminate_id)
        .expect("termination audit");
    let permit_index = audit
        .iter()
        .position(|(event, _)| event == "repair_permit_issued")
        .expect("permit audit");
    let outcome_index = audit
        .iter()
        .position(|(event, _)| event == "repair_proof_outcome")
        .expect("outcome audit");
    let transition_index = audit
        .iter()
        .position(|(event, _)| event == "compensated")
        .expect("transition audit");
    assert!(permit_index < outcome_index && outcome_index < transition_index);

    let acknowledge_id = "00000000-0000-4000-8000-00000000a015";
    fixture.quarantine_process(acknowledge_id);
    let acknowledge_proof = clean_process_proof("acknowledged");
    let acknowledge_permit =
        fixture.repair_permit(acknowledge_id, RepairOperation::AcknowledgeSafeState);
    let acknowledged = fixture
        .gateway
        .apply_operator_repair(
            &acknowledge_permit,
            Some(DerivedTerminal {
                disposition: DerivedDisposition::PostState,
                proof: acknowledge_proof.clone(),
            }),
        )
        .expect("proof-backed acknowledgement commits");
    assert_eq!(acknowledged.phase, Phase::CommittedFsynced);
    assert_eq!(
        fixture
            .store
            .load(acknowledge_id)
            .expect("acknowledged record")
            .proof,
        Some(acknowledge_proof)
    );

    let stale_id = "00000000-0000-4000-8000-00000000a016";
    fixture.quarantine_process(stale_id);
    let stale_authority = OperatorRecoveryAuthority::new(
        Arc::clone(&fixture.store),
        LocalLeaseEvidence::new(
            fixture.held.object_identity.clone(),
            fixture.held.generation + 1,
        ),
    );
    let stale_permit = stale_authority
        .issue(stale_id, RepairOperation::Compensate)
        .expect("foreign-generation permit is minted for gateway validation");
    let error = fixture
        .gateway
        .apply_operator_repair(
            &stale_permit,
            Some(DerivedTerminal {
                disposition: DerivedDisposition::PreState,
                proof: clean_process_proof("stale"),
            }),
        )
        .expect_err("foreign-generation repair is stale");
    match error {
        EffectError::StalePermit(message) => assert_eq!(
            message,
            "repair permit does not match the held project object identity or generation"
        ),
        other => panic!("expected stale repair permit, got {other}"),
    }

    let wrong_phase_id = "00000000-0000-4000-8000-00000000a017";
    fixture.quarantine_process(wrong_phase_id);
    let wrong_phase_permit = fixture.repair_permit(wrong_phase_id, RepairOperation::Compensate);
    fixture
        .store
        .repair_transition(
            wrong_phase_id,
            Phase::Compensated,
            Some(clean_process_proof("already-settled")),
            "already settled".to_owned(),
            TerminalOutcome::failed(ErrorKind::Unavailable),
        )
        .expect("settle before replaying permit");
    let error = fixture
        .gateway
        .apply_operator_repair(
            &wrong_phase_permit,
            Some(DerivedTerminal {
                disposition: DerivedDisposition::PreState,
                proof: clean_process_proof("replayed"),
            }),
        )
        .expect_err("a repair permit cannot be replayed after settlement");
    assert!(matches!(
        error,
        EffectError::WrongPhase {
            transaction_id,
            phase: Phase::Compensated,
        } if transaction_id == wrong_phase_id
    ));
}

#[test]
fn ambiguous_process_survivor_accepts_only_a_post_state_proof() {
    // Contract: the documented non-quarantined survivor exception may advance
    // effect_started to committed once, and a repeated committed proof is idempotent.
    let fixture = GatewayFixture::with_defaults();
    let transaction_id = "00000000-0000-4000-8000-00000000a018";
    fixture
        .gateway
        .open_process_transaction(&fixture.process_intent(transaction_id))
        .expect("open process transaction");
    fixture
        .store
        .advance(
            transaction_id,
            Phase::EffectStartedFsynced,
            None,
            None,
            None,
        )
        .expect("leave ambiguous survivor");
    let authority = fixture.recovery_authority();
    let permit = authority
        .issue_ambiguous_survivor(transaction_id)
        .expect("issue survivor permit");
    let proof = clean_process_proof("survivor");
    let outcome = fixture
        .gateway
        .apply_operator_repair(
            &permit,
            Some(DerivedTerminal {
                disposition: DerivedDisposition::PostState,
                proof: proof.clone(),
            }),
        )
        .expect("settle survivor");
    assert_eq!(outcome.phase, Phase::CommittedFsynced);
    let committed = fixture
        .store
        .load(transaction_id)
        .expect("committed survivor");
    assert_eq!(committed.proof, Some(proof.clone()));
    let history = committed.phase_history.clone();

    let repeated = authority
        .issue_ambiguous_survivor(transaction_id)
        .expect("committed survivor remains inspectable");
    let repeated_outcome = fixture
        .gateway
        .apply_operator_repair(
            &repeated,
            Some(DerivedTerminal {
                disposition: DerivedDisposition::PostState,
                proof,
            }),
        )
        .expect("repeat observation is report-only");
    assert_eq!(repeated_outcome.phase, Phase::CommittedFsynced);
    assert_eq!(
        fixture
            .store
            .load(transaction_id)
            .expect("survivor record")
            .phase_history,
        history
    );
}

#[test]
fn operator_file_repairs_require_gateway_rederivation_and_file_semantics() {
    // Contract: a file repair is accepted only when both observations match the
    // recorded state and any linked process proof is clean and terminal.
    let fixture = GatewayFixture::with_defaults();

    let post_id = "00000000-0000-4000-8000-00000000a020";
    let post_record =
        fixture.quarantine_file(post_id, "scenes/operator-post.tscn", b"post\n", true, None);
    let post_proof = TypedProof::File {
        entries: post_record.intended_post_hashes.clone(),
    };
    let post_permit = fixture.repair_permit(post_id, RepairOperation::AcknowledgeSafeState);
    let post = fixture
        .gateway
        .apply_operator_repair(
            &post_permit,
            Some(DerivedTerminal {
                disposition: DerivedDisposition::PostState,
                proof: post_proof.clone(),
            }),
        )
        .expect("matching post-state commits");
    assert_eq!(post.phase, Phase::CommittedFsynced);
    assert_eq!(post.conclusion, Conclusion::ProvenPostState);
    assert_eq!(
        fixture.store.load(post_id).expect("post record").proof,
        Some(post_proof)
    );

    let pre_id = "00000000-0000-4000-8000-00000000a021";
    let pre_record =
        fixture.quarantine_file(pre_id, "scenes/operator-pre.tscn", b"post\n", false, None);
    let pre_proof = TypedProof::File {
        entries: pre_record.pre_hashes.clone(),
    };
    let pre_permit = fixture.repair_permit(pre_id, RepairOperation::Compensate);
    let pre = fixture
        .gateway
        .apply_operator_repair(
            &pre_permit,
            Some(DerivedTerminal {
                disposition: DerivedDisposition::PreState,
                proof: pre_proof.clone(),
            }),
        )
        .expect("matching pre-state compensates");
    assert_eq!(pre.phase, Phase::Compensated);
    assert_eq!(pre.conclusion, Conclusion::ProvenPreState);
    assert_eq!(
        fixture.store.load(pre_id).expect("pre record").proof,
        Some(pre_proof)
    );

    let mismatch_id = "00000000-0000-4000-8000-00000000a022";
    let mismatch_record = fixture.quarantine_file(
        mismatch_id,
        "scenes/operator-mismatch.tscn",
        b"post\n",
        false,
        None,
    );
    let mismatch_permit = fixture.repair_permit(mismatch_id, RepairOperation::AcknowledgeSafeState);
    let error = fixture
        .gateway
        .apply_operator_repair(
            &mismatch_permit,
            Some(DerivedTerminal {
                disposition: DerivedDisposition::PostState,
                proof: TypedProof::File {
                    entries: mismatch_record.intended_post_hashes,
                },
            }),
        )
        .expect_err("the gateway independently observes the missing post-state");
    match error {
        EffectError::Unauthorized(message) => assert_eq!(
            message,
            "the gateway's own re-derivation of the recorded post-state state reports mismatch"
        ),
        other => panic!("expected independent-proof refusal, got {other}"),
    }
    assert_eq!(
        fixture
            .store
            .load(mismatch_id)
            .expect("mismatch record")
            .phase,
        Phase::Quarantined
    );

    let composite_id = "00000000-0000-4000-8000-00000000a023";
    let composite_record = fixture.quarantine_file(
        composite_id,
        "scenes/operator-composite.tscn",
        b"post\n",
        true,
        Some("00000000-0000-4000-8000-00000000afff"),
    );
    let composite_permit =
        fixture.repair_permit(composite_id, RepairOperation::AcknowledgeSafeState);
    let error = fixture
        .gateway
        .apply_operator_repair(
            &composite_permit,
            Some(DerivedTerminal {
                disposition: DerivedDisposition::PostState,
                proof: TypedProof::File {
                    entries: composite_record.intended_post_hashes,
                },
            }),
        )
        .expect_err("an absent helper proof refuses file repair");
    match error {
        EffectError::Unauthorized(message) => assert_eq!(
            message,
            "the linked process_launch sub-proof does not confirm a contained, clean, terminal \
             helper run"
        ),
        other => panic!("expected composite-proof refusal, got {other}"),
    }

    let terminate_id = "00000000-0000-4000-8000-00000000a024";
    let terminate_record = fixture.quarantine_file(
        terminate_id,
        "scenes/operator-terminate.tscn",
        b"post\n",
        false,
        None,
    );
    let terminate_permit = fixture.repair_permit(terminate_id, RepairOperation::Terminate);
    let error = fixture
        .gateway
        .apply_operator_repair(
            &terminate_permit,
            Some(DerivedTerminal {
                disposition: DerivedDisposition::PreState,
                proof: TypedProof::File {
                    entries: terminate_record.pre_hashes,
                },
            }),
        )
        .expect_err("terminate is process-only");
    match error {
        EffectError::Unauthorized(message) => assert_eq!(
            message,
            "terminate applies only to a process_launch transaction, not file"
        ),
        other => panic!("expected process-only refusal, got {other}"),
    }
}

#[test]
fn file_repair_reconciles_without_transition_and_acknowledges_proven_post_state() {
    // Contract: gateway-derived repair records an audited report-only reconcile,
    // then persists an observed file proof when post-state acknowledgement succeeds.
    let fixture = GatewayFixture::with_defaults();
    let reconcile_id = "00000000-0000-4000-8000-00000000a025";
    fixture.quarantine_file(
        reconcile_id,
        "scenes/reconcile.tscn",
        b"post\n",
        false,
        None,
    );
    let permit = fixture.repair_permit(reconcile_id, RepairOperation::Reconcile);
    let outcome = fixture
        .gateway
        .apply_repair(&permit)
        .expect("reconcile reports current proofs");
    assert_eq!(outcome.phase, Phase::Quarantined);
    assert_eq!(outcome.conclusion, Conclusion::Quarantine);
    assert_eq!(outcome.reason, "reconcile only: post=mismatch pre=matches");
    assert_eq!(
        fixture
            .store
            .load(reconcile_id)
            .expect("reconciled record")
            .phase,
        Phase::Quarantined
    );

    let acknowledge_id = "00000000-0000-4000-8000-00000000a026";
    let intended = fixture.quarantine_file(
        acknowledge_id,
        "scenes/acknowledge.tscn",
        b"post\n",
        true,
        None,
    );
    let permit = fixture.repair_permit(acknowledge_id, RepairOperation::AcknowledgeSafeState);
    let outcome = fixture
        .gateway
        .apply_repair(&permit)
        .expect("matching post-state is acknowledged");
    assert_eq!(outcome.phase, Phase::CommittedFsynced);
    assert_eq!(outcome.conclusion, Conclusion::ProvenPostState);
    let record = fixture
        .store
        .load(acknowledge_id)
        .expect("acknowledged file record");
    assert_eq!(record.phase, Phase::CommittedFsynced);
    assert_eq!(
        record.proof,
        Some(TypedProof::File {
            entries: intended.intended_post_hashes,
        })
    );
}

#[test]
fn permit_validation_rejects_wrong_phase_scope_and_late_quarantine() {
    // Contract: the gateway independently enforces phase, boundary, capability
    // pairing, and a quarantine that appears after an ordinary permit was issued.
    let wrong_phase = GatewayFixture::with_defaults();
    let wrong_phase_id = "00000000-0000-4000-8000-00000000a030";
    let wrong_phase_intent = wrong_phase.file_intent(
        wrong_phase_id,
        "scenes/wrong-phase.tscn",
        FileOperation::Create,
        Some(b"post\n"),
    );
    wrong_phase
        .gateway
        .open_file_transaction(&wrong_phase_intent)
        .expect("open intent only");
    let permit = wrong_phase.permit(
        wrong_phase_id,
        Capability::ProjectWrite,
        OperationFamily::ProjectMutation,
        EffectBoundaryKind::File,
    );
    let error = wrong_phase
        .gateway
        .execute_file_effect(&permit)
        .expect_err("intent-only transaction cannot execute");
    assert!(matches!(
        error,
        EffectError::WrongPhase {
            transaction_id,
            phase: Phase::IntentFsynced,
        } if transaction_id == wrong_phase_id
    ));

    let wrong_boundary = GatewayFixture::with_defaults();
    let wrong_boundary_id = "00000000-0000-4000-8000-00000000a031";
    let intent = wrong_boundary.file_intent(
        wrong_boundary_id,
        "scenes/wrong-boundary.tscn",
        FileOperation::Create,
        Some(b"post\n"),
    );
    wrong_boundary.open_staged_file(&intent);
    let permit = wrong_boundary.permit(
        wrong_boundary_id,
        Capability::RuntimeExecute,
        OperationFamily::RuntimeExecution,
        EffectBoundaryKind::Process,
    );
    let error = wrong_boundary
        .gateway
        .execute_file_effect(&permit)
        .expect_err("a process permit cannot drive a file effect");
    match error {
        EffectError::Unauthorized(message) => assert_eq!(
            message,
            "permit authorizes a different effect boundary (Process)"
        ),
        other => panic!("expected boundary mismatch, got {other}"),
    }

    let wrong_family = GatewayFixture::with_defaults();
    let wrong_family_id = "00000000-0000-4000-8000-00000000a032";
    let intent = wrong_family.file_intent(
        wrong_family_id,
        "scenes/wrong-family.tscn",
        FileOperation::Create,
        Some(b"post\n"),
    );
    wrong_family.open_staged_file(&intent);
    let permit = wrong_family.permit(
        wrong_family_id,
        Capability::ProjectWrite,
        OperationFamily::RuntimeExecution,
        EffectBoundaryKind::File,
    );
    let error = wrong_family
        .gateway
        .execute_file_effect(&permit)
        .expect_err("capability and family must agree");
    match error {
        EffectError::Unauthorized(message) => assert_eq!(
            message,
            "permit capability does not match its operation family"
        ),
        other => panic!("expected capability mismatch, got {other}"),
    }

    let late_quarantine = GatewayFixture::with_defaults();
    let pending_id = "00000000-0000-4000-8000-00000000a033";
    let pending = late_quarantine.file_intent(
        pending_id,
        "scenes/pending.tscn",
        FileOperation::Create,
        Some(b"post\n"),
    );
    late_quarantine.open_staged_file(&pending);
    let permit = late_quarantine.permit(
        pending_id,
        Capability::ProjectWrite,
        OperationFamily::ProjectMutation,
        EffectBoundaryKind::File,
    );
    let quarantine_id = "00000000-0000-4000-8000-00000000a034";
    late_quarantine.quarantine_file(
        quarantine_id,
        "scenes/quarantine.tscn",
        b"post\n",
        false,
        None,
    );
    let error = late_quarantine
        .gateway
        .execute_file_effect(&permit)
        .expect_err("late quarantine invalidates an already-issued permit");
    match error {
        EffectError::Quarantined(transactions) => {
            assert_eq!(transactions, vec![quarantine_id.to_owned()]);
        }
        other => panic!("expected quarantine refusal, got {other}"),
    }
    assert_eq!(
        late_quarantine
            .store
            .load(pending_id)
            .expect("pending record")
            .phase,
        Phase::StagedFsynced
    );
    assert!(!late_quarantine.root.join("scenes/pending.tscn").exists());
}

#[test]
fn boundary_attempts_forward_canonical_actions_and_fail_closed_for_file_process() {
    // Contract: non-file boundary probes receive the journaled canonical tool
    // action, while file and unresolved process attempts return specific refusals.
    let probe = BoundaryProbe::default();
    let fixture = GatewayFixture::new({
        let probe = probe.clone();
        move |lease| EffectBoundaries {
            file: Box::new(SafeRootFileBoundary::new(lease.safe_root())),
            process: Box::new(probe.clone()),
            lsp_dap: Box::new(probe.clone()),
            editor: Box::new(probe.clone()),
            runtime: Box::new(probe),
        }
    });
    let cases = [
        (
            "00000000-0000-4000-8000-00000000a040",
            EffectBoundaryKind::LspDap,
            Capability::ProjectWrite,
            OperationFamily::ProjectMutation,
        ),
        (
            "00000000-0000-4000-8000-00000000a041",
            EffectBoundaryKind::Editor,
            Capability::EditorMutate,
            OperationFamily::EditorMutation,
        ),
        (
            "00000000-0000-4000-8000-00000000a042",
            EffectBoundaryKind::Runtime,
            Capability::RuntimeExecute,
            OperationFamily::RuntimeExecution,
        ),
    ];
    for (transaction_id, boundary, capability, family) in cases {
        let intent = fixture.file_intent(
            transaction_id,
            &format!("scenes/{transaction_id}.tscn"),
            FileOperation::Create,
            Some(b"post\n"),
        );
        fixture.open_staged_file(&intent);
        let permit = fixture.permit(transaction_id, capability, family, boundary);
        let proof = fixture
            .gateway
            .attempt_boundary_effect(&permit, boundary)
            .expect("implemented probe returns proof");
        match boundary {
            EffectBoundaryKind::LspDap => {
                assert_eq!(
                    proof,
                    TypedProof::File {
                        entries: Vec::new()
                    }
                );
            }
            EffectBoundaryKind::Editor | EffectBoundaryKind::Runtime => {
                assert!(matches!(
                    proof,
                    TypedProof::EditorRuntime {
                        bridge_reachable: true,
                        ..
                    }
                ));
            }
            EffectBoundaryKind::File | EffectBoundaryKind::Process => unreachable!(),
        }
    }
    assert_eq!(
        probe.calls.lock().expect("boundary calls").as_slice(),
        &[
            "lsp:00000000-0000-4000-8000-00000000a040:resource.update:",
            "editor:00000000-0000-4000-8000-00000000a041:resource.update:",
            "runtime:00000000-0000-4000-8000-00000000a042:resource.update:",
        ]
    );

    let file_id = "00000000-0000-4000-8000-00000000a043";
    let file_intent = fixture.file_intent(
        file_id,
        "scenes/file-attempt.tscn",
        FileOperation::Create,
        Some(b"post\n"),
    );
    fixture.open_staged_file(&file_intent);
    let permit = fixture.permit(
        file_id,
        Capability::ProjectWrite,
        OperationFamily::ProjectMutation,
        EffectBoundaryKind::File,
    );
    let error = fixture
        .gateway
        .attempt_boundary_effect(&permit, EffectBoundaryKind::File)
        .expect_err("file has a dedicated execution path");
    match error {
        EffectError::Unauthorized(message) => assert_eq!(
            message,
            "the file boundary is driven by execute_file_effect"
        ),
        other => panic!("expected file-path refusal, got {other}"),
    }

    let process_id = "00000000-0000-4000-8000-00000000a044";
    fixture
        .gateway
        .open_process_transaction(&fixture.process_intent(process_id))
        .expect("open process transaction");
    let permit = fixture.permit(
        process_id,
        Capability::RuntimeExecute,
        OperationFamily::RuntimeExecution,
        EffectBoundaryKind::Process,
    );
    let error = fixture
        .gateway
        .attempt_boundary_effect(&permit, EffectBoundaryKind::Process)
        .expect_err("process attempt needs a resolved launch request");
    match error {
        EffectError::Boundary(message) => assert_eq!(
            message,
            "the process boundary requires a resolved supervised launch request"
        ),
        other => panic!("expected resolved-request refusal, got {other}"),
    }

    let wrong_phase_id = "00000000-0000-4000-8000-00000000a045";
    let wrong_phase_intent = fixture.file_intent(
        wrong_phase_id,
        "scenes/boundary-wrong-phase.tscn",
        FileOperation::Create,
        Some(b"post\n"),
    );
    fixture
        .gateway
        .open_file_transaction(&wrong_phase_intent)
        .expect("open intent only");
    let permit = fixture.permit(
        wrong_phase_id,
        Capability::ProjectWrite,
        OperationFamily::ProjectMutation,
        EffectBoundaryKind::LspDap,
    );
    let error = fixture
        .gateway
        .attempt_boundary_effect(&permit, EffectBoundaryKind::LspDap)
        .expect_err("boundary attempt requires staged transaction");
    assert!(matches!(
        error,
        EffectError::WrongPhase {
            transaction_id,
            phase: Phase::IntentFsynced,
        } if transaction_id == wrong_phase_id
    ));
}

#[test]
fn effect_errors_preserve_the_stable_domain_taxonomy() {
    // Contract: authorization, phase/quarantine, and boundary failures retain
    // their distinct stable domain codes when exposed to a caller.
    let unauthorized = EffectError::Unauthorized("scope mismatch".to_owned()).to_outcome();
    assert_eq!(unauthorized.code(), "permission_denied");
    assert_eq!(
        unauthorized.message(),
        "effect permit does not authorize this operation: scope mismatch"
    );

    let wrong_phase = EffectError::WrongPhase {
        transaction_id: "00000000-0000-4000-8000-00000000a050".to_owned(),
        phase: Phase::IntentFsynced,
    }
    .to_outcome();
    assert_eq!(wrong_phase.code(), "conflict");
    assert!(wrong_phase.message().contains("intent_fsynced"));

    let unavailable = EffectError::Boundary("publisher unavailable".to_owned()).to_outcome();
    assert_eq!(unavailable.code(), "unavailable");
    assert_eq!(
        unavailable.message(),
        "file effect failed: publisher unavailable"
    );
}
