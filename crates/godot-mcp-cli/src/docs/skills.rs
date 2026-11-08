//! The Agent Skills under `skills/`, checked like documentation.
//!
//! Each skill is one `skills/<name>/SKILL.md` that `godot-mcp skill install`
//! copies into an agent's skill directory, where the agent loads it when its
//! description matches the task. godot-mcp itself never reads it, so nothing
//! else would notice a renamed tool, an example the tool's schema refuses or a
//! skill outgrowing its budget. Seven rules, all fail-closed:
//!
//! 1. `skill-layout` — `skills/` holds exactly one directory per skill the
//!    binary embeds ([`crate::skill::SKILLS`]), and each holds exactly
//!    `SKILL.md`: one file, so the installer's SHA-256 covers the whole skill.
//! 2. `skill-frontmatter` — the frontmatter holds exactly `name` (the directory
//!    name, by the Agent Skills naming rules), `description` (plain ASCII
//!    English, at most 1,024 characters), `license: MIT`, `compatibility` (at
//!    most 500 characters) and `metadata` with exactly `contract` and
//!    `profiles`. Every value is one line; a `metadata` value is double-quoted,
//!    so it is a string. `allowed-tools` would pre-approve tools for the user,
//!    so it is refused like every other key.
//! 3. `skill-contract` — `metadata.contract` equals [`CONTRACT_VERSION`], so a
//!    change of contract updates the skills in the same change.
//! 4. `skill-budget` — the file stays within its pinned line and byte budget
//!    ([`SKILL_BUDGETS`]) and never above [`MAX_SKILL_LINES`] lines.
//! 5. `skill-tool` — every dotted name in a code span outside a code block is a
//!    tool of the catalog (`godot_mcp_domain::catalog::TOOL_CATALOG`) that every
//!    profile in `metadata.profiles` lists.
//! 6. `skill-example` — every `json tool=<name>` code block is JSON that the
//!    tool's advertised input schema accepts, for a tool every declared profile
//!    lists. The schemas are the ones `tools/list` serves, and the checker is
//!    the one the schema/handler agreement tests use; both need the
//!    `test-hooks` feature, and a build without it reports every example as
//!    unchecked rather than passing it.
//! 7. `skill-link` — no relative link: an installed `SKILL.md` sits alone in
//!    an agent's directory, where a relative target does not exist.

use std::collections::BTreeMap;
use std::path::Path;

use godot_mcp_domain::catalog::descriptor;
use godot_mcp_domain::{CONTRACT_VERSION, CapabilityProfile};

use super::Finding;
use super::canonical::PROFILES;
use super::checker::{is_dotted_name_shape, is_probably_file_name};
use super::scan::{Scanned, inline_spans, scan};
use crate::skill::{SKILL_FILE, SKILLS};

/// The skills tree, relative to the workspace root.
pub const SKILLS_DIR: &str = "skills";

/// The Agent Skills ceiling on a `name`.
pub const MAX_NAME_CHARS: usize = 64;

/// The Agent Skills ceiling on a `description`.
pub const MAX_DESCRIPTION_CHARS: usize = 1024;

/// The Agent Skills ceiling on a `compatibility`.
pub const MAX_COMPATIBILITY_CHARS: usize = 500;

/// No skill grows past this many lines, whatever its budget says.
pub const MAX_SKILL_LINES: usize = 400;

/// One skill's pinned size, measured on the committed file with a little
/// headroom. Growing past it means raising the pin in the same change and
/// saying why.
#[derive(Debug, Clone, Copy)]
pub struct SkillBudget {
    /// The skill's directory name.
    pub name: &'static str,
    /// Most lines, counted as `str::lines` counts them.
    pub max_lines: usize,
    /// Most bytes.
    pub max_bytes: usize,
}

/// Every skill's budget: the size measured when it was pinned, plus about 10%.
pub const SKILL_BUDGETS: &[SkillBudget] = &[
    // Measured at 311 lines and 18,921 bytes.
    SkillBudget {
        name: "godot-mcp",
        max_lines: 340,
        max_bytes: 20_500,
    },
    // Measured at 187 lines and 12,201 bytes.
    SkillBudget {
        name: "godot-mcp-export",
        max_lines: 210,
        max_bytes: 13_500,
    },
    // Measured at 233 lines and 13,801 bytes.
    SkillBudget {
        name: "godot-mcp-playtest",
        max_lines: 260,
        max_bytes: 15_000,
    },
    // Measured at 264 lines and 13,225 bytes.
    SkillBudget {
        name: "godot-mcp-setup",
        max_lines: 290,
        max_bytes: 14_500,
    },
];

