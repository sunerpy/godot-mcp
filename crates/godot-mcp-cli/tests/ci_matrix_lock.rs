//! Todo 35: the cross-platform / Godot-compatibility CI harness and the canonical
//! release-gates schema.
//!
//! Four tests, by descriptor: [`ci_matrix_lock_happy_path`] proves the CI matrices
//! and the required-check catalogue are derived from
//! `tests/fixtures/godot-releases.lock.json` alone and that a complete set of
//! passing observations yields a schema-valid NON-QUALIFYING document with a clean
//! release-graph audit; [`ci_matrix_lock_failure_paths`] proves every drift and
//! every non-executed observation fails closed, and that a qualifying emission is
//! refused outside Todo 37's producer-owned orchestration context;
//! [`missing_command_receipt_fails_binding`] and
//! [`switched_archive_set_fails_receipt_binding`] lock the receipt provenance that
//! makes those conclusions meaningful.
//!
//! ## What these tests do and do not claim
//!
//! They exercise the harness's logic on this machine. They make no claim that the
//! Godot version matrix, the GPU/display job, the Docker isolation check or
//! the macOS/Windows packaged smokes have run here: this host has no real Godot
//! install, no GPU/display session, no container runtime, no MSVC toolchain and no
//! macOS toolchain. Those checks are *bound* here and *executed* on the runners
//! the workflows target. What is proved locally is exactly the property that makes
//! that safe: an unexecuted required check is a failed entry, never a pass.
//!
//! Nothing here invokes the harness with `QUALIFYING=1`, and nothing here writes
//! the canonical final gates document — that is Todo 37's alone.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use godot_mcp_cli::audit;
use godot_mcp_cli::release_gates::harness::{
    self, AUTH_SMOKE_CHECK, CANONICAL_FINAL_OUTPUT, CANONICAL_FINAL_OUTPUT_SHA, CheckKind,
    EXIT_BINDING_MISMATCH, EXIT_MISSING, EXIT_SKIPPED, EXIT_UNAVAILABLE,
    GDEXT_REFERENCE_LANE_CHECK, HarnessRequest, Observation, Qualifying, RELEASE_GRAPH_AUDIT_CHECK,
    REQUIRED_VARIABLES,
};
use godot_mcp_cli::release_gates::lock_matrix::{
    self, GodotReleasesLock, LINUX_PLATFORM, MatrixEntry, PACKAGED_SMOKE_PLATFORMS,
};
use godot_mcp_cli::release_gates::schema;

/// The fixture candidate identity `qa/tasks/35.json` supplies, verbatim.
const FIXTURE_COMMIT: &str = "1111111111111111111111111111111111111111";
const FIXTURE_TREE: &str = "2222222222222222222222222222222222222222";

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the workspace root is two levels above this crate manifest")
        .to_path_buf()
}

/// The workspace version the committed release-candidate fixture was recorded at.
/// Its observations and the fixture archive set name `godot-mcp-<version>-<target>`
/// archives, so the harness is evaluated at this version rather than at the one the
/// workspace has since been released to: a release that bumps the workspace version
/// leaves the fixture valid.
const FIXTURE_VERSION: &str = "0.1.0";

fn version() -> &'static str {
    FIXTURE_VERSION
}

fn lock() -> GodotReleasesLock {
    GodotReleasesLock::load_from_workspace(&workspace_root()).expect("the committed lock parses")
}

fn fixture_candidate() -> PathBuf {
    workspace_root().join("tests/fixtures/release-candidate")
}

fn fixture_archives() -> PathBuf {
    workspace_root().join("tests/fixtures/release-archives")
}

fn request_in(output_dir: &Path, qualifying: Qualifying) -> HarnessRequest {
    HarnessRequest {
        candidate_worktree: fixture_candidate(),
        source_commit: FIXTURE_COMMIT.to_owned(),
        source_tree: FIXTURE_TREE.to_owned(),
        archives_dir: fixture_archives(),
        output: output_dir.join("release-gates.json"),
        output_sha: output_dir.join("release-gates.sha256"),
        qualifying,
    }
}

/// Copy the committed candidate fixture into `destination` so a negative case can
/// mutate observations without touching the committed tree.
fn clone_candidate(destination: &Path) {
    let observations = destination.join("ci/observations");
    std::fs::create_dir_all(&observations).expect("create the observation directory");
    std::fs::create_dir_all(destination.join("ci/oidc")).expect("create the oidc directory");
    std::fs::copy(
        fixture_candidate().join("ci/oidc/openid-configuration.json"),
        destination.join("ci/oidc/openid-configuration.json"),
    )
    .expect("copy the oidc fixture");
    let source = fixture_candidate().join("ci/observations");
    for entry in std::fs::read_dir(&source).expect("read the committed observations") {
        let path = entry.expect("a readable directory entry").path();
        if path.extension().is_some_and(|ext| ext == "json") {
            let name = path.file_name().expect("a file name");
            std::fs::copy(&path, observations.join(name)).expect("copy an observation file");
        }
    }
}

/// The committed observation records, as a mutable array.
fn cloned_records(candidate: &Path) -> (PathBuf, Vec<Value>) {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(candidate.join("ci/observations"))
        .expect("read the cloned observations")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();
    let path = paths
        .into_iter()
        .next()
        .expect("the fixture deposits at least one observation file");
    let text = std::fs::read_to_string(&path).expect("read the observation file");
    let records: Vec<Value> = serde_json::from_str(&text).expect("the fixture holds an array");
    (path, records)
}

