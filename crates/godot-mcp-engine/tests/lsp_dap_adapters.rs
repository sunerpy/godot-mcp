//! Todo 18 acceptance suite: bounded LSP/DAP adapters and engine outcome tools.
//!
//! Nothing here needs a real Godot installation. Every exchange runs against the
//! committed deterministic fake under `tests/fixtures/adapters/`, which binds a real
//! loopback `TcpListener`, accepts real connections and speaks the real
//! `Content-Length` framing — so the code under test is the production codec, the
//! production channel and the production adapters, not mocks of them.
//!
//! # What this suite is really about
//!
//! One rule, asserted from every angle:
//!
//! > **A loopback listener is not an endpoint. A server-owned launch receipt is.**
//!
//! Godot's language server and debug adapter are unauthenticated loopback TCP servers
//! hosted only by an editor build. So "something is listening on this port" is
//! compatible with a different editor, a different project, or an unrelated process
//! that took the port after our editor exited. Every adapter entry point therefore
//! requires an `OwnedEndpoint`, which can only be produced by verifying the full
//! tuple — PID, start time, binary hash, project root URI, endpoint, generation —
//! against the launch receipt this server itself journaled.
//!
//! Two tests are named by `qa/tasks/18.json` and are the only entry points the QA
//! descriptor invokes:
//!
//! * `lsp_dap_adapters_happy_path`
//! * `lsp_dap_adapters_failure_paths`

#![allow(
    clippy::too_many_lines,
    reason = "acceptance suites assert one whole operation end to end"
)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use godot_mcp_domain::ErrorKind;
use godot_mcp_engine::adapters::admission::{AdmissionError, ClientAdmission, MAX_CLIENTS};
use godot_mcp_engine::adapters::codec::{
    CodecError, FrameReader, MAX_HEADER_BYTES, MAX_PAYLOAD_BYTES, encode_frame,
};
use godot_mcp_engine::adapters::connect::{EndpointRegistry, exchange_tool_name};
use godot_mcp_engine::adapters::dap::{
    DapAdapter, ENGINE_REQUEST_TIMEOUT, IMPLEMENTED_REQUESTS, LaunchState, UNIMPLEMENTED_REQUESTS,
};
use godot_mcp_engine::adapters::error::AdapterError;
use godot_mcp_engine::adapters::lsp::{LspSession, UNBOUND_METHODS};
use godot_mcp_engine::adapters::receipt::{
    EditorEndpoint, EndpointKind, EndpointReceipt, ObservedEndpoint, OwnershipFailure,
};
use godot_mcp_engine::adapters::tools::{
    EffectBoundary, EffectGate, EffectRequest, EngineTool, EngineTools, StopRequest, ToolPermit,
};
use godot_mcp_engine::supervision::journal::{
    BoundaryMembership, Disposition, JournalError, LaunchJournal, LaunchPayload, LaunchPhase,
    LaunchRecord, OperationFamily, ProcessLaunchProof, ProjectIdentityClaim, ReleaseStatus,
    TransactionEnvelope, TransportMode, validate_canonical,
};
use godot_mcp_engine::validation::outcome::{
    Classification, Evidence, ExitAssertion, Observed, classify,
};
use serde_json::{Value, json};

#[path = "fixtures/adapters/fake_protocol_server.rs"]
mod fake;

use fake::{
    FakeProtocolServer, FixtureEditorReceipt, Malformed, Reply, Script, closed_loopback_address,
};

// ---------------------------------------------------------------------------
// Committed fixtures
// ---------------------------------------------------------------------------

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/adapters")
}

fn receipt_fixture() -> FixtureEditorReceipt {
    FixtureEditorReceipt::load(&fixture_dir().join("fixture-editor-receipt.json"))
}

// ---------------------------------------------------------------------------
// One durable store, exposed through Todo 14's journal port
// ---------------------------------------------------------------------------

/// A durable launch journal over a real directory, using Todo 6's canonical schema
/// and Todo 14's four fsynced phases. This suite creates no second persistence layer.
struct DurableTestJournal {
    dir: PathBuf,
}

impl DurableTestJournal {
    fn open(dir: &Path) -> Self {
        std::fs::create_dir_all(dir.join("transactions")).expect("journal dir");
        std::fs::create_dir_all(dir.join("staged")).expect("staged dir");
        Self {
            dir: dir.to_path_buf(),
        }
    }

    fn write_value(
        &self,
        transaction_id: &str,
        phase: LaunchPhase,
        value: &Value,
    ) -> Result<(), JournalError> {
        validate_canonical(value)?;
        let path = self
            .dir
            .join("transactions")
            .join(format!("{transaction_id}.json"));
        let bytes = serde_json::to_vec(value).map_err(|error| JournalError::Durability {
            phase,
            reason: error.to_string(),
        })?;
        std::fs::write(&path, &bytes).map_err(|error| JournalError::Durability {
            phase,
            reason: error.to_string(),
        })?;
        if let Ok(file) = std::fs::File::open(&path) {
            let _ignored = file.sync_all();
        }
        Ok(())
    }

    fn stored_values(&self) -> Vec<Value> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(self.dir.join("transactions")) else {
            return out;
        };
        let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
        paths.sort();
        for path in paths {
            if let Ok(bytes) = std::fs::read(&path)
                && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
            {
                out.push(value);
            }
        }
        out
    }

    fn launch_records(
        &self,
        accept: &dyn Fn(&str) -> bool,
    ) -> Result<Vec<LaunchRecord>, JournalError> {
        let mut out = Vec::new();
        for value in self.stored_values() {
            if value["effect_kind"] != json!("process_launch") {
                continue;
            }
            if !accept(value["phase"].as_str().unwrap_or_default()) {
                continue;
            }
            out.push(LaunchRecord::from_canonical_value(&value, &|staging_id| {
                std::fs::read(self.dir.join("staged").join(staging_id)).ok()
            })?);
        }
        Ok(out)
    }
}

impl LaunchJournal for DurableTestJournal {
    fn write_phase(&self, record: &LaunchRecord) -> Result<(), JournalError> {
        let value = record.to_canonical_value()?;
        for payload in &record.payloads {
            let staged = self.dir.join("staged").join(payload.staging_id());
            std::fs::write(&staged, &payload.bytes).map_err(|error| JournalError::Durability {
                phase: record.phase,
                reason: error.to_string(),
            })?;
        }
        self.write_value(&record.envelope.transaction_id, record.phase, &value)
    }

