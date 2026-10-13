// A complete, self-contained qualification fixture: a frozen candidate, the four
// independently supplied review artifacts, a Red evidence index and the empty
// output slots the six Todo-37 binaries expect.
//
// It is deliberately dependency-free beyond `serde_json`, `sha2` and `std`, so
// the `godot-mcp-server` end-to-end suite can compile the same builder without
// duplicating it or introducing a dependency edge onto the non-shipping
// `qualification-tools` crate.
//
// Nothing here ever writes into the real
// `.omo/evidence/godot-mcp-server-architecture/**` tree: every output lives under
// a caller-supplied temporary root.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// The workspace version the committed release fixtures were recorded at:
/// `tests/fixtures/release-candidate` observes `godot-mcp-<version>-<target>`
/// archives, and `tests/fixtures/release-archives` names its stand-ins the same way.
///
/// `ci_matrix_lock` calls the harness in process and evaluates the fixture at this
/// version. The producer here runs the harness as a separate binary, which
/// evaluates at its own version, so the fixture copy is renamed to that version
/// instead ([`reversion_release_fixture`]).
const FIXTURE_VERSION: &str = "0.1.0";

/// The version the release-gates harness evaluates a candidate at: the one the
/// `release-package` binary was built with, which is this workspace's.
#[must_use]
pub fn harness_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The host archive the F3 fixture turns into a real, extractable package.
#[must_use]
pub fn host_archive() -> String {
    format!(
        "godot-mcp-{}-x86_64-unknown-linux-musl.tar.gz",
        harness_version()
    )
}

/// The frozen candidate commit/tree the committed observation fixture records.
pub const CANDIDATE_COMMIT: &str = "1111111111111111111111111111111111111111";
pub const CANDIDATE_TREE: &str = "2222222222222222222222222222222222222222";

/// Evidence step names `f1-plan-compliance` requires the report to prove.
pub const REQUIRED_EVIDENCE: [&str; 7] = [
    "parity-stdio-http",
    "profile-transport-e2e",
    "artifact-parity",
    "trust-boundary",
    "durable-transaction",
    "auth",
    "identity-mutation",
];

/// The workspace root, two levels above the crate manifest this file is compiled
/// into.
#[must_use]
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the workspace root is two levels above the crate manifest")
        .to_path_buf()
}

/// The cargo target directory holding the built binaries, derived from the running
/// test executable rather than from a hard-coded `target/debug`.
#[must_use]
pub fn target_dir() -> PathBuf {
    let executable = std::env::current_exe().expect("the test executable has a path");
    executable
        .parent()
        .and_then(Path::parent)
        .expect("a test binary lives in <target>/<profile>/deps")
        .to_path_buf()
}

/// Path of one built `qualification-tools` binary.
#[must_use]
pub fn binary(name: &str) -> PathBuf {
    target_dir().join(name)
}

/// Lowercase hexadecimal SHA-256.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// SHA-256 of a file's bytes.
#[must_use]
pub fn file_sha256(path: &Path) -> String {
    sha256_hex(&std::fs::read(path).unwrap_or_else(|error| {
        panic!("fixture input {} must be readable: {error}", path.display())
    }))
}

/// Serialize to the canonical artifact byte form the gate machinery uses: compact
/// JSON with sorted keys and no trailing whitespace.
#[must_use]
pub fn canonical(value: &Value) -> Vec<u8> {
    serde_json::to_vec(value).expect("a fixture document always serializes")
}

/// Write a file, creating parents.
pub fn write(path: &Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("fixture parent directory");
    }
    std::fs::write(path, bytes).expect("fixture write");
}

/// Write a canonical document and its strict `<name>.sha256` sidecar.
pub fn write_pair(path: &Path, value: &Value) -> String {
    let bytes = canonical(value);
    let digest = sha256_hex(&bytes);
    write(path, &bytes);
    write(
        &path.with_extension("sha256"),
        format!("{digest}\n").as_bytes(),
    );
    digest
}

/// Recursively copy a directory tree.
pub fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("fixture destination directory");
    for entry in std::fs::read_dir(from).expect("fixture source directory") {
        let entry = entry.expect("fixture directory entry");
        let source = entry.path();
        let destination = to.join(entry.file_name());
        if source.is_dir() {
            copy_tree(&source, &destination);
        } else {
            std::fs::copy(&source, &destination).expect("fixture file copy");
        }
    }
}

