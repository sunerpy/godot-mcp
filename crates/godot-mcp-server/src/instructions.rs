//! The `instructions` text a client receives in the `initialize` answer.
//!
//! The text is generated from the selected profile's manifest, so it names
//! only tools that profile lists, and it stays within
//! [`MAX_INSTRUCTIONS_BYTES`]: a client that places it in the model's context
//! pays for every byte on every turn. Detailed workflows ship as Agent Skills,
//! which `godot-mcp skill install` writes into the agent's skill directory.

use godot_mcp_domain::CapabilityProfile;
use godot_mcp_domain::manifests::canonical_manifest;

/// Upper bound on the generated text, in bytes.
pub const MAX_INSTRUCTIONS_BYTES: usize = 2_000;

/// Tools that run caller-written code and refuse unless the operator started
/// the server with `--allow-scripts` (ADR 0012).
const SCRIPT_TOOLS: [&str; 3] = ["script.run", "editor.script.run", "runtime.script.run"];

/// The instructions for `profile`. `bootstrap` is true while the server holds
/// a workspace and has not bound a project yet.
#[must_use]
pub fn server_instructions(profile: CapabilityProfile, bootstrap: bool) -> String {
    let manifest = canonical_manifest(profile);
    let has = |tool: &str| manifest.tools.contains(tool);
    let mut lines = vec![
        "Use canonical Godot MCP tools and opaque resources; storage paths are never public."
            .to_owned(),
    ];

    let bootstrap_tools = &manifest.bootstrap_tools;
    if bootstrap
        && bootstrap_tools.contains("workspace.projects.list")
        && bootstrap_tools.contains("project.bind")
    {
        let create = if bootstrap_tools.contains("project.create") {
            " or `project.create`"
        } else {
            ""
        };
        lines.push(format!(
            "No project is bound yet: call `workspace.projects.list`, then `project.bind`{create}; \
             the project's tools are listed after that."
        ));
    }

    if has("scene.batch.apply") || has("file.patch") {
        let readers = present(&["project.files.list", "scene.tree.get"], &has);
        if !readers.is_empty() {
            lines.push(format!(
                "Writes take the file's current SHA-256 as `expectedSha256`: read it first from \
                 {}, and after a `conflict` read again instead of repeating the write.",
                spoken(&readers, "or")
            ));
        }
    }

    if has("scene.batch.apply") && has("editor.scene.edit") {
        lines.push(
            "On a scene open in the connected editor, `scene.batch.apply` runs as one undoable \
             editor action and saves the scene; it refuses while the editor holds unsaved changes \
             to that scene, which `editor.scene.edit` changes as the editor holds them, without \
             saving."
                .to_owned(),
        );
    }

    if has("scene.recipe.apply") {
        lines.push(
            "For common node setups, check `scene.recipe.apply` and the `godot-mcp.recipe.*` \
             prompts before writing a batch by hand."
                .to_owned(),
        );
    }

    let health = if has("health.get") {
        "; `health.get` reports which"
    } else {
        ""
    };
    lines.push(format!(
        "`unavailable` means a plane (editor, engine or game session) cannot be reached right \
         now, not that the tool is missing{health}. Never retry an `uncertain` outcome: its \
         effect is unknown, and project writes may stay blocked until the operator resolves it."
    ));

    let scripts = present(&SCRIPT_TOOLS, &has);
    if !scripts.is_empty() {
        lines.push(format!(
            "{} run caller-written code and refuse unless the operator started the server with \
             `--allow-scripts`.",
            spoken(&scripts, "and")
        ));
    }

    lines.push(
        "Detailed workflows ship as Agent Skills: `godot-mcp skill install`, run on the agent's \
         machine, installs them."
            .to_owned(),
    );
    lines.join("\n")
}

/// The tools of `candidates` the profile lists, in order.
fn present<'a>(candidates: &[&'a str], has: &impl Fn(&str) -> bool) -> Vec<&'a str> {
    candidates
        .iter()
        .copied()
        .filter(|tool| has(tool))
        .collect()
}

/// `a`, `` `a` and `b` `` or `` `a`, `b` and `c` ``, joined with `conjunction`.
fn spoken(tools: &[&str], conjunction: &str) -> String {
    let quoted: Vec<String> = tools.iter().map(|tool| format!("`{tool}`")).collect();
    match quoted.split_last() {
        None => String::new(),
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} {conjunction} {last}", rest.join(", ")),
    }
}
