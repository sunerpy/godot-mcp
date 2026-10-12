//! Todo 16 acceptance suite: disposable-copy execution and the explicit
//! untrusted-project posture.
//!
//! Nothing here needs a real Godot installation and nothing touches the network.
//! Every engine invocation runs the committed fake under
//! `tests/fixtures/disposable/binaries/`, against the committed project fixture
//! under `tests/fixtures/disposable/projects/untrusted/`, and every process is
//! created by Todo 14's gapless launch protocol inside the committed
//! container/VM adapter boundary — there is no second, bare `Command::spawn`
//! path.
//!
//! # This suite does not test a sandbox
//!
//! Read this before adding an assertion. Disposable execution is **mutation
//! containment**: it redirects which project tree an untrusted run may change.
//! It does not confine what the process can do. An absolute-path write, a
//! network connection or a read of any file the user's account can read are all
//! still possible, and none of them is asserted against here — asserting them
//! would be asserting something false.
//!
//! Two tests are named by `qa/tasks/16.json` and are the only entry points the
//! QA descriptor invokes:
//!
//! * `disposable_execution_happy_path`
//! * `disposable_execution_failure_paths`

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
use std::sync::Mutex;
use std::time::Duration;

use godot_mcp_domain::ErrorKind;
use godot_mcp_engine::disposable::exclusion::{ExclusionPolicy, ExclusionReason};
use godot_mcp_engine::disposable::posture::{NOT_A_SANDBOX, UNCONTAINED_BY_A_COPY};
use godot_mcp_engine::disposable::session::{DisposableRequest, DisposableSession};
use godot_mcp_engine::disposable::{DisposableError, digest_source_tree};
use godot_mcp_engine::supervision::adapter::ContainerAdapterProvider;
use godot_mcp_engine::supervision::boundary::{
    BoundaryKind, BoundaryProvider, BoundaryStrength, TrustLevel,
};
use godot_mcp_engine::supervision::journal::{
    Disposition, JournalError, LaunchJournal, LaunchPhase, LaunchRecord, OperationFamily,
    ProcessLaunchProof, ProjectIdentityClaim, TransactionEnvelope, TransportMode, hex_sha256,
    validate_canonical,
};
use godot_mcp_engine::supervision::protocol::{EnvironmentAllowlist, OutputCaps};
use godot_mcp_engine::supervision::weak_unix::UnixLocatorProvider;
use godot_mcp_engine::validation::import::ImportRequest;
use godot_mcp_engine::validation::outcome::Classification;
use godot_mcp_engine::validation::runner::{EngineBinding, TransactionSource};

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
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/disposable")
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
        .join(format!("godot-untrusted.{extension}"));
    let bytes = std::fs::read(&source).unwrap_or_else(|e| panic!("read {}: {e}", source.display()));
    let target = dir.join(format!("godot-untrusted.{extension}"));
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