/// Clone the repository, apply every tracked working-tree change, and commit the
/// result so candidate-binding tests always audit the exact source under test.
#[must_use]
pub fn clean_candidate_worktree(root: &Path) -> PathBuf {
    let workspace = repo_root();
    let worktree = root.join("candidate-worktree");
    let cloned = std::process::Command::new("git")
        .args(["clone", "--quiet", "--shared"])
        .arg(&workspace)
        .arg(&worktree)
        .status()
        .expect("git clone runs");
    assert!(cloned.success(), "the candidate worktree must clone");

    let changed = std::process::Command::new("git")
        .args(["diff", "--name-only", "-z", "HEAD", "--"])
        .current_dir(&workspace)
        .output()
        .expect("git diff runs");
    assert!(
        changed.status.success(),
        "tracked changes must be enumerable"
    );
    for raw in changed
        .stdout
        .split(|byte| *byte == 0)
        .filter(|raw| !raw.is_empty())
    {
        let relative = PathBuf::from(String::from_utf8(raw.to_vec()).expect("UTF-8 git path"));
        let source = workspace.join(&relative);
        let destination = worktree.join(&relative);
        if source.is_file() {
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent).expect("candidate parent directory");
            }
            std::fs::copy(&source, &destination).expect("copy tracked candidate change");
        } else if destination.exists() {
            std::fs::remove_file(&destination).expect("remove deleted candidate file");
        }
    }

    // A file a change introduces is part of the source under test as much as
    // a file it modifies: the producer binds every JSON file under `schemas/`
    // from the workspace it runs in, tracked or not, and the checkers review
    // the sources, real-entry scripts and documents a candidate ships. So every
    // untracked file under the source roots is carried into the candidate too.
    let untracked = std::process::Command::new("git")
        .args([
            "ls-files",
            "--others",
            "--exclude-standard",
            "-z",
            "--",
            "crates",
            "docs",
            "gdext-reference",
            "schemas",
            "scripts",
            "tests",
        ])
        .current_dir(&workspace)
        .output()
        .expect("git ls-files runs");
    assert!(
        untracked.status.success(),
        "untracked source files must be enumerable"
    );
    for raw in untracked
        .stdout
        .split(|byte| *byte == 0)
        .filter(|raw| !raw.is_empty())
    {
        let relative = PathBuf::from(String::from_utf8(raw.to_vec()).expect("UTF-8 git path"));
        let destination = worktree.join(&relative);
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).expect("candidate parent directory");
        }
        std::fs::copy(workspace.join(&relative), &destination)
            .expect("copy untracked candidate source");
    }

    commit_candidate_changes(&worktree);
    worktree
}

pub fn commit_candidate_changes(worktree: &Path) {
    let staged = std::process::Command::new("git")
        .args(["add", "-A"])
        .current_dir(worktree)
        .status()
        .expect("git add runs");
    assert!(staged.success(), "candidate changes must stage");
    let clean = std::process::Command::new("git")
        .args(["diff", "--cached", "--quiet"])
        .current_dir(worktree)
        .status()
        .expect("git diff --cached runs");
    if clean.success() {
        return;
    }
    let committed = std::process::Command::new("git")
        .args(["commit", "--quiet", "-m", "test candidate snapshot"])
        .env("GIT_AUTHOR_NAME", "Qualification Test")
        .env("GIT_AUTHOR_EMAIL", "qualification@example.invalid")
        .env("GIT_COMMITTER_NAME", "Qualification Test")
        .env("GIT_COMMITTER_EMAIL", "qualification@example.invalid")
        .current_dir(worktree)
        .status()
        .expect("git commit runs");
    assert!(committed.success(), "candidate snapshot must commit");
}

/// Resolve one git revision in a fixture candidate worktree.
#[must_use]
pub fn git_revision(worktree: &Path, revision: &str) -> String {
    let output = std::process::Command::new("git")
        .args(["rev-parse", revision])
        .current_dir(worktree)
        .output()
        .expect("git rev-parse runs");
    assert!(output.status.success(), "{revision} must resolve");
    String::from_utf8(output.stdout)
        .expect("git revision is UTF-8")
        .trim()
        .to_owned()
}

