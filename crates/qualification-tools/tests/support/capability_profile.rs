use std::path::{Path, PathBuf};
use std::sync::Once;

use serde_json::Value;

use crate::support::{self, Fixture, Outcome, binary, canonical, read_json, write};

pub const VARIANTS: [(&str, &str); 4] = [
    ("Core", "core"),
    ("Editor", "editor"),
    ("Runtime", "runtime"),
    ("Full", "full"),
];
pub const SAFE_DESCRIPTIONS: [&str; 4] = [
    "Adds static analysis tools.",
    "Adds file-backed project tools.",
    "Adds process-backed project tools.",
    "Combines every profile.",
];

pub struct GuardHarness {
    _root: tempfile::TempDir,
    fixture: Fixture,
    gate_plan: PathBuf,
}

impl GuardHarness {
    pub fn build() -> Self {
        static BUILD_RELEASE_PACKAGE: Once = Once::new();
        BUILD_RELEASE_PACKAGE.call_once(support::build_release_package);
        let root = tempfile::tempdir().expect("temporary guard fixture");
        let fixture = Fixture::build(root.path());
        let produced = fixture.run_producer(None);
        assert!(
            produced.succeeded(),
            "the guard fixture producer must succeed: {}\n{}",
            produced.stdout,
            produced.stderr
        );
        let gate_plan = root.path().join("gate-plan.json");
        write(&gate_plan, &canonical(&support::passing_step_plan()));
        Self {
            _root: root,
            fixture,
            gate_plan,
        }
    }

    pub fn mirror(&self, name: &str) -> PathBuf {
        let root = self.fixture.root.join(name);
        let mirror = support::mirror_workspace(&root);
        support::copy_tree(&support::repo_root().join("crates"), &mirror.join("crates"));
        // These guards are the first release's: a mirror without the contract
        // 0.2 discovery manifests is a first-release candidate, whose profile
        // descriptions must not claim a cancelled capability.
        let complete = mirror.join("schemas/domain/profiles/v0.2");
        if complete.exists() {
            std::fs::remove_dir_all(&complete).expect("drop the 0.2 manifests");
        }
        let initialized = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&mirror)
            .status()
            .expect("git init runs");
        assert!(initialized.success(), "the guard mirror must initialize");
        support::commit_candidate_changes(&mirror);
        mirror
    }

    pub fn run_f2(&self, mirror: &Path, label: &str) -> (Outcome, PathBuf) {
        support::commit_candidate_changes(mirror);
        support::rebind_qualification_candidate(&self.fixture, mirror);
        let output = mirror.join(format!("f2-{label}.json"));
        let mut command = std::process::Command::new(binary("f2-code-quality"));
        command
            .arg("--identity")
            .arg(&self.fixture.input_identity)
            .arg("--report")
            .arg(&self.fixture.qualification_report)
            .arg("--worktree")
            .arg(mirror)
            .arg("--output")
            .arg(&output)
            .current_dir(&self.fixture.workspace)
            .env("GODOT_MCP_QUALIFICATION_GATE_PLAN", &self.gate_plan);
        (Outcome::of_command(command), output)
    }

    pub fn run_f4(&self, mirror: &Path, label: &str) -> Outcome {
        let mut command = std::process::Command::new(binary("f4-scope-fidelity"));
        command
            .arg("--identity")
            .arg(&self.fixture.input_identity)
            .arg("--report")
            .arg(&self.fixture.qualification_report)
            .arg("--plan")
            .arg(&self.fixture.plan)
            .arg("--output")
            .arg(mirror.join(format!("f4-{label}.json")))
            .current_dir(mirror);
        Outcome::of_command(command)
    }
}

pub fn set_descriptions(mirror: &Path, descriptions: [&str; 4]) {
    let rustdoc = VARIANTS
        .iter()
        .zip(descriptions)
        .map(|((rust_name, _), description)| format!("    /// {description}\n    {rust_name},\n"))
        .collect::<String>();
    write(
        &mirror.join("crates/godot-mcp-domain/src/lib.rs"),
        format!("#![forbid(unsafe_code)]\n\npub enum CapabilityProfile {{\n{rustdoc}}}\n")
            .as_bytes(),
    );

    let schema_path = mirror.join("schemas/domain/contract-v1.schema.json");
    let mut schema = read_json(&schema_path);
    let variants = schema
        .pointer_mut("/$defs/CapabilityProfile/oneOf")
        .and_then(Value::as_array_mut)
        .expect("CapabilityProfile oneOf array");
    for ((_, schema_name), description) in VARIANTS.iter().zip(descriptions) {
        let variant = variants
            .iter_mut()
            .find(|variant| variant.get("const").and_then(Value::as_str) == Some(*schema_name))
            .expect("named CapabilityProfile variant");
        variant["description"] = Value::String(description.to_owned());
    }
    write(
        &schema_path,
        &serde_json::to_vec_pretty(&schema).expect("schema JSON"),
    );
}

pub fn line_of(path: &Path, needle: &str) -> usize {
    std::fs::read_to_string(path)
        .expect("fixture text")
        .lines()
        .position(|line| line.contains(needle))
        .map_or(0, |index| index + 1)
}

pub fn finding_summaries(path: &Path) -> String {
    read_json(path)
        .get("findings")
        .and_then(Value::as_array)
        .expect("receipt findings")
        .iter()
        .filter_map(|finding| finding.get("summary").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}