fn write_records(path: &Path, records: &[Value]) {
    let mut bytes = serde_json::to_vec_pretty(&records).expect("serialize records");
    bytes.push(b'\n');
    std::fs::write(path, bytes).expect("write the mutated observations");
}

fn write_switched_archives(destination: &Path) {
    let specs = harness::required_checks(&lock(), version()).expect("the catalogue derives");
    let mut manifest = String::new();
    for name in specs
        .iter()
        .filter_map(|spec| spec.archive_file_name.as_deref())
    {
        let mut bytes = std::fs::read(fixture_archives().join(name)).expect("read fixture archive");
        bytes.extend_from_slice(b"\nbyte-different qualification archive\n");
        std::fs::write(destination.join(name), &bytes).expect("write switched archive");
        manifest.push_str(&format!("{}  {name}\n", harness::sha256_hex(&bytes)));
    }
    std::fs::write(destination.join(harness::ARCHIVE_CHECKSUMS), manifest)
        .expect("write switched archive manifest");
}

/// Evaluate a mutated candidate and return the entry for `check`.
fn entry_after<F>(check: &str, mutate: F) -> harness::CheckEntry
where
    F: FnOnce(&mut Vec<Value>),
{
    let temp = tempfile::tempdir().expect("temp dir");
    clone_candidate(temp.path());
    let (path, mut records) = cloned_records(temp.path());
    mutate(&mut records);
    write_records(&path, &records);
    let request = HarnessRequest {
        candidate_worktree: temp.path().to_path_buf(),
        source_commit: FIXTURE_COMMIT.to_owned(),
        source_tree: FIXTURE_TREE.to_owned(),
        archives_dir: fixture_archives(),
        output: temp.path().join("out.json"),
        output_sha: temp.path().join("out.sha256"),
        qualifying: Qualifying::No,
    };
    let outcome = harness::evaluate(&request, &workspace_root(), version())
        .expect("a mutated observation set is still evaluable");
    outcome
        .entries
        .into_iter()
        .find(|entry| entry.name == check)
        .unwrap_or_else(|| panic!("check `{check}` is in the catalogue"))
}

/// Find the record for `check` in a mutable record array.
fn record_mut<'a>(records: &'a mut [Value], check: &str) -> &'a mut Value {
    records
        .iter_mut()
        .find(|record| record["name"] == json!(check))
        .unwrap_or_else(|| panic!("the fixture observes `{check}`"))
}

/// Every CI/harness source that is forbidden to restate a lock value.
fn ci_and_harness_sources() -> Vec<PathBuf> {
    let root = workspace_root();
    let mut paths = Vec::new();
    for directory in [
        ".github/workflows",
        "crates/godot-mcp-cli/src/release_gates",
    ] {
        let base = root.join(directory);
        let mut entries: Vec<PathBuf> = std::fs::read_dir(&base)
            .unwrap_or_else(|error| panic!("read {}: {error}", base.display()))
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_file())
            .collect();
        entries.sort();
        paths.extend(entries);
    }
    for relative in [
        "Makefile",
        "crates/godot-mcp-cli/src/bin/release-package.rs",
        "scripts/ci-observe.sh",
        "scripts/godot-artifact.sh",
        "scripts/packaged-smoke.sh",
        "scripts/isolation-smoke.sh",
    ] {
        paths.push(root.join(relative));
    }
    paths
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

// ---------------------------------------------------------------------------