/// Point the producer's candidate identity at an actual clean git worktree.
pub fn bind_candidate_identity(fixture: &Fixture, worktree: &Path) {
    let commit = git_revision(worktree, "HEAD");
    let tree = git_revision(worktree, "HEAD^{tree}");
    write(
        &fixture.candidate_identity,
        &canonical(&json!({
            "commit": commit,
            "frozen_at": "fixture",
            "tree": tree,
        })),
    );
    replace_candidate_placeholders(&fixture.candidate_worktree, &commit, &tree);
}

pub fn rebind_qualification_candidate(fixture: &Fixture, worktree: &Path) {
    let commit = git_revision(worktree, "HEAD");
    let tree = git_revision(worktree, "HEAD^{tree}");
    write(
        &fixture.candidate_identity,
        &canonical(&json!({
            "commit": commit,
            "frozen_at": "fixture",
            "tree": tree,
        })),
    );
    let mut identity = read_json(&fixture.input_identity);
    identity["candidate_commit"] = json!(commit);
    identity["candidate_tree"] = json!(tree);
    identity["bound_inputs"]["candidate-identity"]["sha256"] =
        json!(file_sha256(&fixture.candidate_identity));
    let identity_sha256 = write_pair(&fixture.input_identity, &identity);
    let mut report = read_json(&fixture.qualification_report);
    report["input_identity_sha256"] = json!(identity_sha256);
    write_pair(&fixture.qualification_report, &report);
}

fn replace_candidate_placeholders(root: &Path, commit: &str, tree: &str) {
    for entry in std::fs::read_dir(root).expect("candidate fixture directory") {
        let path = entry.expect("candidate fixture entry").path();
        if path.is_dir() {
            replace_candidate_placeholders(&path, commit, tree);
            continue;
        }
        let bytes = std::fs::read(&path).expect("candidate fixture bytes");
        let text = String::from_utf8(bytes).expect("candidate fixture is textual");
        let replaced = text
            .replace(CANDIDATE_COMMIT, commit)
            .replace(CANDIDATE_TREE, tree);
        std::fs::write(&path, replaced).expect("candidate fixture identity rewrite");
    }
}

/// Package the fake release directory into the host archive F3 will extract.
pub fn archive_package(package: &Path, archive: &Path) {
    let parent = package.parent().expect("the package has a parent");
    let name = package.file_name().expect("the package has a name");
    let status = std::process::Command::new("tar")
        .arg("-czf")
        .arg(archive)
        .arg("-C")
        .arg(parent)
        .arg(name)
        .status()
        .expect("tar runs");
    assert!(status.success(), "the fake package archive must be created");
}

pub fn bind_host_archive_observation(fixture: &Fixture, archive: &Path) {
    let path = fixture
        .candidate_worktree
        .join("ci/observations/all-required-checks-passed.json");
    let mut observations: Value = serde_json::from_slice(
        &std::fs::read(&path).expect("candidate observations must be readable"),
    )
    .expect("candidate observations must be JSON");
    let entry = observations
        .as_array_mut()
        .expect("candidate observations array")
        .iter_mut()
        .find(|entry| {
            entry.get("name").and_then(Value::as_str) == Some("packaged-smoke-linux-x86_64-4.7.1")
        })
        .expect("host packaged-smoke observation");
    entry["archive_sha256"] = json!(file_sha256(archive));
    write(&path, &canonical(&observations));
}

/// Rename the copied release fixture from [`FIXTURE_VERSION`] to
/// [`harness_version`]: the archive stand-ins, their checksum lines and the
/// packaged-smoke observations that name them.
///
/// The harness binds every packaged smoke to the archive name of the version it
/// was built with, so a fixture left at the version it was recorded at fails from
/// the first release after it. The stand-ins are opaque bytes bound by digest, so
/// renaming one changes no digest.
fn reversion_release_fixture(candidate: &Path, archives: &Path) {
    let from = format!("godot-mcp-{FIXTURE_VERSION}-");
    let to = format!("godot-mcp-{}-", harness_version());
    if from == to {
        return;
    }
    let names: Vec<String> = std::fs::read_dir(archives)
        .expect("archive fixture directory")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    for name in names {
        if let Some(rest) = name.strip_prefix(&from) {
            std::fs::rename(archives.join(&name), archives.join(format!("{to}{rest}")))
                .expect("rename an archive stand-in");
        }
    }
    for file in [
        archives.join("SHA256SUMS.txt"),
        candidate.join("ci/observations/all-required-checks-passed.json"),
    ] {
        let text = std::fs::read_to_string(&file).expect("a fixture text file");
        assert!(
            text.contains(&from),
            "{} names no {from} archive",
            file.display()
        );
        write(&file, text.replace(&from, &to).as_bytes());
    }
}

