//! Todo 17 acceptance suite: engine-owned canonical `.tscn` / `.tres` writes and
//! `ResourceUID` operations.
//!
//! Nothing here needs a real Godot installation and nothing touches the network.
//! Every engine invocation runs the committed fake under
//! `tests/fixtures/canonical/binaries/`, against the committed project fixture
//! under `tests/fixtures/canonical/projects/canonical/`, and every process is
//! created by Todo 14's gapless launch protocol inside the committed container/VM
//! adapter boundary — there is no second, bare `Command::spawn` path.
//!
//! # One store, two ports
//!
//! [`DurableTestJournal`] implements **both** Todo 14's `LaunchJournal` and Todo
//! 17's `FileJournal` over the *same* `transactions/` directory, with the same
//! canonical schema and the same four fsynced phases. That is deliberate: this
//! todo reuses Todo 6's journal and must not create a second persistence layer, so
//! the test wiring makes the single store structural rather than asserted.
//!
//! # What this suite is really about
//!
//! One rule, asserted from every angle:
//!
//! > **Matching file bytes are never proof.** A canonical write commits only when
//! > its own post-hash evidence *and* the linked `process_launch` proof's
//! > clean-terminal facts both hold.
//!
//! Two tests are named by `qa/tasks/17.json` and are the only entry points the QA
//! descriptor invokes:
//!
//! * `canonical_writes_happy_path`
//! * `canonical_writes_failure_paths`

// Every launch here goes through the committed `/bin/sh` container adapter
// fixture or the trusted-Unix locator, neither of which exists on Windows; the
// Windows Job Object path is exercised by `job_object_boundary.rs` and by the
// real-client matrix instead.
#![cfg(unix)]
#![allow(
    clippy::too_many_lines,
    reason = "acceptance suites assert one whole operation end to end"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::lease::LeaseManager;
use godot_mcp_domain::safe_root::SafeRoot;
use godot_mcp_engine::canonical::error::CanonicalError;
use godot_mcp_engine::canonical::helper::{HELPER_SHA256, HelperExit, HelperScript};
use godot_mcp_engine::canonical::publish::CanonicalWriter;
use godot_mcp_engine::canonical::request::CanonicalRequest;
use godot_mcp_engine::canonical::stage::EngineContext;
use godot_mcp_engine::canonical::transaction::{
    self, Existence, FileEntry, FileJournal, FileOperation, FileProof, FileRecord, LinkedProof,
    StartupClassification,
};
use godot_mcp_engine::supervision::adapter::ContainerAdapterProvider;
use godot_mcp_engine::supervision::boundary::{BoundaryProvider, TrustLevel};
use godot_mcp_engine::supervision::journal::{
    BoundaryMembership, Disposition, JournalError, LaunchJournal, LaunchPhase, LaunchRecord,
    OperationFamily, ProcessLaunchProof, ProjectIdentityClaim, ReleaseStatus, TransactionEnvelope,
    TransportMode, hex_sha256, validate_canonical,
};
use godot_mcp_engine::supervision::protocol::{EnvironmentAllowlist, OutputCaps};
use godot_mcp_engine::validation::runner::EngineBinding;

// ---------------------------------------------------------------------------
// Committed fixtures
// ---------------------------------------------------------------------------

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the engine crate lives at <repo>/crates/godot-mcp-engine")
        .to_path_buf()
}

fn crate_fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/canonical")
}

fn supervisor_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_godot-mcp-supervisor"))
}

