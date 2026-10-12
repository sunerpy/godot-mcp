//! `f1-plan-compliance` — NON-SHIPPING qualification tool (implemented by Todo 37).
//!
//! The deterministic plan-compliance checker. The binary computes the verdict; no
//! human judgment enters it anywhere.
//!
//! Committed CLI surface (normative, from the plan's F1 body):
//!
//! ```text
//! f1-plan-compliance --identity <input-identity.json>
//!                    --report   <qualification-report.json>
//!                    --plan     <plan.md>
//!                    --output   <final/f1-plan-compliance.json>
//! ```
//!
//! It binds exactly `input-identity.sha256` and `qualification-report.sha256` per
//! the canonical hash-only receipt contract, never re-embedding either document,
//! and atomically writes both `f1-plan-compliance.json` and its content-hash
//! sidecar `f1-plan-compliance.sha256`.
//!
//! ## What is verified, mechanically
//!
//! * The identity/report pair verifies against its strict sidecars and canonical
//!   schemas, and the report binds exactly this identity's digest.
//! * Both documents declare the Todo-37-owned `produce-qualification` binary and
//!   its bound binary hash as producer, and no field attributes final emission to
//!   Todo 35. Missing or mismatched producer provenance is a REJECT, never an
//!   assumption that the artifacts appeared implicitly.
//! * Stage-order evidence is release gates → review bindings → input
//!   identity/sidecar → qualification evidence/report/sidecar, proved from the
//!   content hashes rather than from timestamps.
//! * Every bound input still hashes to the frozen digest, so receipt-time evidence
//!   mutation and drift are detected rather than trusted.
//! * Every **non-cancelled** todo carries a complete acceptance descriptor and is
//!   checked off in the plan, while every cancelled todo (20–25) instead carries
//!   the machine-recognizable cancelled QA status — `"status": "cancelled"` with a
//!   nonempty `cancelled_reason`/`cancelled_on` and zero commands — and
//!   `make qa-task TASK=<N>` exits 0 having executed nothing. A claimed
//!   acceptance-command run for a cancelled todo is itself a REJECT.
//! * Each identity-bound Red patch reproduced its exact failure before Green at
//!   the frozen tree, proved from the recorded artifacts and their digests.
//! * The report's evidence covers every named qualification category, so the
//!   topology it proves is the local single-user stdio plus optional
//!   loopback-backed HTTP one and nothing wider.
//! * No F1–F4 receipt already present in the final directory binds a different
//!   identity/report pair, which is how a receipt predating the qualification
//!   artifacts is caught: it could not bind their digests.

use std::path::Path;
use std::process::{Command, ExitCode};

use serde_json::Value;

use qualification_tools::gates::{
    GateError, PlanDocument, hex_sha256, implied_sidecar, read_bytes, report,
    validate_against_schema,
};
use qualification_tools::qualification::{
    ALL_TODOS, BINDINGS_SCHEMA, Bound, CHECKERS, Findings, Flags, REQUIRED_BOUND_INPUTS,
    RedEvidenceIndex, bound_hash, bound_inputs, check_no_todo35_attribution, check_provenance,
    emit_receipt, is_cancelled, load_bound, print_verdict, read_canonical, remeasure_bound_hash,
    require_absent, require_file, resolve, verify_candidate_worktree, verify_release_gates,
};

const CHECKER: &str = "f1-plan-compliance";

const USAGE: &str = "f1-plan-compliance --identity <input-identity.json> --report \
     <qualification-report.json> --plan <plan.md> --output <f1-plan-compliance.json>";

const FLAGS: [&str; 4] = ["--identity", "--report", "--plan", "--output"];

/// The qualification-evidence categories the report must name, so the proven
/// topology is exactly the shipped one.
const REQUIRED_EVIDENCE: [&str; 7] = [
    "parity-stdio-http",
    "profile-transport-e2e",
    "artifact-parity",
    "trust-boundary",
    "durable-transaction",
    "auth",
    "identity-mutation",
];

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
    let candidate_input = verify_candidate_worktree(&mut findings, &workspace, &bound);
    if candidate_input.verified() {
        audit(&mut findings, &bound, &plan, &workspace, &output);
    }
    let after = remeasure_bound_hash(&bound)?;

    let blocking = findings.blocking_summaries();
    let outcome = emit_receipt(
        CHECKER,
        &output,
        &workspace,
        &bound,
        (&before, &after),
        &candidate_input,
        findings.into_vec(),
    )?;
    print_verdict(&outcome, "findings", &blocking);
    Ok(outcome.exit_code())
}