/// Rewrite the fixture checksum catalogue after replacing one opaque stand-in
/// with a real archive.
pub fn refresh_archive_checksums(archives: &Path) {
    let mut names: Vec<String> = std::fs::read_dir(archives)
        .expect("archive fixture directory")
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            (name.ends_with(".tar.gz") || name.ends_with(".zip")).then_some(name)
        })
        .collect();
    names.sort();
    let mut manifest = String::new();
    for name in names {
        manifest.push_str(&format!("{}  {name}\n", file_sha256(&archives.join(&name))));
    }
    write(&archives.join("SHA256SUMS.txt"), manifest.as_bytes());
}

pub fn rebind_qualification_archive(fixture: &Fixture, archive: &Path) {
    let name = archive
        .file_name()
        .expect("candidate archive name")
        .to_string_lossy();
    let mut identity = read_json(&fixture.input_identity);
    identity["bound_inputs"][format!("archive:{name}")]["sha256"] = json!(file_sha256(archive));
    let identity_sha256 = write_pair(&fixture.input_identity, &identity);
    let mut report = read_json(&fixture.qualification_report);
    report["input_identity_sha256"] = json!(identity_sha256);
    write_pair(&fixture.qualification_report, &report);
}

/// A complete qualification fixture rooted at a caller-owned temporary directory.
pub struct Fixture {
    pub root: PathBuf,
    pub workspace: PathBuf,
    pub candidate_worktree: PathBuf,
    pub candidate_identity: PathBuf,
    pub plan: PathBuf,
    pub cargo_lock: PathBuf,
    pub rust_toolchain: PathBuf,
    pub tooling_lock: PathBuf,
    pub godot_lock: PathBuf,
    pub archives_dir: PathBuf,
    pub momus_receipt: PathBuf,
    pub codex_receipt: PathBuf,
    pub b3_approval: PathBuf,
    pub b4_approval: PathBuf,
    pub red_evidence_index: PathBuf,
    pub release_gates: PathBuf,
    pub review_bindings: PathBuf,
    pub input_identity: PathBuf,
    pub qualification_report: PathBuf,
    pub final_dir: PathBuf,
    pub evidence_plan: PathBuf,
    pub plan_sha256: String,
}

impl Fixture {
    /// Build the fixture. Only read-only inputs are created; all seven producer
    /// output slots stay absent.
    #[must_use]
    pub fn build(root: &Path) -> Self {
        let workspace = repo_root();
        let plan = workspace
            .join(".omo")
            .join("plans")
            .join("godot-mcp-server-architecture.md");
        let plan_sha256 = file_sha256(&plan);

        let candidate_worktree = root.join("candidate");
        copy_tree(
            &workspace
                .join("tests")
                .join("fixtures")
                .join("release-candidate"),
            &candidate_worktree,
        );
        let archives_dir = root.join("archives");
        copy_tree(
            &workspace
                .join("tests")
                .join("fixtures")
                .join("release-archives"),
            &archives_dir,
        );
        reversion_release_fixture(&candidate_worktree, &archives_dir);

        let candidate_identity = root.join("candidate-identity.json");
        write(
            &candidate_identity,
            &canonical(&json!({
                "commit": CANDIDATE_COMMIT,
                "frozen_at": "fixture",
                "tree": CANDIDATE_TREE,
            })),
        );

        let reviews = root.join("reviews");
        let momus_receipt = reviews.join("current-momus-receipt.json");
        let codex_receipt = reviews.join("current-codex-receipt.json");
        write_pair(
            &momus_receipt,
            &architecture_receipt("momus", "round-fixture-1", &plan_sha256),
        );
        write_pair(
            &codex_receipt,
            &architecture_receipt("codex", "round-fixture-1", &plan_sha256),
        );

        let gates = root.join("gates");
        let b3_approval = gates.join("b3-approval-manifest.json");
        let b4_approval = gates.join("b4-approval-manifest.json");
        write_pair(&b3_approval, &batch_approval("b3", &plan_sha256));
        write_pair(&b4_approval, &batch_approval("b4", &plan_sha256));

        let red_evidence_index = root.join("red-evidence-index.json");
        write(&red_evidence_index, &canonical(&red_index(&workspace)));

        let evidence_plan = root.join("evidence-plan.json");
        write(&evidence_plan, &canonical(&passing_step_plan()));

        Self {
            root: root.to_path_buf(),
            workspace: workspace.clone(),
            candidate_worktree,
            candidate_identity,
            plan,
            cargo_lock: workspace.join("Cargo.lock"),
            rust_toolchain: workspace.join("rust-toolchain.toml"),
            tooling_lock: workspace.join("tooling.lock.json"),
            godot_lock: workspace
                .join("tests")
                .join("fixtures")
                .join("godot-releases.lock.json"),
            archives_dir,
            momus_receipt,
            codex_receipt,
            b3_approval,
            b4_approval,
            red_evidence_index,
            release_gates: root.join("external").join("release-gates.json"),
            review_bindings: root.join("qualification").join("review-bindings.json"),
            input_identity: root.join("qualification").join("input-identity.json"),
            qualification_report: root.join("qualification").join("qualification-report.json"),
            final_dir: root.join("final"),
            evidence_plan,
            plan_sha256,
        }
    }

