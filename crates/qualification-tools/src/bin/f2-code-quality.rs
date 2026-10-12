//! `f2-code-quality` — NON-SHIPPING qualification tool (implemented by Todo 37).
//!
//! Runs the complete aggregate gate set inside the frozen clean worktree and
//! aggregates their **real** exit codes. Nothing is waived, nothing is
//! special-cased, and a gate that cannot be spawned records a nonzero sentinel
//! rather than a pass.
//!
//! Committed CLI surface (normative, from the plan's F2 body):
//!
//! ```text
//! f2-code-quality --identity  <input-identity.json>
//!                 --report    <qualification-report.json>
//!                 --worktree  <frozen-clean-worktree>
//!                 --output    <final/f2-code-quality.json>
//! ```
//!
//! ## The seven aggregate gates
//!
//! Every one of these must have captured exit 0:
//!
//! 1. `cargo +1.96.0 fmt --all -- --check`
//! 2. `cargo +1.96.0 clippy --workspace --all-targets --all-features -- -D warnings`
//! 3. `cargo +1.96.0 test --workspace --all-features`
//! 4. `cargo +1.96.0 build --workspace --all-features --locked`
//! 5. `cargo +1.96.0 deny check`
//! 6. `cargo +1.96.0 cyclonedx --format json`
//! 7. `RUSTUP_TOOLCHAIN=1.96.0 make godot-test`
//!
//! ## The cancelled editor-plane sub-check
//!
//! The main-thread thread-ID sentinel and the `EditorUndoRedoManager` mutation
//! code the plan's F2 line originally also named do **not** exist, because Todos
//! 20–25 are cancelled. That sub-check is therefore satisfied only by proving
//! their **absence** together with the honest `unavailable` refusal on the
//! editor/runtime branches. F2 never reports either as reviewed working
//! behaviour.
//!
//! ## The complete-release sub-check
//!
//! A candidate that ships the contract 0.2 discovery manifests is the complete
//! release ([`CandidateRelease`]): it delivers the editor, runtime and export
//! planes, so proving them absent would pass only for the wrong reason. For it
//! F2 proves them **present** instead — the add-on and runtime probe sources, the
//! real-entry checks that drive them against real Godot builds, the full
//! profile discovering one tool of every delivered plane — and that a native
//! extension appears nowhere but the isolated gdext reference lane.

use std::path::Path;
use std::process::ExitCode;

use qualification_tools::gates::{GateError, implied_sidecar, read_bytes, report};
use qualification_tools::qualification::{
    Bound, COMPLETE_RELEASE_EVIDENCE, COMPLETE_RELEASE_MANIFEST, COMPLETE_RELEASE_TOOLS,
    CandidateRelease, Findings, Flags, GATE_PLAN_ENV, GDEXT_REFERENCE_LANE, PINNED_TOOLCHAIN,
    StepSpec, bound_hash, emit_receipt, load_bound, load_step_plan, normalize, print_verdict,
    read_seam, remeasure_bound_hash, require_absent, require_dir, require_file, run_step,
    verify_candidate_worktree,
};

const CHECKER: &str = "f2-code-quality";

const USAGE: &str = "f2-code-quality --identity <input-identity.json> --report \
     <qualification-report.json> --worktree <frozen-clean-worktree> --output \
     <f2-code-quality.json>";

const FLAGS: [&str; 4] = ["--identity", "--report", "--worktree", "--output"];

/// Cancelled capability phrases as they appear in delivery claims. Each entry is
/// multi-word or CamelCase, except the established domain term `screenshot`, so
/// ordinary prose such as "input validation" cannot collide with the guard.
const CANCELLED_PROFILE_CLAIMS: [&str; 16] = [
    "editor bridge",
    "editor-bridge",
    "editor plugin",
    "EditorPlugin",
    "EditorUndoRedoManager",
    "undoable scene/resource mutation",
    "undoable scene mutation",
    "undoable resource mutation",
    "LSP/DAP debug",
    "run / debug",
    "screenshot",
    "runtime recording",
    "recording capture",
    "runtime inspection",
    "input simulation",
    "screenshot / input",
];