/// The frontmatter keys a skill holds, all required.
const KEYS: &[&str] = &[
    "name",
    "description",
    "license",
    "compatibility",
    "metadata",
];

/// The `metadata` keys a skill holds, all required.
const METADATA_KEYS: &[&str] = &["contract", "profiles"];

/// The only license a skill carries: the repository's.
const LICENSE: &str = "MIT";

/// The info-string attribute naming the tool a JSON example calls.
const TOOL_ATTRIBUTE: &str = "tool=";

/// One parsed frontmatter: top-level values and `metadata` values.
#[derive(Debug, Default)]
struct Frontmatter {
    values: BTreeMap<String, String>,
    metadata: BTreeMap<String, String>,
    /// The 0-based index of the line after the closing `---`.
    body_start: usize,
}

/// Run every skill rule over `workspace_root/skills`.
#[must_use]
pub fn check(workspace_root: &Path) -> Vec<Finding> {
    let mut findings = Vec::new();
    let root = workspace_root.join(SKILLS_DIR);
    let mut on_disk = Vec::new();
    match std::fs::read_dir(&root) {
        Err(error) => findings.push(Finding::file_level(
            SKILLS_DIR,
            "skill-layout",
            format!("cannot read {}: {error}", root.display()),
        )),
        Ok(entries) => {
            for entry in entries {
                match entry {
                    Ok(entry) => on_disk.push(entry),
                    Err(error) => findings.push(Finding::file_level(
                        SKILLS_DIR,
                        "skill-layout",
                        format!("cannot read {}: {error}", root.display()),
                    )),
                }
            }
        }
    }
    let mut names = Vec::new();
    for entry in on_disk {
        let name = entry.file_name().to_string_lossy().into_owned();
        let relative = format!("{SKILLS_DIR}/{name}");
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            findings.push(Finding::file_level(
                &relative,
                "skill-layout",
                "skills/ holds only skill directories",
            ));
            continue;
        }
        check_layout(&entry.path(), &relative, &mut findings);
        names.push(name);
    }
    names.sort();
    let mut embedded: Vec<&str> = SKILLS.iter().map(|skill| skill.name).collect();
    embedded.sort_unstable();
    if names != embedded {
        findings.push(Finding::file_level(
            SKILLS_DIR,
            "skill-layout",
            format!(
                "skills/ holds {names:?} but the binary embeds {embedded:?}; add or remove the \
                 skill in both places (crate::skill::SKILLS)"
            ),
        ));
    }
    let mut budgets: Vec<&str> = SKILL_BUDGETS.iter().map(|budget| budget.name).collect();
    budgets.sort_unstable();
    if budgets != embedded {
        findings.push(Finding::file_level(
            SKILLS_DIR,
            "skill-budget",
            format!("the pinned budgets cover {budgets:?} but the skills are {embedded:?}"),
        ));
    }

    let schemas = advertised_schemas();
    for name in &names {
        let relative = format!("{SKILLS_DIR}/{name}/{SKILL_FILE}");
        let path = root.join(name).join(SKILL_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => findings.extend(check_skill(name, &relative, &text, schemas.as_ref())),
            Err(error) => findings.push(Finding::file_level(
                &relative,
                "skill-layout",
                format!("cannot read the skill as UTF-8 text: {error}"),
            )),
        }
    }
    findings
}

fn check_layout(dir: &Path, relative: &str, findings: &mut Vec<Finding>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            findings.push(Finding::file_level(
                relative,
                "skill-layout",
                format!("cannot read the skill directory: {error}"),
            ));
            return;
        }
    };
    let mut has_skill_file = false;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let regular = entry.file_type().is_ok_and(|kind| kind.is_file());
        if name == SKILL_FILE && regular {
            has_skill_file = true;
        } else {
            findings.push(Finding::file_level(
                &format!("{relative}/{name}"),
                "skill-layout",
                "a skill is exactly one regular SKILL.md: no scripts, references or other files",
            ));
        }
    }
    if !has_skill_file {
        findings.push(Finding::file_level(
            relative,
            "skill-layout",
            "the skill directory holds no SKILL.md",
        ));
    }
}