    /// The seven producer output paths, in publication order.
    #[must_use]
    pub fn outputs(&self) -> Vec<PathBuf> {
        vec![
            self.release_gates.clone(),
            sidecar(&self.release_gates),
            self.review_bindings.clone(),
            self.input_identity.clone(),
            sidecar(&self.input_identity),
            self.qualification_report.clone(),
            sidecar(&self.qualification_report),
        ]
    }

    /// Whether every producer output slot is empty.
    #[must_use]
    pub fn all_outputs_absent(&self) -> bool {
        self.outputs().iter().all(|path| !path.exists())
    }

    /// The exact producer argument vector the plan's command specifies.
    #[must_use]
    pub fn producer_args(&self) -> Vec<String> {
        let pair =
            |flag: &str, path: &Path| vec![flag.to_owned(), path.to_string_lossy().to_string()];
        let mut args = Vec::new();
        args.extend(pair("--candidate-worktree", &self.candidate_worktree));
        args.extend(pair("--candidate-identity", &self.candidate_identity));
        args.extend(pair("--plan", &self.plan));
        args.extend(pair("--cargo-lock", &self.cargo_lock));
        args.extend(pair("--rust-toolchain", &self.rust_toolchain));
        args.extend(pair("--tooling-lock", &self.tooling_lock));
        args.extend(pair("--godot-lock", &self.godot_lock));
        args.extend(pair("--archives-dir", &self.archives_dir));
        args.extend(pair("--architecture-momus-receipt", &self.momus_receipt));
        args.extend(pair(
            "--architecture-momus-receipt-sha",
            &sidecar(&self.momus_receipt),
        ));
        args.extend(pair("--architecture-codex-receipt", &self.codex_receipt));
        args.extend(pair(
            "--architecture-codex-receipt-sha",
            &sidecar(&self.codex_receipt),
        ));
        args.extend(pair("--b3-approval", &self.b3_approval));
        args.extend(pair("--b3-approval-sha", &sidecar(&self.b3_approval)));
        args.extend(pair("--b4-approval", &self.b4_approval));
        args.extend(pair("--b4-approval-sha", &sidecar(&self.b4_approval)));
        args.extend(pair("--red-evidence-index", &self.red_evidence_index));
        args.extend(pair("--release-gates", &self.release_gates));
        args.extend(pair("--release-gates-sha", &sidecar(&self.release_gates)));
        args.extend(pair("--review-bindings", &self.review_bindings));
        args.extend(pair("--input-identity", &self.input_identity));
        args.extend(pair("--input-identity-sha", &sidecar(&self.input_identity)));
        args.extend(pair("--qualification-report", &self.qualification_report));
        args.extend(pair(
            "--qualification-report-sha",
            &sidecar(&self.qualification_report),
        ));
        args
    }

    /// Run `produce-qualification` with the fixture's exact argument vector.
    #[must_use]
    pub fn run_producer(&self, faults: Option<&str>) -> Outcome {
        let mut command = std::process::Command::new(binary("produce-qualification"));
        command
            .args(self.producer_args())
            .current_dir(&self.workspace)
            .env("GODOT_MCP_QUALIFICATION_EVIDENCE_PLAN", &self.evidence_plan)
            .env("GODOT_MCP_RELEASE_GATES_HARNESS", release_package_binary());
        if let Some(point) = faults {
            command.env("GODOT_MCP_QUALIFICATION_FAULT", point);
        }
        Outcome::of(command)
    }

