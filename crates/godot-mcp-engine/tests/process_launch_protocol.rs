//! Todo 14 acceptance suite: bounded cross-platform process supervision with a
//! gapless launch protocol.
//!
//! The suite never needs a real Godot installation and never touches the
//! network. Every supervised process is a committed fake under
//! `tests/fixtures/launch/`, and the containment boundary is either the
//! committed container/VM adapter fixture (a *strong* boundary) or the real
//! trusted-Unix weak locator protocol driven against the bundled
//! `godot-mcp-supervisor` binary.
//!
//! Two tests are named by `qa/tasks/14.json` and are the only entry points the
//! QA descriptor invokes:
//!
//! * `process_launch_protocol_happy_path`
//! * `process_launch_protocol_failure_paths`

// Every launch here goes through the committed `/bin/sh` container adapter
// fixture or the trusted-Unix locator, neither of which exists on Windows; the
// Windows Job Object path is exercised by `job_object_boundary.rs` and by the
// real-client matrix instead.
#![cfg(unix)]
#![allow(
    clippy::too_many_lines,
    reason = "acceptance suites assert one whole protocol end to end"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use godot_mcp_engine::supervision::adapter::ContainerAdapterProvider;
use godot_mcp_engine::supervision::boundary::{
    BoundaryProvider, BoundaryStrength, TrustLevel, WEAK_RESIDUAL_RISKS, select_provider,
};
use godot_mcp_engine::supervision::cgroup::CgroupV2Provider;
use godot_mcp_engine::supervision::gate::LaunchPaths;
use godot_mcp_engine::supervision::journal::{
    BoundaryMembership, Disposition, JournalError, LAUNCH_ABANDONED_PRE_RELEASE, LaunchJournal,
    LaunchPhase, LaunchRecord, OperationFamily, ProcessLaunchProof, ProjectIdentityClaim,
    ReleaseStatus, TransactionEnvelope, TransportMode, canonical_terminal_value, hex_sha256,
    validate_canonical,
};
use godot_mcp_engine::supervision::procfs;
use godot_mcp_engine::supervision::protocol::{
    CrashStage, EnvironmentAllowlist, FaultInjection, FaultMode, LaunchError, LaunchEvent,
    LaunchObserver, LaunchProtocol, LaunchRequest, LaunchView, OutputCaps, TerminationKind,
};
use godot_mcp_engine::supervision::reconcile::{
    ReconcileCase, ReconciledLaunch, mutation_block, reconcile_launches,
};
use godot_mcp_engine::supervision::weak_unix::UnixLocatorProvider;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the engine crate lives at <repo>/crates/godot-mcp-engine")
        .to_path_buf()
}

/// Copy one committed launch fixture into `dir`, returning the executable path.
fn install_fixture(dir: &Path, name: &str) -> PathBuf {
    let source = repo_root().join("tests/fixtures/launch").join(name);
    let bytes = std::fs::read(&source).unwrap_or_else(|e| panic!("read {}: {e}", source.display()));
    let target = dir.join(name);
    std::fs::write(&target, &bytes).expect("write the launch fixture");
    make_executable(&target);
    settle_executable(&target);
    target
}

/// Wait until `path` can be executed. A child another test thread forks while
/// the copy is still open for writing inherits that descriptor until it execs,
/// and exec'ing a file some process holds open for writing fails with
/// `ETXTBSY`. The copy is closed by now, so no new writer can appear: one
/// successful exec (both fixtures exit at once without arguments) proves every
/// later one succeeds too.
fn settle_executable(path: &Path) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match std::process::Command::new(path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
        {
            Ok(_) => return,
            Err(error)
                if error.kind() == std::io::ErrorKind::ExecutableFileBusy
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(error) => panic!("execute the launch fixture {}: {error}", path.display()),
        }
    }
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(path).expect("stat").permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).expect("chmod");
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) {}

/// The bundled supervisor/barrier binary, located by cargo.
fn supervisor_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_godot-mcp-supervisor"))
}