/// Install the committed fake Godot into `dir`, returning its path.
fn install_binary(dir: &Path) -> PathBuf {
    let (subdir, extension) = if cfg!(windows) {
        ("windows", "cmd")
    } else {
        ("unix", "sh")
    };
    let source = crate_fixtures()
        .join("binaries")
        .join(subdir)
        .join(format!("godot-canonical.{extension}"));
    let bytes = std::fs::read(&source).unwrap_or_else(|e| panic!("read {}: {e}", source.display()));
    let target = dir.join(format!("godot-canonical.{extension}"));
    std::fs::write(&target, &bytes).expect("write the fake engine");
    make_executable(&target);
    target
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

/// Copy the committed project fixture into a scratch directory.
///
/// The committed fixture itself is never written to: a canonical write
/// legitimately creates `godot_mcp_helpers/2/` inside the project it writes, so the
/// project under test is always a scratch copy.
fn install_project(into: &Path) -> PathBuf {
    let source = crate_fixtures().join("projects/canonical");
    let target = into.join("canonical");
    copy_tree(&source, &target);
    target
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("create the scratch project directory");
    let mut entries: Vec<PathBuf> = std::fs::read_dir(from)
        .unwrap_or_else(|e| panic!("read {}: {e}", from.display()))
        .map(|entry| entry.expect("directory entry").path())
        .collect();
    entries.sort();
    for path in entries {
        let target = to.join(path.file_name().expect("named entry"));
        if path.is_dir() {
            copy_tree(&path, &target);
        } else {
            std::fs::copy(&path, &target).expect("copy the fixture file");
        }
    }
}

/// Rewrite the fixture project's engine-behaviour declaration.
fn set_helper_mode(project: &Path, mode: &str) {
    std::fs::write(
        project.join("fake-engine.conf"),
        format!("helper={mode}\nuid=uid://bcanonicalfixture01\n"),
    )
    .expect("write the fixture engine configuration");
}

/// The capability set a Godot 4.4+ editor build advertises, as recorded by Todo
/// 13's committed release lock. The fake advertises the same set.
fn editor_capabilities() -> BTreeSet<String> {
    [
        "--help",
        "--version",
        "--verbose",
        "--headless",
        "--path",
        "--quit",
        "--quit-after",
        "--script",
        "--check-only",
        "--editor",
        "--import",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn uuid(serial: u32) -> String {
    format!("00000000-0000-4000-8000-{serial:012}")
}

// ---------------------------------------------------------------------------
// One durable store, exposed through both journal ports.
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

    fn write_value(
        &self,
        transaction_id: &str,
        phase: LaunchPhase,
        value: &serde_json::Value,
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
            let _ = file.sync_all();
        }
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

    fn value_of(&self, transaction_id: &str) -> Option<serde_json::Value> {
        let path = self
            .dir
            .join("transactions")
            .join(format!("{transaction_id}.json"));
        std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
    }

    /// Overwrite one stored launch record's typed proof, standing in for a
    /// tampered, stale or differently-terminated launch. Used only to build the
    /// composite-proof fixtures.
    fn rewrite_launch_proof(&self, transaction_id: &str, proof: &ProcessLaunchProof) {
        let mut value = self
            .value_of(transaction_id)
            .expect("the launch transaction is journaled");
        value["proof"] = proof.to_canonical_value();
        validate_canonical(&value).expect("a rewritten launch record stays canonical");
        let path = self
            .dir
            .join("transactions")
            .join(format!("{transaction_id}.json"));
        std::fs::write(&path, serde_json::to_vec(&value).expect("serialize")).expect("rewrite");
    }

    /// Remove one stored transaction, standing in for a launch whose durable
    /// record never became readable.
    fn forget(&self, transaction_id: &str) {
        let _ = std::fs::remove_file(
            self.dir
                .join("transactions")
                .join(format!("{transaction_id}.json")),
        );
    }

    fn effect_kinds(&self) -> BTreeMap<String, String> {
        self.stored_values()
            .into_iter()
            .filter_map(|value| {
                Some((
                    value["transaction_id"].as_str()?.to_owned(),
                    value["effect_kind"].as_str()?.to_owned(),
                ))
            })
            .collect()
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
        Ok(self.quarantined_ids())
    }
}

impl DurableTestJournal {
    fn launch_records(
        &self,
        accept: &dyn Fn(&str) -> bool,
    ) -> Result<Vec<LaunchRecord>, JournalError> {
        let mut out = Vec::new();
        for value in self.stored_values() {
            if value["effect_kind"] != serde_json::json!("process_launch") {
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

    fn quarantined_ids(&self) -> Vec<String> {
        self.stored_values()
            .into_iter()
            .filter(|value| value["phase"] == serde_json::json!("quarantined"))
            .filter_map(|value| {
                value["transaction_id"]
                    .as_str()
                    .map(std::borrow::ToOwned::to_owned)
            })
            .collect()
    }
}

impl FileJournal for DurableTestJournal {
    fn write_phase(&self, record: &FileRecord) -> Result<(), JournalError> {
        let value = record.to_canonical_value()?;
        for staged in &record.staged {
            let path = self.dir.join("staged").join(&staged.staging_id);
            // The staged bytes themselves live in the project's staging area; the
            // journal records their digest and length, which is what the canonical
            // schema carries.
            std::fs::write(&path, staged.sha256.as_bytes()).map_err(|error| {
                JournalError::Durability {
                    phase: record.phase,
                    reason: error.to_string(),
                }
            })?;
        }
        self.write_value(&record.envelope.transaction_id, record.phase, &value)
    }

    fn settle(
        &self,
        record: &FileRecord,
        disposition: Disposition,
        reason: &str,
        proof: Option<&FileProof>,
    ) -> Result<(), JournalError> {
        let value = transaction::canonical_terminal_value(record, disposition, reason, proof)?;
        self.write_value(&record.envelope.transaction_id, record.phase, &value)
    }

    fn open_transactions(&self) -> Result<Vec<FileRecord>, JournalError> {
        let mut out = Vec::new();
        for value in self.stored_values() {
            if value["effect_kind"] != serde_json::json!("file") {
                continue;
            }
            let phase = value["phase"].as_str().unwrap_or_default();
            if !LaunchPhase::ALL
                .iter()
                .any(|known| known.as_str() == phase && *known != LaunchPhase::CommittedFsynced)
            {
                continue;
            }
            out.push(FileRecord::from_canonical_value(&value)?);
        }
        Ok(out)
    }

    fn quarantined(&self) -> Result<Vec<String>, JournalError> {
        Ok(self.quarantined_ids())
    }
}

// ---------------------------------------------------------------------------
// Scenario wiring
// ---------------------------------------------------------------------------

struct Scenario {
    _root: tempfile::TempDir,
    journal_dir: PathBuf,
    work_dir: PathBuf,
    boundary_root: PathBuf,
    adapter: PathBuf,
    bin_dir: PathBuf,
    projects_dir: PathBuf,
    lease_state: PathBuf,
}

impl Scenario {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("scenario tempdir");
        let base = root.path().to_path_buf();
        let bin_dir = base.join("bin");
        std::fs::create_dir_all(&bin_dir).expect("bin dir");
        let launch_adapter = repo_root().join("tests/fixtures/launch/container-adapter.sh");
        let adapter = bin_dir.join("container-adapter.sh");
        std::fs::copy(&launch_adapter, &adapter).expect("install the boundary adapter");
        make_executable(&adapter);
        let journal_dir = base.join("journal");
        let work_dir = base.join("work");
        let boundary_root = base.join("boundaries");
        let projects_dir = base.join("projects");
        // The lease manager creates its own state directory and proves it is
        // user-private (0700), so this scenario must not pre-create it with the
        // process umask's permissions.
        let lease_state = base.join("lease-state");
        for dir in [&journal_dir, &work_dir, &boundary_root, &projects_dir] {
            std::fs::create_dir_all(dir).expect("scenario dir");
        }
        Self {
            _root: root,
            journal_dir,
            work_dir,
            boundary_root,
            adapter,
            bin_dir,
            projects_dir,
            lease_state,
        }
    }

    fn journal(&self) -> DurableTestJournal {
        DurableTestJournal::open(&self.journal_dir)
    }

    fn provider(&self) -> ContainerAdapterProvider {
        ContainerAdapterProvider::new(self.adapter.clone(), self.boundary_root.clone())
    }

    fn binding(&self, project: &Path) -> EngineBinding {
        let program = install_binary(&self.bin_dir);
        let mut allow = BTreeSet::new();
        allow.insert("PATH".to_owned());
        EngineBinding {
            binary_sha256: hex_sha256(&std::fs::read(&program).expect("engine bytes")),
            program,
            supervisor_binary: supervisor_binary(),
            project_root: project.to_path_buf(),
            work_dir: self.work_dir.clone(),
            environment: EnvironmentAllowlist {
                allow,
                extra: BTreeMap::new(),
            },
            caps: OutputCaps {
                stdout_bytes: 65_536,
                stderr_bytes: 65_536,
            },
            trust: TrustLevel::Untrusted,
            capabilities: editor_capabilities(),
        }
    }
}

/// One canonical envelope, minted the way the server layer does in production.
fn envelope(serial: u32, tool: &str, request: &CanonicalRequest) -> TransactionEnvelope {
    TransactionEnvelope {
        transaction_id: uuid(serial),
        project: ProjectIdentityClaim {
            object_identity: format!("project_{}", "1".repeat(64)),
            root_object: "unix:dev=1,ino=2".to_owned(),
            file_object: "unix:dev=1,ino=3".to_owned(),
            generation: 17,
        },
        transport_mode: TransportMode::Stdio,
        principal: None,
        session: None,
        operation_family: OperationFamily::ProjectMutation,
        tool_name: tool.to_owned(),
        idempotency_id: format!("idem-{serial}"),
        canonical_request_hash: request.canonical_request_hash(),
        canonical_request_bytes: request.canonical_bytes().len() as u64,
        opened_at_unix_ms: 1_700_000_000_000,
    }
}

fn clean_terminal(boundary: &str) -> ProcessLaunchProof {
    ProcessLaunchProof {
        boundary_identity: boundary.to_owned(),
        pid: Some(4242),
        start_time: Some("991122".to_owned()),
        boundary_membership: BoundaryMembership::Confirmed,
        release_status: ReleaseStatus::ExitedClean,
        exit_code: Some(0),
        signal: None,
        boundary_empty_after_exit: Some(true),
    }
}

// ---------------------------------------------------------------------------
// Happy path
// ---------------------------------------------------------------------------

#[test]
fn canonical_writes_happy_path() {
    let scenario = Scenario::new();
    let project = install_project(&scenario.projects_dir);
    set_helper_mode(&project, "ok");
    let journal = scenario.journal();
    let provider = scenario.provider();
    provider
        .available()
        .expect("the committed boundary adapter is available");

    let leases = LeaseManager::new(&scenario.lease_state);
    let lease = leases.acquire(&project).expect("the fixture is a project");
    let root: SafeRoot = lease.safe_root();

    let writer = CanonicalWriter::new(
        &journal,
        &root,
        EngineContext {
            launch_journal: &journal,
            provider: &provider,
            binding: scenario.binding(&project),
            deadline: Duration::from_secs(30),
        },
    );

    // -- 0. The bundled helper is pinned, not generated. ----------------------
    assert_eq!(
        hex_sha256(HelperScript::source().as_bytes()),
        HELPER_SHA256,
        "the helper shipped with this build is the pinned one"
    );
    assert_eq!(
        HelperScript::install_path(),
        "godot_mcp_helpers/2/canonical_write.gd",
        "the install path is versioned, so two server versions can coexist"
    );

    // -- 1. Creating a text scene goes through a journaled engine transaction. -
    let create = CanonicalRequest {
        root_type: Some("Node2D".to_owned()),
        root_name: Some("Created".to_owned()),
        ..CanonicalRequest::create("scenes/created.tscn")
    };
    let file_tx = envelope(1701, "canonical.write_resource", &create);
    let launch_tx = envelope(1702, "canonical.helper_launch", &create);
    let written = writer
        .execute(&create, &file_tx, &launch_tx)
        .expect("the canonical write completes");

    assert!(written.is_committed());
    assert_eq!(
        written.classification,
        StartupClassification::ExactPostState
    );
    assert_eq!(written.transaction_id, uuid(1701));
    assert_eq!(written.launch_transaction_id, uuid(1702));

    // The engine produced the bytes and the engine minted the UID. Neither is
    // derived in Rust.
    let text = String::from_utf8(written.bytes.clone()).expect("canonical text");
    assert!(
        text.starts_with("[gd_scene "),
        "the published bytes are engine-canonical TSCN text: {text:?}"
    );
    assert!(
        text.contains("type=\"Node2D\""),
        "the requested root type reached the engine: {text:?}"
    );
    assert_eq!(
        written.uid.as_deref(),
        Some("uid://bcanonicalfixture01"),
        "the UID comes from the engine, never from a Rust-side derivation"
    );
    assert!(
        text.contains("uid://bcanonicalfixture01"),
        "the engine-minted UID is registered in the resource itself: {text:?}"
    );

    // -- 2. Reopening it: the target really holds those bytes, and a second
    //       create is refused because the resource now exists. ---------------
    let on_disk = root
        .read(Path::new("scenes/created.tscn"))
        .expect("the created resource reopens through the lease");
    assert_eq!(
        on_disk, written.bytes,
        "the published target is byte-identical to the engine's artifact"
    );
    assert_eq!(
        written.proof.entries[0].sha256.as_deref(),
        Some(hex_sha256(&on_disk).as_str()),
        "the typed file proof records the digest actually on disk"
    );
    let again = envelope(1703, "canonical.write_resource", &create);
    let again_launch = envelope(1704, "canonical.helper_launch", &create);
    let refused = writer
        .execute(&create, &again, &again_launch)
        .expect_err("creating an existing resource is refused");
    assert!(
        matches!(refused, CanonicalError::ExternallyChanged { .. }),
        "a create over an existing resource fails closed: {refused}"
    );
    assert_eq!(refused.error_kind(), ErrorKind::Conflict);

    // -- 3. The transaction is a `file` effect and names its linked launch. ----
    let record = journal
        .value_of(&uuid(1701))
        .expect("the file transaction is journaled");
    assert_eq!(record["effect_kind"], serde_json::json!("file"));
    assert_eq!(record["effect_boundary"], serde_json::json!("file"));
    assert_ne!(
        record["effect_kind"],
        serde_json::json!("editor_runtime"),
        "there is no live editor session here, so editor_runtime would be a false claim"
    );
    assert_eq!(record["phase"], serde_json::json!("committed_fsynced"));
    assert_eq!(
        record["required_subproofs"]["process_launch_transaction_id"],
        serde_json::json!(uuid(1702)),
        "the file transaction names the helper-launch transaction its commit required"
    );
    assert_eq!(record["proof"]["kind"], serde_json::json!("file"));
    assert_eq!(
        record["pre_hashes"][0]["existence"],
        serde_json::json!("absent"),
        "the pre-state was recorded before anything existed"
    );
    assert_eq!(
        record["intended_post_hashes"][0]["sha256"],
        serde_json::json!(hex_sha256(&on_disk)),
        "the intended post-state was recorded before the publish"
    );
    assert_eq!(
        record["staged_write_set"][0]["sha256"],
        serde_json::json!(hex_sha256(&on_disk)),
        "the staged engine artifact is what got published"
    );
    let staged_path = record["staged_write_set"][0]["path"]
        .as_str()
        .expect("a staged path");
    assert!(
        staged_path.starts_with("godot_mcp_helpers/2/staging/"),
        "the helper only ever wrote inside its own versioned staging area: {staged_path}"
    );
    assert!(
        record["touched_paths"]
            .as_array()
            .expect("touched paths")
            .iter()
            .any(|path| path == &serde_json::json!("scenes/created.tscn")),
        "the touched path identities include the target"
    );

    // -- 4. The linked launch transaction independently proves a clean exit. ---
    let launch = journal
        .value_of(&uuid(1702))
        .expect("the helper launch is journaled");
    assert_eq!(launch["effect_kind"], serde_json::json!("process_launch"));
    assert_eq!(launch["phase"], serde_json::json!("committed_fsynced"));
    let proof = ProcessLaunchProof::from_canonical_value(&launch["proof"])
        .expect("the launch carries a process_launch proof");
    assert!(
        proof.is_clean_terminal_process(),
        "the linked proof shows confirmed containment, a clean exit 0, no signal and an empty \
         boundary: {proof:?}"
    );
    assert_eq!(proof.exit_code, Some(0));
    assert_eq!(proof.signal, None);
    assert_eq!(proof.boundary_empty_after_exit, Some(true));
    assert_eq!(proof.release_status, ReleaseStatus::ExitedClean);
    assert_eq!(proof.boundary_membership, BoundaryMembership::Confirmed);

    // One store, two effect kinds. There is no second persistence layer.
    let kinds = journal.effect_kinds();
    assert_eq!(kinds.get(&uuid(1701)).map(String::as_str), Some("file"));
    assert_eq!(
        kinds.get(&uuid(1702)).map(String::as_str),
        Some("process_launch")
    );

    // -- 5. The invocation went through the launch protocol as an argument array.
    assert!(
        written
            .argv
            .windows(2)
            .any(|pair| pair[0] == "--script" && pair[1] == HelperScript::script_argument()),
        "the helper was run with --script res://…: {:?}",
        written.argv
    );
    assert!(
        written.argv.contains(&"--".to_owned()),
        "user arguments follow a bare -- so they reach OS.get_cmdline_user_args(): {:?}",
        written.argv
    );
    assert!(
        written.argv.contains(&"--headless".to_owned()),
        "--headless is explicit: {:?}",
        written.argv
    );
    assert!(
        !written
            .argv
            .iter()
            .any(|argument| argument == "scenes/created.tscn"
                || argument.ends_with("/scenes/created.tscn")),
        "the target path is never handed to the helper as an artifact to write: {:?}",
        written.argv
    );
    assert!(
        written
            .argv
            .iter()
            .all(|argument| !argument.contains("&&") && !argument.contains(';')),
        "arguments are an array, never a shell string: {:?}",
        written.argv
    );

    // -- 6. Updating an existing text resource preserves loadability. ----------
    let update = CanonicalRequest {
        resource_name: Some("Renamed".to_owned()),
        ..CanonicalRequest::update("data/existing.tres")
    };
    let before = root
        .read(Path::new("data/existing.tres"))
        .expect("the fixture resource exists");
    let updated = writer
        .execute(
            &update,
            &envelope(1705, "canonical.write_resource", &update),
            &envelope(1706, "canonical.helper_launch", &update),
        )
        .expect("the update completes");
    assert!(updated.is_committed());
    let after = root
        .read(Path::new("data/existing.tres"))
        .expect("the updated resource reopens");
    assert_ne!(after, before, "the update actually changed the resource");
    let after_text = String::from_utf8(after.clone()).expect("canonical text");
    assert!(
        after_text.starts_with("[gd_resource "),
        "the update produced engine-canonical TRES text: {after_text:?}"
    );
    assert!(
        after_text.contains("resource_name = \"Renamed\""),
        "the requested property reached the engine: {after_text:?}"
    );
    assert!(
        after_text.contains("uid://"),
        "the resource still carries a registered UID: {after_text:?}"
    );

    // -- 7. Registering a UID for an existing resource. -----------------------
    let register = CanonicalRequest::register_uid("scenes/existing.tscn");
    let registered = writer
        .execute(
            &register,
            &envelope(1707, "canonical.write_resource", &register),
            &envelope(1708, "canonical.helper_launch", &register),
        )
        .expect("the UID registration completes");
    assert!(registered.is_committed());
    assert_eq!(
        registered.uid.as_deref(),
        Some("uid://bcanonicalfixture01"),
        "the registered UID is the engine's, never a Rust-derived value"
    );

    // -- 8. A dry run changes no bytes at all. --------------------------------
    let dry = CanonicalRequest {
        dry_run: true,
        resource_name: Some("NeverPublished".to_owned()),
        ..CanonicalRequest::update("data/existing.tres")
    };
    let before_dry = root
        .read(Path::new("data/existing.tres"))
        .expect("the resource exists");
    let diff = writer
        .preview(
            &dry,
            &envelope(1709, "canonical.write_resource", &dry),
            &envelope(1710, "canonical.helper_launch", &dry),
        )
        .expect("the dry run completes");
    assert!(diff.changes_bytes(), "the diff reports a real change");
    assert_eq!(
        diff.before.sha256.as_deref(),
        Some(hex_sha256(&before_dry).as_str())
    );
    assert!(
        diff.after
            .sha256
            .as_deref()
            .is_some_and(|digest| digest != hex_sha256(&before_dry)),
        "the diff's after-state differs from the current bytes"
    );
    assert_eq!(
        root.read(Path::new("data/existing.tres"))
            .expect("the resource still exists"),
        before_dry,
        "a dry run publishes nothing: the target's bytes are unchanged"
    );
    assert!(
        String::from_utf8_lossy(&diff.staged_bytes).contains("NeverPublished"),
        "the diff carries the engine-produced bytes it would have published"
    );

    // -- 9. The helper's own scratch documents are cleaned up, and the pinned
    //       script is installed exactly once. ---------------------------------
    assert!(
        project.join(HelperScript::install_path()).is_file(),
        "the pinned helper is installed in the project"
    );
    assert_eq!(
        std::fs::read(project.join(HelperScript::install_path())).expect("read the helper"),
        HelperScript::source().as_bytes(),
        "the installed helper is byte-identical to the pinned bytes"
    );
    let staging = project.join("godot_mcp_helpers/2/staging");
    let leftovers: Vec<PathBuf> = std::fs::read_dir(&staging)
        .expect("the staging directory exists")
        .flatten()
        .map(|entry| entry.path())
        .collect();
    assert!(
        leftovers.is_empty(),
        "every run's staged artifact is discarded afterwards: {leftovers:?}"
    );

    // -- 10. Nothing is quarantined, so the next mutation is permitted. -------
    assert!(
        FileJournal::quarantined(&journal)
            .expect("readable journal")
            .is_empty()
    );
    writer
        .ensure_mutation_permitted()
        .expect("a clean journal permits the next mutation");
}

// ---------------------------------------------------------------------------
// Failure paths
// ---------------------------------------------------------------------------

#[test]
fn canonical_writes_failure_paths() {
    let scenario = Scenario::new();
    let project = install_project(&scenario.projects_dir);
    set_helper_mode(&project, "ok");
    let journal = scenario.journal();
    let provider = scenario.provider();
    provider
        .available()
        .expect("the committed boundary adapter is available");

    let leases = LeaseManager::new(&scenario.lease_state);
    let lease = leases.acquire(&project).expect("the fixture is a project");
    let root: SafeRoot = lease.safe_root();

    let writer = CanonicalWriter::new(
        &journal,
        &root,
        EngineContext {
            launch_journal: &journal,
            provider: &provider,
            binding: scenario.binding(&project),
            deadline: Duration::from_secs(30),
        },
    );

    // -- 1. `.scn` / `.res` mutation is refused before anything happens. -------
    for binary in ["scenes/binary.scn", "art/binary.res"] {
        let before = std::fs::read(project.join(binary)).expect("the binary fixture exists");
        let request = CanonicalRequest::update(binary);
        let error = writer
            .execute(
                &request,
                &envelope(1720, "canonical.write_resource", &request),
                &envelope(1721, "canonical.helper_launch", &request),
            )
            .expect_err("a binary resource is never mutated");
        assert!(
            matches!(error, CanonicalError::BinaryResource { .. }),
            "{binary} is refused as a binary resource: {error}"
        );
        assert_eq!(error.error_kind(), ErrorKind::InvalidInput);
        assert!(!error.blocks_mutation());
        assert_eq!(
            std::fs::read(project.join(binary)).expect("still there"),
            before,
            "{binary} is byte-unchanged: a binary resource is never rewritten as text"
        );
        assert!(
            journal.value_of(&uuid(1720)).is_none(),
            "a refusal decidable from the request alone journals nothing"
        );
    }

    // The helper itself refuses a binary target too, so the refusal holds even if
    // a caller reached it by another route.
    assert!(
        HelperScript::source().contains("BINARY_EXTENSIONS"),
        "the bundled helper refuses binary resources on its own"
    );

    // -- 2. A duplicate / conflicting UID fails closed. ------------------------
    set_helper_mode(&project, "uid-conflict:scenes/existing.tscn");
    let conflict = CanonicalRequest::register_uid("data/existing.tres");
    let before_conflict = root
        .read(Path::new("data/existing.tres"))
        .expect("the resource exists");
    let error = writer
        .execute(
            &conflict,
            &envelope(1722, "canonical.write_resource", &conflict),
            &envelope(1723, "canonical.helper_launch", &conflict),
        )
        .expect_err("a conflicting UID is never registered");
    match &error {
        CanonicalError::UidConflict {
            uid,
            existing,
            requested,
        } => {
            assert_eq!(uid, "uid://bcanonicalfixture01");
            assert_eq!(existing, "res://scenes/existing.tscn");
            assert_eq!(requested, "res://data/existing.tres");
        }
        other => panic!("a uid_conflict must surface as a UID conflict, got {other}"),
    }
    assert_eq!(error.error_kind(), ErrorKind::Conflict);
    assert_eq!(
        root.read(Path::new("data/existing.tres"))
            .expect("still there"),
        before_conflict,
        "a refused UID registration publishes nothing"
    );
    let conflict_record = journal
        .value_of(&uuid(1722))
        .expect("the attempt is journaled");
    assert_ne!(
        conflict_record["phase"],
        serde_json::json!("committed_fsynced"),
        "a refused write never reaches committed_fsynced"
    );

    // -- 3. An interrupted non-atomic replacement never leaves a mixed target. -
    //
    // The fixture writes half its artifact and still exits 0. The publish is a
    // compare-and-swap, so the target ends up holding either its exact pre-state
    // or exactly the staged bytes — never a splice of the two.
    set_helper_mode(&project, "partial-write");
    let partial = CanonicalRequest {
        resource_name: Some("Partial".to_owned()),
        ..CanonicalRequest::update("data/existing.tres")
    };
    let before_partial = root
        .read(Path::new("data/existing.tres"))
        .expect("the resource exists");
    let partial_result = writer.execute(
        &partial,
        &envelope(1724, "canonical.write_resource", &partial),
        &envelope(1725, "canonical.helper_launch", &partial),
    );
    let after_partial = root
        .read(Path::new("data/existing.tres"))
        .expect("the resource still exists");
    match &partial_result {
        Ok(write) => {
            assert_eq!(
                after_partial, write.bytes,
                "the target holds exactly the staged artifact, never a partial splice"
            );
            assert!(
                after_partial.len() < before_partial.len(),
                "the truncated artifact really is shorter than what it replaced, so a \
                 write-in-place would have left a tail of the old bytes behind"
            );
            assert!(
                !after_partial
                    .windows(before_partial.len().min(16))
                    .any(|window| window == &before_partial[..before_partial.len().min(16)]),
                "no run of the previous bytes survived the atomic replacement"
            );
            assert_eq!(
                write.proof.entries[0].sha256.as_deref(),
                Some(hex_sha256(&after_partial).as_str()),
                "the typed file proof records the bytes actually on disk, partial artifact and \
                 all — it never claims a state the disk does not hold"
            );
        }
        Err(error) => {
            assert_eq!(
                after_partial, before_partial,
                "a refused publish leaves the exact pre-state: {error}"
            );
        }
    }
    assert!(
        after_partial == before_partial
            || after_partial
                == partial_result
                    .as_ref()
                    .map(|write| write.bytes.clone())
                    .unwrap_or_default(),
        "the target is one of the two exact states and never anything in between"
    );

    // -- 4. A build that silently ignores `--script` is inconclusive, not a pass.
    set_helper_mode(&project, "silent-ignore");
    let ignored = CanonicalRequest::create("scenes/ignored.tscn");
    let error = writer
        .execute(
            &ignored,
            &envelope(1726, "canonical.write_resource", &ignored),
            &envelope(1727, "canonical.helper_launch", &ignored),
        )
        .expect_err("a silently ignored --script is never a successful write");
    assert!(
        matches!(error, CanonicalError::HelperInconclusive { .. }),
        "an exit-0 no-op is inconclusive, not a pass: {error}"
    );
    assert_eq!(error.error_kind(), ErrorKind::Uncertain);
    assert!(
        !project.join("scenes/ignored.tscn").exists(),
        "nothing was published"
    );

    // -- 5. A helper that fails is a refusal, and a helper that answers nothing
    //       is inconclusive. ---------------------------------------------------
    set_helper_mode(&project, "save-failed");
    let failed = CanonicalRequest::create("scenes/failed.tscn");
    let error = writer
        .execute(
            &failed,
            &envelope(1728, "canonical.write_resource", &failed),
            &envelope(1729, "canonical.helper_launch", &failed),
        )
        .expect_err("a failed helper never publishes");
    assert!(
        matches!(
            error,
            CanonicalError::HelperRefused { .. } | CanonicalError::HelperInconclusive { .. }
        ),
        "a helper exit 70 is a refusal or an inconclusive run, never a success: {error}"
    );
    assert!(!project.join("scenes/failed.tscn").exists());

    set_helper_mode(&project, "no-response");
    let mute = CanonicalRequest::create("scenes/mute.tscn");
    let error = writer
        .execute(
            &mute,
            &envelope(1730, "canonical.write_resource", &mute),
            &envelope(1731, "canonical.helper_launch", &mute),
        )
        .expect_err("a helper that answers nothing cannot be interpreted");
    assert!(
        matches!(error, CanonicalError::HelperInconclusive { .. }),
        "no response document means the run is uninterpretable: {error}"
    );
    assert_eq!(error.error_kind(), ErrorKind::Uncertain);
    assert!(!project.join("scenes/mute.tscn").exists());

    // -- 6. An editor-open resource is never overwritten from outside. ---------
    set_helper_mode(&project, "ok");
    let open = CanonicalRequest {
        open_resources: vec!["res://data/existing.tres".to_owned()],
        ..CanonicalRequest::update("data/existing.tres")
    };
    let before_open = root
        .read(Path::new("data/existing.tres"))
        .expect("the resource exists");
    let error = writer
        .execute(
            &open,
            &envelope(1732, "canonical.write_resource", &open),
            &envelope(1733, "canonical.helper_launch", &open),
        )
        .expect_err("an open resource is never overwritten");
    assert!(matches!(error, CanonicalError::ResourceOpen { .. }));
    assert_eq!(error.error_kind(), ErrorKind::Conflict);
    assert_eq!(
        root.read(Path::new("data/existing.tres"))
            .expect("still there"),
        before_open
    );

    // -- 7. An externally changed resource is never overwritten. ---------------
    //
    // The publish is a compare-and-swap against the preimage captured before the
    // helper ran, and the domain layer fails closed when that preimage no longer
    // matches. There is no force-overwrite path to fall back to.
    let stale = root
        .cas_write(
            Path::new("data/existing.tres"),
            Some(b"bytes that were never on disk"),
            b"[gd_resource type=\"Resource\" format=3]\n",
        )
        .expect_err("a stale preimage is refused");
    let mapped: CanonicalError = stale.into();
    assert!(
        matches!(mapped, CanonicalError::ExternallyChanged { .. }),
        "a stale preimage surfaces as an external change: {mapped}"
    );
    assert_eq!(mapped.error_kind(), ErrorKind::Conflict);
    assert_eq!(
        root.read(Path::new("data/existing.tres"))
            .expect("still there"),
        before_open,
        "the refused compare-and-swap changed nothing"
    );

    // An update whose target vanished, and a create whose target appeared, both
    // fail closed as conflicts rather than being forced through.
    let vanished = CanonicalRequest::update("data/absent.tres");
    let error = writer
        .execute(
            &vanished,
            &envelope(1734, "canonical.write_resource", &vanished),
            &envelope(1735, "canonical.helper_launch", &vanished),
        )
        .expect_err("updating an absent resource is refused");
    assert!(matches!(error, CanonicalError::ExternallyChanged { .. }));

    // -- 8. Escaping and helper-owned targets are refused. --------------------
    for (target, label) in [
        ("../outside.tscn", "a parent-directory escape"),
        ("/etc/passwd.tscn", "an absolute path"),
        (
            "godot_mcp_helpers/2/staging/sneaky.tscn",
            "the helper's own staging area",
        ),
        ("scripts/player.gd", "a non-resource file"),
    ] {
        let request = CanonicalRequest::update(target);
        let error = writer
            .execute(
                &request,
                &envelope(1736, "canonical.write_resource", &request),
                &envelope(1737, "canonical.helper_launch", &request),
            )
            .expect_err("an unwritable target is refused");
        assert!(
            matches!(
                error,
                CanonicalError::UnusablePath { .. } | CanonicalError::NotATextResource { .. }
            ),
            "{label} is refused: {error}"
        );
        assert_eq!(error.error_kind(), ErrorKind::InvalidInput);
    }

    // -- 9. The composite proof rule: matching post-hash bytes with a linked
    //       proof that is missing / mismatched / live / signaled / nonzero /
    //       non-empty-boundary stay quarantined. ------------------------------
    set_helper_mode(&project, "ok");
    let composite = CanonicalRequest::create("scenes/composite.tscn");
    let composite_file = envelope(1740, "canonical.write_resource", &composite);
    let composite_launch = envelope(1741, "canonical.helper_launch", &composite);
    let committed = writer
        .execute(&composite, &composite_file, &composite_launch)
        .expect("the reference write commits with a clean linked proof");
    assert!(committed.is_committed());

    let record = FileRecord::from_canonical_value(
        &journal
            .value_of(&uuid(1740))
            .expect("the file transaction is journaled"),
    )
    .expect("a canonical file record");
    assert_eq!(
        record.required_launch_transaction_id.as_deref(),
        Some(uuid(1741).as_str())
    );
    let on_disk = root
        .read(Path::new("scenes/composite.tscn"))
        .expect("the resource is on disk");
    let observed_post = vec![FileEntry::present(
        "scenes/composite.tscn",
        FileOperation::Create,
        &on_disk,
    )];
    assert!(
        record
            .intended_for("scenes/composite.tscn")
            .expect("an intended post-state")
            .matches(&observed_post[0]),
        "the file bytes on disk match the intended post-state exactly"
    );

    // The reference case: both halves hold, so it commits.
    let clean = LinkedProof {
        transaction_id: uuid(1741),
        proof: clean_terminal("container:tx-1741"),
    };
    assert_eq!(
        transaction::classify_startup(&record, &observed_post, Some(&clean)),
        StartupClassification::ExactPostState
    );

    // Every way the linked proof can fail to be a clean terminal process. The
    // bytes are identical in every one of these cases.
    let mut unclean: Vec<(&str, Option<LinkedProof>)> = vec![("missing", None)];

    let mut mismatched = clean.clone();
    mismatched.transaction_id = uuid(1799);
    unclean.push(("mismatched", Some(mismatched)));

    let mut live = clean.clone();
    live.proof.release_status = ReleaseStatus::ResumedContained;
    live.proof.exit_code = None;
    live.proof.boundary_empty_after_exit = Some(false);
    unclean.push(("live-survivor", Some(live)));

    let mut suspended = clean.clone();
    suspended.proof.release_status = ReleaseStatus::Suspended;
    suspended.proof.exit_code = None;
    unclean.push(("suspended", Some(suspended)));

    let mut signaled = clean.clone();
    signaled.proof.release_status = ReleaseStatus::Signaled;
    signaled.proof.signal = Some("SIGKILL".to_owned());
    signaled.proof.exit_code = None;
    unclean.push(("signaled", Some(signaled)));

    let mut nonzero = clean.clone();
    nonzero.proof.release_status = ReleaseStatus::NonzeroExit;
    nonzero.proof.exit_code = Some(70);
    unclean.push(("nonzero-exit", Some(nonzero)));

    let mut unconfirmed = clean.clone();
    unconfirmed.proof.boundary_membership = BoundaryMembership::Unconfirmed;
    unclean.push(("unconfirmed-membership", Some(unconfirmed)));

    let mut occupied = clean.clone();
    occupied.proof.boundary_empty_after_exit = Some(false);
    unclean.push(("non-empty-boundary", Some(occupied)));

    let mut unknown = clean.clone();
    unknown.proof.release_status = ReleaseStatus::Unknown;
    unknown.proof.exit_code = None;
    unclean.push(("unknown-release", Some(unknown)));

    for (label, linked) in &unclean {
        let classification =
            transaction::classify_startup(&record, &observed_post, linked.as_ref());
        assert!(
            matches!(classification, StartupClassification::Quarantine { .. }),
            "{label}: matching file bytes must never commit without a clean terminal linked \
             proof, got {classification}"
        );
        assert_eq!(classification.disposition(), Disposition::Quarantined);
        assert!(classification.blocks_mutation(), "{label} blocks mutation");

        // Neither repair can turn this into a settled state: acknowledging a safe
        // pre-state would assert a state the disk contradicts, and compensating
        // would assert that nothing happened.
        let acknowledged =
            transaction::acknowledge_safe_state(&record, &observed_post, linked.as_ref());
        assert!(
            matches!(acknowledged, Err(CanonicalError::RepairRefused { .. })),
            "{label} cannot be repaired via acknowledge-safe-state"
        );
        let compensated = transaction::compensate(&record, &observed_post, linked.as_ref());
        assert!(
            matches!(compensated, Err(CanonicalError::RepairRefused { .. })),
            "{label} cannot be repaired via compensate"
        );
        assert_eq!(
            acknowledged.expect_err("refused").error_kind(),
            ErrorKind::Uncertain
        );
    }

    // The same conclusion is reached through the production reconcile path, from
    // what is actually durable — not from a hand-built proof.
    journal.rewrite_launch_proof(&uuid(1741), &{
        let mut tampered = clean_terminal("container:tx-1741");
        tampered.release_status = ReleaseStatus::NonzeroExit;
        tampered.exit_code = Some(70);
        tampered
    });
    let reconciled = writer
        .reconcile(&record)
        .expect("the durable state is readable");
    assert!(
        matches!(reconciled, StartupClassification::Quarantine { .. }),
        "reading the durable proof back yields quarantine, not a commit: {reconciled}"
    );

    // -- 10. A quarantined transaction blocks the next mutation until repaired. -
    let disposition = writer
        .settle_reconciled(&record, &reconciled)
        .expect("the quarantine is durable");
    assert_eq!(disposition, Disposition::Quarantined);
    let blocked = FileJournal::quarantined(&journal).expect("readable journal");
    assert_eq!(blocked, vec![uuid(1740)]);
    let error = writer
        .ensure_mutation_permitted()
        .expect_err("a quarantine blocks the next mutation");
    assert!(matches!(error, CanonicalError::MutationBlocked { .. }));
    assert_eq!(error.error_kind(), ErrorKind::Conflict);
    assert!(error.blocks_mutation());

    let next = CanonicalRequest::create("scenes/after-quarantine.tscn");
    let error = writer
        .execute(
            &next,
            &envelope(1742, "canonical.write_resource", &next),
            &envelope(1743, "canonical.helper_launch", &next),
        )
        .expect_err("no new mutation starts while a quarantine is outstanding");
    assert!(matches!(error, CanonicalError::MutationBlocked { .. }));
    assert!(
        !project.join("scenes/after-quarantine.tscn").exists(),
        "the blocked mutation wrote nothing"
    );

    // -- 11. Fault injection at every stage yields exactly one of three terminal
    //        classifications. ---------------------------------------------------
    //
    // Each `LaunchPhase` is a crash point: a record durable at that phase is
    // exactly what a restarted process finds. For every phase, every on-disk state
    // and every linked-proof state, the classification must be one of the three —
    // and an unproven success must never be reachable.
    let mut seen: BTreeSet<&'static str> = BTreeSet::new();
    let pre_state = vec![FileEntry::absent(
        "scenes/composite.tscn",
        FileOperation::Create,
    )];
    let foreign = vec![FileEntry::present(
        "scenes/composite.tscn",
        FileOperation::Create,
        b"bytes nobody recorded",
    )];
    let gone = LinkedProof {
        transaction_id: uuid(1741),
        proof: {
            let mut proof = clean_terminal("container:tx-1741");
            proof.release_status = ReleaseStatus::NonzeroExit;
            proof.exit_code = Some(65);
            proof
        },
    };
    for phase in LaunchPhase::ALL {
        let mut staged_record = record.clone();
        staged_record.phase = phase;
        for (state_label, observed) in [
            ("exact-pre", &pre_state),
            ("exact-post", &observed_post),
            ("foreign", &foreign),
        ] {
            for (proof_label, linked) in [
                ("clean-terminal", Some(&clean)),
                ("terminal-nonzero", Some(&gone)),
                ("absent", None),
            ] {
                let classification =
                    transaction::classify_startup(&staged_record, observed, linked.copied_ref());
                let name: &'static str = match &classification {
                    StartupClassification::ExactPreState => "exact_pre_state",
                    StartupClassification::ExactPostState => "exact_post_state",
                    StartupClassification::Quarantine { .. } => "quarantine",
                };
                seen.insert(name);
                assert!(
                    matches!(
                        classification,
                        StartupClassification::ExactPreState
                            | StartupClassification::ExactPostState
                            | StartupClassification::Quarantine { .. }
                    ),
                    "{phase}/{state_label}/{proof_label} produced a fourth classification"
                );
                if name == "exact_post_state" {
                    assert_eq!(
                        proof_label, "clean-terminal",
                        "{phase}/{state_label}: a post-state commit is only reachable with a \
                         clean terminal linked proof"
                    );
                    assert_eq!(
                        state_label, "exact-post",
                        "{phase}: a post-state commit is only reachable with matching bytes"
                    );
                }
            }
        }
    }
    assert_eq!(
        seen,
        BTreeSet::from(["exact_post_state", "exact_pre_state", "quarantine"]),
        "all three terminal classifications are reachable, and only those three"
    );

    // -- 12. The helper's four exit statuses, and nothing optimistic outside them.
    assert_eq!(HelperExit::ALL.len(), 4);
    assert_eq!(HelperExit::classify(Some(0)), Some(HelperExit::Ok));
    assert_eq!(HelperExit::classify(Some(64)), Some(HelperExit::Usage));
    assert_eq!(HelperExit::classify(Some(65)), Some(HelperExit::Refused));
    assert_eq!(HelperExit::classify(Some(70)), Some(HelperExit::Failed));
    assert_eq!(
        HelperExit::classify(Some(1)),
        None,
        "an unrecognised status is never read as one of the helper's outcomes"
    );

    // -- 13. A launch whose durable record vanished leaves the file transaction
    //        unprovable rather than committed. --------------------------------
    journal.forget(&uuid(1741));
    let orphaned = writer
        .reconcile(&record)
        .expect("the durable state is readable");
    assert!(
        matches!(orphaned, StartupClassification::Quarantine { .. }),
        "with no readable linked proof the write is unprovable: {orphaned}"
    );
    assert_eq!(
        root.read(Path::new("scenes/composite.tscn"))
            .expect("the bytes are still there"),
        on_disk,
        "quarantine changes no bytes; it withholds a verdict"
    );
    assert_eq!(
        Existence::Present,
        record
            .intended_for("scenes/composite.tscn")
            .expect("an intended entry")
            .existence,
        "the intended post-state is still recorded, and still not proven"
    );
}

/// `Option<&T>` from `Option<&T>` without moving out of the loop's borrow.
trait CopiedRef<'a, T> {
    fn copied_ref(self) -> Option<&'a T>;
}

impl<'a, T> CopiedRef<'a, T> for Option<&'a T> {
    fn copied_ref(self) -> Option<&'a T> {
        self
    }
}