    /// Remove every published producer output so a later case starts from empty
    /// slots.
    pub fn clear_outputs(&self) {
        for path in self.outputs() {
            let _ = std::fs::remove_file(path);
        }
    }

    /// Run one F1–F4 checker against the published qualification pair.
    #[must_use]
    pub fn run_checker(&self, checker: &str, extra: &[(&str, &Path)]) -> Outcome {
        let output = self.final_dir.join(format!("{checker}.json"));
        let mut command = std::process::Command::new(binary(checker));
        command
            .arg("--identity")
            .arg(&self.input_identity)
            .arg("--report")
            .arg(&self.qualification_report);
        for (flag, value) in extra {
            command.arg(flag).arg(value);
        }
        command
            .arg("--output")
            .arg(&output)
            .current_dir(&self.workspace);
        Outcome::of(command)
    }

    /// Run `finalize-approval` over the four published receipts.
    #[must_use]
    pub fn run_finalizer(&self) -> Outcome {
        let mut command = std::process::Command::new(binary("finalize-approval"));
        command
            .arg("--input")
            .arg(&self.input_identity)
            .arg("--report")
            .arg(&self.qualification_report)
            .arg("--reviews")
            .arg(&self.review_bindings)
            .arg("--release-gates")
            .arg(&self.release_gates)
            .arg("--receipts-dir")
            .arg(&self.final_dir)
            .arg("--schema")
            .arg(
                self.workspace
                    .join("schemas/final-approval-manifest.schema.json"),
            )
            .arg("--output")
            .arg(self.final_dir.join("final-manifest.json"))
            .current_dir(&self.workspace);
        Outcome::of(command)
    }
}

/// The `.sha256` sidecar path implied by an artifact path.
#[must_use]
pub fn sidecar(artifact: &Path) -> PathBuf {
    artifact.with_extension("sha256")
}

/// The already-built `release-package` binary the producer drives as Todo 35's
/// frozen harness.
#[must_use]
pub fn release_package_binary() -> PathBuf {
    let path = binary("release-package");
    assert!(
        path.is_file(),
        "the frozen Todo 35 harness binary must be built before the qualification suite runs: {}",
        path.display()
    );
    path
}

/// Build `release-package` once, so the fixture never shells out to a nested
/// `cargo run` inside the producer.
pub fn build_release_package() {
    let status = std::process::Command::new(env!("CARGO"))
        .args([
            "build",
            "--locked",
            "-p",
            "godot-mcp-cli",
            "--features",
            "test-hooks",
            "--bin",
            "release-package",
        ])
        .current_dir(repo_root())
        .status()
        .expect("cargo build runs");
    assert!(status.success(), "the Todo 35 harness binary must build");
}

/// One captured process result.
pub struct Outcome {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Outcome {
    /// Run a fully configured command and capture its real result.
    #[must_use]
    pub fn of_command(command: std::process::Command) -> Self {
        Self::of(command)
    }

    fn of(mut command: std::process::Command) -> Self {
        let output = command.output().expect("a qualification binary runs");
        Self {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        }
    }

    #[must_use]
    pub fn succeeded(&self) -> bool {
        self.code == Some(0)
    }

    #[must_use]
    pub fn failed(&self) -> bool {
        !self.succeeded()
    }