/// Every rule but the layout, over one skill's text.
fn check_skill(
    name: &str,
    relative: &str,
    text: &str,
    schemas: Option<&BTreeMap<String, serde_json::Value>>,
) -> Vec<Finding> {
    let mut findings = Vec::new();
    check_budget(name, relative, text, &mut findings);
    let scanned = scan(text);
    let Some(frontmatter) = parse_frontmatter(relative, &scanned.lines, &mut findings) else {
        return findings;
    };
    check_frontmatter(name, relative, &frontmatter, &mut findings);
    let profiles = declared_profiles(relative, &frontmatter, &mut findings);
    check_tool_names(
        relative,
        &scanned,
        frontmatter.body_start,
        &profiles,
        &mut findings,
    );
    check_examples(relative, &scanned, &profiles, schemas, &mut findings);
    check_links(relative, &scanned, &mut findings);
    findings
}

fn check_budget(name: &str, relative: &str, text: &str, findings: &mut Vec<Finding>) {
    let lines = text.lines().count();
    let bytes = text.len();
    if lines > MAX_SKILL_LINES {
        findings.push(Finding::file_level(
            relative,
            "skill-budget",
            format!("{lines} lines; a skill stays within {MAX_SKILL_LINES} lines"),
        ));
    }
    if let Some(budget) = SKILL_BUDGETS.iter().find(|budget| budget.name == name) {
        if lines > budget.max_lines {
            findings.push(Finding::file_level(
                relative,
                "skill-budget",
                format!(
                    "{lines} lines, over the pinned {}; raise the pin in SKILL_BUDGETS in the \
                     same change and say why",
                    budget.max_lines
                ),
            ));
        }
        if bytes > budget.max_bytes {
            findings.push(Finding::file_level(
                relative,
                "skill-budget",
                format!(
                    "{bytes} bytes, over the pinned {}; raise the pin in SKILL_BUDGETS in the \
                     same change and say why",
                    budget.max_bytes
                ),
            ));
        }
    }
}

/// A single-line scalar, plain or double-quoted, as a string; `None` when YAML
/// could read it as anything but this exact string.
fn scalar(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if let Some(inner) = raw
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    {
        // No escapes and no inner quote: the text between the quotes is the value.
        return (!inner.contains('"') && !inner.contains('\\')).then(|| inner.to_owned());
    }
    let first = raw.chars().next()?;
    let indicator = "-?:,[]{}#&*!|>'\"%@`".contains(first);
    let special = matches!(
        raw.to_ascii_lowercase().as_str(),
        "true" | "false" | "yes" | "no" | "on" | "off" | "null" | "~" | "y" | "n"
    );
    let numeric = raw.parse::<f64>().is_ok();
    if indicator
        || special
        || numeric
        || raw.contains(": ")
        || raw.contains(" #")
        || raw.ends_with(':')
    {
        return None;
    }
    Some(raw.to_owned())
}