const DESCRIPTION_HEDGES: [&str; 6] = [
    "first-release-absent",
    "not shipped",
    "unavailable",
    "cancelled",
    "does not provide",
    "not delivered",
];

fn run() -> Result<ExitCode, GateError> {
    let flags = Flags::parse(USAGE, &FLAGS)?;
    let identity_path = flags.path("--identity")?;
    let report_path = flags.path("--report")?;
    let worktree = flags.path("--worktree")?;
    let output = flags.path("--output")?;

    require_file("--identity", &identity_path)?;
    require_file("--report", &report_path)?;
    require_dir("--worktree", &worktree)?;
    require_absent("--output", &output)?;
    require_absent("--output sidecar", &implied_sidecar(&output))?;

    let workspace = std::env::current_dir().map_err(|error| GateError::Io {
        path: "<cwd>".to_owned(),
        reason: error.to_string(),
    })?;
    let bound = load_bound(&identity_path, &report_path, &workspace)?;
    let before = bound_hash(&bound.identity_sha256, &bound.report_sha256);

    let mut findings = Findings::new();
    let candidate_input = verify_candidate_worktree(&mut findings, &worktree, &bound);
    let mut failing = Vec::new();
    if candidate_input.verified() {
        let gates = aggregate_gates(&output)?;
        for spec in &gates {
            let outcome = run_step(spec, &worktree);
            if !outcome.passed() {
                failing.push(format!("{}: exit {}", outcome.spec.name, outcome.exit_code));
                findings.blocker(format!(
                    "aggregate gate `{}` ({}) exited {} in the frozen clean worktree; F2 approves \
                     only when every one of the seven gates captured exit 0",
                    outcome.spec.name, outcome.spec.command, outcome.exit_code
                ));
            }
        }
        review_frozen_source(&mut findings, &worktree, &bound);
    } else {
        failing.push("candidate-input: worktree is not the frozen candidate".to_owned());
    }
    let after = remeasure_bound_hash(&bound)?;

    let outcome = emit_receipt(
        CHECKER,
        &output,
        &workspace,
        &bound,
        (&before, &after),
        &candidate_input,
        findings.into_vec(),
    )?;
    print_verdict(&outcome, "failing_gates", &failing);
    Ok(outcome.exit_code())
}

/// The seven aggregate gates, or a caller-supplied catalogue when the structural
/// test seam is armed for an output outside the evidence tree.
fn aggregate_gates(output: &Path) -> Result<Vec<StepSpec>, GateError> {
    let outputs = [output.to_path_buf()];
    if let Some(path) = read_seam(GATE_PLAN_ENV, &outputs)? {
        return load_step_plan(Path::new(&path));
    }
    let toolchain = format!("+{PINNED_TOOLCHAIN}");
    let cargo_gate = |name: &str, tail: &[&str]| {
        let mut args = vec![toolchain.clone()];
        args.extend(tail.iter().map(|argument| (*argument).to_owned()));
        StepSpec::program(name, "cargo", &args, 0)
    };
    Ok(vec![
        cargo_gate("format", &["fmt", "--all", "--", "--check"]),
        cargo_gate(
            "lint",
            &[
                "clippy",
                "--workspace",
                "--all-targets",
                "--all-features",
                "--",
                "-D",
                "warnings",
            ],
        ),
        cargo_gate("test", &["test", "--workspace", "--all-features"]),
        cargo_gate(
            "build-locked",
            &["build", "--workspace", "--all-features", "--locked"],
        ),
        cargo_gate("deny", &["deny", "check"]),
        cargo_gate("sbom", &["cyclonedx", "--format", "json"]),
        StepSpec::program("godot-test", "make", &["godot-test".to_owned()], 0)
            .with_env("RUSTUP_TOOLCHAIN", PINNED_TOOLCHAIN),
    ])
}

