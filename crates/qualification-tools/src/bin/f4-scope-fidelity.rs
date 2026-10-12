//! `f4-scope-fidelity` — NON-SHIPPING qualification tool (implemented by Todo 37).
//!
//! The deterministic scope-fidelity checker. It diffs the frozen tree, binaries,
//! manifests, tools/resources, config and docs against the plan's Scope section and
//! computes the verdict itself.
//!
//! Committed CLI surface (normative, from the plan's F4 body):
//!
//! ```text
//! f4-scope-fidelity --identity <input-identity.json>
//!                   --report   <qualification-report.json>
//!                   --plan     <plan.md>
//!                   --output   <final/f4-scope-fidelity.json>
//! ```
//!
//! ## What counts as a creep match
//!
//! Structural evidence only, so no prose phrasing can create or hide a match:
//! a resolved `godot`/gdext dependency, a native `.gdextension` descriptor, a
//! shipping `addons/godot_mcp` EditorPlugin tree, a C# source file, an
//! arbitrary-script-execution tool in any profile manifest, or an unsupported
//! tool-count claim.
//!
//! ## The two inverse distortions
//!
//! The **absence** of screenshot-derived capabilities, including recording, is not
//! a match: Todo 24 is cancelled, so they were never delivered and F4 must not
//! demand them. What F4 rejects is the inverse — any tree, binary, manifest, tool,
//! resource, config or doc that presents a cancelled Todo 20–25 editor-plane
//! capability **as delivered**. Text that states the plane is not shipped and that
//! such a call answers `unavailable` is correct and passes, so each cancelled
//! capability phrase is checked against the not-shipped disclaimer in the same
//! document rather than against its bare presence.
//!
//! ## The complete release
//!
//! A candidate that ships the contract 0.2 discovery manifests is the complete
//! release ([`CandidateRelease`]). It delivers the editor, runtime and export
//! planes, so naming them is not creep there and the cancelled-capability phrase
//! checks do not apply; what F4 requires instead is that its full profile
//! discovers one tool of every delivered plane. A native descriptor is creep in
//! either release unless it lives in the isolated gdext reference lane, which only
//! the complete release may carry.
//!
//! A half-exposed configuration surface is the same defect: `bridge.endpoint` is
//! validated by the loader and reported by `check-config` but never read by the
//! production `serve` path, so every document naming it must call it reserved with
//! no runtime effect. Describing it as an effective endpoint is a REJECT.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde_json::Value;

use qualification_tools::gates::{GateError, PlanDocument, implied_sidecar, read_bytes, report};
use qualification_tools::qualification::{
    COMPLETE_RELEASE_MANIFEST, COMPLETE_RELEASE_TOOLS, CandidateRelease, Findings, Flags,
    GDEXT_REFERENCE_LANE, bound_hash, bound_inputs, emit_receipt, load_bound, print_verdict,
    remeasure_bound_hash, require_absent, require_file, verify_candidate_worktree,
};

const CHECKER: &str = "f4-scope-fidelity";

const USAGE: &str = "f4-scope-fidelity --identity <input-identity.json> --report \
     <qualification-report.json> --plan <plan.md> --output <f4-scope-fidelity.json>";

const FLAGS: [&str; 4] = ["--identity", "--report", "--plan", "--output"];

/// Cancelled Todo 20–25 capability phrases. Each is a multi-word or CamelCase
/// identifier so an ordinary English gerund such as "recording each payload file"
/// can never collide with the capability named "runtime recording".
const CANCELLED_CAPABILITIES: [&str; 9] = [
    "screenshot",
    "runtime recording",
    "EditorUndoRedoManager",
    "EditorPlugin",
    "input simulation",
    "synthetic input",
    "runtime inspection",
    "TypeCatalog",
    "editor selection",
];

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

/// General-document audits accept these anywhere in the document; profile
/// descriptions require one in the same sentence or semicolon clause as the claim.
const NOT_SHIPPED_MARKERS: [&str; 7] = [
    "first-release-absent",
    "not shipped",
    "unavailable",
    "cancelled",
    "not a first-release",
    "does not provide",
    "not delivered",
];

/// A document naming the reserved configuration key must say both of these.
const RESERVED_KEY: &str = "bridge.endpoint";
const RESERVED_MARKERS: [&str; 2] = ["eserved", "no runtime effect"];