#[test]
fn ci_matrix_lock_happy_path() {
    let root = workspace_root();
    let lock = lock();

    // --- the lock is a usable matrix source -------------------------------
    lock.validate().expect("the committed lock validates");
    let versions = lock.versions();
    assert!(
        versions.len() >= 4,
        "the compatibility matrix needs every exact release in the lock, found {}",
        versions.len()
    );
    for release_version in &versions {
        assert_eq!(
            release_version.split('.').count(),
            3,
            "`{release_version}` must be an exact MAJOR.MINOR.PATCH version, never an alias"
        );
        let alias = lock_matrix::alias_of(release_version).expect("an exact version has an alias");
        assert_ne!(
            &alias, release_version,
            "an alias must never equal the exact version it shortens"
        );
    }

    // --- the engine/editor matrix is exactly the lock ---------------------
    let engine = lock
        .engine_editor_matrix()
        .expect("the engine/editor matrix derives");
    assert_eq!(
        engine.len(),
        versions.len() * 2,
        "every exact release contributes a Linux editor leg and an export-template leg"
    );
    let by_job: BTreeSet<&str> = engine.iter().map(|entry| entry.job.as_str()).collect();
    assert_eq!(
        by_job.len(),
        engine.len(),
        "matrix job identities must be unique"
    );
    for entry in &engine {
        let release = lock
            .releases
            .iter()
            .find(|release| release.version == entry.version)
            .expect("every matrix row names a locked release");
        let artifact = release
            .artifacts
            .iter()
            .find(|artifact| artifact.platform == entry.platform)
            .expect("every matrix row names a locked artifact");
        assert_eq!(
            (
                entry.download_url.as_str(),
                entry.sha256.as_str(),
                entry.artifact_name.as_str(),
                entry.size_bytes
            ),
            (
                artifact.download_url.as_str(),
                artifact.sha256.as_str(),
                artifact.artifact_name.as_str(),
                artifact.size_bytes
            ),
            "a matrix row must carry the lock's exact URL, digest, asset name and size"
        );
        assert!(
            entry.download_url.contains(&entry.git_tag),
            "the URL must spell the exact tag, never a shortened form"
        );
    }

    // --- the packaged smoke is the newest locked release, three platforms --
    let packaged = lock
        .packaged_smoke_matrix()
        .expect("the packaged-smoke matrix derives");
    let newest = lock.newest().expect("the lock records a newest release");
    assert_eq!(packaged.len(), PACKAGED_SMOKE_PLATFORMS.len());
    for (entry, platform) in packaged.iter().zip(PACKAGED_SMOKE_PLATFORMS) {
        assert_eq!(entry.platform, platform);
        assert_eq!(
            entry.version, newest.version,
            "the packaged smoke is pinned to the newest locked release, derived not named"
        );
    }
    assert_eq!(
        harness::packaged_archive_file_name(LINUX_PLATFORM, version())
            .expect("Linux has a published release target"),
        format!("godot-mcp-{}-x86_64-unknown-linux-musl.tar.gz", version()),
        "the Linux Godot platform must consume the self-contained musl archive"
    );

    // --- the required-check catalogue -------------------------------------
    let specs = harness::required_checks(&lock, version()).expect("the catalogue derives");
    assert!(
        specs.iter().all(|spec| spec.required),
        "every check the harness derives is required; none may be labelled otherwise"
    );
    let names: Vec<&str> = specs.iter().map(|spec| spec.name.as_str()).collect();
    // Pinned as an exact set, never spot-checked for membership: a membership
    // loop cannot notice a gate that was deleted from the catalogue.
    let workspace: BTreeSet<&str> = specs
        .iter()
        .filter(|spec| spec.kind == CheckKind::Workspace)
        .map(|spec| spec.name.as_str())
        .collect();
    let expected_workspace: BTreeSet<&str> = BTreeSet::from([
        "format",
        "lint",
        "unit",
        "contract",
        "golden",
        "build-locked",
        "license",
        "advisory",
        "sbom",
        RELEASE_GRAPH_AUDIT_CHECK,
        "reproducibility",
    ]);
    assert_eq!(
        workspace, expected_workspace,
        "the workspace gate catalogue must equal this exact set; dropping any gate (notably \
         `reproducibility`) or adding one silently is a release-gate regression"
    );
    assert_eq!(
        specs
            .iter()
            .filter(|spec| spec.kind == CheckKind::Workspace)
            .count(),
        expected_workspace.len(),
        "no workspace gate may be listed twice, which would let a set comparison hide a swap"
    );
    for expected in [RELEASE_GRAPH_AUDIT_CHECK, AUTH_SMOKE_CHECK] {
        assert!(
            names.contains(&expected),
            "`{expected}` must be a required release-gate check"
        );
    }
    assert!(
        !names.contains(&GDEXT_REFERENCE_LANE_CHECK),
        "the isolated non-shipping gdext reference lane must never be a release-gate check, so it \
         can never stand in for the release-graph audit"
    );
    let isolation: Vec<_> = specs
        .iter()
        .filter(|spec| spec.kind == CheckKind::Isolation)
        .collect();
    assert_eq!(
        isolation.len(),
        1,
        "strong isolation is one required Docker check, not a Docker-plus-VM pair"
    );
    assert_eq!(isolation[0].name, "isolation-docker-clean-room");
    assert!(isolation[0].required);
    assert_eq!(
        isolation[0].command,
        "docker run --rm --network=none --read-only --cap-drop=ALL \
         --security-opt=no-new-privileges \
         --mount type=bind,source=$PWD/target/release-dist,target=/dist,readonly \
         $RELEASE_GATES_ISOLATION_IMAGE /dist/smoke.sh"
    );
    for kind in [
        CheckKind::GodotMatrix,
        CheckKind::GpuDisplay,
        CheckKind::Isolation,
        CheckKind::PackagedSmoke,
        CheckKind::DeploymentSmoke,
    ] {
        assert!(
            specs.iter().any(|spec| spec.kind == kind),
            "the catalogue must bind at least one `{}` check",
            kind.label()
        );
    }
    let matrix_commands: Vec<&str> = specs
        .iter()
        .filter(|spec| spec.kind == CheckKind::GodotMatrix)
        .map(|spec| spec.command.as_str())
        .collect();
    assert_eq!(matrix_commands.len(), engine.len());
    for entry in &engine {
        assert!(
            matrix_commands
                .iter()
                .any(|command| command.contains(&entry.download_url)
                    && command.contains(&entry.sha256)),
            "every matrix leg's command must carry the locked URL and digest it verifies"
        );
    }
    for spec in specs
        .iter()
        .filter(|spec| spec.kind == CheckKind::GpuDisplay)
    {
        assert!(
            !spec.command.contains("--headless"),
            "a GPU/display check that ran headless would prove nothing about a real display"
        );
    }

    // --- the release-graph audit is clean --------------------------------
    let forbidden = audit::audit_release_graph(&root).expect("the release graph is auditable");
    assert!(
        forbidden.is_empty(),
        "the release graph must carry no gdext artifact: {forbidden:?}"
    );
    let edges = audit::audit_architecture_boundaries(&root).expect("the layer graph is auditable");
    assert!(edges.is_empty(), "forbidden layer edges: {edges:?}");

    // --- the canonical schema is frozen ---------------------------------
    schema::assert_embedded_matches_disk(&root)
        .expect("the embedded schema matches the committed file");
    let schema_document = schema::schema_value().expect("the schema parses");
    assert_eq!(
        schema_document["additionalProperties"],
        json!(false),
        "the canonical schema stays restrictive"
    );
    let check_properties = &schema_document["properties"]["checks"]["items"]["properties"];
    for member in [
        "name",
        "required",
        "command",
        "observation",
        "exit_code",
        "runner_identity",
        "source_commit",
        "source_tree",
    ] {
        assert!(
            check_properties.get(member).is_some(),
            "a check entry must record `{member}`"
        );
    }
    assert_eq!(
        schema_document["properties"]["checks"]["items"]["properties"]["observation"]["enum"],
        json!(["executed", "missing", "skipped", "unavailable"]),
        "the observation label is a closed set so a non-executed check cannot hide"
    );

    // --- the harness over the committed passing fixture ------------------
    let temp = tempfile::tempdir().expect("temp dir");
    let request = request_in(temp.path(), Qualifying::No);
    let outcome = harness::run(&request, &root, version())
        .expect("a complete set of passing observations passes the harness");
    assert!(outcome.written, "the non-qualifying document is written");
    assert_eq!(outcome.entries.len(), specs.len());
    assert!(
        outcome.all_required_passed(),
        "failures: {:?}",
        outcome.failures()
    );
    assert_eq!(
        outcome.document["qualifying"],
        json!(false),
        "this todo only ever produces NON-QUALIFYING observations"
    );
    assert_eq!(
        outcome.document["schema_version"],
        json!(schema::SCHEMA_VERSION)
    );
    schema::validate(&outcome.document).expect("the emitted document is schema-valid");
    for entry in &outcome.entries {
        assert_eq!(entry.observation, Observation::Executed);
        assert_eq!(entry.exit_code, 0);
        assert_eq!(entry.source_commit, FIXTURE_COMMIT);
        assert_eq!(entry.source_tree, FIXTURE_TREE);
        assert!(
            !entry.runner_identity.os.is_empty()
                && !entry.runner_identity.os_version.is_empty()
                && !entry.runner_identity.arch.is_empty(),
            "check `{}` must be bound to a complete runner identity",
            entry.name
        );
    }
    let (_, fixture_records) = cloned_records(&fixture_candidate());
    let archive_digests =
        harness::verified_archive_digests(&fixture_archives()).expect("fixture archives verify");
    for entry in outcome
        .entries
        .iter()
        .filter(|entry| entry.name.starts_with("packaged-smoke-"))
    {
        let receipt_digest = fixture_records
            .iter()
            .find(|record| record["name"] == json!(entry.name))
            .and_then(|record| record["archive_sha256"].as_str())
            .expect("a packaged-smoke receipt records the tested archive digest");
        let archive_name = specs
            .iter()
            .find(|spec| spec.name == entry.name)
            .and_then(|spec| spec.archive_file_name.as_deref())
            .expect("a packaged smoke names its archive");
        let qualification_digest = archive_digests
            .get(archive_name)
            .expect("the qualification archive set carries the bound archive");
        assert_eq!(
            receipt_digest, qualification_digest,
            "the receipt must identify the exact archive supplied at qualification"
        );
        assert_eq!(
            entry.archive_sha256.as_deref(),
            Some(receipt_digest),
            "the emitted digest must come from matching receipt and qualification provenance"
        );
    }
    let recorded_sidecar =
        std::fs::read_to_string(&request.output_sha).expect("the sidecar is written");
    assert_eq!(recorded_sidecar, format!("{}\n", outcome.digest));
    assert_eq!(
        harness::sha256_hex(&std::fs::read(&request.output).expect("the document is written")),
        outcome.digest,
        "the sidecar must be the document's real digest"
    );

    // --- no CI/harness source restates a lock value ----------------------
    for path in ci_and_harness_sources() {
        let relative = path
            .strip_prefix(&root)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();
        let restated = lock_matrix::scan_for_restated_lock_values(&relative, &read(&path), &lock)
            .expect("the scan runs");
        assert!(
            restated.is_empty(),
            "{relative} restates lock values instead of deriving them: {restated:?}"
        );
    }

    // --- the workflows exist and wire the required jobs ------------------
    let workflows = root.join(".github/workflows");
    let ci = read(&workflows.join("ci.yml"));
    let release = read(&workflows.join("release.yml"));
    let reference = read(&workflows.join("gdext-reference.yml"));
    for retired in [
        "gates.yml",
        "compatibility.yml",
        "coverage.yml",
        "release-gates.yml",
    ] {
        assert!(
            !workflows.join(retired).exists(),
            "{retired} was folded into ci.yml; a second copy of a required job would drift"
        );
    }
    for (label, text) in [("ci.yml", &ci), ("release.yml", &release)] {
        // Only the code regions matter: a comment stating that no job continues
        // on error is documentation, not a `continue-on-error:` key.
        let code = lock_matrix::code_regions_only(text);
        assert!(
            !code.contains("continue-on-error"),
            "{label} must not let a required job pass by continuing on error"
        );
        // The trigger key, not the word: the API's `workflow_runs` list is fine.
        assert!(
            !code
                .lines()
                .any(|line| line.trim_start().starts_with("workflow_run:")),
            "{label} must not chain on another workflow's completion: one run observes and \
             judges its own checks"
        );
    }
    let ci_code = lock_matrix::code_regions_only(&ci);
    // A release verifies itself against CI Success on the exact release commit
    // (release.yml, verify-ci), so no later run may cancel a main commit's CI,
    // running or pending. Only a pull request's runs share a group, so a new head
    // cancels its stale run; every other run's group is its own event and commit.
    let concurrency_group = ci_code
        .lines()
        .skip_while(|line| line.trim_end() != "concurrency:")
        .nth(1)
        .and_then(|line| line.trim().strip_prefix("group: "))
        .expect("ci.yml declares a workflow-level concurrency group");
    assert_eq!(
        concurrency_group,
        "${{ github.workflow }}-${{ github.event_name }}-${{ github.event.pull_request.number || \
         github.sha }}",
        "a main commit's CI must never share a concurrency group with a later push, or the \
         release built from that commit loses its CI Success"
    );
    // Every catalogue check that needs no external environment is one row of the
    // gates matrix, so none of them can be left out of a run.
    for spec in specs
        .iter()
        .filter(|spec| matches!(spec.kind, CheckKind::Workspace | CheckKind::DeploymentSmoke))
    {
        assert!(
            ci_code
                .lines()
                .any(|line| line.trim() == format!("- {}", spec.name)),
            "ci.yml's gates matrix must observe `{}`",
            spec.name
        );
    }
    assert!(
        ci_code.contains("scripts/ci-observe.sh"),
        "every check is observed through ci-observe.sh, so its command comes from the catalogue"
    );
    assert!(
        !ci_code.contains("gdext-reference"),
        "ci.yml must not depend on the isolated gdext reference lane in any way"
    );
    assert!(
        !reference.contains("ci-observe.sh")
            && !lock_matrix::code_regions_only(&reference).contains("needs:"),
        "the reference lane must write no release-gate observation and take no needs edge, so a \
         green run there can never satisfy the release-graph audit"
    );
    assert!(
        ci.contains("fromJSON(needs.matrix.outputs.engine_editor)")
            && ci.contains("fromJSON(needs.matrix.outputs.packaged_smoke)"),
        "both matrices must be consumed from the lock-derived job output"
    );
    assert!(
        ci.contains("key: godot-artifact-${{ matrix.entry.sha256 }}"),
        "the download cache must be keyed by the locked digest, so a hit is hash-verified"
    );
    assert!(
        !ci_code.contains("QUALIFYING=1") && !ci_code.contains(CANONICAL_FINAL_OUTPUT),
        "CI never emits the final qualifying gates document"
    );
    assert!(
        ci_code.contains("QUALIFYING=0"),
        "CI runs the harness over its own observations as a non-qualifying observation"
    );

    // --- the Make target's contract --------------------------------------
    let makefile = read(&root.join("Makefile"));
    assert!(makefile.contains("release-gates-harness:"));
    for variable in REQUIRED_VARIABLES {
        assert!(
            makefile.contains(variable),
            "`make release-gates-harness` must require {variable}"
        );
    }

    // --- the deterministic local OIDC/JWKS fixture -----------------------
    let kids = harness::verify_oidc_fixture(&fixture_candidate(), &root)
        .expect("the local OIDC/JWKS fixture verifies without a real identity provider");
    assert!(
        kids.len() >= 3,
        "the fixture must publish the rotation pair, the ES256 key and the confusion probe, found \
         {}",
        kids.len()
    );
}