// ---------------------------------------------------------------------------
// A durable stand-in for Todo 6's `TransactionStore`.
//
// This is NOT a second journal and NOT a second schema: it stores exactly the
// canonical record value produced by `LaunchRecord::to_canonical_value()`,
// validated against the repository's canonical schema
// (`schemas/transactions/v1.json`) on the way in *and* on the way out, plus the
// staged payload bytes keyed by their canonical staging ID — the same split
// Todo 6's SQLite store uses (`transactions` + `staged_bytes`). The engine crate
// may not import the server crate, so the acceptance suite injects this adapter
// where production injects Todo 6's store.
// ---------------------------------------------------------------------------

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

    fn record_path(&self, transaction_id: &str) -> PathBuf {
        self.dir
            .join("transactions")
            .join(format!("{transaction_id}.json"))
    }

    fn fsync_dir(path: &Path) {
        if let Ok(dir) = std::fs::File::open(path) {
            let _ = dir.sync_all();
        }
    }

    fn store(&self, value: &serde_json::Value, record: &LaunchRecord) -> Result<(), JournalError> {
        validate_canonical(value)?;
        for payload in &record.payloads {
            let staged = self.dir.join("staged").join(payload.staging_id());
            std::fs::write(&staged, &payload.bytes).map_err(|error| JournalError::Durability {
                phase: record.phase,
                reason: error.to_string(),
            })?;
            if let Ok(file) = std::fs::File::open(&staged) {
                let _ = file.sync_all();
            }
        }
        Self::fsync_dir(&self.dir.join("staged"));
        let path = self.record_path(&record.envelope.transaction_id);
        let bytes = serde_json::to_vec(value).map_err(|error| JournalError::Durability {
            phase: record.phase,
            reason: error.to_string(),
        })?;
        std::fs::write(&path, &bytes).map_err(|error| JournalError::Durability {
            phase: record.phase,
            reason: error.to_string(),
        })?;
        if let Ok(file) = std::fs::File::open(&path) {
            let _ = file.sync_all();
        }
        Self::fsync_dir(&self.dir.join("transactions"));
        Ok(())
    }

    fn stored_values(&self) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(self.dir.join("transactions")) else {
            return out;
        };
        let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
        paths.sort();
        for path in paths {
            if let Ok(bytes) = std::fs::read(&path)
                && let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes)
            {
                out.push(value);
            }
        }
        out
    }

    fn stored(&self, transaction_id: &str) -> Option<serde_json::Value> {
        let bytes = std::fs::read(self.record_path(transaction_id)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    fn records_at(&self, accept: &dyn Fn(&str) -> bool) -> Result<Vec<LaunchRecord>, JournalError> {
        let mut out = Vec::new();
        for value in self.stored_values() {
            if !accept(value["phase"].as_str().unwrap_or_default()) {
                continue;
            }
            out.push(LaunchRecord::from_canonical_value(&value, &|staging_id| {
                std::fs::read(self.dir.join("staged").join(staging_id)).ok()
            })?);
        }
        Ok(out)
    }

    fn phase_of(&self, transaction_id: &str) -> Option<String> {
        self.stored(transaction_id)?
            .get("phase")
            .and_then(|phase| phase.as_str())
            .map(str::to_owned)
    }

    fn terminal_reason(&self, transaction_id: &str) -> Option<String> {
        self.stored(transaction_id)?
            .get("terminal_reason")
            .and_then(|reason| reason.as_str())
            .map(str::to_owned)
    }

    /// Overwrite one durable staged payload document, simulating a PID/start-time
    /// mismatch discovered at reconciliation time (PID reuse).
    fn tamper_payload(
        &self,
        transaction_id: &str,
        name: &str,
        key: &str,
        value: serde_json::Value,
    ) {
        let stored = self.stored(transaction_id).expect("record exists");
        let staged = stored["staged_write_set"]
            .as_array()
            .expect("staged array")
            .iter()
            .find(|entry| entry["path"] == serde_json::json!(name))
            .expect("named payload")
            .clone();
        let staging_id = staged["staging_id"]
            .as_str()
            .expect("staging id")
            .to_owned();
        let path = self.dir.join("staged").join(&staging_id);
        let bytes = std::fs::read(&path).expect("payload bytes");
        let mut facts: BTreeMap<String, serde_json::Value> =
            serde_json::from_slice(&bytes).expect("payload object");
        facts.insert(key.to_owned(), value);
        let rewritten = serde_json::to_vec(&serde_json::Value::Object(facts.into_iter().collect()))
            .expect("payload bytes");
        std::fs::write(&path, &rewritten).expect("rewrite payload");
        let _ = transaction_id;
    }
}

impl LaunchJournal for DurableTestJournal {
    fn write_phase(&self, record: &LaunchRecord) -> Result<(), JournalError> {
        let value = record.to_canonical_value()?;
        self.store(&value, record)
    }

    fn settle(
        &self,
        record: &LaunchRecord,
        disposition: Disposition,
        reason: &str,
        proof: Option<&ProcessLaunchProof>,
    ) -> Result<(), JournalError> {
        let value = canonical_terminal_value(record, disposition, reason, proof)?;
        self.store(&value, record)
    }

    fn open_launches(&self) -> Result<Vec<LaunchRecord>, JournalError> {
        // Todo 6's phase machine makes `committed_fsynced` terminal, so an open
        // transaction is one sitting at one of the three earlier marks.
        self.records_at(&|phase| {
            LaunchPhase::ALL
                .iter()
                .any(|known| known.as_str() == phase && *known != LaunchPhase::CommittedFsynced)
        })
    }

    fn committed_launches(&self) -> Result<Vec<LaunchRecord>, JournalError> {
        self.records_at(&|phase| phase == LaunchPhase::CommittedFsynced.as_str())
    }

    fn quarantined(&self) -> Result<Vec<String>, JournalError> {
        Ok(self
            .stored_values()
            .into_iter()
            .filter(|value| value["phase"] == serde_json::json!("quarantined"))
            .filter_map(|value| {
                value["transaction_id"]
                    .as_str()
                    .map(std::borrow::ToOwned::to_owned)
            })
            .collect())
    }
}

// ---------------------------------------------------------------------------
// Observation
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Recorder {
    events: Mutex<Vec<String>>,
    /// What the world looked like at each stage the protocol announced.
    stage_notes: Mutex<Vec<String>>,
}

impl Recorder {
    fn events(&self) -> Vec<String> {
        self.events.lock().expect("recorder").clone()
    }

    fn notes(&self) -> Vec<String> {
        self.stage_notes.lock().expect("recorder").clone()
    }
}

impl LaunchObserver for Recorder {
    fn event(&self, event: &LaunchEvent) {
        self.events
            .lock()
            .expect("recorder")
            .push(event.label().to_owned());
    }

    fn stage(&self, stage: CrashStage, view: &LaunchView<'_>) {
        // At `AfterReceiptBeforeRelease` the world must show: a live child, a
        // child that has NOT exec'd user code yet, and no side effect on disk.
        if stage == CrashStage::AfterReceiptBeforeRelease {
            let pid = view
                .child_pid
                .expect("a child exists once the receipt is durable");
            let identity = procfs::process_identity(pid)
                .expect("the child is inspectable")
                .expect("the child is live");
            let blocked = identity.is_gated_barrier();
            let evidence = view
                .evidence_path
                .map(|path| path.exists())
                .unwrap_or(false);
            self.stage_notes.lock().expect("recorder").push(format!(
                "receipt-durable: live={} gated={blocked} side_effects={evidence}",
                identity.pid
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// Request construction
// ---------------------------------------------------------------------------

fn envelope(suffix: &str) -> TransactionEnvelope {
    TransactionEnvelope {
        transaction_id: format!("00000000-0000-4000-8000-{suffix:0>12}"),
        project: ProjectIdentityClaim {
            object_identity: format!("project_{}", "a".repeat(64)),
            root_object: "unix:dev=1,ino=2".to_owned(),
            file_object: "unix:dev=1,ino=3".to_owned(),
            generation: 7,
        },
        transport_mode: TransportMode::Stdio,
        principal: None,
        session: None,
        operation_family: OperationFamily::RuntimeExecution,
        tool_name: "process.launch".to_owned(),
        idempotency_id: format!("idem-{suffix}"),
        canonical_request_hash: "b".repeat(64),
        canonical_request_bytes: 42,
        opened_at_unix_ms: 1_700_000_000_000,
    }
}

struct Scenario {
    _root: tempfile::TempDir,
    journal_dir: PathBuf,
    work_dir: PathBuf,
    boundary_root: PathBuf,
    adapter: PathBuf,
    helper: PathBuf,
    state_dir: PathBuf,
}

impl Scenario {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("scenario tempdir");
        let base = root.path().to_path_buf();
        let bin = base.join("bin");
        std::fs::create_dir_all(&bin).expect("bin dir");
        let adapter = install_fixture(&bin, "container-adapter.sh");
        let helper = install_fixture(&bin, "gated-helper.sh");
        let journal_dir = base.join("journal");
        let work_dir = base.join("work");
        let boundary_root = base.join("boundaries");
        let state_dir = base.join("state");
        for dir in [&journal_dir, &work_dir, &boundary_root, &state_dir] {
            std::fs::create_dir_all(dir).expect("scenario dir");
        }
        Self {
            _root: root,
            journal_dir,
            work_dir,
            boundary_root,
            adapter,
            helper,
            state_dir,
        }
    }

    fn journal(&self) -> DurableTestJournal {
        DurableTestJournal::open(&self.journal_dir)
    }

    fn strong(&self) -> ContainerAdapterProvider {
        ContainerAdapterProvider::new(self.adapter.clone(), self.boundary_root.clone())
    }

    fn weak(&self) -> UnixLocatorProvider {
        UnixLocatorProvider::new(self.state_dir.clone(), supervisor_binary())
    }

    fn request(&self, suffix: &str, trust: TrustLevel) -> LaunchRequest {
        let evidence = self.work_dir.join(format!("evidence-{suffix}"));
        let mut allow = BTreeSet::new();
        allow.insert("PATH".to_owned());
        LaunchRequest {
            envelope: envelope(suffix),
            trust,
            program: self.helper.clone(),
            arguments: vec![
                "--evidence".to_owned(),
                evidence.display().to_string(),
                "--payload".to_owned(),
                format!("released-{suffix}"),
            ],
            binary_sha256: hex_sha256(&std::fs::read(&self.helper).expect("helper bytes")),
            supervisor_binary: supervisor_binary(),
            project_root_display: self.work_dir.clone(),
            environment: EnvironmentAllowlist {
                allow,
                extra: BTreeMap::new(),
            },
            caps: OutputCaps {
                stdout_bytes: 4096,
                stderr_bytes: 4096,
            },
            deadline: Duration::from_secs(30),
            work_dir: self.work_dir.clone(),
            evidence_path: Some(evidence),
        }
    }
}

// ---------------------------------------------------------------------------
// Happy path
// ---------------------------------------------------------------------------

/// An adapter whose `run` forks, as a sandbox that creates its namespaces
/// before the program does must: the spawned process is not the gated barrier,
/// which is found among the boundary's members instead, and the launch is the
/// same gapless protocol.
#[test]
fn an_adapter_that_forks_has_its_gated_barrier_found_among_the_members() {
    let scenario = Scenario::new();
    let journal = scenario.journal();
    let forking = install_fixture(
        scenario.adapter.parent().expect("fixture directory"),
        "forking-container-adapter.sh",
    );
    let provider = ContainerAdapterProvider::new(forking, scenario.boundary_root.clone());
    let recorder = Recorder::default();
    let protocol = LaunchProtocol::new(&journal);
    let request = scenario.request("311", TrustLevel::Untrusted);
    let evidence = request.evidence_path.clone().expect("evidence path");
    let outcome = protocol
        .launch(&request, &provider, &FaultInjection::none(), &recorder)
        .expect("a forking adapter launches through the same protocol");
    assert_eq!(outcome.diagnostics.exit_code, Some(0), "{outcome:?}");
    assert_eq!(
        std::fs::read(&evidence).expect("the released helper ran"),
        b"released-311"
    );
    assert_eq!(outcome.proof.boundary_empty_after_exit, Some(true));
    let notes = recorder.notes();
    assert!(
        notes.iter().any(|note| note.contains("gated=true")),
        "the receipt names the gated barrier: {notes:?}"
    );
}

#[test]
fn process_launch_protocol_happy_path() {
    let scenario = Scenario::new();
    let journal = scenario.journal();
    let provider = scenario.strong();
    let recorder = Recorder::default();
    let protocol = LaunchProtocol::new(&journal);

    // -- 1. A strong boundary is required for untrusted code and available here.
    assert_eq!(provider.strength(), BoundaryStrength::Strong);
    provider
        .available()
        .expect("the committed container/VM adapter fixture is available");

    let request = scenario.request("301", TrustLevel::Untrusted);
    let expected_payload = format!("released-{}", "301");
    let evidence = request.evidence_path.clone().expect("evidence path");

    let outcome = protocol
        .launch(&request, &provider, &FaultInjection::none(), &recorder)
        .expect("the gapless launch protocol completes");

    // -- 2. Normative ordering: the boundary is pre-created BEFORE the intent
    //       fsync, the child is created inside it after `staged`, containment and
    //       the receipt are durable before any release. -----------------------
    let events = recorder.events();
    let expected_order = vec![
        "boundary_precreated",
        "phase:intent_fsynced",
        "member_created",
        "phase:staged_fsynced",
        "membership_confirmed",
        "phase:effect_started_fsynced",
        "gate_released",
        "child_exited",
        "phase:committed_fsynced",
        "boundary_closed",
    ];
    assert_eq!(events, expected_order, "observed launch ordering");

    // -- 3. No user code ran before the gate: at receipt time the child was live,
    //       still the gated barrier, and had produced no side effect. ---------
    let notes = recorder.notes();
    assert_eq!(
        notes.len(),
        1,
        "exactly one receipt-time observation: {notes:?}"
    );
    assert!(
        notes[0].contains("gated=true") && notes[0].contains("side_effects=false"),
        "the helper must remain non-executing until the receipt is durable: {}",
        notes[0]
    );

    // -- 4. The child then ran and exited cleanly with matching hashes. -------
    assert_eq!(
        outcome.diagnostics.termination,
        TerminationKind::ExitedClean
    );
    assert_eq!(outcome.diagnostics.exit_code, Some(0));
    assert!(outcome.diagnostics.signal.is_none());
    assert!(!outcome.diagnostics.stdout.truncated);
    assert_eq!(
        outcome.diagnostics.stdout.bytes.len() as u64,
        outcome.diagnostics.stdout.total_bytes,
        "a quiet launch must explicitly retain its complete stdout"
    );
    let produced = std::fs::read(&evidence).expect("the released helper wrote its evidence");
    assert_eq!(
        hex_sha256(&produced),
        hex_sha256(expected_payload.as_bytes()),
        "the released child's output hash matches the expected payload hash"
    );
    assert_eq!(outcome.binary_sha256, request.binary_sha256);

    // -- 5. The proof is Todo 6's `process_launch` form, never a content hash. -
    let proof = &outcome.proof;
    assert_eq!(proof.boundary_membership, BoundaryMembership::Confirmed);
    assert_eq!(proof.release_status, ReleaseStatus::ExitedClean);
    assert_eq!(proof.exit_code, Some(0));
    assert_eq!(proof.boundary_empty_after_exit, Some(true));
    assert!(proof.is_clean_terminal_process());
    let stored = journal
        .stored(&request.envelope.transaction_id)
        .expect("the transaction is durable");
    assert_eq!(stored["effect_kind"], serde_json::json!("process_launch"));
    assert_eq!(stored["proof"]["kind"], serde_json::json!("process_launch"));
    assert_eq!(stored["phase"], serde_json::json!("committed_fsynced"));
    assert_eq!(
        stored["pre_hashes"],
        serde_json::json!([]),
        "a launch is never proven by a content hash"
    );
    assert_eq!(stored["intended_post_hashes"], serde_json::json!([]));
    assert!(
        stored["proof"].get("sha256").is_none(),
        "the process_launch proof carries no content hash"
    );

    // -- 6. Launch-specific facts travel in the canonical staged write set. ---
    let phases: Vec<String> = stored["phase_history"]
        .as_array()
        .expect("history")
        .iter()
        .map(|step| step["phase"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(
        phases,
        vec![
            "intent_fsynced",
            "staged_fsynced",
            "effect_started_fsynced",
            "committed_fsynced"
        ]
    );
    let staged_names: Vec<String> = stored["staged_write_set"]
        .as_array()
        .expect("staged")
        .iter()
        .map(|entry| entry["path"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(
        staged_names.contains(&"launch/intent.json".to_owned())
            && staged_names.contains(&"launch/staged.json".to_owned())
            && staged_names.contains(&"launch/receipt.json".to_owned()),
        "every phase's launch payload is a canonical staged document: {staged_names:?}"
    );
    let open = journal.open_launches().expect("reload");
    assert!(
        open.is_empty(),
        "a committed launch is not an open transaction"
    );
    let intent = LaunchRecord::from_canonical_value(&stored, &|staging_id| {
        std::fs::read(scenario.journal_dir.join("staged").join(staging_id)).ok()
    })
    .expect("the canonical record round-trips");
    let intent_facts = intent
        .payload("launch/intent.json")
        .expect("intent payload")
        .facts()
        .expect("intent facts");
    assert_eq!(
        intent_facts
            .get("boundary_identity")
            .and_then(|v| v.as_str()),
        Some(outcome.boundary_identity.as_str()),
        "the intent record durably binds the already-pre-created boundary identity"
    );
    assert_eq!(
        intent_facts
            .get("boundary_strength")
            .and_then(|v| v.as_str()),
        Some("strong")
    );

    // -- 7. Argument arrays only, never a shell command string. ---------------
    assert_eq!(outcome.argv[0], scenario.helper.display().to_string());
    assert!(
        outcome.argv.iter().all(|argument| !argument.contains("&&")
            && !argument.contains(';')
            && !argument.contains('|')),
        "the launch never assembles a shell command string: {:?}",
        outcome.argv
    );

    // -- 8. The boundary is torn down and proven empty. -----------------------
    let reopened = provider
        .reopen(&outcome.boundary)
        .expect("reopen is inspectable");
    assert!(reopened.is_none(), "a completed launch closes its boundary");

    // -- 9. Nothing is left blocking mutation. --------------------------------
    assert!(
        mutation_block(&journal).expect("survivor state").is_none(),
        "a clean launch leaves no unresolved survivor"
    );
}

#[test]
fn terminal_launches_do_not_accumulate_scratch_directories() {
    let scenario = Scenario::new();
    let journal = scenario.journal();
    let provider = scenario.strong();
    let protocol = LaunchProtocol::new(&journal);
    let scratch_root = scenario.work_dir.join(".godot-mcp");

    for suffix in ["302", "303"] {
        let request = scenario.request(suffix, TrustLevel::Untrusted);
        protocol
            .launch(
                &request,
                &provider,
                &FaultInjection::none(),
                &Recorder::default(),
            )
            .expect("the repeated launch completes");

        let count = std::fs::read_dir(&scratch_root)
            .map(|entries| entries.flatten().count())
            .unwrap_or_default();
        assert_eq!(
            count, 0,
            "a durably settled launch with a closed boundary must not leave scratch behind"
        );
    }
}

#[test]
fn restart_reconciliation_retains_open_scratch_until_terminal_then_sweeps_it() {
    let scenario = Scenario::new();
    let journal = scenario.journal();
    let provider = scenario.strong();
    let protocol = LaunchProtocol::new(&journal);
    let request = scenario.request("304", TrustLevel::Untrusted);
    let paths = LaunchPaths::for_transaction(&request.work_dir, &request.envelope.transaction_id);

    protocol
        .launch(
            &request,
            &provider,
            &FaultInjection::crash_at(CrashStage::AfterReceiptBeforeRelease),
            &Recorder::default(),
        )
        .expect_err("the injected crash leaves an open launch for restart reconciliation");

    let restarted_journal = scenario.journal();
    assert_eq!(
        restarted_journal
            .open_launches()
            .expect("restart reloads the journal")
            .len(),
        1
    );
    assert!(
        paths.gate.exists(),
        "scratch for an open launch must survive until restart reconciliation can inspect its gate"
    );

    let providers: Vec<&dyn BoundaryProvider> = vec![&provider];
    let settled =
        reconcile_launches(&restarted_journal, &providers).expect("restart reconciliation settles");

    assert_case(
        &settled[0],
        ReconcileCase::ReceiptWithoutCommit,
        Disposition::Compensated,
    );
    assert!(
        !paths.gate.exists(),
        "scratch must be swept after restart reconciliation durably settles the launch and closes its boundary"
    );
}

// ---------------------------------------------------------------------------
// Failure paths
// ---------------------------------------------------------------------------

#[test]
fn process_launch_protocol_failure_paths() {
    // -- A. Untrusted code without a strong boundary fails closed before spawn.
    {
        let scenario = Scenario::new();
        let journal = scenario.journal();
        let weak = scenario.weak();
        let cgroup = CgroupV2Provider::new(PathBuf::from("/sys/fs/cgroup/godot-mcp-absent"));
        assert_eq!(weak.strength(), BoundaryStrength::Weak);
        assert!(
            cgroup.available().is_err(),
            "an unwritable cgroup v2 root is not an available strong boundary"
        );
        let providers: Vec<&dyn BoundaryProvider> = vec![&cgroup, &weak];
        match select_provider(TrustLevel::Untrusted, &providers) {
            Ok(_) => panic!("an untrusted launch never uses the weak anchor protocol"),
            Err(error) => {
                assert!(matches!(
                    error,
                    LaunchError::StrongBoundaryUnavailable { .. }
                ));
            }
        }

        let protocol = LaunchProtocol::new(&journal);
        let request = scenario.request("310", TrustLevel::Untrusted);
        let error = protocol
            .launch(
                &request,
                &weak,
                &FaultInjection::none(),
                &Recorder::default(),
            )
            .expect_err("the weak path refuses an untrusted launch");
        assert!(matches!(
            error,
            LaunchError::StrongBoundaryUnavailable { .. }
        ));
        assert!(
            journal.open_launches().expect("reload").is_empty(),
            "a refused launch journals nothing and creates no process"
        );
        // A trusted launch may select the weak path.
        match select_provider(TrustLevel::Trusted, &providers) {
            Ok(selected) => assert_eq!(selected.kind(), weak.kind()),
            Err(error) => panic!("a trusted launch may use the weak anchor protocol: {error}"),
        }
        assert_eq!(
            WEAK_RESIDUAL_RISKS,
            ["deliberate_detach", "post_release_supervisor_sigkill"],
            "the weak mode never claims to contain detach or survive supervisor SIGKILL"
        );
    }

    // -- B. Crash before `intent_fsynced` creates no process of any kind. -----
    for trust in [TrustLevel::Untrusted, TrustLevel::Trusted] {
        let scenario = Scenario::new();
        let journal = scenario.journal();
        let protocol = LaunchProtocol::new(&journal);
        let request = scenario.request("320", trust);
        let fault = FaultInjection::crash_at(CrashStage::BeforeIntentFsync);
        let error = if trust == TrustLevel::Untrusted {
            let provider = scenario.strong();
            protocol
                .launch(&request, &provider, &fault, &Recorder::default())
                .expect_err("the injected crash aborts the launch")
        } else {
            let provider = scenario.weak();
            protocol
                .launch(&request, &provider, &fault, &Recorder::default())
                .expect_err("the injected crash aborts the launch")
        };
        assert!(matches!(error, LaunchError::InjectedFault { .. }));
        assert!(
            journal.open_launches().expect("reload").is_empty(),
            "nothing durable exists, so reconciliation has nothing to classify"
        );
        let settled = reconcile_launches(&journal, &[]).expect("reconciliation runs");
        assert!(
            settled.is_empty(),
            "case 1 is a no-op proven pre-state, not a disposition record"
        );
    }

    // -- C. Case 2, strong path: intent durable, staged absent. ---------------
    for child_created in [false, true] {
        let scenario = Scenario::new();
        let journal = scenario.journal();
        let provider = scenario.strong();
        let protocol = LaunchProtocol::new(&journal);
        let request = scenario.request(
            if child_created { "331" } else { "330" },
            TrustLevel::Untrusted,
        );
        let fault = if child_created {
            FaultInjection::crash_at(CrashStage::AfterChildCreationBeforeStagedFsync)
        } else {
            FaultInjection::crash_at(CrashStage::AfterIntentBeforeChildCreation)
        };
        let error = protocol
            .launch(&request, &provider, &fault, &Recorder::default())
            .expect_err("the injected crash aborts the launch");
        assert!(matches!(error, LaunchError::InjectedFault { .. }));

        // The crash left the durable intent — and, in the second case, a real
        // orphaned member inside the boundary.
        let open = journal.open_launches().expect("reload");
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].phase, LaunchPhase::IntentFsynced);
        let boundary = provider
            .reopen(
                &open[0]
                    .boundary_identity()
                    .expect("intent binds the boundary"),
            )
            .expect("the boundary is enumerable from the intent record alone")
            .expect("the boundary still exists");
        let members = boundary.members().expect("enumerate");
        assert_eq!(
            members.len(),
            usize::from(child_created),
            "enumeration distinguishes an empty boundary from one with an orphan"
        );

        let providers: Vec<&dyn BoundaryProvider> = vec![&provider];
        let settled = reconcile_launches(&journal, &providers).expect("reconciliation runs");
        assert_eq!(settled.len(), 1);
        assert_case(
            &settled[0],
            ReconcileCase::IntentWithoutStaged,
            Disposition::Compensated,
        );
        assert_eq!(
            journal
                .terminal_reason(&request.envelope.transaction_id)
                .as_deref(),
            Some(LAUNCH_ABANDONED_PRE_RELEASE)
        );
        assert!(
            provider
                .reopen(&open[0].boundary_identity().expect("boundary"))
                .expect("reopen")
                .is_none(),
            "a compensated launch proves the boundary empty and closes it"
        );
    }

    // -- D. Case 2, weak path: locator durable, supervisor may or may not exist.
    #[cfg(unix)]
    for supervisor_spawned in [false, true] {
        let scenario = Scenario::new();
        let journal = scenario.journal();
        let provider = scenario.weak();
        let protocol = LaunchProtocol::new(&journal);
        let request = scenario.request(
            if supervisor_spawned { "341" } else { "340" },
            TrustLevel::Trusted,
        );
        let fault = if supervisor_spawned {
            FaultInjection::crash_at(CrashStage::AfterChildCreationBeforeStagedFsync)
        } else {
            FaultInjection::crash_at(CrashStage::AfterIntentBeforeChildCreation)
        };
        let error = protocol
            .launch(&request, &provider, &fault, &Recorder::default())
            .expect_err("the injected crash aborts the launch");
        assert!(matches!(error, LaunchError::InjectedFault { .. }));

        let open = journal.open_launches().expect("reload");
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].phase, LaunchPhase::IntentFsynced);
        let locator = open[0]
            .boundary_identity()
            .expect("the intent record durably binds the inert locator");
        let boundary = provider
            .reopen(&locator)
            .expect("the locator is enumerable from the intent record alone")
            .expect("the locator still exists");
        assert_eq!(
            boundary.members().expect("enumerate").len(),
            usize::from(supervisor_spawned),
            "the locator distinguishes 'no peer' from 'a live authenticated peer'"
        );

        let providers: Vec<&dyn BoundaryProvider> = vec![&provider];
        let settled = reconcile_launches(&journal, &providers).expect("reconciliation runs");
        assert_case(
            &settled[0],
            ReconcileCase::IntentWithoutStaged,
            Disposition::Compensated,
        );
        assert!(
            provider.reopen(&locator).expect("reopen").is_none(),
            "the private locator is reclaimed once it is proven to have no peer"
        );
    }

    // -- E. Case 3: staged durable, receipt absent. ---------------------------
    {
        let scenario = Scenario::new();
        let journal = scenario.journal();
        let provider = scenario.strong();
        let protocol = LaunchProtocol::new(&journal);
        let request = scenario.request("350", TrustLevel::Untrusted);
        let error = protocol
            .launch(
                &request,
                &provider,
                &FaultInjection::crash_at(CrashStage::AfterStagedBeforeReceiptFsync),
                &Recorder::default(),
            )
            .expect_err("the injected crash aborts the launch");
        assert!(matches!(error, LaunchError::InjectedFault { .. }));
        let open = journal.open_launches().expect("reload");
        assert_eq!(open[0].phase, LaunchPhase::StagedFsynced);
        let providers: Vec<&dyn BoundaryProvider> = vec![&provider];
        let settled = reconcile_launches(&journal, &providers).expect("reconciliation runs");
        assert_case(
            &settled[0],
            ReconcileCase::StagedWithoutReceipt,
            Disposition::Compensated,
        );
        assert!(
            !request.evidence_path.expect("evidence").exists(),
            "a child killed before its receipt never ran user code"
        );
    }

    // -- F. Case 3, weak path, PID/start-time reuse is never trusted. ---------
    #[cfg(unix)]
    {
        let scenario = Scenario::new();
        let journal = scenario.journal();
        let provider = scenario.weak();
        let protocol = LaunchProtocol::new(&journal);
        let request = scenario.request("351", TrustLevel::Trusted);
        let _ = protocol
            .launch(
                &request,
                &provider,
                &FaultInjection::crash_at(CrashStage::AfterStagedBeforeReceiptFsync),
                &Recorder::default(),
            )
            .expect_err("the injected crash aborts the launch");
        // Rewrite the durable supervisor start time: the PID is still live, but it
        // is now provably a *different* process generation (PID reuse).
        journal.tamper_payload(
            &request.envelope.transaction_id,
            "launch/staged.json",
            "supervisor_start_time",
            serde_json::json!("1"),
        );
        let providers: Vec<&dyn BoundaryProvider> = vec![&provider];
        let settled = reconcile_launches(&journal, &providers).expect("reconciliation runs");
        assert_case(
            &settled[0],
            ReconcileCase::StagedWithoutReceipt,
            Disposition::Quarantined,
        );
        assert!(
            mutation_block(&journal).expect("survivor state").is_some(),
            "an unresolved survivor blocks every mutation"
        );
    }

    // -- G. Case 4(i): receipt durable, release provably never happened. ------
    {
        let scenario = Scenario::new();
        let journal = scenario.journal();
        let provider = scenario.strong();
        let protocol = LaunchProtocol::new(&journal);
        let request = scenario.request("360", TrustLevel::Untrusted);
        let _ = protocol
            .launch(
                &request,
                &provider,
                &FaultInjection::crash_at(CrashStage::AfterReceiptBeforeRelease),
                &Recorder::default(),
            )
            .expect_err("the injected crash aborts the launch");
        let open = journal.open_launches().expect("reload");
        assert_eq!(open[0].phase, LaunchPhase::EffectStartedFsynced);
        let providers: Vec<&dyn BoundaryProvider> = vec![&provider];
        let settled = reconcile_launches(&journal, &providers).expect("reconciliation runs");
        assert_case(
            &settled[0],
            ReconcileCase::ReceiptWithoutCommit,
            Disposition::Compensated,
        );
        assert!(
            !request.evidence_path.expect("evidence").exists(),
            "a live-but-unresumed child is terminated, never adopted as a survivor"
        );
    }

    // -- H. Case 4(ii) → case 5: receipt durable, resume independently proven. -
    {
        let scenario = Scenario::new();
        let journal = scenario.journal();
        let provider = scenario.strong();
        let protocol = LaunchProtocol::new(&journal);
        let mut request = scenario.request("370", TrustLevel::Untrusted);
        request
            .arguments
            .extend(["--linger-ms".to_owned(), "4000".to_owned()]);
        let _ = protocol
            .launch(
                &request,
                &provider,
                &FaultInjection::crash_at(CrashStage::AfterReleaseBeforeCommitFsync),
                &Recorder::default(),
            )
            .expect_err("the injected crash aborts the launch");
        let providers: Vec<&dyn BoundaryProvider> = vec![&provider];
        let settled = reconcile_launches(&journal, &providers).expect("reconciliation runs");
        assert_case(
            &settled[0],
            ReconcileCase::SurvivorReconciliation,
            Disposition::Committed,
        );
        assert_eq!(
            journal
                .phase_of(&request.envelope.transaction_id)
                .as_deref(),
            Some("committed_fsynced"),
            "a proven resume plus proven containment commits retroactively"
        );
    }

    // -- I. Case 4(iii): dead child with an unprovable resume history. --------
    {
        let scenario = Scenario::new();
        let journal = scenario.journal();
        let provider = scenario.strong();
        let protocol = LaunchProtocol::new(&journal);
        let request = scenario.request("380", TrustLevel::Untrusted);
        let _ = protocol
            .launch(
                &request,
                &provider,
                &FaultInjection::crash_at(CrashStage::AfterReleaseBeforeCommitFsync),
                &Recorder::default(),
            )
            .expect_err("the injected crash aborts the launch");
        // The released child is short-lived; wait for it to be gone, so
        // reconciliation faces a dead process with no resume proof either way.
        let boundary_id = journal.open_launches().expect("reload")[0]
            .boundary_identity()
            .expect("boundary");
        wait_until_empty(&provider, &boundary_id);
        let providers: Vec<&dyn BoundaryProvider> = vec![&provider];
        let settled = reconcile_launches(&journal, &providers).expect("reconciliation runs");
        assert_case(
            &settled[0],
            ReconcileCase::ReceiptWithoutCommit,
            Disposition::Quarantined,
        );
        assert!(
            mutation_block(&journal).expect("survivor state").is_some(),
            "death alone never proves which side of the resume boundary the crash hit"
        );
    }

    // -- J. An ordinary failure at the release step tears the boundary down. --
    {
        let scenario = Scenario::new();
        let journal = scenario.journal();
        let provider = scenario.strong();
        let protocol = LaunchProtocol::new(&journal);
        let request = scenario.request("390", TrustLevel::Untrusted);
        let error = protocol
            .launch(
                &request,
                &provider,
                &FaultInjection::error_at(CrashStage::AfterReceiptBeforeRelease),
                &Recorder::default(),
            )
            .expect_err("the injected release failure aborts the launch");
        assert!(matches!(error, LaunchError::InjectedFault { .. }));
        assert!(
            !request.evidence_path.expect("evidence").exists(),
            "a failed release never lets user code run"
        );
        assert_eq!(
            journal
                .phase_of(&request.envelope.transaction_id)
                .as_deref(),
            Some("compensated"),
            "an ordinary error terminates the child and closes the boundary"
        );
        assert!(
            journal.open_launches().expect("reload").is_empty(),
            "a compensated launch is no longer open"
        );
    }

    // -- K. Supervisor SIGKILL after release is the documented residual risk. -
    #[cfg(unix)]
    {
        let scenario = Scenario::new();
        let journal = scenario.journal();
        let provider = scenario.weak();
        let protocol = LaunchProtocol::new(&journal);
        let mut request = scenario.request("400", TrustLevel::Trusted);
        request
            .arguments
            .extend(["--linger-ms".to_owned(), "4000".to_owned()]);
        let outcome = protocol.launch(
            &request,
            &provider,
            &FaultInjection::sigkill_supervisor_at(CrashStage::AfterReleaseBeforeCommitFsync),
            &Recorder::default(),
        );
        let error = outcome.expect_err("a supervisor SIGKILL after release is not a clean launch");
        assert!(
            matches!(error, LaunchError::SupervisorLost { .. }),
            "the weak mode reports the residual risk rather than claiming containment: {error}"
        );
        let providers: Vec<&dyn BoundaryProvider> = vec![&provider];
        let settled = reconcile_launches(&journal, &providers).expect("reconciliation runs");
        assert!(
            settled[0].disposition != Disposition::Committed,
            "a lost supervisor is never silently committed"
        );
        assert!(mutation_block(&journal).expect("survivor state").is_some());
    }

    // -- L. Structured diagnostics: timeout, truncation, nonzero exit, signal. -
    {
        let scenario = Scenario::new();
        let journal = scenario.journal();
        let provider = scenario.strong();
        let protocol = LaunchProtocol::new(&journal);

        let mut nonzero = scenario.request("410", TrustLevel::Untrusted);
        nonzero
            .arguments
            .extend(["--exit-code".to_owned(), "23".to_owned()]);
        let outcome = protocol
            .launch(
                &nonzero,
                &provider,
                &FaultInjection::none(),
                &Recorder::default(),
            )
            .expect("a nonzero exit is a completed launch with diagnostics");
        assert_eq!(
            outcome.diagnostics.termination,
            TerminationKind::NonzeroExit { code: 23 }
        );
        assert_eq!(outcome.proof.release_status, ReleaseStatus::NonzeroExit);
        assert!(!outcome.proof.is_clean_terminal_process());

        let mut noisy = scenario.request("411", TrustLevel::Untrusted);
        noisy
            .arguments
            .extend(["--stdout-bytes".to_owned(), "512".to_owned()]);
        noisy.caps = OutputCaps {
            stdout_bytes: 64,
            stderr_bytes: 64,
        };
        let outcome = protocol
            .launch(
                &noisy,
                &provider,
                &FaultInjection::none(),
                &Recorder::default(),
            )
            .expect("a noisy child is capped, not unbounded");
        assert!(outcome.diagnostics.stdout.truncated);
        assert_eq!(outcome.diagnostics.stdout.bytes.len(), 64);
        assert_eq!(outcome.diagnostics.stdout.total_bytes, 512);

        let mut slow = scenario.request("412", TrustLevel::Untrusted);
        slow.arguments
            .extend(["--linger-ms".to_owned(), "9000".to_owned()]);
        slow.deadline = Duration::from_millis(700);
        let slow_paths =
            LaunchPaths::for_transaction(&slow.work_dir, &slow.envelope.transaction_id);
        let outcome = protocol
            .launch(
                &slow,
                &provider,
                &FaultInjection::none(),
                &Recorder::default(),
            )
            .expect("a deadline is enforced by the supervisor, not by the child");
        assert_eq!(outcome.diagnostics.termination, TerminationKind::TimedOut);
        assert_eq!(outcome.proof.release_status, ReleaseStatus::Signaled);
        assert_eq!(
            journal.phase_of(&slow.envelope.transaction_id).as_deref(),
            Some("compensated"),
            "a timed-out launch is compensated, never committed"
        );
        assert!(
            !slow_paths.gate.exists(),
            "a timed-out launch is durably compensated and closed, so its scratch is removed"
        );

        // The environment reaching the child is exactly the allowlist.
        let mut scoped = scenario.request("413", TrustLevel::Untrusted);
        let dump = scenario.work_dir.join("env-413");
        scoped
            .arguments
            .extend(["--env-dump".to_owned(), dump.display().to_string()]);
        scoped
            .environment
            .extra
            .insert("GODOT_MCP_SCOPED".to_owned(), "yes".to_owned());
        protocol
            .launch(
                &scoped,
                &provider,
                &FaultInjection::none(),
                &Recorder::default(),
            )
            .expect("the allowlisted launch completes");
        let dumped = std::fs::read_to_string(&dump).expect("env dump");
        let names: BTreeSet<&str> = dumped
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(name, _)| name)
            .collect();
        assert!(names.contains("GODOT_MCP_SCOPED"));
        assert!(
            !names.contains("CARGO_PKG_NAME") && !names.contains("RUSTUP_TOOLCHAIN"),
            "only allowlisted variables reach the child: {names:?}"
        );
    }

    // -- M. Every crash stage is covered and every disposition is one of three.
    assert_eq!(CrashStage::ALL.len(), 6);
    assert_eq!(Disposition::ALL.len(), 3);
    assert_eq!(ReconcileCase::ALL.len(), 5);
    assert_eq!(FaultMode::ALL.len(), 3);
}

// ---------------------------------------------------------------------------
// Assertions shared by the failure cases
// ---------------------------------------------------------------------------

#[track_caller]
fn assert_case(reconciled: &ReconciledLaunch, expected_case: ReconcileCase, expected: Disposition) {
    assert_eq!(
        reconciled.case, expected_case,
        "reconciliation case: {}",
        reconciled.reason
    );
    assert_eq!(
        reconciled.disposition, expected,
        "disposition for {:?}: {}",
        reconciled.case, reconciled.reason
    );
}

fn wait_until_empty(
    provider: &dyn BoundaryProvider,
    identity: &godot_mcp_engine::supervision::boundary::BoundaryIdentity,
) {
    for _ in 0..200 {
        let Some(boundary) = provider.reopen(identity).expect("reopen") else {
            return;
        };
        if boundary.members().expect("enumerate").is_empty() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("the released child never exited");
}

// ---------------------------------------------------------------------------
// Detached launches: a session that outlives the call that started it.
// ---------------------------------------------------------------------------

fn wait_for_file(path: &Path) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn with_arguments(mut request: LaunchRequest, extra: &[&str]) -> LaunchRequest {
    request
        .arguments
        .extend(extra.iter().map(|argument| (*argument).to_owned()));
    request
}

#[test]
fn a_detached_launch_returns_while_its_child_runs_and_its_lifetime_ends_it() {
    for weak in [false, true] {
        let label = if weak {
            "weak locator"
        } else {
            "strong adapter"
        };
        let scenario = Scenario::new();
        let journal = scenario.journal();
        let recorder = Recorder::default();
        let protocol = LaunchProtocol::new(&journal);
        let strong = scenario.strong();
        let locator = scenario.weak();
        let provider: &dyn BoundaryProvider = if weak { &locator } else { &strong };
        let request = with_arguments(
            scenario.request(if weak { "401" } else { "402" }, TrustLevel::Trusted),
            &["--linger-ms", "60000"],
        );

        let started = std::time::Instant::now();
        let mut running = protocol
            .start(&request, provider, &FaultInjection::none(), &recorder)
            .unwrap_or_else(|error| panic!("{label}: a detached launch starts: {error}"));
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "{label}: start returns without waiting for the child"
        );

        // The child was released and is running user code inside its boundary.
        wait_for_file(request.evidence_path.as_deref().expect("evidence path"));
        let identity = procfs::process_identity(running.child_pid())
            .expect("inspect the child")
            .expect("the child is live");
        assert!(!identity.is_gated_barrier(), "{label}: the gate was opened");
        assert_eq!(running.transaction_id(), request.envelope.transaction_id);
        assert_eq!(running.binary_sha256(), request.binary_sha256);

        // `committed_fsynced` records it as resumed and contained, with no exit
        // yet, and nothing is left open for reconciliation.
        let committed = journal.committed_launches().expect("committed launches");
        let record = committed
            .iter()
            .find(|record| record.envelope.transaction_id == running.transaction_id())
            .unwrap_or_else(|| panic!("{label}: the launch is committed"));
        let proof = record.proof.as_ref().expect("a committed proof");
        assert_eq!(
            proof.release_status,
            ReleaseStatus::ResumedContained,
            "{label}"
        );
        assert_eq!(proof.boundary_membership, BoundaryMembership::Confirmed);
        assert_eq!(proof.pid, Some(running.child_pid()));
        assert_eq!(proof.exit_code, None);
        assert_eq!(proof.boundary_empty_after_exit, None);
        assert!(journal.open_launches().expect("open launches").is_empty());
        assert_eq!(
            recorder.events(),
            vec![
                "boundary_precreated",
                "phase:intent_fsynced",
                "member_created",
                "phase:staged_fsynced",
                "membership_confirmed",
                "phase:effect_started_fsynced",
                "gate_released",
                "phase:committed_fsynced",
            ],
            "{label}: the child is committed as running, not as exited"
        );

        // The session's lifetime ends it: the whole boundary is terminated and
        // proven empty, and the exit is reported rather than journaled again.
        let exit = running
            .wait(Duration::from_millis(300))
            .unwrap_or_else(|error| panic!("{label}: the lifetime ends the launch: {error}"));
        assert_eq!(
            exit.diagnostics.termination,
            TerminationKind::TimedOut,
            "{label}"
        );
        assert_eq!(exit.proof.boundary_empty_after_exit, Some(true), "{label}");
        assert_eq!(
            exit.proof.release_status,
            ReleaseStatus::Signaled,
            "{label}"
        );
        assert!(
            provider
                .reopen(running.boundary())
                .expect("reopen")
                .is_none_or(|boundary| boundary.members().expect("enumerate").is_empty()),
            "{label}: nothing of the launch survives"
        );
        running.remove_scratch().expect("scratch is removable");
    }
}

#[test]
fn a_detached_launch_reports_the_childs_own_exit_and_output() {
    let scenario = Scenario::new();
    let journal = scenario.journal();
    let protocol = LaunchProtocol::new(&journal);
    let provider = scenario.weak();
    let request = with_arguments(
        scenario.request("403", TrustLevel::Trusted),
        &["--exit-code", "3", "--stdout-bytes", "5"],
    );
    let mut running = protocol
        .start(
            &request,
            &provider,
            &FaultInjection::none(),
            &Recorder::default(),
        )
        .expect("a detached launch starts");
    let exit = running
        .wait(Duration::from_secs(30))
        .expect("the child ends on its own");
    assert_eq!(
        exit.diagnostics.termination,
        TerminationKind::NonzeroExit { code: 3 }
    );
    assert_eq!(exit.diagnostics.stdout.bytes, b"xxxxx");
    assert_eq!(exit.proof.release_status, ReleaseStatus::NonzeroExit);
    assert_eq!(exit.proof.exit_code, Some(3));
    assert_eq!(exit.proof.boundary_empty_after_exit, Some(true));
}

#[test]
fn a_detached_launch_is_stopped_from_its_recorded_boundary_identity_alone() {
    for weak in [false, true] {
        let label = if weak {
            "weak locator"
        } else {
            "strong adapter"
        };
        let scenario = Scenario::new();
        let journal = scenario.journal();
        let protocol = LaunchProtocol::new(&journal);
        let strong = scenario.strong();
        let locator = scenario.weak();
        let provider: &dyn BoundaryProvider = if weak { &locator } else { &strong };
        let request = with_arguments(
            scenario.request(if weak { "404" } else { "405" }, TrustLevel::Trusted),
            &["--linger-ms", "60000"],
        );
        let mut running = protocol
            .start(
                &request,
                provider,
                &FaultInjection::none(),
                &Recorder::default(),
            )
            .unwrap_or_else(|error| panic!("{label}: a detached launch starts: {error}"));
        wait_for_file(request.evidence_path.as_deref().expect("evidence path"));

        // Another holder of nothing but the durable identity stops it, the way
        // a stop request or a restarted server does.
        let identity = running.boundary().clone();
        let stopper = std::thread::spawn({
            let strong = scenario.strong();
            let locator = scenario.weak();
            move || {
                let provider: &dyn BoundaryProvider = if weak { &locator } else { &strong };
                provider
                    .reopen(&identity)
                    .expect("reopen the recorded boundary")
                    .expect("the boundary still exists")
                    .terminate_all()
                    .expect("terminate every member");
            }
        });
        let started = std::time::Instant::now();
        let exit = running
            .wait(Duration::from_secs(60))
            .unwrap_or_else(|error| panic!("{label}: a stopped launch ends: {error}"));
        stopper.join().expect("the stopper finished");
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "{label}: the stop, not the lifetime, ended the launch"
        );
        assert_ne!(
            exit.diagnostics.termination,
            TerminationKind::TimedOut,
            "{label}"
        );
        assert_ne!(
            exit.diagnostics.termination,
            TerminationKind::ExitedClean,
            "{label}"
        );
        assert_eq!(exit.proof.boundary_empty_after_exit, Some(true), "{label}");
    }
}

/// The weak locator's directory is removed while its launch runs, as when a
/// project's state directory is deleted under a running server. The supervisor
/// still holds the recorded control socket, so the boundary still exists:
/// reopening it from the recorded identity finds it, and a stop ends it.
#[cfg(target_os = "linux")]
#[test]
fn a_weak_launch_whose_locator_directory_was_removed_is_still_stopped() {
    let scenario = Scenario::new();
    let journal = scenario.journal();
    let protocol = LaunchProtocol::new(&journal);
    let provider = scenario.weak();
    let request = with_arguments(
        scenario.request("406", TrustLevel::Trusted),
        &["--linger-ms", "60000"],
    );
    let mut running = protocol
        .start(
            &request,
            &provider,
            &FaultInjection::none(),
            &Recorder::default(),
        )
        .expect("a detached launch starts");
    wait_for_file(request.evidence_path.as_deref().expect("evidence path"));

    let identity = running.boundary().clone();
    std::fs::remove_dir_all(&identity.identity).expect("remove the locator directory");
    let boundary = provider
        .reopen(&identity)
        .expect("reopen the recorded boundary")
        .expect("the boundary exists while its supervisor holds the recorded socket");
    assert!(
        !boundary.members().expect("enumerate").is_empty(),
        "the running launch is a member"
    );
    drop(boundary);
    let stopper = std::thread::spawn({
        let provider = scenario.weak();
        let identity = identity.clone();
        move || {
            provider
                .reopen(&identity)
                .expect("reopen the recorded boundary")
                .expect("the boundary still exists")
                .terminate_all()
        }
    });
    let started = std::time::Instant::now();
    // The launch's captured output went with the directory, so only the
    // timing of its end is asserted, not its diagnostics.
    let _ = running.wait(Duration::from_secs(60));
    stopper
        .join()
        .expect("the stopper finished")
        .expect("terminate every member");
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "the stop, not the lifetime, ended the launch"
    );
    assert!(
        provider.reopen(&identity).expect("reopen").is_none(),
        "once its supervisor is gone, the boundary no longer exists"
    );
}

#[test]
fn a_detached_editor_launch_is_the_receipt_an_owned_endpoint_needs() {
    use godot_mcp_engine::adapters::receipt::{
        EditorEndpoint, EndpointKind, EndpointReceipt, receipt_for,
    };

    let scenario = Scenario::new();
    let journal = scenario.journal();
    let protocol = LaunchProtocol::new(&journal);
    let provider = scenario.weak();
    let root_uri = "file:///project";
    let endpoint = EditorEndpoint::loopback(EndpointKind::LanguageServer, 61_234);
    let editor_argv = ["--editor", "--lsp-port", "61234"];

    // A bounded launch that has already ended can never host an endpoint.
    let bounded = with_arguments(scenario.request("406", TrustLevel::Trusted), &editor_argv);
    protocol
        .launch(
            &bounded,
            &provider,
            &FaultInjection::none(),
            &Recorder::default(),
        )
        .expect("the bounded launch completes");
    let committed = journal.committed_launches().expect("committed launches");
    assert_eq!(committed.len(), 1);
    assert!(
        EndpointReceipt::from_launch(&committed[0], EndpointKind::LanguageServer, root_uri)
            .is_err(),
        "an exited launch is not a live editor"
    );

    // A detached launch is committed as resumed and contained: the receipt.
    let detached = with_arguments(scenario.request("407", TrustLevel::Trusted), &editor_argv);
    let running = protocol
        .start(
            &detached,
            &provider,
            &FaultInjection::none(),
            &Recorder::default(),
        )
        .expect("the detached launch starts");
    let receipt = receipt_for(
        &journal.committed_launches().expect("committed launches"),
        EndpointKind::LanguageServer,
        root_uri,
        &endpoint,
    )
    .expect("the detached editor launch owns its endpoint");
    assert_eq!(receipt.launch_transaction_id, running.transaction_id());
    assert_eq!(receipt.pid, running.child_pid());
    assert_eq!(receipt.binary_sha256, detached.binary_sha256);
}