    fn settle(
        &self,
        record: &LaunchRecord,
        disposition: Disposition,
        reason: &str,
        proof: Option<&ProcessLaunchProof>,
    ) -> Result<(), JournalError> {
        let value = godot_mcp_engine::supervision::journal::canonical_terminal_value(
            record,
            disposition,
            reason,
            proof,
        )?;
        self.write_value(&record.envelope.transaction_id, record.phase, &value)
    }

    fn open_launches(&self) -> Result<Vec<LaunchRecord>, JournalError> {
        self.launch_records(&|phase| {
            LaunchPhase::ALL
                .iter()
                .any(|known| known.as_str() == phase && *known != LaunchPhase::CommittedFsynced)
        })
    }

    fn committed_launches(&self) -> Result<Vec<LaunchRecord>, JournalError> {
        self.launch_records(&|phase| phase == LaunchPhase::CommittedFsynced.as_str())
    }

    fn quarantined(&self) -> Result<Vec<String>, JournalError> {
        Ok(self
            .stored_values()
            .into_iter()
            .filter(|value| value["phase"] == json!("quarantined"))
            .filter_map(|value| value["transaction_id"].as_str().map(str::to_owned))
            .collect())
    }
}

// ---------------------------------------------------------------------------
// The effect gate: Todo 6's gateway, as a recording port
// ---------------------------------------------------------------------------

/// Records every authorization request, so a test can prove that a refused operation
/// dispatched **no** effect at all.
struct RecordingGate {
    seen: std::cell::RefCell<Vec<EffectRequest>>,
    refuse: Option<String>,
    serial: std::cell::Cell<u32>,
}

impl RecordingGate {
    fn allowing() -> Self {
        Self {
            seen: std::cell::RefCell::new(Vec::new()),
            refuse: None,
            serial: std::cell::Cell::new(0),
        }
    }

    fn refusing(reason: &str) -> Self {
        Self {
            seen: std::cell::RefCell::new(Vec::new()),
            refuse: Some(reason.to_owned()),
            serial: std::cell::Cell::new(0),
        }
    }

    fn dispatches(&self) -> usize {
        self.seen.borrow().len()
    }

    fn boundaries(&self) -> Vec<EffectBoundary> {
        self.seen
            .borrow()
            .iter()
            .map(|request| request.boundary)
            .collect()
    }

    fn tool_names(&self) -> Vec<String> {
        self.seen
            .borrow()
            .iter()
            .map(|request| request.tool_name.clone())
            .collect()
    }
}

impl EffectGate for RecordingGate {
    fn authorize(&self, request: &EffectRequest) -> Result<ToolPermit, String> {
        self.seen.borrow_mut().push(request.clone());
        if let Some(reason) = &self.refuse {
            return Err(reason.clone());
        }
        let serial = self.serial.get() + 1;
        self.serial.set(serial);
        Ok(ToolPermit::issue(&uuid(1_900 + serial), request))
    }
}

fn uuid(serial: u32) -> String {
    format!("00000000-0000-4000-8000-{serial:012}")
}

// ---------------------------------------------------------------------------
// Journaling the fixture editor launch
// ---------------------------------------------------------------------------

fn envelope(fixture: &FixtureEditorReceipt, generation: u64) -> TransactionEnvelope {
    TransactionEnvelope {
        transaction_id: fixture.transaction_id.clone(),
        project: ProjectIdentityClaim {
            object_identity: fixture.project_object_identity.clone(),
            root_object: fixture.project_root_object.clone(),
            file_object: fixture.project_file_object.clone(),
            generation,
        },
        transport_mode: TransportMode::Stdio,
        principal: None,
        session: None,
        operation_family: OperationFamily::RuntimeExecution,
        tool_name: "adapters.launch_editor".to_owned(),
        idempotency_id: "idem-1801".to_owned(),
        canonical_request_hash: "0".repeat(64),
        canonical_request_bytes: 2,
        opened_at_unix_ms: 1_700_000_000_000,
    }
}

fn live_proof(fixture: &FixtureEditorReceipt) -> ProcessLaunchProof {
    ProcessLaunchProof {
        boundary_identity: fixture.boundary_identity.clone(),
        pid: Some(fixture.child_pid),
        start_time: Some(fixture.child_start_time.clone()),
        boundary_membership: BoundaryMembership::Confirmed,
        release_status: ReleaseStatus::ResumedContained,
        exit_code: None,
        signal: None,
        boundary_empty_after_exit: None,
    }
}

/// Journal the fixture editor launch through Todo 14's four canonical phases, in
/// order, so the committed record the adapters read is one a real launch would have
/// produced.
fn journal_editor_launch(
    journal: &DurableTestJournal,
    fixture: &FixtureEditorReceipt,
    lsp_port: u16,
    dap_port: u16,
    generation: u64,
) -> LaunchRecord {
    let envelope = envelope(fixture, generation);
    let intent = LaunchPayload::new(
        LaunchRecord::INTENT_PAYLOAD,
        &fixture.intent_facts(lsp_port, dap_port),
        vec![
            (
                "project.godot".to_owned(),
                Some(fixture.project_file_object.clone()),
            ),
            (
                "project_root".to_owned(),
                Some(fixture.project_root_object.clone()),
            ),
        ],
    );
    let staged = LaunchPayload::new(LaunchRecord::STAGED_PAYLOAD, &BTreeMap::new(), Vec::new());
    let receipt = LaunchPayload::new(
        LaunchRecord::RECEIPT_PAYLOAD,
        &fixture.receipt_facts(),
        Vec::new(),
    );

    let mut record = LaunchRecord {
        envelope,
        phase: LaunchPhase::IntentFsynced,
        payloads: vec![intent],
        proof: None,
    };
    journal.write_phase(&record).expect("intent_fsynced");

    record.phase = LaunchPhase::StagedFsynced;
    record.payloads.push(staged);
    journal.write_phase(&record).expect("staged_fsynced");

    record.phase = LaunchPhase::EffectStartedFsynced;
    record.payloads.push(receipt);
    record.proof = Some(ProcessLaunchProof {
        release_status: ReleaseStatus::Suspended,
        ..live_proof(fixture)
    });
    journal
        .write_phase(&record)
        .expect("effect_started_fsynced");

    record.phase = LaunchPhase::CommittedFsynced;
    record.proof = Some(live_proof(fixture));
    journal.write_phase(&record).expect("committed_fsynced");
    record
}

