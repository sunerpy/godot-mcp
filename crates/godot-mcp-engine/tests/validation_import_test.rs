//! Todo 15 acceptance suite: script/project validation, import and automated
//! test execution.
//!
//! Nothing here needs a real Godot installation and nothing touches the network.
//! Every engine invocation runs a committed fake under
//! `tests/fixtures/validation/binaries/`, against a committed project fixture
//! under `tests/fixtures/validation/projects/`, and every process is created by
//! Todo 14's gapless launch protocol inside the committed container/VM adapter
//! boundary — there is no second, bare `Command::spawn` path.
//!
//! Two tests are named by `qa/tasks/15.json` and are the only entry points the
//! QA descriptor invokes:
//!
//! * `validation_import_test_happy_path`
//! * `validation_import_test_failure_paths`

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
use godot_mcp_domain::lease::{Lease, LeaseManager};
use godot_mcp_domain::safe_root::SafeRoot;
use godot_mcp_engine::supervision::adapter::ContainerAdapterProvider;
use godot_mcp_engine::supervision::boundary::{BoundaryProvider, TrustLevel};
use godot_mcp_engine::supervision::journal::{
    Disposition, JournalError, LaunchJournal, LaunchPhase, LaunchRecord, OperationFamily,
    ProcessLaunchProof, ProjectIdentityClaim, TransactionEnvelope, TransportMode, hex_sha256,
    validate_canonical,
};
use godot_mcp_engine::supervision::protocol::{EnvironmentAllowlist, OutputCaps};
use godot_mcp_engine::validation::diagnostic::{
    DiagnosticCategory, DiagnosticSeverity, parse_engine_diagnostics,
};
use godot_mcp_engine::validation::framework::{FrameworkKind, adapters, detect_frameworks};
use godot_mcp_engine::validation::import::{ImportRequest, import_project};
use godot_mcp_engine::validation::outcome::{
    Classification, Evidence, ExitAssertion, Observed, classify,
};
use godot_mcp_engine::validation::runner::{EngineBinding, EngineRunner, TransactionSource};
use godot_mcp_engine::validation::script::{ScriptCheckRequest, check_script};
use godot_mcp_engine::validation::suite::{TestRunRequest, run_tests};

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
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/validation")
}

/// Install one committed fake Godot into `dir`, returning its path.
fn install_binary(dir: &Path, scenario: &str) -> PathBuf {
    let (subdir, extension) = if cfg!(windows) {
        ("windows", "cmd")
    } else {
        ("unix", "sh")
    };
    let source = crate_fixtures()
        .join("binaries")
        .join(subdir)
        .join(format!("{scenario}.{extension}"));
    let bytes = std::fs::read(&source).unwrap_or_else(|e| panic!("read {}: {e}", source.display()));
    let target = dir.join(format!("{scenario}.{extension}"));
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

/// Copy one committed project fixture into a scratch directory.
///
/// The committed fixture itself is never written to: an import legitimately
/// creates `.godot/` inside the project it imports, so the project under test is
/// always a scratch copy. This is test scaffolding, not the disposable-copy
/// execution posture Todo 16 owns.
fn install_project(into: &Path, name: &str) -> PathBuf {
    let source = crate_fixtures().join("projects").join(name);
    let target = into.join(name);
    copy_tree(&source, &target);
    target
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("create the scratch project directory");
    for entry in std::fs::read_dir(from).unwrap_or_else(|e| panic!("read {}: {e}", from.display()))
    {
        let entry = entry.expect("directory entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy the fixture file");
        }
    }
}

fn supervisor_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_godot-mcp-supervisor"))
}

/// The capability set a Godot 4.4+ editor build advertises, as recorded by Todo
/// 13's committed release lock. The fakes advertise the same set.
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