/// Copy the committed untrusted fixture into a scratch source tree.
///
/// The committed fixture is never written to — the untrusted run deliberately
/// mutates the tree it is pointed at, so "the source" under test is always a
/// scratch copy. The fixture's `dot-git/` directory is renamed to `.git/` here
/// because git refuses to track any path with a `.git` component; the production
/// exclusion policy only ever sees the real name.
fn install_source(into: &Path) -> PathBuf {
    let source = crate_fixtures().join("projects/untrusted");
    let target = into.join("untrusted");
    copy_tree(&source, &target);
    std::fs::rename(target.join("dot-git"), target.join(".git"))
        .expect("rename the committed dot-git fixture to its real name");
    target
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("create the scratch directory");
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

/// The files the fixture carries that must never reach a disposable copy.
const EXCLUDED_FIXTURE_PATHS: [&str; 8] = [
    ".env",
    ".env.production",
    "private.pem",
    "credentials.json",
    "export_presets.cfg",
    ".godot/project.godot",
    ".godot/uid_cache.bin",
    ".git/config",
];

// ---------------------------------------------------------------------------
// A durable stand-in for Todo 6's store, identical in shape to Todo 14/15's.
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

    fn store(&self, value: &serde_json::Value, record: &LaunchRecord) -> Result<(), JournalError> {
        validate_canonical(value)?;
        for payload in &record.payloads {
            let staged = self.dir.join("staged").join(payload.staging_id());
            std::fs::write(&staged, &payload.bytes).map_err(|error| JournalError::Durability {
                phase: record.phase,
                reason: error.to_string(),
            })?;
        }
        let path = self
            .dir
            .join("transactions")
            .join(format!("{}.json", record.envelope.transaction_id));
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

    fn phases(&self) -> Vec<String> {
        self.stored_values()
            .into_iter()
            .filter_map(|value| value["phase"].as_str().map(std::borrow::ToOwned::to_owned))
            .collect()
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
        let value = godot_mcp_engine::supervision::journal::canonical_terminal_value(
            record,
            disposition,
            reason,
            proof,
        )?;
        self.store(&value, record)
    }

    fn open_launches(&self) -> Result<Vec<LaunchRecord>, JournalError> {
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

/// Mints one canonical envelope per engine invocation, the way the server layer
/// does in production.
struct Transactions {
    next: Mutex<u32>,
}

impl Transactions {
    fn new(base: u32) -> Self {
        Self {
            next: Mutex::new(base),
        }
    }

    fn issued(&self) -> u32 {
        *self.next.lock().expect("transaction counter")
    }
}

impl TransactionSource for Transactions {
    fn next(&self, tool_name: &str, family: OperationFamily) -> TransactionEnvelope {
        let mut guard = self.next.lock().expect("transaction counter");
        *guard += 1;
        let suffix = format!("{:0>12}", *guard);
        TransactionEnvelope {
            transaction_id: format!("00000000-0000-4000-8000-{suffix}"),
            project: ProjectIdentityClaim {
                object_identity: format!("project_{}", "e".repeat(64)),
                root_object: "unix:dev=1,ino=2".to_owned(),
                file_object: "unix:dev=1,ino=3".to_owned(),
                generation: 16,
            },
            transport_mode: TransportMode::Stdio,
            principal: None,
            session: None,
            operation_family: family,
            tool_name: tool_name.to_owned(),
            idempotency_id: format!("idem-{suffix}"),
            canonical_request_hash: "f".repeat(64),
            canonical_request_bytes: 64,
            opened_at_unix_ms: 1_700_000_000_000,
        }
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
    sources_dir: PathBuf,
    disposable_dir: PathBuf,
    locator_dir: PathBuf,
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
        let sources_dir = base.join("sources");
        let disposable_dir = base.join("disposable");
        let locator_dir = base.join("locator");
        for dir in [
            &journal_dir,
            &work_dir,
            &boundary_root,
            &sources_dir,
            &disposable_dir,
            &locator_dir,
        ] {
            std::fs::create_dir_all(dir).expect("scenario dir");
        }
        Self {
            _root: root,
            journal_dir,
            work_dir,
            boundary_root,
            adapter,
            bin_dir,
            sources_dir,
            disposable_dir,
            locator_dir,
        }
    }

    fn journal(&self) -> DurableTestJournal {
        DurableTestJournal::open(&self.journal_dir)
    }

    fn strong_provider(&self) -> ContainerAdapterProvider {
        ContainerAdapterProvider::new(self.adapter.clone(), self.boundary_root.clone())
    }

    fn weak_provider(&self) -> UnixLocatorProvider {
        UnixLocatorProvider::new(self.locator_dir.clone(), supervisor_binary())
    }

    /// A binding whose `project_root` deliberately still names the *original*
    /// tree. Disposable execution must never use it: the session overrides it
    /// with the copy's root, which is what the argv assertions prove.
    fn binding(&self, original_root: &Path) -> EngineBinding {
        let program = install_binary(&self.bin_dir);
        let mut allow = BTreeSet::new();
        allow.insert("PATH".to_owned());
        EngineBinding {
            binary_sha256: hex_sha256(&std::fs::read(&program).expect("engine bytes")),
            program,
            supervisor_binary: supervisor_binary(),
            project_root: original_root.to_path_buf(),
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

/// Every file under `root`, as project-relative slash-separated paths.
fn tree_paths(root: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    collect(root, root, &mut out);
    out
}

fn collect(root: &Path, dir: &Path, out: &mut BTreeSet<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let kind = entry.file_type().expect("file type");
        if kind.is_dir() {
            collect(root, &path, out);
        } else {
            let relative = path
                .strip_prefix(root)
                .expect("entries live under the root")
                .to_string_lossy()
                .replace('\\', "/");
            out.insert(relative);
        }
    }
}

#[test]
fn disposable_import_refuses_invalid_artifact_paths_before_launch() {
    let scenario = Scenario::new();
    let source = install_source(&scenario.sources_dir);
    let strong = scenario.strong_provider();
    let session = DisposableSession::prepare(&DisposableRequest {
        source_root: source.clone(),
        disposable_parent: scenario.disposable_dir.join("invalid-artifacts"),
        trust: TrustLevel::Untrusted,
        exclusions: ExclusionPolicy::default(),
        providers: vec![&strong],
    })
    .expect("the disposable copy is prepared");
    let journal = scenario.journal();
    let transactions = Transactions::new(750);

    for artifact in ["/etc/passwd", "res://", "../x"] {
        let error = match session.import(
            &journal,
            &transactions,
            scenario.binding(&source),
            &ImportRequest {
                expected_artifacts: vec![artifact.to_owned()],
                deadline: Duration::from_secs(30),
            },
        ) {
            Ok(run) => panic!(
                "invalid disposable artifact {artifact:?} reached launch/classification: verdict={:?}, produced={:?}, missing={:?}",
                run.import.verdict, run.import.produced, run.import.missing
            ),
            Err(error) => error,
        };
        assert_eq!(error.error_kind(), ErrorKind::InvalidInput, "{artifact:?}");
        assert_eq!(
            transactions.issued(),
            750,
            "invalid disposable artifact {artifact:?} must be refused before launch"
        );
    }

    let absent = session
        .import(
            &journal,
            &transactions,
            scenario.binding(&source),
            &ImportRequest {
                expected_artifacts: vec!["x.import".to_owned()],
                deadline: Duration::from_secs(30),
            },
        )
        .expect("a valid but absent disposable artifact still launches");
    assert_eq!(
        absent.import.verdict.classification,
        Classification::Inconclusive
    );
    assert!(absent.import.produced.is_empty());
    assert_eq!(absent.import.missing, vec!["x.import".to_owned()]);
    assert_eq!(
        transactions.issued(),
        751,
        "the control launched exactly once"
    );
}

// ---------------------------------------------------------------------------
// Happy path
// ---------------------------------------------------------------------------

#[test]
fn disposable_execution_happy_path() {
    let scenario = Scenario::new();
    let source = install_source(&scenario.sources_dir);
    let policy = ExclusionPolicy::default();

    // The source starts out carrying every secret and every generated cache the
    // policy is supposed to keep out of a copy.
    let source_before = tree_paths(&source);
    for path in EXCLUDED_FIXTURE_PATHS {
        assert!(
            source_before.contains(path),
            "the fixture source carries {path}: {source_before:?}"
        );
    }
    let source_digest_before =
        digest_source_tree(&source, &policy).expect("the source tree can be digested");
    let project_godot_before =
        std::fs::read(source.join("project.godot")).expect("read the source project marker");

    // -- 1. Preparing a disposable copy selects a strong boundary first, then
    //       copies only what the policy admits. ------------------------------
    let strong = scenario.strong_provider();
    strong
        .available()
        .expect("the committed boundary adapter is available");
    let providers: Vec<&dyn BoundaryProvider> = vec![&strong];
    let session = DisposableSession::prepare(&DisposableRequest {
        source_root: source.clone(),
        disposable_parent: scenario.disposable_dir.join("run-a"),
        trust: TrustLevel::Untrusted,
        exclusions: policy.clone(),
        providers: providers.clone(),
    })
    .expect("a strong boundary is available, so the copy is permitted");

    assert_eq!(session.boundary().trust, TrustLevel::Untrusted);
    assert_eq!(session.boundary().strength, BoundaryStrength::Strong);
    assert_eq!(session.boundary().kind, BoundaryKind::ContainerAdapter);
    assert_eq!(
        session.source_tree_sha256(),
        source_digest_before,
        "the session records the source digest it observed"
    );

    // The copy contains the project, and none of the excluded material.
    let copy_paths = tree_paths(session.project_root());
    assert!(copy_paths.contains("project.godot"));
    assert!(copy_paths.contains("main.tscn"));
    assert!(copy_paths.contains("scripts/player.gd"));
    assert!(copy_paths.contains("scripts/game_state.gd"));
    for path in EXCLUDED_FIXTURE_PATHS {
        assert!(
            !copy_paths.contains(path),
            "{path} must never reach a disposable copy: {copy_paths:?}"
        );
    }
    assert!(
        !session.project_root().join(".godot").exists(),
        "`.godot/` is engine-generated cache and is never copied"
    );
    assert!(
        !session.project_root().join(".git").exists(),
        "version-control metadata is never copied"
    );

    // The manifest states the exclusions explicitly, with a reason each.
    let manifest = session.manifest();
    let excluded: BTreeMap<&str, ExclusionReason> = manifest
        .excluded
        .iter()
        .map(|entry| (entry.path.as_str(), entry.reason))
        .collect();
    assert_eq!(excluded.get(".env"), Some(&ExclusionReason::SecretMaterial));
    assert_eq!(
        excluded.get(".env.production"),
        Some(&ExclusionReason::SecretMaterial)
    );
    assert_eq!(
        excluded.get("private.pem"),
        Some(&ExclusionReason::SecretMaterial)
    );
    assert_eq!(
        excluded.get("credentials.json"),
        Some(&ExclusionReason::SecretMaterial)
    );
    assert_eq!(
        excluded.get("export_presets.cfg"),
        Some(&ExclusionReason::SecretMaterial)
    );
    assert_eq!(
        excluded.get(".godot/project.godot"),
        Some(&ExclusionReason::EngineGeneratedCache),
        "the engine's own copy of project.godot inside its cache is not source"
    );
    assert_eq!(
        excluded.get(".git/config"),
        Some(&ExclusionReason::VersionControlMetadata)
    );

    // Every copied file's source and copy hashes are recorded and agree.
    assert!(!manifest.copied.is_empty());
    for entry in &manifest.copied {
        assert_eq!(
            entry.source_sha256, entry.copy_sha256,
            "a copied file is byte-identical to its source: {}",
            entry.path
        );
        assert_eq!(entry.source_sha256.len(), 64);
    }
    assert!(
        manifest
            .copied
            .windows(2)
            .all(|pair| pair[0].path < pair[1].path),
        "manifest entries are sorted, so the manifest is byte-stable"
    );
    assert!(
        manifest
            .excluded
            .windows(2)
            .all(|pair| pair[0].path < pair[1].path)
    );

    // -- 2. The copy is deterministic: the same source and policy produce the
    //       same manifest and the same digests. -----------------------------
    let repeat = DisposableSession::prepare(&DisposableRequest {
        source_root: source.clone(),
        disposable_parent: scenario.disposable_dir.join("run-b"),
        trust: TrustLevel::Untrusted,
        exclusions: policy.clone(),
        providers: providers.clone(),
    })
    .expect("a second copy of the same source is permitted");
    assert_eq!(
        repeat.manifest().to_canonical_json(),
        manifest.to_canonical_json(),
        "the same input produces a byte-identical copy manifest"
    );
    assert_eq!(repeat.manifest().digest(), manifest.digest());
    assert_eq!(
        repeat.manifest().source_tree_sha256,
        manifest.source_tree_sha256
    );
    assert_eq!(
        repeat.manifest().copy_tree_sha256,
        manifest.copy_tree_sha256
    );
    assert_ne!(
        repeat.disposable_root(),
        session.disposable_root(),
        "two sessions never share a disposable root"
    );
    repeat.discard().expect("the second copy is discarded");

    // -- 3. The untrusted run executes against the copy, never the original. --
    let journal = scenario.journal();
    let transactions = Transactions::new(800);
    let run = session
        .import(
            &journal,
            &transactions,
            scenario.binding(&source),
            &ImportRequest {
                expected_artifacts: vec![".godot/uid_cache.bin".to_owned()],
                deadline: Duration::from_secs(30),
            },
        )
        .expect("the import runs inside the disposable copy");

    assert_eq!(run.import.verdict.classification, Classification::Passed);
    assert_eq!(run.import.verdict.error_kind(), None);
    assert_eq!(
        run.import.produced,
        vec![".godot/uid_cache.bin".to_owned()],
        "the import proof is the observed artifact, not the exit status"
    );
    let copy_root_argument = session.project_root().display().to_string();
    let source_argument = source.display().to_string();
    assert!(
        run.import.argv.contains(&copy_root_argument),
        "the engine is pointed at the disposable copy: {:?}",
        run.import.argv
    );
    assert!(
        !run.import.argv.contains(&source_argument),
        "the original tree is never handed to an untrusted run: {:?}",
        run.import.argv
    );
    assert_eq!(run.boundary.strength, BoundaryStrength::Strong);
    assert!(
        run.posture.contains("not a code sandbox"),
        "every run carries the posture disclaimer verbatim: {}",
        run.posture
    );

    // -- 4. The untrusted project mutated the copy, and only the copy. --------
    let copy_after = tree_paths(session.project_root());
    assert!(
        !copy_after.contains("main.tscn"),
        "the untrusted run deleted a file inside the copy: {copy_after:?}"
    );
    assert!(copy_after.contains(".godot/uid_cache.bin"));
    let saw = session.project_root().join("godot-mcp-saw-excluded.txt");
    let saw_text = std::fs::read_to_string(&saw).expect("the run reports what it could see");
    assert_eq!(
        saw_text, "",
        "the untrusted run could not see one excluded file: {saw_text:?}"
    );

    // The escape marker landed inside the disposable root, and nowhere else.
    let inside = session
        .disposable_root()
        .join("godot-mcp-escape-marker.txt");
    assert!(
        inside.is_file(),
        "the marker the untrusted run wrote above its project root stayed inside \
         the disposable root: {}",
        inside.display()
    );
    let beside_source = scenario.sources_dir.join("godot-mcp-escape-marker.txt");
    assert!(
        !beside_source.exists(),
        "no marker appears beside the original project: {}",
        beside_source.display()
    );
    assert!(
        !scenario
            .disposable_dir
            .join("godot-mcp-escape-marker.txt")
            .exists(),
        "no marker appears above the disposable root"
    );

    // -- 5. The source tree is byte-for-byte what it was. --------------------
    assert_eq!(
        digest_source_tree(&source, &policy).expect("re-digest the source"),
        source_digest_before,
        "validation on a copied fixture leaves the source hash unchanged"
    );
    assert_eq!(tree_paths(&source), source_before);
    assert_eq!(
        std::fs::read(source.join("project.godot")).expect("re-read the source project marker"),
        project_godot_before,
        "the in-place mutation the run performed landed in the copy only"
    );
    assert!(
        !source.join(".godot/uid_cache.bin").exists()
            || std::fs::read(source.join(".godot/uid_cache.bin")).expect("read cache")
                == b"godot-mcp fixture: stale engine uid cache, never source\n",
        "the source's own stale cache was not rewritten"
    );

    // -- 6. The invocation went through the launch protocol and committed. ----
    assert_eq!(transactions.issued(), 801, "exactly one engine invocation");
    let phases = journal.phases();
    assert_eq!(phases, vec!["committed_fsynced".to_owned()]);
    assert!(journal.open_launches().expect("reload").is_empty());

    // -- 7. Discarding removes the copy and leaves the source alone. ---------
    let root = session.disposable_root().to_path_buf();
    session.discard().expect("the disposable copy is discarded");
    assert!(!root.exists(), "the copy is gone: {}", root.display());
    assert_eq!(
        digest_source_tree(&source, &policy).expect("re-digest the source"),
        source_digest_before
    );

    // -- 8. The committed fixture itself was never written to. ---------------
    let committed = crate_fixtures().join("projects/untrusted");
    assert!(
        committed.join("dot-git/config").is_file(),
        "the committed fixture keeps its git-safe spelling"
    );
    assert!(
        !committed.join(".git").exists(),
        "the committed fixture is never renamed in place"
    );
    assert!(!committed.join("godot-mcp-saw-excluded.txt").exists());
}

// ---------------------------------------------------------------------------
// Failure paths
// ---------------------------------------------------------------------------

#[test]
fn disposable_execution_failure_paths() {
    // -- A. An untrusted run with only a weak boundary is refused before any
    //       copy exists, and never silently downgraded. ---------------------
    {
        let scenario = Scenario::new();
        let source = install_source(&scenario.sources_dir);
        let weak = scenario.weak_provider();
        assert_eq!(
            weak.strength(),
            BoundaryStrength::Weak,
            "the locator protocol is the weak path by construction"
        );
        let parent = scenario.disposable_dir.join("weak");
        let error = DisposableSession::prepare(&DisposableRequest {
            source_root: source.clone(),
            disposable_parent: parent.clone(),
            trust: TrustLevel::Untrusted,
            exclusions: ExclusionPolicy::default(),
            providers: vec![&weak],
        })
        .expect_err("a weak boundary is never sufficient for untrusted code");
        assert!(
            matches!(error, DisposableError::IsolationUnavailable { .. }),
            "{error:?}"
        );
        assert_eq!(error.error_kind(), ErrorKind::Unsupported);
        assert!(
            error.to_string().contains("weak"),
            "the refusal names the weak mechanism it declined: {error}"
        );
        assert!(
            !parent.exists(),
            "isolation is decided before anything is copied: {}",
            parent.display()
        );

        // No providers at all is the same refusal, not a fallback.
        let error = DisposableSession::prepare(&DisposableRequest {
            source_root: source.clone(),
            disposable_parent: scenario.disposable_dir.join("none"),
            trust: TrustLevel::Untrusted,
            exclusions: ExclusionPolicy::default(),
            providers: Vec::new(),
        })
        .expect_err("no boundary at all is a refusal");
        assert_eq!(error.error_kind(), ErrorKind::Unsupported);
    }

    // -- B. A symlink in the source is rejected during the copy, never
    //       followed, and leaves nothing behind. ----------------------------
    #[cfg(unix)]
    {
        let scenario = Scenario::new();
        let source = install_source(&scenario.sources_dir);
        let outside = scenario.sources_dir.join("outside-secret.txt");
        std::fs::write(&outside, b"godot-mcp fixture: outside the project\n").expect("write");
        std::os::unix::fs::symlink(&outside, source.join("escape.txt")).expect("symlink");

        let strong = scenario.strong_provider();
        let parent = scenario.disposable_dir.join("symlink");
        let error = DisposableSession::prepare(&DisposableRequest {
            source_root: source.clone(),
            disposable_parent: parent.clone(),
            trust: TrustLevel::Untrusted,
            exclusions: ExclusionPolicy::default(),
            providers: vec![&strong],
        })
        .expect_err("a symlink is rejected at copy time, not followed");
        assert!(
            matches!(error, DisposableError::SymlinkComponent { .. }),
            "{error:?}"
        );
        assert_eq!(error.error_kind(), ErrorKind::PermissionDenied);
        assert!(
            error.to_string().contains("escape.txt"),
            "the refusal names the offending entry: {error}"
        );
        assert!(
            !parent.exists(),
            "a refused copy leaves no partial tree behind: {}",
            parent.display()
        );
        assert_eq!(
            std::fs::read(&outside).expect("the outside file is untouched"),
            b"godot-mcp fixture: outside the project\n"
        );
    }

    // -- C. A source that is not a project is refused. -----------------------
    {
        let scenario = Scenario::new();
        let bare = scenario.sources_dir.join("bare");
        std::fs::create_dir_all(&bare).expect("bare dir");
        std::fs::write(bare.join("readme.txt"), b"no project marker here\n").expect("write");
        let strong = scenario.strong_provider();
        let error = DisposableSession::prepare(&DisposableRequest {
            source_root: bare,
            disposable_parent: scenario.disposable_dir.join("bare"),
            trust: TrustLevel::Untrusted,
            exclusions: ExclusionPolicy::default(),
            providers: vec![&strong],
        })
        .expect_err("a directory with no project.godot is not a project");
        assert!(
            matches!(error, DisposableError::NotAProject { .. }),
            "{error:?}"
        );
        assert_eq!(error.error_kind(), ErrorKind::NotFound);
    }

    // -- D. A run that outlives its deadline is a timeout, and the copy is
    //       still removed. -------------------------------------------------
    {
        let scenario = Scenario::new();
        let source = install_source(&scenario.sources_dir);
        std::fs::write(source.join("fake-engine.conf"), b"import_sleep_ms=5000\n")
            .expect("make the fake outlive a short deadline");
        let policy = ExclusionPolicy::default();
        let digest_before = digest_source_tree(&source, &policy).expect("digest");
        let strong = scenario.strong_provider();
        let session = DisposableSession::prepare(&DisposableRequest {
            source_root: source.clone(),
            disposable_parent: scenario.disposable_dir.join("timeout"),
            trust: TrustLevel::Untrusted,
            exclusions: policy.clone(),
            providers: vec![&strong],
        })
        .expect("the copy is permitted");
        let journal = scenario.journal();
        let transactions = Transactions::new(900);
        let run = session
            .import(
                &journal,
                &transactions,
                scenario.binding(&source),
                &ImportRequest {
                    expected_artifacts: vec![".godot/uid_cache.bin".to_owned()],
                    deadline: Duration::from_millis(300),
                },
            )
            .expect("a deadline is a report, not a launch failure");
        assert_eq!(run.import.verdict.classification, Classification::TimedOut);
        assert_eq!(run.import.verdict.error_kind(), Some(ErrorKind::Timeout));

        let root = session.disposable_root().to_path_buf();
        session.discard().expect("timeout cleanup removes the copy");
        assert!(
            !root.exists(),
            "the copy is removed after a timeout: {}",
            root.display()
        );
        assert_eq!(
            digest_source_tree(&source, &policy).expect("re-digest"),
            digest_before,
            "a timed-out run changed nothing in the source"
        );
        assert!(
            journal
                .phases()
                .iter()
                .all(|phase| phase != "committed_fsynced"),
            "a timed-out launch is compensated, never committed: {:?}",
            journal.phases()
        );
    }

    // -- E. Dropping a session without discarding still removes the copy. ----
    {
        let scenario = Scenario::new();
        let source = install_source(&scenario.sources_dir);
        let strong = scenario.strong_provider();
        let root = {
            let session = DisposableSession::prepare(&DisposableRequest {
                source_root: source,
                disposable_parent: scenario.disposable_dir.join("dropped"),
                trust: TrustLevel::Untrusted,
                exclusions: ExclusionPolicy::default(),
                providers: vec![&strong],
            })
            .expect("the copy is permitted");
            session.disposable_root().to_path_buf()
        };
        assert!(
            !root.exists(),
            "an abandoned session discards its copy: {}",
            root.display()
        );
    }

    // -- F. The posture text refuses the word "sandbox" as a claim, in the
    //       shipped constants and in the module documentation. --------------
    {
        assert!(
            NOT_A_SANDBOX.contains("not a code sandbox"),
            "{NOT_A_SANDBOX}"
        );
        assert!(
            NOT_A_SANDBOX.contains("mutation containment"),
            "{NOT_A_SANDBOX}"
        );
        assert!(
            UNCONTAINED_BY_A_COPY.len() >= 3,
            "the uncontained risks are enumerated, not summarised"
        );
        for risk in UNCONTAINED_BY_A_COPY {
            assert!(!risk.trim().is_empty());
        }
        assert!(
            UNCONTAINED_BY_A_COPY
                .iter()
                .any(|risk| risk.contains("absolute")),
            "an absolute-path write is named as uncontained: {UNCONTAINED_BY_A_COPY:?}"
        );
        assert!(
            UNCONTAINED_BY_A_COPY
                .iter()
                .any(|risk| risk.contains("network")),
            "a network connection is named as uncontained: {UNCONTAINED_BY_A_COPY:?}"
        );

        let module_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/disposable");
        for name in ["mod.rs", "posture.rs", "session.rs"] {
            let text = std::fs::read_to_string(module_root.join(name))
                .unwrap_or_else(|e| panic!("read src/disposable/{name}: {e}"));
            assert!(
                text.contains("not a code sandbox"),
                "src/disposable/{name} must reject the sandbox claim in its own documentation"
            );
        }
        let readme = std::fs::read_to_string(crate_fixtures().join("README.md"))
            .expect("read the fixture README");
        assert!(
            readme.contains("not** a code sandbox") || readme.contains("not a code sandbox"),
            "the fixture README rejects the sandbox claim"
        );
    }
}