/// A document using the word must also carry an explicit denial.
const SANDBOX_DENIALS: [&str; 4] = [
    "not a code sandbox",
    "not a sandbox",
    "never a sandbox",
    "is not sandboxed",
];

/// Unsupported tool-count claims the plan names by number.
const TOOL_COUNT_CLAIMS: [&str; 4] = ["169 tools", "175 tools", "169 tool", "175 tool"];

fn run() -> Result<ExitCode, GateError> {
    let flags = Flags::parse(USAGE, &FLAGS)?;
    let identity_path = flags.path("--identity")?;
    let report_path = flags.path("--report")?;
    let plan_path = flags.path("--plan")?;
    let output = flags.path("--output")?;

    require_file("--identity", &identity_path)?;
    require_file("--report", &report_path)?;
    require_file("--plan", &plan_path)?;
    require_absent("--output", &output)?;
    require_absent("--output sidecar", &implied_sidecar(&output))?;

    let workspace = std::env::current_dir().map_err(|error| GateError::Io {
        path: "<cwd>".to_owned(),
        reason: error.to_string(),
    })?;
    let bound = load_bound(&identity_path, &report_path, &workspace)?;
    let before = bound_hash(&bound.identity_sha256, &bound.report_sha256);
    let plan = PlanDocument::load(&plan_path)?;

    let mut findings = Findings::new();
    let mut matches = Vec::new();
    let mut creep = |findings: &mut Findings, matches: &mut Vec<String>, summary: String| {
        matches.push(summary.clone());
        findings.blocker(summary);
    };

    let candidate_input = verify_candidate_worktree(&mut findings, &workspace, &bound);
    if candidate_input.verified() {
        let release = CandidateRelease::of(&workspace);
        audit_structure(&workspace, release, &mut findings, &mut matches, &mut creep);
        audit_documents(&workspace, release, &mut findings, &mut matches, &mut creep);
        audit_plan_agreement(&plan, &bound, &mut findings);
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
    print_verdict(&outcome, "creep_matches", &matches);
    Ok(outcome.exit_code())
}

type Creep<'a> = &'a mut dyn FnMut(&mut Findings, &mut Vec<String>, String);

fn audit_structure(
    workspace: &Path,
    release: CandidateRelease,
    findings: &mut Findings,
    matches: &mut Vec<String>,
    creep: Creep<'_>,
) {
    for relative in ["addons", "addons/godot_mcp"] {
        if workspace.join(relative).exists() {
            creep(
                findings,
                matches,
                match release {
                    CandidateRelease::FirstRelease => format!(
                        "the tree ships {relative}, presenting the cancelled editor plane as \
                         delivered"
                    ),
                    CandidateRelease::Complete => format!(
                        "the tree ships {relative} at its root; the add-on ships from \
                         crates/godot-mcp-bridge/addon and reaches a project only through \
                         editor.addon.install"
                    ),
                },
            );
        }
    }
    let mut extensions = Vec::new();
    collect(workspace, &["gdextension"], &mut extensions);
    if release == CandidateRelease::Complete {
        let lane = workspace.join(GDEXT_REFERENCE_LANE);
        extensions.retain(|path| !path.starts_with(&lane));
    }
    for path in &extensions {
        creep(
            findings,
            matches,
            format!(
                "{} is a native gdext descriptor; the first release ships pure GDScript only",
                path.display()
            ),
        );
    }
    let mut csharp = Vec::new();
    collect(workspace, &["cs", "csproj", "sln"], &mut csharp);
    for path in &csharp {
        creep(
            findings,
            matches,
            format!(
                "{} introduces C# into a scope that excludes it",
                path.display()
            ),
        );
    }
    if let Ok(bytes) = read_bytes(&workspace.join("Cargo.lock")) {
        let text = String::from_utf8_lossy(&bytes);
        for forbidden in [
            "name = \"godot\"",
            "name = \"godot-core\"",
            "name = \"gdext\"",
        ] {
            if text.contains(forbidden) {
                creep(
                    findings,
                    matches,
                    format!("Cargo.lock resolves {forbidden}, a shipping gdext dependency"),
                );
            }
        }
    } else {
        findings
            .blocker("Cargo.lock is not readable, so dependency creep is unverified".to_owned());
    }
    audit_profiles(workspace, findings, matches, creep);
    match release {
        CandidateRelease::FirstRelease => {
            audit_capability_profile_descriptions(workspace, findings, matches, creep);
        }
        CandidateRelease::Complete => audit_delivered_tools(workspace, findings),
    }
}

/// The complete release's full profile discovers one tool of every plane it
/// delivers.
fn audit_delivered_tools(workspace: &Path, findings: &mut Findings) {
    let manifest = workspace.join(COMPLETE_RELEASE_MANIFEST);
    let tools: Vec<String> = read_bytes(&manifest)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|document| document.get("tools").cloned())
        .and_then(|tools| serde_json::from_value(tools).ok())
        .unwrap_or_default();
    for tool in COMPLETE_RELEASE_TOOLS {
        findings.require(
            tools.iter().any(|listed| listed == tool),
            format!(
                "{COMPLETE_RELEASE_MANIFEST} does not discover `{tool}`, so the complete \
                 release's scope is not what it delivers"
            ),
        );
    }
}