/// The boundary, correctness, unsafe-code, dependency, schema and security review
/// sub-checks, each computed from the exact frozen source.
fn review_frozen_source(findings: &mut Findings, worktree: &Path, bound: &Bound) {
    match CandidateRelease::of(worktree) {
        CandidateRelease::FirstRelease => prove_cancelled_plane_absent(findings, worktree),
        CandidateRelease::Complete => prove_delivered_planes(findings, worktree),
    }
    prove_unsafe_policy(findings, worktree);
    prove_no_shipping_gdext(findings, worktree);
    prove_report_is_pass(findings, bound);
}

/// The cancelled editor plane is absent, and the honest refusal is present.
fn prove_cancelled_plane_absent(findings: &mut Findings, worktree: &Path) {
    for relative in ["addons", "addons/godot_mcp"] {
        findings.require(
            !worktree.join(relative).exists(),
            format!(
                "{relative} exists in the frozen worktree, but the editor plane (Todos 20-25) is \
                 cancelled and ships nothing"
            ),
        );
    }
    let mut sources = Vec::new();
    collect(&worktree.join("crates"), "rs", &mut sources);
    // F2 reviews the shipping frozen source. `qualification-tools` is the
    // non-shipping review tooling — it is never linked into any shipping crate,
    // binary, archive or SBOM runtime component — and it necessarily names the
    // forbidden markers below in order to search for them, so scanning it would
    // make this checker reject itself.
    sources.retain(|path| !normalize(path).contains("/crates/qualification-tools/"));
    review_capability_profile_rustdoc(findings, worktree);
    review_capability_profile_schema(findings, worktree);
    // These are *code* markers, not prose. A doc comment that explains why the
    // editor plane is absent legitimately names `EditorUndoRedoManager`; only a
    // path expression, a `use`, a type definition or the main-thread thread-ID
    // sentinel would be the cancelled implementation itself.
    for forbidden in [
        "EditorUndoRedoManager::",
        "use godot::",
        "TypeCatalog::",
        "struct TypeCatalog",
        "impl TypeCatalog",
        "main_thread_id",
        "MainThreadSentinel",
    ] {
        let hits: Vec<String> = sources
            .iter()
            .filter(|path| contains(path, forbidden))
            .map(|path| path.display().to_string())
            .collect();
        findings.require(
            hits.is_empty(),
            format!(
                "`{forbidden}` appears as code in the frozen source ({hits:?}), but the editor-plane \
                 mutation path and its main-thread thread-ID sentinel belong to cancelled Todos \
                 20-25 and must be provably absent rather than reviewed as working behaviour"
            ),
        );
    }
    let mut extensions = Vec::new();
    collect(worktree, "gdextension", &mut extensions);
    findings.require(
        extensions.is_empty(),
        format!(
            "the frozen worktree carries a native `.gdextension` descriptor ({extensions:?}), \
             which the first release never ships"
        ),
    );
    findings.require(
        sources
            .iter()
            .any(|path| contains(path, "Unavailable") || contains(path, "unavailable")),
        "the frozen source contains no `unavailable` refusal, so the cancelled editor/runtime \
         branches do not answer honestly"
            .to_owned(),
    );
    for doc in [
        "docs/operator/editor-bridge.md",
        "docs/contributor/adr/0004-editor-plane-cancelled.md",
    ] {
        findings.require(
            worktree.join(doc).is_file(),
            format!("{doc} is absent, so the not-shipped editor plane is undocumented"),
        );
    }
}