fn observed(receipt: &EndpointReceipt) -> ObservedEndpoint {
    ObservedEndpoint {
        endpoint: receipt.endpoint.clone(),
        pid: Some(receipt.pid),
        start_time: Some(receipt.start_time.clone()),
        binary_sha256: receipt.binary_sha256.clone(),
        project_root_uri: receipt.project_root_uri.clone(),
        project_generation: receipt.project_generation,
    }
}

// ---------------------------------------------------------------------------
// Scripted engine replies
// ---------------------------------------------------------------------------

fn lsp_initialize_result(id: i64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "capabilities": {
                "documentSymbolProvider": true,
                "workspaceSymbolProvider": false,
                "foldingRangeProvider": false,
                "completionProvider": { "resolveProvider": true },
                "hoverProvider": true,
                "definitionProvider": true,
                "signatureHelpProvider": { "triggerCharacters": [",", "("] },
            }
        }
    })
}

fn lsp_capabilities_notification() -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "gdscript/capabilities",
        "params": { "classes": [{ "name": "Node2D" }] }
    })
}

fn lsp_publish_diagnostics(uri: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "textDocument/publishDiagnostics",
        "params": {
            "uri": uri,
            "diagnostics": [{
                "severity": 1,
                "range": {
                    "start": { "line": 6, "character": 8 },
                    "end": { "line": 6, "character": 20 },
                },
                "message": "Parse Error: Expected end of statement after expression.",
            }]
        }
    })
}

fn lsp_document_symbols(id: i64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": [{
            "name": "Player",
            "kind": 5,
            "detail": "extends CharacterBody2D",
            "range": { "start": { "line": 0, "character": 0 } },
            "children": [{
                "name": "_physics_process",
                "kind": 6,
                "detail": "func _physics_process(delta: float) -> void",
                "range": { "start": { "line": 11, "character": 0 } },
            }],
        }]
    })
}

fn dap_response(seq: i64, request_seq: i64, command: &str, body: Value) -> Value {
    json!({
        "seq": seq,
        "type": "response",
        "request_seq": request_seq,
        "success": true,
        "command": command,
        "body": body,
    })
}

fn dap_initialize_body() -> Value {
    json!({
        "supportsConfigurationDoneRequest": true,
        "supportsBreakpointLocationsRequest": true,
        "supportsEvaluateForHovers": true,
        "supportsSetVariable": true,
    })
}

// ---------------------------------------------------------------------------
// Happy path
// ---------------------------------------------------------------------------