/// No canonical profile manifest may expose an arbitrary-script-execution tool or a
/// raw artifact filesystem path.
fn audit_profiles(
    workspace: &Path,
    findings: &mut Findings,
    matches: &mut Vec<String>,
    creep: Creep<'_>,
) {
    let profiles = workspace.join("schemas").join("domain").join("profiles");
    let Ok(entries) = std::fs::read_dir(&profiles) else {
        findings.blocker(format!(
            "{} is not readable, so the canonical profile manifests are unverified",
            profiles.display()
        ));
        return;
    };
    let mut seen = 0_usize;
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        seen += 1;
        let Ok(bytes) = read_bytes(&path) else {
            findings.blocker(format!("{} is not readable", path.display()));
            continue;
        };
        let text = String::from_utf8_lossy(&bytes);
        for forbidden in ["unsafe_script", "execute_script", "execute-editor-script"] {
            if text.contains(forbidden) {
                creep(
                    findings,
                    matches,
                    format!(
                        "{} exposes `{forbidden}`, an arbitrary-script-execution tool the scope \
                         forbids",
                        path.display()
                    ),
                );
            }
        }
        for forbidden in ["absolute_path", "artifact_path", "filesystem_path"] {
            if text.contains(forbidden) {
                creep(
                    findings,
                    matches,
                    format!(
                        "{} exposes `{forbidden}`, a raw artifact path in a public outcome surface",
                        path.display()
                    ),
                );
            }
        }
    }
    findings.require(
        seen >= 4,
        format!(
            "{} declares {seen} profile manifests; the frozen set is core/editor/runtime/full",
            profiles.display()
        ),
    );
}