// ---------------------------------------------------------------------------

#[test]
fn missing_command_receipt_fails_binding() {
    let entry = entry_after("format", |records| {
        record_mut(records, "format")
            .as_object_mut()
            .expect("the format receipt is an object")
            .remove("command");
    });

    assert!(
        !entry.passed(),
        "a receipt with no command evidence must fail exact-command binding"
    );
    assert_eq!(
        entry.exit_code, EXIT_BINDING_MISMATCH,
        "a missing command is a provenance binding mismatch"
    );
}

#[test]
fn switched_archive_set_fails_receipt_binding() {
    let archives = tempfile::tempdir().expect("temp archive dir");
    write_switched_archives(archives.path());
    harness::verified_archive_digests(archives.path()).expect("the switched archive set verifies");

    let output = tempfile::tempdir().expect("temp output dir");
    let request = HarnessRequest {
        candidate_worktree: fixture_candidate(),
        source_commit: FIXTURE_COMMIT.to_owned(),
        source_tree: FIXTURE_TREE.to_owned(),
        archives_dir: archives.path().to_path_buf(),
        output: output.path().join("release-gates.json"),
        output_sha: output.path().join("release-gates.sha256"),
        qualifying: Qualifying::No,
    };
    let outcome = harness::evaluate(&request, &workspace_root(), version())
        .expect("a self-consistent replacement archive set is evaluable");
    let packaged_smokes: Vec<_> = outcome
        .entries
        .iter()
        .filter(|entry| entry.name.starts_with("packaged-smoke-"))
        .collect();

    assert_eq!(packaged_smokes.len(), 3);
    assert!(
        packaged_smokes
            .iter()
            .all(|entry| !entry.passed() && entry.exit_code == EXIT_BINDING_MISMATCH),
        "byte-different qualification archives must not satisfy unchanged smoke receipts: \
         {packaged_smokes:?}"
    );
}