/// Parse the frontmatter between the first two `---` lines: `key: value`
/// lines, and `metadata:` followed by lines indented two spaces.
fn parse_frontmatter(
    relative: &str,
    lines: &[String],
    findings: &mut Vec<Finding>,
) -> Option<Frontmatter> {
    let mut fail = |line: usize, detail: String| {
        findings.push(Finding::at(relative, line, "skill-frontmatter", detail));
    };
    if lines.first().map(String::as_str) != Some("---") {
        fail(1, "a skill starts with a `---` frontmatter line".to_owned());
        return None;
    }
    let Some(close) = lines.iter().skip(1).position(|line| line == "---") else {
        fail(1, "the frontmatter has no closing `---` line".to_owned());
        return None;
    };
    let close = close + 1;
    let mut frontmatter = Frontmatter {
        body_start: close + 1,
        ..Frontmatter::default()
    };
    let mut in_metadata = false;
    for (index, line) in lines.iter().enumerate().take(close).skip(1) {
        let number = index + 1;
        if let Some(nested) = line.strip_prefix("  ") {
            if !in_metadata {
                fail(number, "only `metadata` holds indented keys".to_owned());
                continue;
            }
            let Some((key, value)) = nested.split_once(':') else {
                fail(number, format!("{nested:?} is not `key: \"value\"`"));
                continue;
            };
            let quoted = value.trim();
            if !(quoted.len() >= 2 && quoted.starts_with('"') && quoted.ends_with('"')) {
                fail(
                    number,
                    format!("metadata.{key} is double-quoted, so it is a string"),
                );
                continue;
            }
            match scalar(quoted) {
                Some(value) => {
                    if frontmatter.metadata.insert(key.to_owned(), value).is_some() {
                        fail(number, format!("metadata.{key} appears twice"));
                    }
                }
                None => fail(
                    number,
                    format!("metadata.{key} is not a plain quoted string"),
                ),
            }
            continue;
        }
        in_metadata = false;
        let Some((key, value)) = line.split_once(':') else {
            fail(number, format!("{line:?} is not `key: value`"));
            continue;
        };
        if key == "metadata" && value.trim().is_empty() {
            in_metadata = true;
            if frontmatter
                .values
                .insert(key.to_owned(), String::new())
                .is_some()
            {
                fail(number, "metadata appears twice".to_owned());
            }
            continue;
        }
        match scalar(value) {
            Some(value) if !value.is_empty() => {
                if frontmatter.values.insert(key.to_owned(), value).is_some() {
                    fail(number, format!("{key} appears twice"));
                }
            }
            _ => fail(
                number,
                format!(
                    "{key} is not a one-line string YAML reads as written: quote it, and keep \
                     `: `, ` #`, quotes and backslashes out of it"
                ),
            ),
        }
    }
    Some(frontmatter)
}

fn is_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= MAX_NAME_CHARS
        && name.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9'))
        })
}

fn check_frontmatter(
    name: &str,
    relative: &str,
    frontmatter: &Frontmatter,
    findings: &mut Vec<Finding>,
) {
    let mut fail = |detail: String| {
        findings.push(Finding::at(relative, 1, "skill-frontmatter", detail));
    };
    for key in frontmatter.values.keys() {
        if key == "allowed-tools" {
            fail(
                "allowed-tools would pre-approve tools; leave the user to approve write tools"
                    .to_owned(),
            );
        } else if !KEYS.contains(&key.as_str()) {
            fail(format!(
                "{key:?} is not a skill key; a skill holds exactly {KEYS:?}"
            ));
        }
    }
    for key in KEYS {
        if !frontmatter.values.contains_key(*key) {
            fail(format!("the frontmatter has no {key}"));
        }
    }
    let value = |key: &str| frontmatter.values.get(key).map(String::as_str);
    if let Some(declared) = value("name") {
        if !is_skill_name(declared) {
            fail(format!(
                "name {declared:?} is 1 to {MAX_NAME_CHARS} lower-case letters, digits and \
                 single hyphens, neither first nor last"
            ));
        }
        if declared != name {
            fail(format!(
                "name {declared:?} differs from its directory {name:?}"
            ));
        }
    }
    if let Some(description) = value("description") {
        let chars = description.chars().count();
        if chars > MAX_DESCRIPTION_CHARS {
            fail(format!(
                "the description is {chars} characters; at most {MAX_DESCRIPTION_CHARS}"
            ));
        }
        if !description
            .chars()
            .all(|ch| ch.is_ascii() && !ch.is_ascii_control())
        {
            fail("the description is English, in plain ASCII".to_owned());
        }
    }
    if let Some(license) = value("license")
        && license != LICENSE
    {
        fail(format!("license is {LICENSE}, the repository's"));
    }
    if let Some(compatibility) = value("compatibility") {
        let chars = compatibility.chars().count();
        if chars > MAX_COMPATIBILITY_CHARS {
            fail(format!(
                "compatibility is {chars} characters; at most {MAX_COMPATIBILITY_CHARS}"
            ));
        }
    }
    for key in frontmatter.metadata.keys() {
        if !METADATA_KEYS.contains(&key.as_str()) {
            fail(format!(
                "metadata.{key} is not recorded; metadata holds exactly {METADATA_KEYS:?}"
            ));
        }
    }
    for key in METADATA_KEYS {
        if !frontmatter.metadata.contains_key(*key) {
            fail(format!("metadata has no {key}"));
        }
    }
    if let Some(contract) = frontmatter.metadata.get("contract")
        && contract != CONTRACT_VERSION
    {
        findings.push(Finding::at(
            relative,
            1,
            "skill-contract",
            format!(
                "metadata.contract is {contract:?} but the contract is {CONTRACT_VERSION:?}; \
                 update the skill with the contract"
            ),
        ));
    }
}