/// These tests bind loopback listeners, and take "closed" addresses by binding
/// and dropping one. macOS and Windows can hand a dropped port to the very next
/// bind, so one test's closed address could be another's live fake server; the
/// tests therefore run one at a time.
fn one_at_a_time() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[test]
fn lsp_dap_adapters_happy_path() {
    let _serial = one_at_a_time();
    let fixture = receipt_fixture();
    let root = tempfile::tempdir().expect("scenario tempdir");
    let journal = DurableTestJournal::open(root.path());

    // -- 0. The fake servers stand in for an editor's two protocol plugins. ----
    //       Fragmented and coalesced delivery are scripted, so the framing
    //       properties this suite asserts are deterministic rather than timing.
    let lsp_server = FakeProtocolServer::start(
        Script::Replies(vec![
            // `initialize` comes back split across eight-byte writes, so the reader
            // provably sees a partial header and a partial body.
            Reply::fragmented(lsp_initialize_result(1), 8),
            // `didOpen` is a notification: the fake answers it with the two messages
            // Godot really pushes, coalesced into one write.
            Reply::coalesced(lsp_capabilities_notification(), 2),
            Reply::whole(lsp_publish_diagnostics(
                "file:///fixtures/adapters/editor-project/player.gd",
            )),
            Reply::whole(lsp_document_symbols(2)),
        ]),
        1,
    );
    let dap_server = FakeProtocolServer::start(
        Script::Replies(vec![
            Reply::whole(dap_response(1, 1, "initialize", dap_initialize_body())),
            // Godot's req_launch returns an empty body and defers the real work.
            Reply::whole(dap_response(2, 2, "launch", Value::Null)),
            Reply::whole(dap_response(3, 3, "configurationDone", Value::Null)),
            Reply::whole(dap_response(
                4,
                4,
                "setBreakpoints",
                json!({ "breakpoints": [{ "verified": true, "line": 12 }] }),
            )),
            Reply::whole(dap_response(
                5,
                5,
                "stackTrace",
                json!({
                    "stackFrames": [{
                        "id": 1,
                        "name": "Player::_physics_process",
                        "source": { "path": "res://player.gd" },
                        "line": 12,
                    }],
                    "totalFrames": 1,
                }),
            )),
            Reply::whole(dap_response(
                6,
                6,
                "scopes",
                json!({ "scopes": [{ "name": "Locals", "variablesReference": 100 }] }),
            )),
            Reply::whole(dap_response(
                7,
                7,
                "variables",
                json!({
                    "variables": [{
                        "name": "delta",
                        "value": "0.016667",
                        "type": "float",
                        "variablesReference": 0,
                    }]
                }),
            )),
            Reply::whole(dap_response(
                8,
                8,
                "continue",
                json!({ "allThreadsContinued": true }),
            )),
        ]),
        1,
    );

    // -- 1. The endpoints are owned because THIS server journaled the launch. --
    let committed = journal_editor_launch(
        &journal,
        &fixture,
        lsp_server.port(),
        dap_server.port(),
        fixture.project_generation,
    );
    assert_eq!(committed.phase, LaunchPhase::CommittedFsynced);

    let gate = RecordingGate::allowing();
    let registry = EndpointRegistry::new(
        &journal,
        ClientAdmission::new(),
        &fixture.project_object_identity,
        &fixture.project_root_uri,
    );

    let lsp_endpoint = EditorEndpoint::loopback(EndpointKind::LanguageServer, lsp_server.port());
    let lsp_receipt = registry
        .owned(EndpointKind::LanguageServer, &lsp_endpoint)
        .expect("the language-server endpoint is named by our own launch receipt");
    assert_eq!(lsp_receipt.launch_transaction_id, fixture.transaction_id);
    assert_eq!(lsp_receipt.pid, fixture.child_pid);
    assert_eq!(lsp_receipt.start_time, fixture.child_start_time);
    assert_eq!(lsp_receipt.binary_sha256, fixture.binary_sha256);
    assert_eq!(lsp_receipt.project_root_uri, fixture.project_root_uri);
    assert_eq!(lsp_receipt.project_generation, fixture.project_generation);
    assert_eq!(
        lsp_receipt.endpoint.port,
        lsp_server.port(),
        "the port comes from the recorded argv, never from Godot's 6005 default"
    );

    // -- 2. LSP: initialize, then diagnostics, then symbols. ------------------
    let mut connected = registry
        .connect(
            &gate,
            EndpointKind::LanguageServer,
            &lsp_endpoint,
            &observed(&lsp_receipt),
            0,
            Duration::from_secs(5),
        )
        .expect("the owned language-server endpoint connects");
    assert_eq!(
        connected.owned().launch_transaction_id(),
        fixture.transaction_id,
        "the session is bound to the exact owned launch"
    );
    assert_eq!(connected.permit().boundary(), EffectBoundary::LspDap);
    assert_eq!(
        connected.permit().project_generation(),
        fixture.project_generation
    );

    let document_uri = "file:///fixtures/adapters/editor-project/player.gd";
    let mut session = connected
        .lsp(Duration::from_secs(5))
        .expect("an lsp token opens an lsp session");
    session.initialize().expect("initialize completes");
    assert!(
        session.advertises("documentSymbolProvider"),
        "the server advertised documentSymbol"
    );
    assert!(
        !session.advertises("workspaceSymbolProvider"),
        "Godot advertises workspaceSymbolProvider=false"
    );
    assert!(
        !session.advertises("foldingRangeProvider"),
        "a handler exists but the capability is hardcoded false, so it is not usable"
    );

    session
        .did_open(
            document_uri,
            "extends Node\nfunc _ready() -> void:\n\tbad syntax here\n",
        )
        .expect("didOpen is a notification");
    let diagnostics = session
        .diagnostics_for(document_uri)
        .expect("Godot pushes diagnostics from parse_script via sync_script_content");
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].uri, document_uri);
    assert_eq!(diagnostics[0].severity, Some(1));
    assert_eq!(diagnostics[0].line, Some(6));
    assert_eq!(diagnostics[0].character, Some(8));
    assert!(
        diagnostics[0].message.contains("Parse Error"),
        "the diagnostic text is the engine's own: {:?}",
        diagnostics[0].message
    );
    assert!(
        session.native_symbols_announced(),
        "the coalesced gdscript/capabilities notification was absorbed on the way"
    );

    let symbols = session
        .document_symbols(document_uri)
        .expect("documentSymbol is bound");
    assert_eq!(symbols.len(), 2);
    assert_eq!(symbols[0].name, "Player");
    assert_eq!(symbols[0].kind, Some(5));
    assert_eq!(symbols[1].name, "_physics_process");
    assert_eq!(symbols[1].container.as_deref(), Some("Player"));
    assert_eq!(symbols[1].line, Some(11));

    let closing = session.close();
    assert!(
        closing.contains("closing the TCP connection"),
        "Godot binds neither shutdown nor exit: {closing}"
    );
    drop(connected);

    // -- 3. DAP: launch is pending until configurationDone, then a real stack. -
    let dap_endpoint = EditorEndpoint::loopback(EndpointKind::DebugAdapter, dap_server.port());
    let dap_receipt = registry
        .owned(EndpointKind::DebugAdapter, &dap_endpoint)
        .expect("the debug-adapter endpoint is named by the same launch receipt");
    let mut connected = registry
        .connect(
            &gate,
            EndpointKind::DebugAdapter,
            &dap_endpoint,
            &observed(&dap_receipt),
            0,
            Duration::from_secs(5),
        )
        .expect("the owned debug-adapter endpoint connects");
    let mut session = connected
        .dap(Duration::from_secs(5))
        .expect("a dap token opens a dap session");
    assert_eq!(
        session.deadline(),
        ENGINE_REQUEST_TIMEOUT,
        "a longer deadline is clamped: Godot answers TIMEOUT per message after request_timeout"
    );

    session.initialize().expect("initialize completes");
    let trusted = |name: &str| {
        session
            .capabilities()
            .iter()
            .find(|capability| capability.name == name)
            .map(|capability| (capability.advertised, capability.trusted))
    };
    assert_eq!(
        trusted("supportsConfigurationDoneRequest"),
        Some((true, true))
    );
    assert_eq!(
        trusted("supportsSetVariable"),
        Some((true, false)),
        "advertised in debug_adapter_types.h with no req_setVariable handler behind it"
    );

    assert_eq!(
        session.launch(false).expect("launch is accepted"),
        LaunchState::Pending,
        "req_launch stores the request in pending_launch and starts nothing"
    );
    assert_eq!(
        session
            .configuration_done()
            .expect("configurationDone runs the pending launch"),
        LaunchState::Started,
        "the scene starts only from configurationDone, via play_main_scene()"
    );

    let acknowledged = session
        .set_breakpoints("res://player.gd", &[12], false)
        .expect("a non-@tool script accepts breakpoints");
    assert_eq!(acknowledged.len(), 1);
    assert!(acknowledged[0].verified);
    assert_eq!(acknowledged[0].line, Some(12));

    let frames = session.stack_trace(1).expect("stackTrace is bound");
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].id, 1);
    assert_eq!(frames[0].name, "Player::_physics_process");
    assert_eq!(frames[0].source.as_deref(), Some("res://player.gd"));
    assert_eq!(frames[0].line, Some(12));

    let scopes = session.scopes(frames[0].id).expect("scopes is bound");
    assert_eq!(scopes, vec![("Locals".to_owned(), 100)]);
    let variables = session.variables(100).expect("variables is bound");
    assert_eq!(variables.len(), 1);
    assert_eq!(variables[0].name, "delta");
    assert_eq!(variables[0].type_name.as_deref(), Some("float"));
    session.continue_execution(1).expect("continue is bound");

    // -- 4. Both exchanges went through the gateway, on the LSP/DAP boundary. --
    drop(connected);
    assert_eq!(
        gate.dispatches(),
        2,
        "one permit per connection, and no effect without one"
    );
    assert_eq!(
        gate.boundaries(),
        vec![EffectBoundary::LspDap, EffectBoundary::LspDap]
    );
    assert_eq!(
        gate.tool_names(),
        vec![
            exchange_tool_name(EndpointKind::LanguageServer).to_owned(),
            exchange_tool_name(EndpointKind::DebugAdapter).to_owned(),
        ]
    );

    // -- 5. The engine tools all go through the same gateway. -----------------
    let tool_gate = RecordingGate::allowing();
    let tools = EngineTools::new(
        &tool_gate,
        &fixture.project_object_identity,
        fixture.project_generation,
    );
    for tool in [
        EngineTool::Validate,
        EngineTool::Test,
        EngineTool::Import,
        EngineTool::Run,
        EngineTool::Uid,
    ] {
        let outcome = tools
            .run_supervised(tool, "fixture invocation", &mut |permit| {
                assert_eq!(permit.tool_name(), tool.tool_name());
                assert_eq!(permit.boundary(), tool.boundary());
                Ok((
                    classify(
                        &Observed {
                            exit_code: Some(0),
                            signal: None,
                            timed_out: false,
                        },
                        &Evidence::ParsedClean,
                        &ExitAssertion::exactly(0),
                    ),
                    vec!["--headless".to_owned()],
                ))
            })
            .expect("a permitted tool runs");
        assert_eq!(outcome.tool, tool);
        assert_eq!(outcome.verdict.classification, Classification::Passed);
        assert!(outcome.is_success());
    }
    let stopped = tools
        .stop(
            &StopRequest {
                launch_transaction_id: fixture.transaction_id.clone(),
                deadline: Duration::from_secs(5),
            },
            &mut |_permit, transaction_id, _deadline| {
                assert_eq!(
                    transaction_id, fixture.transaction_id,
                    "stop names a launch transaction, never a caller-supplied PID"
                );
                Ok(classify(
                    &Observed {
                        exit_code: Some(0),
                        signal: None,
                        timed_out: false,
                    },
                    &Evidence::ParsedClean,
                    &ExitAssertion::exactly(0),
                ))
            },
        )
        .expect("stop is permitted");
    assert_eq!(stopped.tool, EngineTool::Stop);
    assert_eq!(
        tool_gate.dispatches(),
        6,
        "all six engine tools consult the gateway"
    );
    assert_eq!(
        tool_gate
            .boundaries()
            .iter()
            .filter(|boundary| **boundary == EffectBoundary::File)
            .count(),
        1,
        "only the UID tool is a file effect"
    );

    // -- 6. A reconnect re-verifies the whole tuple, not a cached answer. -----
    let reconnect_server = FakeProtocolServer::start(
        Script::Replies(vec![Reply::whole(lsp_initialize_result(1))]),
        1,
    );
    let reconnect_committed = journal_editor_launch(
        &journal,
        &FixtureEditorReceipt {
            transaction_id: uuid(1802),
            ..receipt_fixture()
        },
        reconnect_server.port(),
        reconnect_server.port() ^ 1,
        fixture.project_generation,
    );
    assert_eq!(reconnect_committed.phase, LaunchPhase::CommittedFsynced);
    let reconnect_endpoint =
        EditorEndpoint::loopback(EndpointKind::LanguageServer, reconnect_server.port());
    let reconnect_receipt = registry
        .owned(EndpointKind::LanguageServer, &reconnect_endpoint)
        .expect("the reconnect endpoint is owned too");
    let reconnected = registry
        .reconnect(
            &gate,
            EndpointKind::LanguageServer,
            &reconnect_endpoint,
            &observed(&reconnect_receipt),
            2,
            Duration::from_secs(5),
        )
        .expect("a reconnect that still matches its receipt succeeds");
    assert_eq!(
        reconnected.owned().verified_at_reconnect(),
        3,
        "the reconnect counter proves the tuple check was re-run, not reused"
    );
}