#[test]
fn ci_matrix_lock_failure_paths() {
    let root = workspace_root();
    let good = lock();
    let lock_text = read(&GodotReleasesLock::path_in(&root));

    // --- lock drift is rejected -----------------------------------------
    let mutate = |edit: &dyn Fn(&mut Value)| -> String {
        let mut document: Value = serde_json::from_str(&lock_text).expect("the lock parses");
        edit(&mut document);
        let drifted: GodotReleasesLock =
            serde_json::from_value(document).expect("the drifted lock still deserializes");
        drifted
            .validate()
            .expect_err("the drifted lock must be rejected")
    };

    let alias = mutate(&|document| {
        let version = document["releases"][0]["version"]
            .as_str()
            .expect("a version")
            .to_owned();
        let alias = lock_matrix::alias_of(&version).expect("an alias");
        document["releases"][0]["version"] = json!(alias);
    });
    assert!(
        alias.contains("alias") || alias.contains("MAJOR.MINOR.PATCH"),
        "a `major.minor` alias must be rejected as a version: {alias}"
    );

    let shortened = mutate(&|document| {
        let url = document["releases"][0]["artifacts"][0]["download_url"]
            .as_str()
            .expect("a url")
            .to_owned();
        let tag = document["releases"][0]["git_tag"]
            .as_str()
            .expect("a tag")
            .to_owned();
        let version = document["releases"][0]["version"]
            .as_str()
            .expect("a version")
            .to_owned();
        let alias = lock_matrix::alias_of(&version).expect("an alias");
        document["releases"][0]["artifacts"][0]["download_url"] =
            json!(url.replace(&tag, &format!("{alias}-stable")));
    });
    assert!(
        shortened.contains("download_url"),
        "a shortened URL must be rejected: {shortened}"
    );

    let duplicate_url = mutate(&|document| {
        let url = document["releases"][1]["artifacts"][0]["download_url"].clone();
        document["releases"][0]["artifacts"][0]["download_url"] = url;
    });
    assert!(
        duplicate_url.contains("duplicates"),
        "a duplicated download URL must be rejected: {duplicate_url}"
    );

    let duplicate_digest = mutate(&|document| {
        let digest = document["releases"][1]["artifacts"][0]["sha256"].clone();
        document["releases"][0]["artifacts"][0]["sha256"] = digest;
    });
    assert!(
        duplicate_digest.contains("duplicates"),
        "a duplicated digest must be rejected: {duplicate_digest}"
    );

    let bad_digest = mutate(&|document| {
        document["releases"][0]["artifacts"][0]["sha256"] = json!("DEADBEEF");
    });
    assert!(
        bad_digest.contains("sha256"),
        "a malformed digest must be rejected: {bad_digest}"
    );

    let missing_platform = mutate(&|document| {
        let artifacts = document["releases"][0]["artifacts"]
            .as_array()
            .expect("artifacts")
            .clone();
        document["releases"][0]["artifacts"] = json!(artifacts[1..]);
    });
    assert!(
        missing_platform.contains("exactly one"),
        "a release missing a required platform must be rejected: {missing_platform}"
    );

    let unordered = mutate(&|document| {
        let releases = document["releases"].as_array().expect("releases").clone();
        document["releases"] = json!(releases.iter().rev().cloned().collect::<Vec<_>>());
    });
    assert!(
        unordered.contains("ascending"),
        "out-of-order releases must be rejected so `newest` cannot be wrong: {unordered}"
    );

    // --- a restated lock value in a CI/harness source is caught ---------
    let planted_url = good.releases[0].artifacts[0].download_url.clone();
    let planted_digest = good.releases[0].artifacts[0].sha256.clone();
    let planted_alias =
        lock_matrix::alias_of(&good.releases[0].version).expect("an exact version has an alias");
    let planted = format!(
        "GODOT_URL=\"{planted_url}\"\nGODOT_SHA=\"{planted_digest}\"\nGODOT_SERIES=\"{planted_alias}\"\n"
    );
    let caught = lock_matrix::scan_for_restated_lock_values("planted.sh", &planted, &good)
        .expect("the scan runs");
    for kind in ["url", "sha256", "alias"] {
        assert!(
            caught.iter().any(|finding| finding.kind == kind),
            "a restated {kind} must be caught: {caught:?}"
        );
    }
    let commented = format!("# see {planted_url}\n# digest {planted_digest}\n");
    assert!(
        lock_matrix::scan_for_restated_lock_values("commented.sh", &commented, &good)
            .expect("the scan runs")
            .is_empty(),
        "a value named in a comment is documentation, not a restatement"
    );

    // --- a gdext artifact anywhere in the shipping graph is caught -------
    for name in [
        "godot",
        "godot-core",
        "godot-ffi",
        "godot-macros",
        "gdext",
        "gdext-bridge",
    ] {
        assert!(
            audit::forbidden_crate_name(name),
            "`{name}` must be forbidden in the shipping graph"
        );
    }
    assert!(
        !audit::forbidden_crate_name("godot-mcp-server"),
        "this workspace's own crates must not be mistaken for gdext"
    );
    assert!(
        audit::FORBIDDEN_FILE_MARKERS.contains(&".gdextension"),
        "a `.gdextension` descriptor must never ship"
    );

    // --- QUALIFYING parsing is closed -----------------------------------
    for value in ["", "2", "true", "yes", "01", "-1"] {
        Qualifying::parse(value)
            .expect_err("QUALIFYING accepts only `0` and `1`; anything else fails closed");
    }
    assert_eq!(Qualifying::parse("0").expect("zero parses"), Qualifying::No);
    assert_eq!(Qualifying::parse("1").expect("one parses"), Qualifying::Yes);

    // --- QUALIFYING=1 is refused outside Todo 37's producer context ------
    let canonical = HarnessRequest {
        candidate_worktree: fixture_candidate(),
        source_commit: FIXTURE_COMMIT.to_owned(),
        source_tree: FIXTURE_TREE.to_owned(),
        archives_dir: fixture_archives(),
        output: PathBuf::from(CANONICAL_FINAL_OUTPUT),
        output_sha: PathBuf::from(CANONICAL_FINAL_OUTPUT_SHA),
        qualifying: Qualifying::Yes,
    };
    let no_context = harness::authorize(&canonical, None)
        .expect_err("a qualifying emission without a producer context is refused");
    assert!(
        no_context.contains(harness::PRODUCER_CONTEXT_ENV),
        "the refusal must name the missing producer context: {no_context}"
    );
    for wrong in ["", "   ", "not-the-token", &"0".repeat(64)] {
        harness::authorize(&canonical, Some(wrong))
            .expect_err("a wrong producer context is refused");
    }
    let other_candidate = harness::producer_context_token("a-different-commit", FIXTURE_TREE);
    harness::authorize(&canonical, Some(&other_candidate)).expect_err(
        "a producer context bound to another candidate must not authorise this emission",
    );
    // The token for THIS candidate authorises only the canonical paths; a
    // side-channel output is still refused. Todo 37 owns the accepted path, so
    // this test never exercises a successful qualifying emission.
    let token = harness::producer_context_token(FIXTURE_COMMIT, FIXTURE_TREE);
    let sideways = HarnessRequest {
        output: PathBuf::from("target/qa-task-35/pretend-final.json"),
        output_sha: PathBuf::from("target/qa-task-35/pretend-final.sha256"),
        ..canonical.clone()
    };
    let refused = harness::authorize(&sideways, Some(&token))
        .expect_err("a qualifying emission must write exactly the canonical paths");
    assert!(refused.contains(CANONICAL_FINAL_OUTPUT));

    // --- a non-qualifying run may never write into the evidence tree -----
    for (output, output_sha) in [
        (CANONICAL_FINAL_OUTPUT, CANONICAL_FINAL_OUTPUT_SHA),
        (
            ".omo/evidence/godot-mcp-server-architecture/external/look-alike.json",
            ".omo/evidence/godot-mcp-server-architecture/external/look-alike.sha256",
        ),
    ] {
        let request = HarnessRequest {
            output: PathBuf::from(output),
            output_sha: PathBuf::from(output_sha),
            qualifying: Qualifying::No,
            ..canonical.clone()
        };
        let refused = harness::authorize(&request, None)
            .expect_err("a non-qualifying run must not write under the evidence tree");
        assert!(
            refused.contains(harness::EVIDENCE_PREFIX),
            "the refusal must name the evidence tree: {refused}"
        );
        assert!(
            !Path::new(output).exists(),
            "{output} must not exist: a refused run writes nothing"
        );
    }

    // --- missing / skipped / unavailable are explicit failed entries -----
    // The GPU/display check's name is derived from the lock, so it is looked up
    // rather than written here.
    let gpu_check = harness::required_checks(&good, version())
        .expect("the catalogue derives")
        .into_iter()
        .find(|spec| spec.kind == CheckKind::GpuDisplay)
        .expect("a GPU/display check exists")
        .name;

    let absent = entry_after(&gpu_check, |records| {
        let target = records
            .iter()
            .position(|record| record["name"].as_str() == Some(gpu_check.as_str()))
            .expect("the fixture observes it");
        records.remove(target);
    });
    assert_eq!(absent.observation, Observation::Missing);
    assert_eq!(absent.exit_code, EXIT_MISSING);
    assert!(!absent.passed(), "a missing required check can never pass");
    assert!(absent.failure.is_some());

    for (label, observation, sentinel) in [
        ("skipped", Observation::Skipped, EXIT_SKIPPED),
        ("unavailable", Observation::Unavailable, EXIT_UNAVAILABLE),
    ] {
        // Even when the runner claims exit code 0, a non-executed observation
        // must be recorded with a nonzero sentinel and must fail.
        let entry = entry_after(&gpu_check, |records| {
            let record = record_mut(records, &gpu_check);
            record["observation"] = json!(label);
            record["exit_code"] = json!(0);
        });
        assert_eq!(entry.observation, observation);
        assert_eq!(
            entry.exit_code, sentinel,
            "a `{label}` observation must carry its own nonzero sentinel, never 0"
        );
        assert!(
            !entry.passed(),
            "a `{label}` required check must be a failed entry, never a silent non-failure"
        );
    }

    // A GPU/display job that reports success without a display still needs a
    // real exit code; a nonzero one fails.
    let nonzero = entry_after(&gpu_check, |records| {
        record_mut(records, &gpu_check)["exit_code"] = json!(1);
    });
    assert!(!nonzero.passed());
    assert_eq!(nonzero.exit_code, 1);

    let no_exit_code = entry_after(&gpu_check, |records| {
        let record = record_mut(records, &gpu_check)
            .as_object_mut()
            .expect("an object");
        record.remove("exit_code");
    });
    assert!(
        !no_exit_code.passed(),
        "an `executed` observation with no captured exit code is not a pass"
    );

    // --- binding drift fails closed --------------------------------------
    let wrong_command = entry_after(&gpu_check, |records| {
        record_mut(records, &gpu_check)["command"] = json!("true");
    });
    assert!(!wrong_command.passed());
    assert_eq!(wrong_command.exit_code, EXIT_BINDING_MISMATCH);
    assert!(
        wrong_command
            .failure
            .as_deref()
            .is_some_and(|reason| reason.contains("bound command"))
    );

    for field in ["source_commit", "source_tree"] {
        let drifted = entry_after(&gpu_check, |records| {
            record_mut(records, &gpu_check)[field] = json!("3".repeat(40));
        });
        assert!(
            !drifted.passed(),
            "an observation of a different {field} must not pass"
        );
        assert_eq!(drifted.exit_code, EXIT_BINDING_MISMATCH);
    }

    let bad_runner = entry_after(&gpu_check, |records| {
        record_mut(records, &gpu_check)["runner_identity"] =
            json!({ "os": "", "os_version": "", "arch": "" });
    });
    assert!(
        !bad_runner.passed(),
        "a check with no real runner identity is not bound to an environment"
    );

    // --- a failed required check writes nothing --------------------------
    let temp = tempfile::tempdir().expect("temp dir");
    clone_candidate(temp.path());
    let (path, mut records) = cloned_records(temp.path());
    record_mut(&mut records, &gpu_check)["observation"] = json!("unavailable");
    write_records(&path, &records);
    let request = HarnessRequest {
        candidate_worktree: temp.path().to_path_buf(),
        source_commit: FIXTURE_COMMIT.to_owned(),
        source_tree: FIXTURE_TREE.to_owned(),
        archives_dir: fixture_archives(),
        output: temp.path().join("out.json"),
        output_sha: temp.path().join("out.sha256"),
        qualifying: Qualifying::No,
    };
    let refused = harness::run(&request, &root, version())
        .expect_err("a failed required check must fail the harness");
    assert!(refused.contains("unavailable"), "{refused}");
    assert!(
        !request.output.exists() && !request.output_sha.exists(),
        "no gates document may be written when a required check failed"
    );

    // --- duplicate observations of one check are rejected ---------------
    let temp = tempfile::tempdir().expect("temp dir");
    clone_candidate(temp.path());
    let (path, records) = cloned_records(temp.path());
    let duplicate = path.with_file_name("duplicate.json");
    write_records(&duplicate, &records);
    let error = harness::load_observations(temp.path())
        .expect_err("one check observed twice is ambiguous and must be rejected");
    assert!(error.contains("more than once"), "{error}");

    // --- an unobservable candidate is an error, not an empty pass --------
    let empty = tempfile::tempdir().expect("temp dir");
    harness::load_observations(empty.path())
        .expect_err("a candidate with no observation directory cannot pass");

    // --- archive drift fails closed --------------------------------------
    let archives = tempfile::tempdir().expect("temp dir");
    for entry in std::fs::read_dir(fixture_archives()).expect("read the archive fixture") {
        let source = entry.expect("a readable entry").path();
        if source.is_file() {
            let name = source.file_name().expect("a file name");
            std::fs::copy(&source, archives.path().join(name)).expect("copy an archive");
        }
    }
    harness::verified_archive_digests(archives.path()).expect("the copied fixture verifies");
    let tampered = archives.path().join(
        harness::packaged_archive_file_name(LINUX_PLATFORM, version()).expect("a linux archive"),
    );
    std::fs::write(&tampered, b"tampered").expect("tamper with an archive");
    let error = harness::verified_archive_digests(archives.path())
        .expect_err("an archive whose bytes changed must be rejected");
    assert!(error.contains("does not hash"), "{error}");

    std::fs::remove_file(archives.path().join("SHA256SUMS.txt")).expect("remove the manifest");
    harness::verified_archive_digests(archives.path())
        .expect_err("an absent checksum manifest cannot be a pass");

    // --- the OIDC/JWKS fixture fails closed -----------------------------
    let broken = tempfile::tempdir().expect("temp dir");
    clone_candidate(broken.path());
    let oidc = broken.path().join("ci/oidc/openid-configuration.json");
    let mut document: Value = serde_json::from_str(&read(&oidc)).expect("the oidc fixture parses");
    document["keys"] = json!(["absent.jwk.json"]);
    std::fs::write(
        &oidc,
        serde_json::to_vec_pretty(&document).expect("serialize"),
    )
    .expect("write the broken fixture");
    harness::verify_oidc_fixture(broken.path(), &root)
        .expect_err("a declared JWK that is not on disk cannot satisfy the auth smoke");

    document["jwks_source"] = json!("../outside");
    std::fs::write(
        &oidc,
        serde_json::to_vec_pretty(&document).expect("serialize"),
    )
    .expect("write the broken fixture");
    harness::verify_oidc_fixture(broken.path(), &root)
        .expect_err("the JWKS source must stay inside the workspace");

    // --- unknown platforms have no archive name -------------------------
    harness::packaged_archive_file_name("solaris-sparc", version())
        .expect_err("a platform with no published release target has no archive");

    // --- a schema-invalid document is never emitted ---------------------
    let mut invalid = json!({
        "schema_version": schema::SCHEMA_VERSION,
        "qualifying": false,
        "runner_identity": { "os": "Linux", "os_version": "x", "arch": "x86_64" },
        "checks": [],
    });
    schema::validate(&invalid).expect_err("a document with no check is not evidence");
    invalid["checks"] = json!([{
        "name": "format",
        "required": true,
        "command": "cargo fmt",
        "observation": "executed",
        "exit_code": 0,
        "runner_identity": { "os": "Linux", "os_version": "x", "arch": "x86_64" },
        "source_commit": FIXTURE_COMMIT,
        "source_tree": FIXTURE_TREE,
    }]);
    schema::validate(&invalid).expect("a well-formed entry validates");
    invalid["checks"][0]["observation"] = json!("probably-fine");
    schema::validate(&invalid).expect_err("an observation outside the closed set is rejected");
    invalid["checks"][0]["observation"] = json!("executed");
    invalid["checks"][0]
        .as_object_mut()
        .expect("an object")
        .remove("runner_identity");
    schema::validate(&invalid).expect_err("a check with no runner identity is rejected");

    // --- the matrix document exposes both matrices -----------------------
    let document = good.matrix_document().expect("the matrix document derives");
    for key in ["engine_editor", "packaged_smoke"] {
        assert!(
            document[key]
                .as_array()
                .is_some_and(|rows| !rows.is_empty()),
            "the matrix document must expose a non-empty `{key}`"
        );
    }
    let rows: Vec<MatrixEntry> =
        serde_json::from_value(document["engine_editor"].clone()).expect("rows round-trip");
    assert!(rows.iter().any(|row| row.platform == LINUX_PLATFORM));
}