/// The profiles `metadata.profiles` names, in order.
fn declared_profiles(
    relative: &str,
    frontmatter: &Frontmatter,
    findings: &mut Vec<Finding>,
) -> Vec<CapabilityProfile> {
    let Some(list) = frontmatter.metadata.get("profiles") else {
        return Vec::new();
    };
    let mut profiles = Vec::new();
    for name in list.split(',') {
        let named = PROFILES
            .into_iter()
            .find(|profile| profile.manifest_name() == name);
        match named {
            Some(profile) if !profiles.contains(&profile) => profiles.push(profile),
            Some(_) => findings.push(Finding::at(
                relative,
                1,
                "skill-frontmatter",
                format!("metadata.profiles names {name} twice"),
            )),
            None => findings.push(Finding::at(
                relative,
                1,
                "skill-frontmatter",
                format!(
                    "metadata.profiles names {name:?}; it is a comma-separated list of core, \
                     editor, runtime and full, without spaces"
                ),
            )),
        }
    }
    profiles
}

/// Why `tool` cannot be named in a skill for `profiles`, if it cannot.
fn tool_problem(tool: &str, profiles: &[CapabilityProfile]) -> Option<String> {
    let Some(entry) = descriptor(tool) else {
        return Some(format!("{tool:?} is not a tool of the catalog"));
    };
    let missing: Vec<&str> = profiles
        .iter()
        .filter(|profile| !entry.in_profile(**profile))
        .map(|profile| profile.manifest_name())
        .collect();
    (!missing.is_empty()).then(|| {
        format!(
            "{tool} is not a tool of the {} profile the skill declares",
            missing.join(" and ")
        )
    })
}

fn check_tool_names(
    relative: &str,
    scanned: &Scanned,
    body_start: usize,
    profiles: &[CapabilityProfile],
    findings: &mut Vec<Finding>,
) {
    for (index, line) in scanned.lines.iter().enumerate().skip(body_start) {
        if scanned.in_fence[index] {
            continue;
        }
        for span in inline_spans(line) {
            if is_dotted_name_shape(&span)
                && !is_probably_file_name(&span)
                && let Some(problem) = tool_problem(&span, profiles)
            {
                findings.push(Finding::at(relative, index + 1, "skill-tool", problem));
            }
        }
    }
}

/// The tool a fence's info string names, if it is a `json tool=<name>` block.
fn example_tool(info: &str) -> Option<Result<&str, String>> {
    let mut words = info.split_whitespace();
    if words.next() != Some("json") {
        return None;
    }
    let attributes: Vec<&str> = words.collect();
    if attributes.is_empty() {
        return None;
    }
    let mut tool = None;
    for attribute in attributes {
        match attribute.strip_prefix(TOOL_ATTRIBUTE) {
            Some(name) if !name.is_empty() && tool.is_none() => tool = Some(name),
            _ => {
                return Some(Err(format!(
                    "{info:?}: a JSON example's info string is `json tool=<tool name>` and \
                     nothing else"
                )));
            }
        }
    }
    tool.map(Ok)
}

fn check_examples(
    relative: &str,
    scanned: &Scanned,
    profiles: &[CapabilityProfile],
    schemas: Option<&BTreeMap<String, serde_json::Value>>,
    findings: &mut Vec<Finding>,
) {
    for fence in &scanned.fences {
        let line = fence.open_line;
        let mut fail = |detail: String| {
            findings.push(Finding::at(relative, line, "skill-example", detail));
        };
        let tool = match example_tool(&fence.info) {
            None => continue,
            Some(Err(detail)) => {
                fail(detail);
                continue;
            }
            Some(Ok(tool)) => tool,
        };
        if let Some(problem) = tool_problem(tool, profiles) {
            fail(problem);
            continue;
        }
        let instance: serde_json::Value = match serde_json::from_str(&fence.body_text()) {
            Ok(instance) => instance,
            Err(error) => {
                fail(format!("the {tool} example is not JSON: {error}"));
                continue;
            }
        };
        let Some(schemas) = schemas else {
            fail(format!(
                "the {tool} example was not checked: this build has no test-hooks feature, \
                 which carries the schema checker; run docs-check"
            ));
            continue;
        };
        let Some(schema) = schemas.get(tool) else {
            fail(format!("{tool} advertises no input schema"));
            continue;
        };
        let verdict =
            std::panic::catch_unwind(|| schema_conforms(schema, &instance)).unwrap_or(None);
        match verdict {
            Some(true) => {}
            Some(false) => fail(format!(
                "the {tool} example does not satisfy the tool's advertised input schema"
            )),
            None => fail(format!(
                "the schema checker cannot read {tool}'s input schema, so the example is \
                 unchecked"
            )),
        }
    }
}