// ---------------------------------------------------------------------------
// A durable stand-in for Todo 6's store, identical in shape to Todo 14's.
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
                object_identity: format!("project_{}", "c".repeat(64)),
                root_object: "unix:dev=1,ino=2".to_owned(),
                file_object: "unix:dev=1,ino=3".to_owned(),
                generation: 11,
            },
            transport_mode: TransportMode::Stdio,
            principal: None,
            session: None,
            operation_family: family,
            tool_name: tool_name.to_owned(),
            idempotency_id: format!("idem-{suffix}"),
            canonical_request_hash: "d".repeat(64),
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
    projects_dir: PathBuf,
    /// One manager and one owned [`Lease`] per project for the scenario's whole
    /// lifetime. Together they make `safe_root` acquire each project **at most
    /// once**: a repeat call reads the map and never reaches `flock`, so no loop
    /// over the helper can reintroduce lock churn.
    ///
    /// Do not collapse these back into a throwaway `LeaseManager::new(..)
    /// .acquire(..)` per call. An `flock` lock belongs to the *open file
    /// description*, and `FD_CLOEXEC` closes a descriptor at `exec`, not at
    /// `fork`. Sibling tests in this binary spawn real engine children
    /// concurrently, so inside one of their fork windows the child holds a
    /// duplicate of every descriptor this process has open — including a lease
    /// lock file this test already dropped. Closing our own descriptor does not
    /// release the lock while that duplicate keeps the open file description
    /// alive, so a re-acquisition can see its own just-released lock as held and
    /// fail with `LeaseError::Conflict`. That is how this suite failed on CI,
    /// where load stretches the fork window from microseconds to milliseconds.
    /// Acquiring once removes the re-acquisition rather than racing it: a first
    /// `flock` cannot collide, because no child can have inherited a descriptor
    /// for a file this process had never opened.
    lease_manager: LeaseManager,
    held_leases: Mutex<BTreeMap<PathBuf, Lease>>,
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
        for dir in [&journal_dir, &work_dir, &boundary_root, &projects_dir] {
            std::fs::create_dir_all(dir).expect("scenario dir");
        }
        let lease_manager = LeaseManager::new(&journal_dir.join("artifact-leases"));
        Self {
            _root: root,
            journal_dir,
            work_dir,
            boundary_root,
            adapter,
            bin_dir,
            projects_dir,
            lease_manager,
            held_leases: Mutex::new(BTreeMap::new()),
        }
    }

    fn journal(&self) -> DurableTestJournal {
        DurableTestJournal::open(&self.journal_dir)
    }

    fn provider(&self) -> ContainerAdapterProvider {
        ContainerAdapterProvider::new(self.adapter.clone(), self.boundary_root.clone())
    }

    /// The artifact observation root for `project`, acquired on the first call
    /// and served from [`Scenario::held_leases`] afterwards. See that field for
    /// why re-acquisition must stay impossible rather than merely unused.
    fn safe_root(&self, project: &Path) -> SafeRoot {
        self.held_leases
            .lock()
            .expect("scenario lease registry")
            .entry(project.to_path_buf())
            .or_insert_with(|| {
                self.lease_manager
                    .acquire(project)
                    .expect("hold the artifact observation root")
            })
            .safe_root()
    }

    fn binding(&self, scenario: &str, project: &Path) -> EngineBinding {
        let program = install_binary(&self.bin_dir, scenario);
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

#[test]
fn res_prefixed_import_artifacts_match_bare_path_existence() {
    let scenario = Scenario::new();
    let project = install_project(&scenario.projects_dir, "gut");
    let journal = scenario.journal();
    let provider = scenario.provider();
    let transactions = Transactions::new(400);
    let runner = EngineRunner::new(
        &journal,
        &provider,
        &transactions,
        scenario.binding("godot-validation", &project),
    );
    let artifact_root = scenario.safe_root(&project);
    let present = [".godot/uid_cache.bin", "res://.godot/uid_cache.bin"];
    let absent = ["missing.import", "res://missing.import"];

    let report = import_project(
        &runner,
        &artifact_root,
        &ImportRequest {
            expected_artifacts: present
                .into_iter()
                .chain(absent)
                .map(str::to_owned)
                .collect(),
            deadline: Duration::from_secs(30),
        },
    )
    .expect("the import run completes");

    for artifact in present {
        assert!(
            report.produced.iter().any(|path| path == artifact),
            "existing artifact {artifact} must be reported produced in either spelling; produced={:?}, missing={:?}",
            report.produced,
            report.missing
        );
        assert!(!report.missing.iter().any(|path| path == artifact));
    }
    for artifact in absent {
        assert!(
            report.missing.iter().any(|path| path == artifact),
            "absent artifact {artifact} must remain missing in either spelling; produced={:?}, missing={:?}",
            report.produced,
            report.missing
        );
        assert!(!report.produced.iter().any(|path| path == artifact));
    }
}

#[test]
fn import_refuses_invalid_artifact_paths_before_launch() {
    let scenario = Scenario::new();
    let project = install_project(&scenario.projects_dir, "gut");
    let journal = scenario.journal();
    let provider = scenario.provider();
    let transactions = Transactions::new(450);
    let runner = EngineRunner::new(
        &journal,
        &provider,
        &transactions,
        scenario.binding("godot-validation", &project),
    );
    let artifact_root = scenario.safe_root(&project);

    for artifact in [
        "/etc/passwd",
        "res://",
        "../x",
        ".godot\\uid_cache.bin",
        "res://res://x.import",
        "nested://x.import",
    ] {
        let error = match import_project(
            &runner,
            &artifact_root,
            &ImportRequest {
                expected_artifacts: vec![artifact.to_owned()],
                deadline: Duration::from_secs(30),
            },
        ) {
            Ok(report) => panic!(
                "invalid artifact {artifact:?} reached launch/classification: verdict={:?}, produced={:?}, missing={:?}",
                report.verdict, report.produced, report.missing
            ),
            Err(error) => error,
        };
        assert_eq!(error.error_kind(), ErrorKind::InvalidInput, "{artifact:?}");
        assert_eq!(
            transactions.issued(),
            450,
            "invalid artifact {artifact:?} must be refused before launch"
        );
    }

    let absent = import_project(
        &runner,
        &artifact_root,
        &ImportRequest {
            expected_artifacts: vec!["x.import".to_owned()],
            deadline: Duration::from_secs(30),
        },
    )
    .expect("a valid but absent artifact still launches and is classified");
    assert_eq!(absent.verdict.classification, Classification::Inconclusive);
    assert!(absent.produced.is_empty());
    assert_eq!(absent.missing, vec!["x.import".to_owned()]);
    assert_eq!(
        transactions.issued(),
        451,
        "the control launched exactly once"
    );
}

#[test]
fn import_refuses_an_evidence_root_from_another_project_before_launch() {
    let scenario = Scenario::new();
    let project = install_project(&scenario.projects_dir, "gut");
    let other_project = scenario.projects_dir.join("other-project");
    copy_tree(
        &crate_fixtures().join("projects").join("gut"),
        &other_project,
    );
    let journal = scenario.journal();
    let provider = scenario.provider();
    let transactions = Transactions::new(460);
    let runner = EngineRunner::new(
        &journal,
        &provider,
        &transactions,
        scenario.binding("godot-validation", &project),
    );
    // Deliberately a second, distinct project and therefore a distinct lease:
    // the point of this test is that the evidence root belongs to `other_project`
    // while the runner is bound to `project`.
    let other_root = scenario.safe_root(&other_project);

    let error = match import_project(
        &runner,
        &other_root,
        &ImportRequest {
            expected_artifacts: vec!["x.import".to_owned()],
            deadline: Duration::from_secs(30),
        },
    ) {
        Ok(report) => panic!(
            "an evidence root from another project reached launch/classification: verdict={:?}, produced={:?}, missing={:?}",
            report.verdict, report.produced, report.missing
        ),
        Err(error) => error,
    };
    assert_eq!(error.error_kind(), ErrorKind::Conflict);
    assert_eq!(
        transactions.issued(),
        460,
        "a mismatched evidence root must be refused before launch"
    );
}

#[test]
fn script_check_refuses_invalid_path_before_launch() {
    let scenario = Scenario::new();
    let project = install_project(&scenario.projects_dir, "gut");
    let journal = scenario.journal();
    let provider = scenario.provider();
    let transactions = Transactions::new(470);
    let runner = EngineRunner::new(
        &journal,
        &provider,
        &transactions,
        scenario.binding("godot-validation", &project),
    );
    let error = match check_script(
        &runner,
        &ScriptCheckRequest {
            script: "/etc/passwd".to_owned(),
            deadline: Duration::from_secs(30),
        },
    ) {
        Ok(report) => panic!(
            "invalid script path reached launch/classification: verdict={:?}, argv={:?}",
            report.verdict, report.argv
        ),
        Err(error) => error,
    };
    assert_eq!(error.error_kind(), ErrorKind::InvalidInput);
    assert_eq!(transactions.issued(), 470, "invalid script must not launch");
}

#[test]
fn test_run_refuses_invalid_selection_before_launch() {
    let scenario = Scenario::new();
    let project = install_project(&scenario.projects_dir, "gut");
    let journal = scenario.journal();
    let provider = scenario.provider();
    let transactions = Transactions::new(480);
    let runner = EngineRunner::new(
        &journal,
        &provider,
        &transactions,
        scenario.binding("godot-validation", &project),
    );
    let error = match run_tests(
        &runner,
        &TestRunRequest {
            selection: vec!["../outside".to_owned()],
            deadline: Duration::from_secs(30),
        },
    ) {
        Ok(report) => panic!(
            "invalid test selection reached launch/classification: verdict={:?}, argv={:?}",
            report.verdict, report.argv
        ),
        Err(error) => error,
    };
    assert_eq!(error.error_kind(), ErrorKind::InvalidInput);
    assert_eq!(
        transactions.issued(),
        480,
        "invalid test selection must not launch"
    );
}

// ---------------------------------------------------------------------------
// Happy path
// ---------------------------------------------------------------------------

/// Godot's macOS builds answer `--check-only` on a broken script with the
/// located `SCRIPT ERROR` diagnostic and exit status 0 (measured on 4.4.1–4.7.1
/// on macOS 26). The canary still proves the check runs — the error is reported,
/// in the canary, at its line — so the verdict rests on the diagnostics: a broken
/// script fails, a clean one passes, and a build that reports nothing at all is
/// still refused.
#[test]
fn a_build_that_reports_parse_errors_but_exits_zero_is_judged_by_its_diagnostics() {
    let scenario = Scenario::new();
    let project = install_project(&scenario.projects_dir, "gut");
    let conf = project.join("fake-engine.conf");
    let mut settings = std::fs::read_to_string(&conf).expect("the fixture's engine settings");
    settings.push_str("check_failure_exit=0\n");
    std::fs::write(&conf, settings).expect("make the fake exit 0 on a parse error");
    let journal = scenario.journal();
    let provider = scenario.provider();
    let transactions = Transactions::new(700);
    let runner = EngineRunner::new(
        &journal,
        &provider,
        &transactions,
        scenario.binding("godot-validation", &project),
    );

    let broken = check_script(
        &runner,
        &ScriptCheckRequest {
            script: "res://scripts/broken.gd".to_owned(),
            deadline: Duration::from_secs(30),
        },
    )
    .expect("the check-only run completes");
    assert!(
        broken.calibration.honored,
        "the canary's located diagnostic proves the check runs: {}",
        broken.calibration.detail
    );
    assert_eq!(broken.calibration.exit_code, Some(0));
    assert!(
        broken.calibration.detail.contains("exit status"),
        "the report says the exit status does not carry the result: {}",
        broken.calibration.detail
    );
    assert_eq!(
        broken.verdict.classification,
        Classification::Failed,
        "{}",
        broken.verdict.reason
    );
    assert_eq!(broken.diagnostics.len(), 1);
    assert_eq!(
        broken.diagnostics[0]
            .location
            .as_ref()
            .map(|location| (location.file.as_str(), location.line)),
        Some(("res://scripts/broken.gd", Some(7)))
    );

    let clean = check_script(
        &runner,
        &ScriptCheckRequest {
            script: "res://scripts/player.gd".to_owned(),
            deadline: Duration::from_secs(30),
        },
    )
    .expect("the check-only run completes");
    assert!(clean.calibration.honored, "{}", clean.calibration.detail);
    assert_eq!(
        clean.verdict.classification,
        Classification::Passed,
        "{}",
        clean.verdict.reason
    );
    assert!(clean.diagnostics.is_empty(), "{:?}", clean.diagnostics);
}

#[test]
fn validation_import_test_happy_path() {
    let scenario = Scenario::new();
    let project = install_project(&scenario.projects_dir, "gut");
    let journal = scenario.journal();
    let provider = scenario.provider();
    provider
        .available()
        .expect("the committed boundary adapter is available");
    let transactions = Transactions::new(500);
    let runner = EngineRunner::new(
        &journal,
        &provider,
        &transactions,
        scenario.binding("godot-validation", &project),
    );
    let artifact_root = scenario.safe_root(&project);

    // -- 1. A clean script parses: asserted exit status AND a calibration that
    //       proves this build really honours `--check-only`. ------------------
    let clean = check_script(
        &runner,
        &ScriptCheckRequest {
            script: "res://scripts/player.gd".to_owned(),
            deadline: Duration::from_secs(30),
        },
    )
    .expect("the check-only run completes");
    assert!(
        clean.calibration.honored,
        "the calibration canary must prove `--check-only` is honoured: {}",
        clean.calibration.detail
    );
    assert_eq!(
        clean.calibration.diagnostics.len(),
        1,
        "the canary produces exactly one located parse diagnostic"
    );
    assert_eq!(clean.verdict.classification, Classification::Passed);
    assert!(clean.diagnostics.is_empty(), "{:?}", clean.diagnostics);
    assert_eq!(clean.verdict.error_kind(), None);
    assert!(
        clean
            .argv
            .windows(2)
            .any(|pair| pair == ["--check-only".to_owned(), "--script".to_owned()])
            || clean.argv.contains(&"--check-only".to_owned())
                && clean.argv.contains(&"--script".to_owned()),
        "the invocation pairs --check-only with --script: {:?}",
        clean.argv
    );
    assert!(
        clean
            .argv
            .iter()
            .all(|argument| !argument.contains("&&") && !argument.contains(';')),
        "arguments are an array, never a shell string: {:?}",
        clean.argv
    );

    // -- 2. A script with a syntax error is a normalized diagnostic with an
    //       exact file and line. ---------------------------------------------
    let broken = check_script(
        &runner,
        &ScriptCheckRequest {
            script: "res://scripts/broken.gd".to_owned(),
            deadline: Duration::from_secs(30),
        },
    )
    .expect("the check-only run completes");
    assert_eq!(broken.verdict.classification, Classification::Failed);
    assert_eq!(broken.verdict.error_kind(), Some(ErrorKind::InvalidInput));
    assert_eq!(broken.diagnostics.len(), 1);
    let diagnostic = &broken.diagnostics[0];
    assert_eq!(diagnostic.severity, DiagnosticSeverity::Error);
    assert_eq!(diagnostic.category, DiagnosticCategory::ScriptParse);
    let location = diagnostic
        .location
        .as_ref()
        .expect("a parse error is always located");
    assert_eq!(location.file, "res://scripts/broken.gd");
    assert_eq!(location.line, Some(7));
    assert!(
        diagnostic.message.contains("Parse Error"),
        "{}",
        diagnostic.message
    );

    // -- 3. An import succeeds only when the engine provably produced the
    //       artifact the import was asked for. -------------------------------
    let cache = project.join(".godot/uid_cache.bin");
    assert!(
        !cache.exists(),
        "the fixture project starts with no engine import cache"
    );
    let imported = import_project(
        &runner,
        &artifact_root,
        &ImportRequest {
            expected_artifacts: vec![".godot/uid_cache.bin".to_owned()],
            deadline: Duration::from_secs(30),
        },
    )
    .expect("the import run completes");
    assert_eq!(imported.verdict.classification, Classification::Passed);
    assert!(
        imported.diagnostics.is_empty(),
        "{:?}",
        imported.diagnostics
    );
    assert_eq!(
        imported.produced,
        vec![".godot/uid_cache.bin".to_owned()],
        "the import proof is the observed artifact, not the exit status"
    );
    assert!(
        cache.exists(),
        "the import wrote the engine cache it claims"
    );
    assert!(
        imported.argv.contains(&"--import".to_owned())
            && imported.argv.contains(&"--headless".to_owned()),
        "{:?}",
        imported.argv
    );

    // -- 4. A committed GUT setup is detected, run headless, and reports exact
    //       counts parsed out of the framework's own summary. ----------------
    let detection = detect_frameworks(&project);
    assert_eq!(
        detection.detected.len(),
        1,
        "exactly the committed framework is detected: {}",
        detection.detail()
    );
    assert_eq!(detection.detected[0].kind, FrameworkKind::Gut);
    assert_eq!(
        detection.detected[0].runner_script,
        "res://addons/gut/gut_cmdln.gd"
    );

    let run = run_tests(
        &runner,
        &TestRunRequest {
            selection: vec!["res://test".to_owned()],
            deadline: Duration::from_secs(30),
        },
    )
    .expect("the test run completes");
    assert_eq!(run.framework, Some(FrameworkKind::Gut));
    assert_eq!(run.verdict.classification, Classification::Passed);
    let summary = run.summary.as_ref().expect("a pass always has a summary");
    assert_eq!(summary.total, 7);
    assert_eq!(summary.passed, 7);
    assert_eq!(summary.failed, 0);
    assert_eq!(summary.skipped, 0);
    assert_eq!(summary.errors, 0);

    // -- 5. Every engine invocation went through the launch protocol and left a
    //       committed transaction; nothing bypassed containment. -------------
    assert_eq!(
        transactions.issued(),
        506,
        "six engine invocations: two check-only calibrations, two check-only targets, one import, \
         one test run — one transaction each"
    );
    let phases = journal.phases();
    assert_eq!(
        phases.len(),
        transactions.issued() as usize - 500,
        "one durable transaction per invocation: {phases:?}"
    );
    assert!(
        phases.iter().all(|phase| phase == "committed_fsynced"),
        "every completed invocation is committed: {phases:?}"
    );
    assert!(
        journal.open_launches().expect("reload").is_empty(),
        "no invocation is left open"
    );

    // -- 6. The committed project fixture was never mutated outside import. ---
    let committed = crate_fixtures().join("projects/gut");
    assert!(
        !committed.join(".godot").exists(),
        "the committed fixture is never written to"
    );
}

// ---------------------------------------------------------------------------
// Failure paths
// ---------------------------------------------------------------------------

#[test]
fn validation_import_test_failure_paths() {
    // -- A. A fabricated "started" banner with a zero exit is never a pass. ---
    //
    //    This is the Todo-13 lesson made mechanical: a build that silently
    //    ignores the flags it was handed exits 0 with plausible-looking output.
    //    The calibration canary — a script the engine plane owns, with a
    //    deliberate syntax error — must fail; when it does not, the target script
    //    is never launched and the operation is unsupported.
    {
        let scenario = Scenario::new();
        let project = install_project(&scenario.projects_dir, "gut");
        let journal = scenario.journal();
        let provider = scenario.provider();
        let transactions = Transactions::new(600);
        let runner = EngineRunner::new(
            &journal,
            &provider,
            &transactions,
            scenario.binding("godot-silent-ignore", &project),
        );
        let artifact_root = scenario.safe_root(&project);

        let report = check_script(
            &runner,
            &ScriptCheckRequest {
                script: "res://scripts/broken.gd".to_owned(),
                deadline: Duration::from_secs(30),
            },
        )
        .expect("the run itself completes");
        assert!(
            report.stdout.is_empty(),
            "the target run is skipped, so its misleading banner is unreachable: {:?}",
            report.stdout
        );
        assert!(
            !report.calibration.honored,
            "a build that answers the canary with success is not honouring --check-only"
        );
        assert_eq!(
            report.verdict.classification,
            Classification::Unsupported,
            "a failed calibration refuses the target run: {}",
            report.verdict.reason
        );
        assert!(!report.verdict.is_success());
        assert_eq!(report.verdict.error_kind(), Some(ErrorKind::Unsupported));
        assert_eq!(report.transaction_id, None);
        assert_eq!(
            transactions.issued(),
            601,
            "only the calibration canary launches; the target script does not"
        );

        // The same build cannot fake an import either: no artifact, no pass.
        let imported = import_project(
            &runner,
            &artifact_root,
            &ImportRequest {
                expected_artifacts: vec![".godot/uid_cache.bin".to_owned()],
                deadline: Duration::from_secs(30),
            },
        )
        .expect("the run itself completes");
        assert_eq!(
            imported.verdict.classification,
            Classification::Inconclusive
        );
        assert!(imported.produced.is_empty());
        assert_eq!(
            imported.missing,
            vec![".godot/uid_cache.bin".to_owned()],
            "the nominated artifact is reported missing by name"
        );

        // And a test run with no parsable framework summary is inconclusive too.
        let run = run_tests(
            &runner,
            &TestRunRequest {
                selection: vec!["res://test".to_owned()],
                deadline: Duration::from_secs(30),
            },
        )
        .expect("the run itself completes");
        assert_eq!(run.verdict.classification, Classification::Inconclusive);
        assert!(
            run.summary.is_none(),
            "no summary parsed means no counts are invented"
        );
    }

    // -- B. A build that does not advertise `--check-only` is `unsupported`, and
    //       nothing is attempted. -------------------------------------------
    {
        let scenario = Scenario::new();
        let project = install_project(&scenario.projects_dir, "gut");
        let journal = scenario.journal();
        let provider = scenario.provider();
        let transactions = Transactions::new(700);
        let mut binding = scenario.binding("godot-validation", &project);
        binding.capabilities.remove("--check-only");
        let runner = EngineRunner::new(&journal, &provider, &transactions, binding);
        let report = check_script(
            &runner,
            &ScriptCheckRequest {
                script: "res://scripts/player.gd".to_owned(),
                deadline: Duration::from_secs(30),
            },
        )
        .expect("the refusal is a report, not an error");
        assert_eq!(report.verdict.classification, Classification::Unsupported);
        assert_eq!(report.verdict.error_kind(), Some(ErrorKind::Unsupported));
        assert!(
            report.verdict.reason.contains("--check-only"),
            "the refusal names the missing capability: {}",
            report.verdict.reason
        );
        assert_eq!(
            transactions.issued(),
            700,
            "an unsupported operation never launches the engine"
        );
    }

    // -- C. An import that fails produces normalized diagnostics and never
    //       claims the artifact. --------------------------------------------
    {
        let scenario = Scenario::new();
        let project = install_project(&scenario.projects_dir, "import-failure");
        let journal = scenario.journal();
        let provider = scenario.provider();
        let transactions = Transactions::new(800);
        let runner = EngineRunner::new(
            &journal,
            &provider,
            &transactions,
            scenario.binding("godot-validation", &project),
        );
        let artifact_root = scenario.safe_root(&project);
        let report = import_project(
            &runner,
            &artifact_root,
            &ImportRequest {
                expected_artifacts: vec![".godot/uid_cache.bin".to_owned()],
                deadline: Duration::from_secs(30),
            },
        )
        .expect("the import run completes");
        assert_eq!(report.verdict.classification, Classification::Failed);
        assert!(
            report
                .diagnostics
                .iter()
                .any(
                    |diagnostic| diagnostic.category == DiagnosticCategory::ResourceImport
                        && diagnostic
                            .location
                            .as_ref()
                            .is_some_and(|location| location.file == "res://art/sprite.png")
                ),
            "the failure names the resource that could not be imported: {:?}",
            report.diagnostics
        );
        assert!(!project.join(".godot/uid_cache.bin").exists());
        assert_eq!(report.missing, vec![".godot/uid_cache.bin".to_owned()]);
    }

    // -- D. A failing GUT run is `failed` with the framework's own counts. ----
    {
        let scenario = Scenario::new();
        let project = install_project(&scenario.projects_dir, "gut-failing");
        let journal = scenario.journal();
        let provider = scenario.provider();
        let transactions = Transactions::new(900);
        let runner = EngineRunner::new(
            &journal,
            &provider,
            &transactions,
            scenario.binding("godot-validation", &project),
        );
        let report = run_tests(
            &runner,
            &TestRunRequest {
                selection: vec!["res://test".to_owned()],
                deadline: Duration::from_secs(30),
            },
        )
        .expect("the test run completes");
        assert_eq!(report.framework, Some(FrameworkKind::Gut));
        assert_eq!(report.verdict.classification, Classification::Failed);
        let summary = report.summary.as_ref().expect("counts were parsed");
        assert_eq!(summary.total, 7);
        assert_eq!(summary.passed, 5);
        assert_eq!(summary.failed, 2);
    }

    // -- E. A failing gdUnit4 run: a different exit code, a different summary
    //       grammar, the same classification. -------------------------------
    {
        let scenario = Scenario::new();
        let project = install_project(&scenario.projects_dir, "gdunit4-failing");
        let journal = scenario.journal();
        let provider = scenario.provider();
        let transactions = Transactions::new(1000);
        let runner = EngineRunner::new(
            &journal,
            &provider,
            &transactions,
            scenario.binding("godot-validation", &project),
        );
        let detection = detect_frameworks(&project);
        assert_eq!(detection.detected[0].kind, FrameworkKind::GdUnit4);
        let report = run_tests(
            &runner,
            &TestRunRequest {
                selection: vec!["res://test".to_owned()],
                deadline: Duration::from_secs(30),
            },
        )
        .expect("the test run completes");
        assert_eq!(report.framework, Some(FrameworkKind::GdUnit4));
        assert_eq!(report.verdict.classification, Classification::Failed);
        let summary = report.summary.as_ref().expect("counts were parsed");
        assert_eq!(summary.total, 4);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.skipped, 1);
        assert_eq!(
            summary.passed, 2,
            "passed is total minus failed and skipped"
        );
    }

    // -- F. A project with no committed framework is `unsupported`, with the
    //       exact detection detail, and never has one installed for it. ------
    {
        let scenario = Scenario::new();
        let project = install_project(&scenario.projects_dir, "no-framework");
        let journal = scenario.journal();
        let provider = scenario.provider();
        let transactions = Transactions::new(1100);
        let runner = EngineRunner::new(
            &journal,
            &provider,
            &transactions,
            scenario.binding("godot-validation", &project),
        );
        let report = run_tests(
            &runner,
            &TestRunRequest {
                selection: vec!["res://test".to_owned()],
                deadline: Duration::from_secs(30),
            },
        )
        .expect("the refusal is a report, not an error");
        assert_eq!(report.verdict.classification, Classification::Unsupported);
        assert_eq!(report.verdict.error_kind(), Some(ErrorKind::Unsupported));
        assert!(report.framework.is_none());
        assert!(report.summary.is_none());
        for kind in FrameworkKind::ALL {
            assert!(
                report.detection.detail().contains(kind.as_str()),
                "the detail names every framework that was looked for: {}",
                report.detection.detail()
            );
        }
        assert!(
            report
                .detection
                .inspected
                .iter()
                .any(|marker| marker.path == "addons/gut/plugin.cfg" && !marker.present),
            "the detail lists the exact markers inspected: {:?}",
            report.detection.inspected
        );
        assert!(
            !project.join("addons").exists(),
            "a missing framework is never installed into the user's project"
        );
        assert_eq!(
            transactions.issued(),
            1100,
            "an unsupported operation never launches the engine"
        );
    }

    // -- G. A test run that overruns its deadline is `timed_out`, never
    //       `failed`. ------------------------------------------------------
    {
        let scenario = Scenario::new();
        let project = install_project(&scenario.projects_dir, "gut-timeout");
        let journal = scenario.journal();
        let provider = scenario.provider();
        let transactions = Transactions::new(1200);
        let runner = EngineRunner::new(
            &journal,
            &provider,
            &transactions,
            scenario.binding("godot-validation", &project),
        );
        let report = run_tests(
            &runner,
            &TestRunRequest {
                selection: vec!["res://test".to_owned()],
                deadline: Duration::from_millis(700),
            },
        )
        .expect("a deadline is a classification, not a runner error");
        assert_eq!(report.verdict.classification, Classification::TimedOut);
        assert_eq!(report.verdict.error_kind(), Some(ErrorKind::Timeout));
        assert!(
            report.summary.is_none(),
            "a timed-out run never reports counts"
        );
        assert!(
            journal.phases().iter().all(|phase| phase == "compensated"),
            "a timed-out launch is compensated, never committed: {:?}",
            journal.phases()
        );
    }

    // -- H. The classification lattice itself: five disjoint verdicts, exactly
    //       one of which means success, and every framework adapter registered.
    assert_eq!(Classification::ALL.len(), 5);
    assert_eq!(
        Classification::ALL
            .iter()
            .filter(|classification| classification.is_success())
            .count(),
        1
    );
    assert_eq!(FrameworkKind::ALL.len(), 2);
    assert_eq!(
        adapters().len(),
        FrameworkKind::ALL.len(),
        "every framework kind has exactly one registered adapter"
    );
    for (adapter, kind) in adapters().iter().zip(FrameworkKind::ALL) {
        assert_eq!(adapter.kind(), kind);
        assert!(
            !adapter.markers().is_empty(),
            "detection is driven by committed markers"
        );
    }

    // -- I. Neither a bare zero exit nor bare parsed evidence can pass alone. -
    let observed = Observed {
        exit_code: Some(0),
        signal: None,
        timed_out: false,
    };
    assert_eq!(
        classify(
            &observed,
            &Evidence::Absent {
                reason: "only a start banner".to_owned()
            },
            &ExitAssertion::exactly(0)
        )
        .classification,
        Classification::Inconclusive
    );
    assert_eq!(
        classify(
            &Observed {
                exit_code: Some(100),
                signal: None,
                timed_out: false
            },
            &Evidence::ParsedClean,
            &ExitAssertion::exactly(0)
        )
        .classification,
        Classification::Failed
    );

    // -- J. The diagnostic normalizer never invents a project location out of an
    //       engine-internal one. --------------------------------------------
    let engine_internal = concat!(
        "ERROR: Condition \"!f\" is true.\n",
        "   at: _load (core/io/resource_loader.cpp:283)\n"
    );
    let parsed = parse_engine_diagnostics(engine_internal);
    assert_eq!(parsed.len(), 1);
    assert!(
        parsed[0].location.is_none(),
        "an engine C++ source location is never reported as a project location: {:?}",
        parsed[0].location
    );
    assert_eq!(
        parsed[0].engine_location.as_deref(),
        Some("core/io/resource_loader.cpp:283"),
        "it is preserved verbatim instead"
    );
}