/// The complete release delivers the editor, runtime and export planes: each is
/// present as source and as real-entry evidence, the full profile discovers one
/// tool of every one, and no native extension ships outside the isolated lane.
fn prove_delivered_planes(findings: &mut Findings, worktree: &Path) {
    for relative in COMPLETE_RELEASE_EVIDENCE {
        findings.require(
            worktree.join(relative).is_file(),
            format!(
                "{relative} is absent from the frozen worktree, so a plane the complete release \
                 delivers has no source or no real-entry evidence"
            ),
        );
    }
    let manifest = worktree.join(COMPLETE_RELEASE_MANIFEST);
    let tools: Vec<String> = read_bytes(&manifest)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|document| document.get("tools").cloned())
        .and_then(|tools| serde_json::from_value(tools).ok())
        .unwrap_or_default();
    for tool in COMPLETE_RELEASE_TOOLS {
        findings.require(
            tools.iter().any(|listed| listed == tool),
            format!(
                "{COMPLETE_RELEASE_MANIFEST} does not discover `{tool}`, so a plane the complete \
                 release delivers is not reachable"
            ),
        );
    }
    let mut extensions = Vec::new();
    collect(worktree, "gdextension", &mut extensions);
    let lane = worktree.join(GDEXT_REFERENCE_LANE);
    extensions.retain(|path| !path.starts_with(&lane));
    findings.require(
        extensions.is_empty(),
        format!(
            "the frozen worktree carries a native `.gdextension` descriptor outside the isolated \
             {GDEXT_REFERENCE_LANE}/ lane ({extensions:?}); the default package ships pure \
             GDScript only"
        ),
    );
}

fn review_capability_profile_rustdoc(findings: &mut Findings, worktree: &Path) {
    let path = worktree.join("crates/godot-mcp-domain/src/lib.rs");
    let Ok(bytes) = read_bytes(&path) else {
        findings.blocker(format!(
            "{} is not readable, so CapabilityProfile rustdoc claims are unverified",
            path.display()
        ));
        return;
    };
    let text = String::from_utf8_lossy(&bytes);
    let mut inside_profile = false;
    let mut descriptions = 0_usize;
    let mut docs: Vec<(usize, &str)> = Vec::new();

    for (index, line) in text.lines().enumerate() {
        let line_number = index + 1;
        let trimmed = line.trim();
        if !inside_profile {
            inside_profile = trimmed == "pub enum CapabilityProfile {";
            continue;
        }
        if trimmed == "}" {
            break;
        }
        if let Some(doc) = trimmed.strip_prefix("///") {
            docs.push((line_number, doc.trim()));
            continue;
        }
        let Some(variant) = trimmed.strip_suffix(',').filter(|candidate| {
            !candidate.is_empty()
                && candidate
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '_')
        }) else {
            docs.clear();
            continue;
        };
        let description = docs
            .iter()
            .map(|(_, part)| *part)
            .collect::<Vec<_>>()
            .join(" ");
        let description_line = docs.first().map_or(line_number, |(line, _)| *line);
        descriptions += 1;
        reject_unhedged_description(findings, &path, description_line, variant, &description);
        docs.clear();
    }

    findings.require(
        inside_profile && descriptions > 0,
        format!(
            "{} contains no enumerable CapabilityProfile variant rustdoc",
            path.display()
        ),
    );
}

fn review_capability_profile_schema(findings: &mut Findings, worktree: &Path) {
    let path = worktree.join("schemas/domain/contract-v1.schema.json");
    let Ok(bytes) = read_bytes(&path) else {
        findings.blocker(format!(
            "{} is not readable, so emitted CapabilityProfile descriptions are unverified",
            path.display()
        ));
        return;
    };
    let text = String::from_utf8_lossy(&bytes);
    let Ok(schema) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        findings.blocker(format!(
            "{} is not valid JSON, so emitted CapabilityProfile descriptions are unverified",
            path.display()
        ));
        return;
    };
    let Some(variants) = schema
        .pointer("/$defs/CapabilityProfile/oneOf")
        .and_then(serde_json::Value::as_array)
    else {
        findings.blocker(format!(
            "{} has no enumerable CapabilityProfile oneOf descriptions",
            path.display()
        ));
        return;
    };
    findings.require(
        !variants.is_empty(),
        format!(
            "{} has an empty CapabilityProfile oneOf description surface",
            path.display()
        ),
    );
    for variant in variants {
        let Some(name) = variant.get("const").and_then(serde_json::Value::as_str) else {
            findings.blocker(format!(
                "{}:1 contains a CapabilityProfile entry without a string const name",
                path.display()
            ));
            continue;
        };
        let Some(description) = variant
            .get("description")
            .and_then(serde_json::Value::as_str)
        else {
            findings.blocker(format!(
                "{}:1 CapabilityProfile::{name} has no string description",
                path.display()
            ));
            continue;
        };
        let line = json_description_line(&text, description);
        reject_unhedged_description(findings, &path, line, name, description);
    }
}