/// The advertised input schema of every tool the full profile lists in either
/// server state, by name.
#[cfg(feature = "test-hooks")]
fn advertised_schemas() -> Option<BTreeMap<String, serde_json::Value>> {
    use godot_mcp_server::kernel::ServiceKernel;
    let mut schemas = BTreeMap::new();
    for kernel in [
        ServiceKernel::new(CapabilityProfile::Full),
        ServiceKernel::bootstrap(CapabilityProfile::Full),
    ] {
        for tool in kernel.tools() {
            schemas
                .entry(tool.name)
                .or_insert(serde_json::Value::Object(tool.input_schema));
        }
    }
    Some(schemas)
}

#[cfg(not(feature = "test-hooks"))]
fn advertised_schemas() -> Option<BTreeMap<String, serde_json::Value>> {
    None
}

#[cfg(feature = "test-hooks")]
fn schema_conforms(schema: &serde_json::Value, instance: &serde_json::Value) -> Option<bool> {
    Some(godot_mcp_server::test_support::conforms(schema, instance))
}

#[cfg(not(feature = "test-hooks"))]
fn schema_conforms(_schema: &serde_json::Value, _instance: &serde_json::Value) -> Option<bool> {
    None
}

/// Whether a link target still resolves once the file sits alone elsewhere.
fn is_portable_target(target: &str) -> bool {
    let target = target.trim().trim_start_matches('<');
    target.starts_with('#')
        || target.starts_with("https://")
        || target.starts_with("http://")
        || target.starts_with("mailto:")
}

fn link_targets(line: &str) -> Vec<String> {
    let mut targets = Vec::new();
    let mut rest = line;
    while let Some(open) = rest.find("](") {
        let after = &rest[open + 2..];
        let Some(close) = after.find(')') else {
            break;
        };
        targets.push(after[..close].to_owned());
        rest = &after[close + 1..];
    }
    let trimmed = line.trim_start();
    if trimmed.starts_with('[')
        && let Some((label, target)) = trimmed.split_once("]:")
        && !label.contains(']')
    {
        targets.push(target.trim().to_owned());
    }
    for attribute in ["href=\"", "src=\""] {
        let mut rest = line;
        while let Some(open) = rest.find(attribute) {
            let after = &rest[open + attribute.len()..];
            let Some(close) = after.find('"') else {
                break;
            };
            targets.push(after[..close].to_owned());
            rest = &after[close + 1..];
        }
    }
    targets
}