fn audit_capability_profile_descriptions(
    workspace: &Path,
    findings: &mut Findings,
    matches: &mut Vec<String>,
    creep: Creep<'_>,
) {
    let path = workspace.join("schemas/domain/contract-v1.schema.json");
    let Ok(bytes) = read_bytes(&path) else {
        findings.blocker(format!(
            "{} is not readable, so CapabilityProfile descriptions are unverified",
            path.display()
        ));
        return;
    };
    let text = String::from_utf8_lossy(&bytes);
    let Ok(schema) = serde_json::from_slice::<Value>(&bytes) else {
        findings.blocker(format!(
            "{} is not valid JSON, so CapabilityProfile descriptions are unverified",
            path.display()
        ));
        return;
    };
    let Some(variants) = schema
        .pointer("/$defs/CapabilityProfile/oneOf")
        .and_then(Value::as_array)
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
        let Some(name) = variant.get("const").and_then(Value::as_str) else {
            findings.blocker(format!(
                "{}:1 contains a CapabilityProfile entry without a string const name",
                path.display()
            ));
            continue;
        };
        let Some(description) = variant.get("description").and_then(Value::as_str) else {
            findings.blocker(format!(
                "{}:1 CapabilityProfile::{name} has no string description",
                path.display()
            ));
            continue;
        };
        let line = json_description_line(&text, description);
        for clause in description.split(['.', ';', '\n']) {
            let lower = clause.to_ascii_lowercase();
            let hedged = NOT_SHIPPED_MARKERS
                .iter()
                .any(|marker| lower.contains(marker));
            for claim in CANCELLED_PROFILE_CLAIMS {
                if !hedged && lower.contains(&claim.to_ascii_lowercase()) {
                    creep(
                        findings,
                        matches,
                        format!(
                            "{}:{line} CapabilityProfile::{name} makes the unhedged \
                             cancelled-capability claim `{claim}` in a shipped description: \
                             {description:?}",
                            path.display()
                        ),
                    );
                }
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

fn audit_documents(
    workspace: &Path,
    release: CandidateRelease,
    findings: &mut Findings,
    matches: &mut Vec<String>,
    creep: Creep<'_>,
) {
    let mut documents = Vec::new();
    collect(&workspace.join("docs"), &["md"], &mut documents);
    if !findings.require(
        !documents.is_empty(),
        "docs/ carries no page, so no documented scope can be compared".to_owned(),
    ) {
        return;
    }
    for path in &documents {
        let Ok(bytes) = read_bytes(path) else {
            findings.blocker(format!("{} is not readable", path.display()));
            continue;
        };
        let text = String::from_utf8_lossy(&bytes).to_string();
        let lower = text.to_ascii_lowercase();
        let disclaims = NOT_SHIPPED_MARKERS
            .iter()
            .any(|marker| lower.contains(marker));
        // The complete release delivers these capabilities, so naming one is
        // what its documentation should do.
        for capability in CANCELLED_CAPABILITIES {
            if release == CandidateRelease::FirstRelease
                && lower.contains(&capability.to_ascii_lowercase())
                && !disclaims
            {
                creep(
                    findings,
                    matches,
                    format!(
                        "{} names the cancelled capability `{capability}` without stating that the \
                         editor plane is not shipped and that such a call answers `unavailable`",
                        path.display()
                    ),
                );
            }
        }
        if text.contains(RESERVED_KEY) {
            for marker in RESERVED_MARKERS {
                if !text.contains(marker) {
                    creep(
                        findings,
                        matches,
                        format!(
                            "{} documents `{RESERVED_KEY}` without saying `{marker}`; a key the \
                             loader validates and `check-config` reports but the production \
                             `serve` path never reads must be documented as reserved with no \
                             runtime effect",
                            path.display()
                        ),
                    );
                }
            }
        }
        if lower.contains("sandbox") && !SANDBOX_DENIALS.iter().any(|denial| lower.contains(denial))
        {
            creep(
                findings,
                matches,
                format!(
                    "{} uses `sandbox` without an explicit denial; a process group is never a \
                     sandbox",
                    path.display()
                ),
            );
        }
        for claim in TOOL_COUNT_CLAIMS {
            if lower.contains(claim) {
                creep(
                    findings,
                    matches,
                    format!(
                        "{} repeats the unsupported `{claim}` count claim",
                        path.display()
                    ),
                );
            }
        }
    }
}

/// The frozen tree the identity binds is the tree that was compared, and the plan
/// the identity binds is the plan whose Scope section defines the comparison.
fn audit_plan_agreement(
    plan: &PlanDocument,
    bound: &qualification_tools::qualification::Bound,
    findings: &mut Findings,
) {
    let Ok(inputs) = bound_inputs(&bound.identity) else {
        findings.blocker("the frozen identity's bound inputs are unusable".to_owned());
        return;
    };
    if let Some(bound_plan) = inputs.get("plan") {
        findings.require(
            bound_plan.sha256 == plan.sha256(),
            format!(
                "the identity binds plan {} but the Scope section being compared comes from {} \
                 which hashes to {}",
                bound_plan.sha256,
                plan.path().display(),
                plan.sha256()
            ),
        );
    } else {
        findings.blocker("the frozen identity binds no plan".to_owned());
    }
    findings.require(
        bound.report.get("result") == Some(&Value::String("PASS".to_owned())),
        "the bound qualification report does not record a PASS result".to_owned(),
    );
}

fn collect(root: &Path, extensions: &[&str], out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let mut children: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    children.sort();
    for path in children {
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        if path.is_dir() {
            if name == "target" || name == ".git" || name == ".omo" {
                continue;
            }
            collect(&path, extensions, out);
        } else if path
            .extension()
            .is_some_and(|found| extensions.contains(&found.to_string_lossy().as_ref()))
        {
            out.push(path);
        }
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => report(&error),
    }
}