fn reject_unhedged_description(
    findings: &mut Findings,
    path: &Path,
    line: usize,
    variant: &str,
    description: &str,
) {
    for clause in description.split(['.', ';', '\n']) {
        let lower = clause.to_ascii_lowercase();
        let hedged = DESCRIPTION_HEDGES
            .iter()
            .any(|marker| lower.contains(marker));
        for claim in CANCELLED_PROFILE_CLAIMS {
            if !hedged && lower.contains(&claim.to_ascii_lowercase()) {
                findings.blocker(format!(
                    "{}:{line} CapabilityProfile::{variant} makes the unhedged \
                     cancelled-capability claim `{claim}` in a shipped description: \
                     {description:?}",
                    path.display()
                ));
            }
        }
    }
}

fn json_description_line(text: &str, description: &str) -> usize {
    let Ok(encoded) = serde_json::to_string(description) else {
        return 1;
    };
    text.lines()
        .position(|line| line.contains(&encoded))
        .map_or(1, |index| index + 1)
}

/// Every crate declares an explicit unsafe-code policy.
fn prove_unsafe_policy(findings: &mut Findings, worktree: &Path) {
    let crates = worktree.join("crates");
    let Ok(entries) = std::fs::read_dir(&crates) else {
        findings.blocker(format!(
            "{} is not readable, so the unsafe-code policy cannot be reviewed",
            crates.display()
        ));
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let root = entry.path();
        if !root.is_dir() {
            continue;
        }
        let name = root
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let lib = root.join("src").join("lib.rs");
        if !lib.is_file() {
            continue;
        }
        findings.require(
            contains(&lib, "forbid(unsafe_code)") || contains(&lib, "deny(unsafe_code)"),
            format!("crate `{name}` declares no explicit unsafe-code policy in src/lib.rs"),
        );
    }
}

/// No shipping crate carries a `godot`/gdext dependency or native artifact.
fn prove_no_shipping_gdext(findings: &mut Findings, worktree: &Path) {
    let lock = worktree.join("Cargo.lock");
    let Ok(bytes) = read_bytes(&lock) else {
        findings.blocker(format!("{} is not readable", lock.display()));
        return;
    };
    let text = String::from_utf8_lossy(&bytes);
    for forbidden in [
        "name = \"godot\"",
        "name = \"godot-core\"",
        "name = \"gdext\"",
    ] {
        findings.require(
            !text.contains(forbidden),
            format!("Cargo.lock resolves {forbidden}, so a gdext dependency entered the release"),
        );
    }
}

/// The bound report must itself record a PASS, otherwise F2 is reviewing a
/// qualification round that already failed.
fn prove_report_is_pass(findings: &mut Findings, bound: &Bound) {
    findings.require(
        bound.report.get("result") == Some(&serde_json::Value::String("PASS".to_owned())),
        "the bound qualification report does not record a PASS result".to_owned(),
    );
}

fn collect(root: &Path, extension: &str, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        if path.is_dir() {
            if name == "target" || name == ".git" {
                continue;
            }
            collect(&path, extension, out);
        } else if path
            .extension()
            .is_some_and(|found| found.to_string_lossy() == extension)
        {
            out.push(path);
        }
    }
}

fn contains(path: &Path, needle: &str) -> bool {
    std::fs::read(path)
        .map(|bytes| String::from_utf8_lossy(&bytes).contains(needle))
        .unwrap_or(false)
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => report(&error),
    }
}