fn check_links(relative: &str, scanned: &Scanned, findings: &mut Vec<Finding>) {
    for (index, line) in scanned.lines.iter().enumerate() {
        if scanned.in_fence[index] {
            continue;
        }
        for target in link_targets(line) {
            if !is_portable_target(&target) {
                findings.push(Finding::at(
                    relative,
                    index + 1,
                    "skill-link",
                    format!(
                        "{target:?} is relative; an installed skill sits alone in an agent's \
                         directory, so link an https:// address instead"
                    ),
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn findings_for(name: &str, text: &str) -> Vec<Finding> {
        check_skill(name, "skills/x/SKILL.md", text, None)
    }

    fn rules(findings: &[Finding]) -> Vec<&str> {
        findings
            .iter()
            .map(|finding| finding.rule.as_str())
            .collect()
    }

    fn skill(frontmatter: &str, body: &str) -> String {
        format!("---\n{frontmatter}---\n\n{body}")
    }

    const GOOD: &str = "name: example-skill\n\
        description: Edit a Godot project through godot-mcp. Use when a task changes a scene.\n\
        license: MIT\n\
        compatibility: Needs a connected godot-mcp server.\n\
        metadata:\n  contract: \"CONTRACT\"\n  profiles: \"editor,full\"\n";

    fn good() -> String {
        GOOD.replace("CONTRACT", CONTRACT_VERSION)
    }

    #[test]
    fn a_well_formed_skill_passes() {
        let text = skill(
            &good(),
            "Read with `scene.tree.get`, then write with `scene.batch.apply`.\n\
             See [the spec](https://agentskills.io/specification) and [below](#rules).\n",
        );
        assert_eq!(findings_for("example-skill", &text), Vec::new());
    }

    #[test]
    fn frontmatter_rules_are_enforced() {
        let cases = [
            (
                good().replace("example-skill", "Example_Skill"),
                "skill-frontmatter",
            ),
            (
                good().replace("name: example-skill", "name: other-skill"),
                "skill-frontmatter",
            ),
            (
                good().replace("license: MIT", "license: Apache-2.0"),
                "skill-frontmatter",
            ),
            (
                format!("{}allowed-tools: Bash\n", good()),
                "skill-frontmatter",
            ),
            (
                good().replace("compatibility: Needs a connected godot-mcp server.\n", ""),
                "skill-frontmatter",
            ),
            (
                good().replace("\"editor,full\"", "editor,full"),
                "skill-frontmatter",
            ),
            (
                good().replace("\"editor,full\"", "\"editor, full\""),
                "skill-frontmatter",
            ),
            (
                good().replace("\"editor,full\"", "\"editor,studio\""),
                "skill-frontmatter",
            ),
            (
                format!("{}  version: \"0.5.0\"\n", good()),
                "skill-frontmatter",
            ),
            (
                good().replace("a scene.", "a scene: soon."),
                "skill-frontmatter",
            ),
            (good().replace("a scene.", "a scène."), "skill-frontmatter"),
            (
                good().replace(&format!("\"{CONTRACT_VERSION}\""), "\"0.0.1\""),
                "skill-contract",
            ),
        ];
        for (frontmatter, rule) in cases {
            let text = skill(&frontmatter, "Body.\n");
            let found = findings_for("example-skill", &text);
            assert!(rules(&found).contains(&rule), "{frontmatter}\n{found:?}");
        }
        let long = format!("description: {}\n", "a".repeat(MAX_DESCRIPTION_CHARS + 1));
        let text = skill(
            &good().replace(
                "description: Edit a Godot project through godot-mcp. Use when a task changes a scene.\n",
                &long,
            ),
            "Body.\n",
        );
        assert!(rules(&findings_for("example-skill", &text)).contains(&"skill-frontmatter"));
        assert!(
            rules(&findings_for("example-skill", "No frontmatter.\n"))
                .contains(&"skill-frontmatter")
        );
    }

    #[test]
    fn yaml_scalars_are_read_strictly() {
        assert_eq!(scalar("MIT"), Some("MIT".to_owned()));
        assert_eq!(scalar("\"0.5.0\""), Some("0.5.0".to_owned()));
        for refused in [
            "yes",
            "0.5",
            "- a",
            "a: b",
            "a #b",
            "\"a\\\"b\"",
            "[a]",
            "> a",
        ] {
            assert_eq!(scalar(refused), None, "{refused}");
        }
    }

    #[test]
    fn tool_names_must_exist_in_every_declared_profile() {
        let unknown = skill(&good(), "Call `scene.tree.fetch`.\n");
        assert!(rules(&findings_for("example-skill", &unknown)).contains(&"skill-tool"));
        let runtime_only = skill(&good(), "Then `project.run.start`.\n");
        assert!(rules(&findings_for("example-skill", &runtime_only)).contains(&"skill-tool"));
        let files = skill(&good(), "Open `project.godot` and `main.tscn`.\n");
        assert!(findings_for("example-skill", &files).is_empty());
        let fenced = skill(&good(), "```text\n`scene.tree.fetch`\n```\n");
        assert!(findings_for("example-skill", &fenced).is_empty());
    }

    #[test]
    fn examples_and_links_are_checked() {
        let not_json = skill(&good(), "```json tool=scene.tree.get\n{\"scene\": \n```\n");
        assert!(rules(&findings_for("example-skill", &not_json)).contains(&"skill-example"));
        let runtime_only = skill(&good(), "```json tool=project.run.start\n{}\n```\n");
        assert!(rules(&findings_for("example-skill", &runtime_only)).contains(&"skill-example"));
        let unknown_attribute = skill(&good(), "```json tool=scene.tree.get lang=en\n{}\n```\n");
        assert!(
            rules(&findings_for("example-skill", &unknown_attribute)).contains(&"skill-example")
        );
        // Without the schemas an example is reported as unchecked, never passed.
        let unchecked = skill(&good(), "```json tool=scene.tree.get\n{}\n```\n");
        assert!(rules(&findings_for("example-skill", &unchecked)).contains(&"skill-example"));
        let plain = skill(&good(), "```json\n{\"mcpServers\": {}}\n```\n");
        assert!(findings_for("example-skill", &plain).is_empty());
        for relative in [
            "See [recipes](../docs/operator/recipes.md).\n",
            "See [recipes](docs/operator/recipes.md).\n",
            "[recipes]: docs/operator/recipes.md\n",
            "<img src=\"diagram.png\">\n",
        ] {
            let text = skill(&good(), relative);
            assert!(
                rules(&findings_for("example-skill", &text)).contains(&"skill-link"),
                "{relative}"
            );
        }
    }

    #[cfg(feature = "test-hooks")]
    #[test]
    fn an_example_must_satisfy_the_advertised_schema() {
        let schemas = advertised_schemas().expect("the schemas build");
        let accepted = skill(
            &good(),
            "```json tool=scene.tree.get\n{\"scene\": \"main.tscn\"}\n```\n",
        );
        assert_eq!(
            check_skill(
                "example-skill",
                "skills/x/SKILL.md",
                &accepted,
                Some(&schemas)
            ),
            Vec::new()
        );
        let refused = skill(
            &good(),
            "```json tool=scene.tree.get\n{\"scene\": 7}\n```\n",
        );
        assert!(
            rules(&check_skill(
                "example-skill",
                "skills/x/SKILL.md",
                &refused,
                Some(&schemas)
            ))
            .contains(&"skill-example")
        );
    }

    #[cfg(feature = "test-hooks")]
    #[test]
    fn every_catalog_tool_has_an_advertised_schema() {
        let schemas = advertised_schemas().expect("the schemas build");
        for tool in godot_mcp_domain::catalog::TOOL_CATALOG {
            assert!(schemas.contains_key(tool.name), "{}", tool.name);
        }
    }

    #[test]
    fn budgets_are_enforced() {
        let long = skill(&good(), &"line\n".repeat(MAX_SKILL_LINES));
        assert!(rules(&findings_for("example-skill", &long)).contains(&"skill-budget"));

        // A pinned skill is held to its own budget, well below the ceiling.
        let budget = SKILL_BUDGETS
            .iter()
            .find(|budget| budget.name == "godot-mcp")
            .expect("godot-mcp is pinned");
        let named = good().replace("example-skill", "godot-mcp");
        let over_lines = skill(&named, &"line\n".repeat(budget.max_lines));
        assert!(over_lines.lines().count() < MAX_SKILL_LINES);
        let findings = findings_for("godot-mcp", &over_lines);
        assert!(
            findings
                .iter()
                .any(|finding| finding.rule == "skill-budget" && finding.detail.contains("lines")),
            "{findings:?}"
        );
        let over_bytes = skill(&named, &format!("{}\n", "x".repeat(budget.max_bytes)));
        let findings = findings_for("godot-mcp", &over_bytes);
        assert!(
            findings
                .iter()
                .any(|finding| finding.rule == "skill-budget" && finding.detail.contains("bytes")),
            "{findings:?}"
        );
        let within = skill(&named, "Read with `scene.tree.get`.\n");
        assert_eq!(findings_for("godot-mcp", &within), Vec::new());
    }

    #[test]
    fn every_pinned_budget_stays_within_the_ceiling() {
        for budget in SKILL_BUDGETS {
            assert!(budget.max_lines <= MAX_SKILL_LINES, "{}", budget.name);
            assert!(budget.max_bytes > 0, "{}", budget.name);
        }
    }
}