    /// The parsed stdout verdict document.
    #[must_use]
    pub fn json(&self) -> Value {
        serde_json::from_str(self.stdout.trim()).unwrap_or_else(|error| {
            panic!("stdout must be one JSON document: {error}\n{}", self.stdout)
        })
    }
}

/// One independent architecture-review receipt.
#[must_use]
pub fn architecture_receipt(reviewer: &str, round: &str, plan_sha256: &str) -> Value {
    json!({
        "findings": [],
        "plan_sha256": plan_sha256,
        "reviewer_identity": reviewer,
        "round_id": round,
        "schema_version": "1",
        "verdict": "APPROVE",
    })
}

/// One B3/B4 approval manifest with two independent receipt identities.
#[must_use]
pub fn batch_approval(batch: &str, plan_sha256: &str) -> Value {
    json!({
        "approved": true,
        "batch": batch,
        "bundle_sha256": sha256_hex(format!("bundle-{batch}").as_bytes()),
        "plan_sha256": plan_sha256,
        "receipts": [
            {
                "receipt_sha256": sha256_hex(format!("{batch}-momus").as_bytes()),
                "reviewer_identity": "momus",
            },
            {
                "receipt_sha256": sha256_hex(format!("{batch}-oracle").as_bytes()),
                "reviewer_identity": "oracle",
            },
        ],
        "schema_version": "1",
    })
}

/// A Red evidence index bound to real committed Red/Green artifacts.
#[must_use]
pub fn red_index(workspace: &Path) -> Value {
    let evidence = workspace
        .join(".omo")
        .join("evidence")
        .join("godot-mcp-server-architecture");
    let mut entries = Vec::new();
    for todo in [7_u32, 8, 9] {
        let red = evidence.join(format!("task-{todo}-red.txt"));
        let green = evidence.join(format!("task-{todo}-green.txt"));
        entries.push(json!({
            "command": format!("cargo +1.96.0 test -p godot-mcp-analysis --test todo{todo}"),
            "green_exit_code": 0,
            "green_output": green.to_string_lossy(),
            "green_output_sha256": file_sha256(&green),
            "red_exit_code": 101,
            "red_output": red.to_string_lossy(),
            "red_output_sha256": file_sha256(&red),
            "todo": todo,
        }));
    }
    json!({ "entries": entries, "schema_version": "1" })
}

/// A step catalogue whose every entry really runs and really exits 0.
#[must_use]
pub fn passing_step_plan() -> Value {
    let mut steps = Vec::new();
    for name in [
        "parity-stdio-http",
        "profile-transport-e2e",
        "artifact-parity",
        "trust-boundary",
        "durable-transaction",
        "queue-cancellation",
        "auth",
        "artifact-outcomes",
        "audit-limits",
        "operator-repair",
        "recovery",
    ] {
        steps.push(json!({
            "args": ["-c", "exit 0"],
            "command": format!("sh -c 'exit 0' # {name}"),
            "expected_exit": 0,
            "name": name,
            "program": "sh",
        }));
    }
    Value::Array(steps)
}

/// A step catalogue whose named entry really fails.
#[must_use]
pub fn failing_step_plan(failing: &str) -> Value {
    let Value::Array(mut steps) = passing_step_plan() else {
        unreachable!("passing_step_plan always builds an array")
    };
    for step in &mut steps {
        if step.get("name").and_then(Value::as_str) == Some(failing) {
            step["args"] = json!(["-c", "exit 3"]);
            step["command"] = json!(format!("sh -c 'exit 3' # {failing}"));
        }
    }
    Value::Array(steps)
}

/// A minimal mirror of the workspace surfaces `f4-scope-fidelity` compares, so a
/// negative case can add an intruding document without ever touching the real
/// repository tree.
#[must_use]
pub fn mirror_workspace(root: &Path) -> PathBuf {
    let source = repo_root();
    let mirror = root.join("mirror");
    for relative in ["schemas", "docs"] {
        copy_tree(&source.join(relative), &mirror.join(relative));
    }
    std::fs::create_dir_all(&mirror).expect("mirror root");
    std::fs::copy(source.join("Cargo.lock"), mirror.join("Cargo.lock")).expect("mirror Cargo.lock");
    mirror
}

/// A minimal extracted package whose `MANIFEST.json` really describes its bytes:
/// the shipping executable and required supervisor under `bin/`, one example
/// config, and the manifest.
#[must_use]
pub fn fake_package(root: &Path) -> PathBuf {
    let package = root.join("package");
    let executable = package.join("bin").join("godot-mcp");
    let supervisor = package.join("bin").join("godot-mcp-supervisor");
    let example = package.join("examples").join("godot-mcp.toml");
    write(
        &executable,
        br##"#!/bin/sh
if [ "$1" != "transaction" ]; then exit 0; fi
fixture_root=${GODOT_MCP_QA_FIXTURE_ROOT:-$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)}
if [ ! -f "$fixture_root/task34-fixture.active" ]; then exit 3; fi
if [ "$2" != "repair" ] || [ "$3" != "--project" ] ||
   [ "$5" != "--transaction-id" ] || [ "$7" != "--operation" ] ||
   [ "$8" != "acknowledge-safe-state" ] || [ "$9" != "--proof-file" ]; then
  exit 2
fi
case "$6:$(basename "${10}")" in
  34000000-0000-4000-8000-000000001001:composite-linked-proof-missing.json|\
  34000000-0000-4000-8000-000000001002:composite-linked-proof-wrong-transaction.json|\
  34000000-0000-4000-8000-000000001003:composite-linked-process-live.json|\
  34000000-0000-4000-8000-000000001004:composite-linked-process-signaled.json|\
  34000000-0000-4000-8000-000000001005:composite-linked-process-nonzero-exit.json|\
  34000000-0000-4000-8000-000000001006:composite-linked-boundary-nonempty.json)
    exit 1
    ;;
  *)
    exit 2
    ;;