fn audit(
    findings: &mut Findings,
    bound: &Bound,
    plan: &PlanDocument,
    workspace: &Path,
    output: &Path,
) {
    check_provenance(findings, "input-identity.json", &bound.identity);
    check_provenance(findings, "qualification-report.json", &bound.report);
    check_no_todo35_attribution(findings, "input-identity.json", &bound.identity);
    check_no_todo35_attribution(findings, "qualification-report.json", &bound.report);

    audit_bound_inputs(findings, bound, plan, workspace);
    audit_stage_order(findings, bound, workspace);
    audit_report_evidence(findings, bound);
    audit_plan_and_descriptors(findings, plan, workspace);
    audit_red_evidence(findings, bound, workspace);
    audit_sibling_receipts(findings, bound, output);
}

fn audit_bound_inputs(
    findings: &mut Findings,
    bound: &Bound,
    plan: &PlanDocument,
    workspace: &Path,
) {
    let inputs = match bound_inputs(&bound.identity) {
        Ok(inputs) => inputs,
        Err(error) => {
            findings.blocker(format!(
                "the frozen identity's bound inputs are unusable: {error}"
            ));
            return;
        }
    };
    for key in REQUIRED_BOUND_INPUTS {
        findings.require(
            inputs.contains_key(key),
            format!("the frozen identity binds no `{key}` input"),
        );
    }
    for (key, input) in &inputs {
        let path = resolve(workspace, Path::new(&input.path));
        match read_bytes(&path) {
            Ok(bytes) => {
                let actual = hex_sha256(&bytes);
                findings.require(
                    actual == input.sha256,
                    format!(
                        "bound input `{key}` at {} now hashes to {actual}, not the frozen \
                         {}; evidence was mutated after the freeze",
                        path.display(),
                        input.sha256
                    ),
                );
            }
            Err(error) => findings.blocker(format!(
                "bound input `{key}` is no longer readable at {}: {error}",
                path.display()
            )),
        }
    }
    if let Some(bound_plan) = inputs.get("plan") {
        findings.require(
            bound_plan.sha256 == plan.sha256(),
            format!(
                "the identity binds plan {} but {} hashes to {}",
                bound_plan.sha256,
                plan.path().display(),
                plan.sha256()
            ),
        );
    }
    let review_plan = bound
        .identity
        .pointer("/architecture_review/plan_sha256")
        .and_then(Value::as_str)
        .unwrap_or_default();
    findings.require(
        review_plan == plan.sha256(),
        format!(
            "the bound architecture review approved plan {review_plan}, not the live {}",
            plan.sha256()
        ),
    );
}

fn audit_stage_order(findings: &mut Findings, bound: &Bound, workspace: &Path) {
    let Ok(inputs) = bound_inputs(&bound.identity) else {
        return;
    };
    let recorded_gates = bound
        .identity
        .get("release_gates_sha256")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let Some(gates) = inputs.get("release-gates") else {
        return;
    };
    findings.require(
        gates.sha256 == recorded_gates,
        format!(
            "the identity's release_gates_sha256 {recorded_gates} does not equal the release-gate \
             evidence it binds ({}), so the gates-first stage order is unproven",
            gates.sha256
        ),
    );
    let candidate_commit = bound
        .identity
        .get("candidate_commit")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let candidate_tree = bound
        .identity
        .get("candidate_tree")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let gates_path = resolve(workspace, Path::new(&gates.path));
    match verify_release_gates(
        &gates_path,
        &implied_sidecar(&gates_path),
        workspace,
        candidate_commit,
        candidate_tree,
        true,
    ) {
        Ok((_, document)) => {
            check_no_todo35_attribution(findings, "release-gates.json", &document);
        }
        Err(error) => findings.blocker(format!(
            "the bound release-gate evidence does not authorize a release: {error}"
        )),
    }

    let Some(bindings) = inputs.get("review-bindings") else {
        return;
    };
    // The plan gives sidecars to exactly two of the five qualification paths, so
    // `review-bindings.json` is integrity-checked by the identity digest verified
    // above rather than by a sidecar of its own.
    let bindings_path = resolve(workspace, Path::new(&bindings.path));
    match read_canonical(&bindings_path) {
        Ok((_, document)) => {
            validate_bindings(findings, workspace, &bindings_path, &document);
            check_provenance(findings, "review-bindings.json", &document);
            check_no_todo35_attribution(findings, "review-bindings.json", &document);
            findings.require(
                document.get("architecture_review") == bound.identity.get("architecture_review"),
                "the review bindings and the frozen identity disagree about the current \
                 architecture-review record"
                    .to_owned(),
            );
        }
        Err(error) => findings.blocker(format!("the bound review bindings are unusable: {error}")),
    }
}