// ---------------------------------------------------------------------------
// Failure paths
// ---------------------------------------------------------------------------

#[test]
fn lsp_dap_adapters_failure_paths() {
    let _serial = one_at_a_time();
    let fixture = receipt_fixture();
    let root = tempfile::tempdir().expect("scenario tempdir");
    let journal = DurableTestJournal::open(root.path());

    let real_server = FakeProtocolServer::start(
        Script::Replies(vec![Reply::whole(lsp_initialize_result(1))]),
        1,
    );
    journal_editor_launch(
        &journal,
        &fixture,
        real_server.port(),
        real_server.port() ^ 1,
        fixture.project_generation,
    );

    let gate = RecordingGate::allowing();
    let admission = ClientAdmission::new();
    let registry = EndpointRegistry::new(
        &journal,
        admission.clone(),
        &fixture.project_object_identity,
        &fixture.project_root_uri,
    );
    let owned_endpoint = EditorEndpoint::loopback(EndpointKind::LanguageServer, real_server.port());
    let receipt = registry
        .owned(EndpointKind::LanguageServer, &owned_endpoint)
        .expect("the fixture endpoint is owned");

    // -- 1. A spoofed loopback listener. --------------------------------------
    //       Something really is listening here, and it is really on loopback. It
    //       is refused solely because no launch of ours names it.
    let spoofed_server = FakeProtocolServer::start(
        Script::Replies(vec![Reply::whole(lsp_initialize_result(1))]),
        1,
    );
    let spoofed_endpoint =
        EditorEndpoint::loopback(EndpointKind::LanguageServer, spoofed_server.port());
    let failure = registry
        .connect(
            &gate,
            EndpointKind::LanguageServer,
            &spoofed_endpoint,
            &ObservedEndpoint {
                endpoint: spoofed_endpoint.clone(),
                ..observed(&receipt)
            },
            0,
            Duration::from_secs(2),
        )
        .expect_err("a listener we did not start is refused");
    assert!(
        matches!(
            failure,
            AdapterError::Ownership(OwnershipFailure::NotOwned { .. })
        ),
        "{failure}"
    );
    assert_eq!(failure.error_kind(), ErrorKind::PermissionDenied);
    assert_eq!(
        gate.dispatches(),
        0,
        "a spoofed endpoint never reaches the gateway, so no effect is dispatched"
    );
    assert_eq!(
        spoofed_server.accepted(),
        0,
        "the refusal precedes the dial, so the spoofed server saw no connection"
    );

    // -- 2. A stale endpoint: PID reuse, then each other tuple element. -------
    let reused = ObservedEndpoint {
        start_time: Some("99999999".to_owned()),
        ..observed(&receipt)
    };
    let failure = registry
        .connect(
            &gate,
            EndpointKind::LanguageServer,
            &owned_endpoint,
            &reused,
            1,
            Duration::from_secs(2),
        )
        .expect_err("PID reuse is not continuity");
    assert!(
        matches!(
            failure,
            AdapterError::Ownership(OwnershipFailure::PidReused { .. })
        ),
        "{failure}"
    );
    assert_eq!(failure.error_kind(), ErrorKind::PermissionDenied);

    for (label, stale, expected) in [
        (
            "the recorded process is gone",
            ObservedEndpoint {
                pid: None,
                ..observed(&receipt)
            },
            ErrorKind::Unavailable,
        ),
        (
            "the engine binary was swapped",
            ObservedEndpoint {
                binary_sha256: "c".repeat(64),
                ..observed(&receipt)
            },
            ErrorKind::Conflict,
        ),
        (
            "the project root moved",
            ObservedEndpoint {
                project_root_uri: "file:///elsewhere".to_owned(),
                ..observed(&receipt)
            },
            ErrorKind::Conflict,
        ),
        (
            "the project generation advanced",
            ObservedEndpoint {
                project_generation: fixture.project_generation + 1,
                ..observed(&receipt)
            },
            ErrorKind::Conflict,
        ),
        (
            "the endpoint is not loopback",
            ObservedEndpoint {
                endpoint: EditorEndpoint {
                    kind: EndpointKind::LanguageServer,
                    host: "203.0.113.7".to_owned(),
                    port: real_server.port(),
                },
                ..observed(&receipt)
            },
            ErrorKind::PermissionDenied,
        ),
    ] {
        match registry.connect(
            &gate,
            EndpointKind::LanguageServer,
            &owned_endpoint,
            &stale,
            1,
            Duration::from_secs(2),
        ) {
            Ok(connected) => panic!("{label} must be refused, but {connected:?} connected"),
            Err(failure) => assert_eq!(
                failure.error_kind(),
                expected,
                "{label}: unexpected kind for {failure}"
            ),
        }
    }
    assert_eq!(
        gate.dispatches(),
        0,
        "no stale-tuple refusal reaches the gateway"
    );

    // -- 3. The ninth client is refused, and the eight survivors are intact. --
    let mut tickets = Vec::new();
    for _ in 0..MAX_CLIENTS {
        tickets.push(
            admission
                .admit(EndpointKind::LanguageServer)
                .expect("within the engine's LSP_MAX_CLIENTS ceiling"),
        );
    }
    let failure = registry
        .connect(
            &gate,
            EndpointKind::LanguageServer,
            &owned_endpoint,
            &observed(&receipt),
            0,
            Duration::from_secs(2),
        )
        .expect_err("the ninth client");
    assert!(
        matches!(
            failure,
            AdapterError::Admission(AdmissionError::AtCapacity { .. })
        ),
        "{failure}"
    );
    assert_eq!(failure.error_kind(), ErrorKind::Unavailable);
    assert_eq!(
        gate.dispatches(),
        0,
        "the ceiling is enforced before the gateway is consulted, so no permit is spent"
    );
    assert_eq!(
        admission.live(EndpointKind::LanguageServer),
        Ok(MAX_CLIENTS),
        "the refusal did not disturb the admitted clients"
    );
    assert_eq!(
        real_server.accepted(),
        0,
        "the refusal precedes the dial, so the owned server saw no ninth connection"
    );
    tickets.clear();

    // -- 4. A frame over the engine's 4 MiB ceiling is refused, and the stream
    //       is abandoned rather than resynchronised onto refused bytes. -------
    let oversize_server = FakeProtocolServer::start(
        Script::Malformed(Malformed::OversizeDeclaration(MAX_PAYLOAD_BYTES as u64 + 1)),
        1,
    );
    let oversize_record = FixtureEditorReceipt {
        transaction_id: uuid(1811),
        ..receipt_fixture()
    };
    journal_editor_launch(
        &journal,
        &oversize_record,
        oversize_server.port(),
        oversize_server.port() ^ 1,
        fixture.project_generation,
    );
    let oversize_endpoint =
        EditorEndpoint::loopback(EndpointKind::LanguageServer, oversize_server.port());
    let oversize_receipt = registry
        .owned(EndpointKind::LanguageServer, &oversize_endpoint)
        .expect("the oversize fixture endpoint is owned");
    let mut connected = registry
        .connect(
            &gate,
            EndpointKind::LanguageServer,
            &oversize_endpoint,
            &observed(&oversize_receipt),
            0,
            Duration::from_secs(2),
        )
        .expect("the endpoint itself is legitimate; the peer's framing is not");
    let mut session = connected
        .lsp(Duration::from_secs(2))
        .expect("the session opens");
    let failure = session
        .initialize()
        .expect_err("a header declaring more than 4 MiB is refused");
    assert!(matches!(failure, AdapterError::Codec(_)), "{failure}");
    assert_eq!(failure.error_kind(), ErrorKind::Unavailable);
    drop(connected);

    // The same refusal, asserted directly on the codec so the reason is explicit.
    let declared = MAX_PAYLOAD_BYTES as u64 + 1;
    let header = format!("Content-Length: {declared}\r\n\r\n");
    let mut reader = FrameReader::new(std::io::Cursor::new(header.into_bytes()));
    assert_eq!(
        reader.read_frame(),
        Err(CodecError::PayloadTooLarge { declared })
    );
    assert!(
        reader.is_abandoned(),
        "the refused bytes are never skipped over"
    );
    assert_eq!(reader.read_frame(), Err(CodecError::Abandoned));
    assert!(
        encode_frame(&vec![0_u8; MAX_PAYLOAD_BYTES + 1]).is_err(),
        "an oversize payload is never written either"
    );

    // -- 5. A malformed header, and a header that never terminates. -----------
    let bad_header_server = FakeProtocolServer::start(Script::Malformed(Malformed::BadHeader), 1);
    let bad_record = FixtureEditorReceipt {
        transaction_id: uuid(1812),
        ..receipt_fixture()
    };
    journal_editor_launch(
        &journal,
        &bad_record,
        bad_header_server.port(),
        bad_header_server.port() ^ 1,
        fixture.project_generation,
    );
    let bad_endpoint =
        EditorEndpoint::loopback(EndpointKind::LanguageServer, bad_header_server.port());
    let bad_receipt = registry
        .owned(EndpointKind::LanguageServer, &bad_endpoint)
        .expect("owned");
    let mut connected = registry
        .connect(
            &gate,
            EndpointKind::LanguageServer,
            &bad_endpoint,
            &observed(&bad_receipt),
            0,
            Duration::from_secs(2),
        )
        .expect("the endpoint is owned");
    let mut session = connected
        .lsp(Duration::from_secs(2))
        .expect("the session opens");
    let failure = session.initialize().expect_err("a malformed header");
    assert!(matches!(failure, AdapterError::Codec(_)), "{failure}");
    drop(connected);

    let noise = vec![b'x'; MAX_HEADER_BYTES + 64];
    let mut reader = FrameReader::new(std::io::Cursor::new(noise));
    assert_eq!(reader.read_frame(), Err(CodecError::HeaderTooLarge));

    // -- 6. Nothing is hosting the protocol at all. ---------------------------
    let closed = closed_loopback_address();
    let closed_record = FixtureEditorReceipt {
        transaction_id: uuid(1813),
        ..receipt_fixture()
    };
    journal_editor_launch(
        &journal,
        &closed_record,
        closed.port(),
        closed.port() ^ 1,
        fixture.project_generation,
    );
    let closed_endpoint = EditorEndpoint::loopback(EndpointKind::LanguageServer, closed.port());
    let closed_receipt = registry
        .owned(EndpointKind::LanguageServer, &closed_endpoint)
        .expect("owned");
    let failure = registry
        .connect(
            &gate,
            EndpointKind::LanguageServer,
            &closed_endpoint,
            &observed(&closed_receipt),
            0,
            Duration::from_millis(500),
        )
        .expect_err("no editor is hosting this");
    assert_eq!(
        failure.error_kind(),
        ErrorKind::Unavailable,
        "an absent host is unavailable, never unsupported: {failure}"
    );

    // -- 7. Unsupported methods fall back honestly. ---------------------------
    for (method, _detail) in UNBOUND_METHODS {
        let failure = LspSession::ensure_bound(method).expect_err("Godot binds none of these");
        assert_eq!(failure.error_kind(), ErrorKind::Unsupported, "{method}");
    }
    for (request, _detail) in UNIMPLEMENTED_REQUESTS {
        let failure = DapAdapter::unimplemented(request);
        assert!(failure.is_some(), "{request} is refused up front");
    }
    for request in IMPLEMENTED_REQUESTS {
        assert!(
            DapAdapter::implemented(request),
            "{request} is one of the 19 bound handlers"
        );
    }
    assert!(
        DapAdapter::unimplemented("stepOut").is_some(),
        "there is no req_stepOut handler, and an unknown command gets no response at all"
    );

    // -- 8. `@tool` breakpoints are refused, never claimed. -------------------
    let tool_server = FakeProtocolServer::start(
        Script::Replies(vec![
            Reply::whole(dap_response(1, 1, "initialize", dap_initialize_body())),
            Reply::whole(dap_response(2, 2, "launch", Value::Null)),
            Reply::whole(dap_response(3, 3, "configurationDone", Value::Null)),
        ]),
        1,
    );
    let tool_record = FixtureEditorReceipt {
        transaction_id: uuid(1814),
        ..receipt_fixture()
    };
    journal_editor_launch(
        &journal,
        &tool_record,
        tool_server.port() ^ 1,
        tool_server.port(),
        fixture.project_generation,
    );
    let tool_endpoint = EditorEndpoint::loopback(EndpointKind::DebugAdapter, tool_server.port());
    let tool_receipt = registry
        .owned(EndpointKind::DebugAdapter, &tool_endpoint)
        .expect("owned");
    let mut connected = registry
        .connect(
            &gate,
            EndpointKind::DebugAdapter,
            &tool_endpoint,
            &observed(&tool_receipt),
            0,
            Duration::from_secs(2),
        )
        .expect("the endpoint is owned");
    let mut session = connected
        .dap(Duration::from_secs(2))
        .expect("the session opens");
    session.initialize().expect("initialize");
    session.launch(false).expect("launch is pending");
    session
        .configuration_done()
        .expect("configurationDone starts the scene");
    let failure = session
        .set_breakpoints("res://addons/my_plugin/tool.gd", &[9], true)
        .expect_err("a @tool script");
    assert_eq!(failure.error_kind(), ErrorKind::Unsupported);
    assert!(
        failure
            .to_string()
            .contains("cannot break in `@tool` scripts"),
        "{failure}"
    );
    assert!(session.step_out().is_err(), "stepOut has no handler");
    assert!(
        session
            .set_function_breakpoints(&["_ready".to_owned()])
            .is_err(),
        "setFunctionBreakpoints has no handler"
    );
    assert!(
        session
            .set_exception_breakpoints(&["all".to_owned()])
            .is_err(),
        "setExceptionBreakpoints has no handler"
    );
    drop(connected);

    // -- 9. A refused permit dispatches nothing at all. ----------------------
    let refusing = RecordingGate::refusing("the authority epoch is stale");
    let refused = registry
        .connect(
            &refusing,
            EndpointKind::LanguageServer,
            &owned_endpoint,
            &observed(&receipt),
            0,
            Duration::from_secs(2),
        )
        .expect_err("a refused permit");
    assert!(
        matches!(refused, AdapterError::PermitRefused { .. }),
        "{refused}"
    );
    assert_eq!(refused.error_kind(), ErrorKind::PermissionDenied);
    assert_eq!(
        real_server.accepted(),
        0,
        "the permit is obtained before the dial, so a refusal never opens a socket"
    );

    let refusing_tools = EngineTools::new(
        &refusing,
        &fixture.project_object_identity,
        fixture.project_generation,
    );
    let mut invoked = 0_u32;
    let refused = refusing_tools
        .run_supervised(EngineTool::Import, "import", &mut |_permit| {
            invoked += 1;
            Ok((
                classify(
                    &Observed {
                        exit_code: Some(0),
                        signal: None,
                        timed_out: false,
                    },
                    &Evidence::ParsedClean,
                    &ExitAssertion::exactly(0),
                ),
                Vec::new(),
            ))
        })
        .expect_err("a refused permit");
    assert!(
        matches!(refused, AdapterError::PermitRefused { .. }),
        "{refused}"
    );
    assert_eq!(invoked, 0, "the effect body never ran");

    // -- 10. Every refusal above left the service usable. --------------------
    let mut connected = registry
        .connect(
            &gate,
            EndpointKind::LanguageServer,
            &owned_endpoint,
            &observed(&receipt),
            0,
            Duration::from_secs(2),
        )
        .expect("the owned endpoint still connects after every refusal");
    let mut session = connected
        .lsp(Duration::from_secs(2))
        .expect("the session opens");
    session
        .initialize()
        .expect("the service survived every refused request");
    assert!(session.advertises("documentSymbolProvider"));
}