esac
"##,
    );
    write(&supervisor, b"#!/bin/sh\nexit 42\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        for binary in [&executable, &supervisor] {
            let mut permissions = std::fs::metadata(binary)
                .expect("packaged binary metadata")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(binary, permissions).expect("packaged binary mode");
        }
    }
    write(&example, b"[server]\nprofile = \"core\"\n");
    let files: Vec<Value> = [
        ("bin/godot-mcp", &executable),
        ("bin/godot-mcp-supervisor", &supervisor),
        ("examples/godot-mcp.toml", &example),
    ]
    .iter()
    .map(|(relative, path)| {
        let bytes = std::fs::read(path).expect("packaged payload");
        json!({
            "path": relative,
            "sha256": sha256_hex(&bytes),
            "size": bytes.len(),
        })
    })
    .collect();
    write(
        &package.join("MANIFEST.json"),
        &canonical(&json!({
            "executable": "bin/godot-mcp",
            "supervisor": "bin/godot-mcp-supervisor",
            "files": files,
            "target": "x86_64-unknown-linux-musl",
            "version": harness_version(),
        })),
    );
    package
}

#[must_use]
pub fn fake_fixture_cargo(root: &Path) -> PathBuf {
    let cargo = root.join("fake-cargo");
    let active = root.join("task34-fixture.active");
    let cleaned = root.join("task34-fixture.cleaned");
    write(
        &cargo,
        format!(
            "#!/bin/sh\n\
             case \"$*\" in\n\
               \"run --locked -p godot-mcp-cli --features test-hooks --bin qa-fixture-34 -- setup --scenario task34\")\n\
                 rm -f '{}'; : > '{}'; exit 0 ;;\n\
               \"run --locked -p godot-mcp-cli --features test-hooks --bin qa-fixture-34 -- cleanup --scenario task34\")\n\
                 rm -f '{}'; : > '{}'; exit 0 ;;\n\
               *) exit 2 ;;\n\
             esac\n",
            cleaned.display(),
            active.display(),
            active.display(),
            cleaned.display(),
        )
        .as_bytes(),
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mut permissions = std::fs::metadata(&cargo)
            .expect("fake cargo metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&cargo, permissions).expect("fake cargo mode");
    }
    cargo
}

/// The four F1–F4 checker names.
pub const CHECKERS: [&str; 4] = [
    "f1-plan-compliance",
    "f2-code-quality",
    "f3-real-qa",
    "f4-scope-fidelity",
];

/// Read a published JSON artifact.
#[must_use]
pub fn read_json(path: &Path) -> Value {
    let bytes = std::fs::read(path)
        .unwrap_or_else(|error| panic!("{} must exist: {error}", path.display()));
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| panic!("{} must be JSON: {error}", path.display()))
}

/// Overwrite a published artifact with a mutated document, leaving its sidecar
/// stale so any consumer detects the mutation.
pub fn mutate(path: &Path, mutation: impl FnOnce(&mut Value)) {
    let mut document = read_json(path);
    mutation(&mut document);
    write(path, &canonical(&document));
}

/// Sidecar digests of the two bound qualification documents.
#[must_use]
pub fn bound_digests(fixture: &Fixture) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    out.insert(
        "identity".to_owned(),
        std::fs::read_to_string(sidecar(&fixture.input_identity))
            .expect("identity sidecar")
            .trim()
            .to_owned(),
    );
    out.insert(
        "report".to_owned(),
        std::fs::read_to_string(sidecar(&fixture.qualification_report))
            .expect("report sidecar")
            .trim()
            .to_owned(),
    );
    out
}