fn validate_bindings(findings: &mut Findings, workspace: &Path, path: &Path, document: &Value) {
    if let Err(error) = validate_against_schema(
        &path.display().to_string(),
        &workspace.join(BINDINGS_SCHEMA),
        document,
    ) {
        findings.blocker(format!(
            "the bound review bindings do not satisfy their canonical schema: {error}"
        ));
    }
}

fn audit_report_evidence(findings: &mut Findings, bound: &Bound) {
    findings.require(
        bound.report.get("result") == Some(&Value::String("PASS".to_owned())),
        "the qualification report does not record a PASS result".to_owned(),
    );
    let gates = bound
        .report
        .get("gates")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut names = Vec::new();
    for gate in &gates {
        let name = gate.get("name").and_then(Value::as_str).unwrap_or_default();
        names.push(name.to_owned());
        let exit_code = gate.get("exit_code").and_then(Value::as_i64);
        findings.require(
            exit_code == Some(0),
            format!(
                "qualification evidence step `{name}` recorded exit {}",
                exit_code.map_or_else(|| "<absent>".to_owned(), |code| code.to_string())
            ),
        );
        let command = gate
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default();
        findings.require(
            !command.trim().is_empty(),
            format!(
                "qualification evidence step `{name}` records no command, so it is a \
                     self-report rather than behaviour evidence"
            ),
        );
    }
    for required in REQUIRED_EVIDENCE {
        findings.require(
            names.iter().any(|name| name == required),
            format!("the qualification report proves no `{required}` evidence"),
        );
    }
}

fn audit_plan_and_descriptors(findings: &mut Findings, plan: &PlanDocument, workspace: &Path) {
    let descriptors = workspace.join("qa").join("tasks");
    let evidence_root = workspace
        .join(".omo")
        .join("evidence")
        .join("godot-mcp-server-architecture");
    for todo in ALL_TODOS {
        let path = descriptors.join(format!("{todo}.json"));
        let Ok(bytes) = read_bytes(&path) else {
            findings.blocker(format!("qa/tasks/{todo}.json is absent"));
            continue;
        };
        let Ok(document) = serde_json::from_slice::<Value>(&bytes) else {
            findings.blocker(format!("qa/tasks/{todo}.json is not valid JSON"));
            continue;
        };
        let status = document
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("active");
        let happy = document.get("happy").and_then(Value::as_array);
        let failure = document.get("failure").and_then(Value::as_array);
        if is_cancelled(todo) {
            audit_cancelled_todo(findings, todo, &document, status, happy, failure, workspace);
            let green = evidence_root.join(format!("task-{todo}-green.txt"));
            findings.require(
                !green.exists(),
                format!(
                    "todo {todo} is cancelled, but {} claims an acceptance-command run for it",
                    green.display()
                ),
            );
        } else {
            findings.require(
                status == "active",
                format!(
                    "todo {todo} is not cancelled but its descriptor declares status {status:?}"
                ),
            );
            findings.require(
                happy.is_some_and(|commands| !commands.is_empty()),
                format!("qa/tasks/{todo}.json owns no happy acceptance command"),
            );
            findings.require(
                failure.is_some_and(|commands| !commands.is_empty()),
                format!("qa/tasks/{todo}.json owns no failure acceptance command"),
            );
            for (label, commands) in [("happy", happy), ("failure", failure)] {
                for command in commands.into_iter().flatten() {
                    let assertions = command.get("assertions").and_then(Value::as_array);
                    findings.require(
                        assertions.is_some_and(|values| !values.is_empty()),
                        format!("qa/tasks/{todo}.json {label} command carries no assertion"),
                    );
                    findings.require(
                        command
                            .get("command")
                            .and_then(Value::as_str)
                            .is_some_and(|text| !text.trim().is_empty()),
                        format!("qa/tasks/{todo}.json {label} command is empty"),
                    );
                }
            }
        }
        audit_plan_checkbox(findings, plan, todo);
    }
}