fn dap_event(seq: i64, event: &str, body: Value) -> Value {
    json!({ "seq": seq, "type": "event", "event": event, "body": body })
}

fn dap_failure(seq: i64, request_seq: i64, command: &str) -> Value {
    json!({
        "seq": seq,
        "type": "response",
        "request_seq": request_seq,
        "success": false,
        "command": command,
        "message": "unknown",
        "body": { "error": { "id": 0, "format": "An unknown error has occurred when processing the request." } },
    })
}

/// What a real Godot 4.7.1 editor does, observed over its debug adapter: the
/// `launch` request is answered only after `configurationDone`, behind a
/// `process` event; a breakpoint stop arrives as a `stopped` event; and a
/// scope's variables can be refused once while the editor is still fetching
/// them from the game.
#[test]
fn a_debug_session_follows_godots_own_launch_stop_and_variables_ordering() {
    let _serial = one_at_a_time();
    let fixture = receipt_fixture();
    let root = tempfile::tempdir().expect("scenario tempdir");
    let journal = DurableTestJournal::open(root.path());
    let dap_server = FakeProtocolServer::start(
        Script::Replies(vec![
            // initialize -> response, then the `initialized` event.
            Reply::coalesced(dap_response(1, 1, "initialize", dap_initialize_body()), 2),
            Reply::whole(dap_event(2, "initialized", json!({}))),
            // launch -> nothing until configurationDone.
            Reply::withheld(),
            // configurationDone -> its response, the process event, the deferred
            // launch response and the breakpoint stop, in one burst.
            Reply::coalesced(dap_response(3, 3, "configurationDone", Value::Null), 4),
            Reply::whole(dap_event(4, "process", json!({ "startMethod": "launch" }))),
            Reply::whole(dap_response(5, 2, "launch", Value::Null)),
            Reply::whole(dap_event(
                6,
                "stopped",
                json!({ "reason": "breakpoint", "threadId": 1 }),
            )),
            // variables -> refused once, then answered.
            Reply::whole(dap_failure(7, 4, "variables")),
            Reply::whole(dap_response(
                8,
                5,
                "variables",
                json!({ "variables": [{ "name": "local_value", "value": "41", "variablesReference": 0 }] }),
            )),
            // pause -> its response and the pause stop.
            Reply::coalesced(dap_response(9, 6, "pause", Value::Null), 2),
            Reply::whole(dap_event(
                10,
                "stopped",
                json!({ "reason": "paused", "threadId": 1 }),
            )),
        ]),
        1,
    );
    journal_editor_launch(
        &journal,
        &fixture,
        1,
        dap_server.port(),
        fixture.project_generation,
    );
    let gate = RecordingGate::allowing();
    let registry = EndpointRegistry::new(
        &journal,
        ClientAdmission::new(),
        &fixture.project_object_identity,
        &fixture.project_root_uri,
    );
    let endpoint = EditorEndpoint::loopback(EndpointKind::DebugAdapter, dap_server.port());
    let receipt = registry
        .owned(EndpointKind::DebugAdapter, &endpoint)
        .expect("owned endpoint");
    let mut connected = registry
        .connect(
            &gate,
            EndpointKind::DebugAdapter,
            &endpoint,
            &observed(&receipt),
            0,
            Duration::from_secs(5),
        )
        .expect("connects");
    let mut session = connected.dap(Duration::from_secs(5)).expect("dap session");
    session.initialize().expect("initialize");
    assert_eq!(
        session.launch(false).expect("launch is sent"),
        LaunchState::Pending,
        "nothing answers a launch before configurationDone"
    );
    assert_eq!(
        session
            .configuration_done()
            .expect("the launch completes behind configurationDone"),
        LaunchState::Started
    );
    let stop = session
        .wait_for_event("stopped", Duration::from_secs(5))
        .expect("the breakpoint stop was observed");
    assert_eq!(stop["body"]["reason"], json!("breakpoint"));
    let variables = session
        .variables(1)
        .expect("a scope's variables arrive once the editor has them");
    assert_eq!(variables[0].name, "local_value");
    assert_eq!(variables[0].value, "41");
    session.pause(1).expect("pause is bound");
    let paused = session
        .wait_for_event("stopped", Duration::from_secs(5))
        .expect("the pause stop was observed");
    assert_eq!(paused["body"]["reason"], json!("paused"));
}