fn audit_plan_checkbox(findings: &mut Findings, plan: &PlanDocument, todo: u32) {
    let accepted = plan.has_accepted_todo(todo);
    let declared = plan.has_todo_heading(todo);
    if !findings.require(declared, format!("the plan declares no todo {todo}")) {
        return;
    }
    if is_cancelled(todo) {
        findings.require(
            !accepted,
            format!("todo {todo} is cancelled but the plan marks it accepted"),
        );
        findings.require(
            plan.todo_heading(todo)
                .is_some_and(|heading| heading.contains("CANCELLED")),
            format!("todo {todo} is cancelled but its plan heading does not say so"),
        );
    } else if todo != 37 {
        // Todo 37 is the qualification todo itself: it is checked off only after
        // this wave completes, so an unchecked 37 is the expected state here.
        findings.require(
            accepted,
            format!("todo {todo} is not cancelled and is not marked accepted in the plan"),
        );
    }
}

fn audit_cancelled_todo(
    findings: &mut Findings,
    todo: u32,
    document: &Value,
    status: &str,
    happy: Option<&Vec<Value>>,
    failure: Option<&Vec<Value>>,
    workspace: &Path,
) {
    findings.require(
        status == "cancelled",
        format!("qa/tasks/{todo}.json does not declare the cancelled QA status"),
    );
    for field in ["cancelled_reason", "cancelled_on"] {
        findings.require(
            document
                .get(field)
                .and_then(Value::as_str)
                .is_some_and(|text| !text.trim().is_empty()),
            format!("qa/tasks/{todo}.json declares no nonempty `{field}`"),
        );
    }
    findings.require(
        happy.is_none_or(Vec::is_empty) && failure.is_none_or(Vec::is_empty),
        format!(
            "qa/tasks/{todo}.json is cancelled yet still owns acceptance commands; a claimed \
             acceptance-command run for a cancelled todo is itself a rejection"
        ),
    );
    let output = Command::new("make")
        .arg("qa-task")
        .arg(format!("TASK={todo}"))
        .current_dir(workspace)
        .output();
    match output {
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            findings.require(
                output.status.success(),
                format!(
                    "`make qa-task TASK={todo}` exited {} for a cancelled todo",
                    output
                        .status
                        .code()
                        .map_or_else(|| "by signal".to_owned(), |code| code.to_string())
                ),
            );
            findings.require(
                stderr.contains("0 commands executed"),
                format!(
                    "`make qa-task TASK={todo}` did not report that it executed nothing for a \
                     cancelled todo"
                ),
            );
            findings.require(
                !stderr.contains("qa-task: run:"),
                format!("`make qa-task TASK={todo}` executed a command for a cancelled todo"),
            );
        }
        Err(error) => findings.blocker(format!(
            "`make qa-task TASK={todo}` could not be invoked, so the cancelled QA status is \
             unverified: {error}"
        )),
    }
}

fn audit_red_evidence(findings: &mut Findings, bound: &Bound, workspace: &Path) {
    let Ok(inputs) = bound_inputs(&bound.identity) else {
        return;
    };
    let Some(index) = inputs.get("red-evidence-index") else {
        return;
    };
    let path = resolve(workspace, Path::new(&index.path));
    match RedEvidenceIndex::verify(&path, workspace) {
        Ok(parsed) => {
            findings.require(
                !parsed.entries.is_empty(),
                "the bound Red evidence index proves no Red-before-Green pair".to_owned(),
            );
        }
        Err(error) => findings.blocker(format!(
            "the identity-bound Red evidence does not reproduce its failures before Green: {error}"
        )),
    }
}

fn audit_sibling_receipts(findings: &mut Findings, bound: &Bound, output: &Path) {
    let Some(directory) = output.parent() else {
        return;
    };
    for checker in CHECKERS {
        let receipt = directory.join(format!("{checker}.json"));
        if receipt == output || !receipt.is_file() {
            continue;
        }
        let Ok((_, document)) = read_canonical(&receipt) else {
            findings.blocker(format!(
                "{} is present but is not a canonical receipt",
                receipt.display()
            ));
            continue;
        };
        let identity = document
            .get("input_identity_sha256")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let report = document
            .get("qualification_report_sha256")
            .and_then(Value::as_str)
            .unwrap_or_default();
        findings.require(
            identity == bound.identity_sha256 && report == bound.report_sha256,
            format!(
                "{} binds a different identity/report pair, so it predates or does not belong to \
                 this qualification round",
                receipt.display()
            ),
        );
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => report(&error),
    }
}