/// Godot announces the workspace it hosts with `gdscript_client/changeWorkspace`
/// whenever the client's `rootPath` is not exactly its own spelling. Naming the
/// held project is a confirmation; naming any other project is refused rather
/// than followed.
#[test]
fn a_workspace_announcement_is_accepted_only_for_the_held_project() {
    let _serial = one_at_a_time();
    let fixture = receipt_fixture();
    let held_path = fixture
        .project_root_uri
        .strip_prefix("file://")
        .expect("a file URI")
        .to_owned();
    for (announced, accepted) in [
        (held_path.clone(), true),
        ("/elsewhere/other-project".to_owned(), false),
    ] {
        let root = tempfile::tempdir().expect("scenario tempdir");
        let journal = DurableTestJournal::open(root.path());
        let lsp_server = FakeProtocolServer::start(
            Script::Replies(vec![
                Reply::coalesced(
                    json!({
                        "jsonrpc": "2.0",
                        "method": "gdscript_client/changeWorkspace",
                        "params": { "path": announced },
                    }),
                    2,
                ),
                Reply::whole(lsp_initialize_result(1)),
            ]),
            1,
        );
        journal_editor_launch(
            &journal,
            &fixture,
            lsp_server.port(),
            1,
            fixture.project_generation,
        );
        let gate = RecordingGate::allowing();
        let registry = EndpointRegistry::new(
            &journal,
            ClientAdmission::new(),
            &fixture.project_object_identity,
            &fixture.project_root_uri,
        );
        let endpoint = EditorEndpoint::loopback(EndpointKind::LanguageServer, lsp_server.port());
        let receipt = registry
            .owned(EndpointKind::LanguageServer, &endpoint)
            .expect("owned endpoint");
        let mut connected = registry
            .connect(
                &gate,
                EndpointKind::LanguageServer,
                &endpoint,
                &observed(&receipt),
                0,
                Duration::from_secs(5),
            )
            .expect("connects");
        let mut session = connected.lsp(Duration::from_secs(5)).expect("lsp session");
        let initialized = session.initialize();
        assert_eq!(
            initialized.is_ok(),
            accepted,
            "{announced}: {initialized:?}"
        );
    }
}
