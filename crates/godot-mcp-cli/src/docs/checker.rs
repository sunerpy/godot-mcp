//! The documentation checker.
//!
//! It answers one question mechanically: does every operator-facing name, link,
//! configuration example and generated page in `docs/` still agree with the
//! canonical owner of that fact?
//!
//! Nine rule families, all fail-closed:
//!
//! 1. `generated-page` — a generated page is regenerated and byte-compared.
//! 2. `stale-name` — every back-ticked dotted lowercase token must be a canonical
//!    tool, configuration key, error code, plane or capability.
//! 3. `stale-env-var` — every `GODOT_MCP_*` name must be recognized by the real
//!    configuration loader.
//! 4. `stale-protocol-name` — bridge kinds, envelope fields, mutation statuses and
//!    the two HTTP protocol constants must match their schema/code owner.
//! 5. `stale-resource-uri` — every `godot-mcp://` URI must be a canonical resource.
//! 6. `config-example` — every TOML example must be annotated `accept` or `reject`
//!    and must get that verdict from the production loader.
//! 7. `broken-link` — every relative Markdown link must resolve on disk.
//! 8. `required-statement` — pages that must state an honesty-critical fact do.
//! 9. `runnable-block` — every executable quickstart block is well-formed.
//!
//! The security lint and the credential scanner run alongside, and both of their
//! self-checks run too, so a weakened scanner fails rather than reporting clean.

use std::collections::BTreeSet;
use std::path::Path;

use super::canonical::{self, Canonical};
use super::scan::{Scanned, inline_spans, scan};
use super::{DocFile, Finding, generated, script, secrets, security};

/// Historical verification reports record what an earlier contract served.
/// They may name a frozen version 0.1 tool that contract 0.2 no longer lists;
/// every other page must use the current catalog.
pub const HISTORICAL_REPORTS: &str = "docs/reports/";

/// The annotation that declares a configuration example's expected verdict.
pub const CONFIG_ANNOTATION: &str = "docs-check:config";

/// Path segments that make a dotted token a file name rather than a canonical
/// name. `project.godot`, `bridge_codec.gd` and `example.invalid` are all
/// legitimate prose and none of them is a tool.
pub const NON_NAME_SEGMENTS: &[&str] = &[
    "godot",
    "toml",
    "json",
    "gd",
    "md",
    "rs",
    "txt",
    "lock",
    "invalid",
    "tscn",
    "tres",
    "sha256",
    "gz",
    "zip",
    "yml",
    "yaml",
    "cfg",
    "png",
    "jpg",
    "so",
    "dylib",
    "dll",
    "exe",
    "cdx",
    "sh",
    "py",
    "gdextension",
    "xml",
    "csv",
    "html",
    "ini",
    "tpz",
    "com",
    "org",
    "net",
    "io",
    "dev",
];

/// One honesty-critical statement a page must carry verbatim.
#[derive(Debug, Clone, Copy)]
pub struct RequiredStatement {
    /// Workspace-relative page.
    pub file: &'static str,
    /// The exact substring the page must contain.
    pub needle: &'static str,
    /// Why the statement is required.
    pub because: &'static str,
}

/// Statements without which a page would overstate what this release delivers.
pub const REQUIRED_STATEMENTS: &[RequiredStatement] = &[
    RequiredStatement {
        file: "docs/operator/editor-bridge.md",
        needle: "is not shipped with the first release",
        because: "the editor plane was cancelled; only the frozen bridge protocol and its \
                  codec ship, and the page must not read as if the plug-in exists",
    },
    RequiredStatement {
        file: "docs/operator/version-matrix.md",
        needle: "a row is not observed for a later tree until the matrix runs there again",
        because: "a real-runner observation of the Godot matrix belongs to the tree it was \
                  made on, and the page must not carry it over to a later one",
    },
    RequiredStatement {
        file: "docs/operator/packaging.md",
        needle: "only the Linux archive has been built and verified on this host",
        because: "the macOS and Windows archive paths are implemented but were never executed \
                  here",
    },
    RequiredStatement {
        file: "docs/security/threat-model.md",
        needle: "A loopback address is never an identity",
        because: "reaching the backend over loopback proves nothing about who is calling",
    },
    RequiredStatement {
        file: "docs/security/threat-model.md",
        needle: security::QUOTED_TERM_MARKER,
        because: "the page must quote the engine's own containment posture verbatim rather \
                  than paraphrasing it into a stronger claim",
    },
    RequiredStatement {
        file: "docs/contributor/adr/0001-gdext-reference-only.md",
        needle: "reference and test only",
        because: "gdext is not a first-release Cargo, runtime or shipping dependency",
    },
    RequiredStatement {
        file: "docs/contributor/adr/0004-editor-plane-cancelled.md",
        needle: "cancelled, not completed",
        because: "the editor-plane work was cancelled by an explicit decision and must never \
                  be presented as delivered",
    },
    RequiredStatement {
        file: "docs/operator/configuration.md",
        needle: "the effective process deadline is the smaller of the call's `deadline_ms`",
        because: "`limits.deadline_ms` must cap all four process-backed engine tools while a \
                  shorter per-call deadline remains effective",
    },
    RequiredStatement {
        file: "docs/operator/process-boundaries.md",
        needle: "an endpoint is dialled only through the server-owned launch receipt",
        because: "the LSP/DAP adapters must never be described as reaching an arbitrary \
                  loopback listener; only a managed editor session's launch receipt makes an \
                  endpoint dialable",
    },
    RequiredStatement {
        file: "docs/operator/managed-sessions.md",
        needle: "something that merely listens on a port proves nothing about who it is",
        because: "an editor session is ready only once its listeners are attributed to the \
                  editor process; a successful connect is never treated as ownership",
    },
    RequiredStatement {
        file: "docs/operator/profiles-and-outcomes.md",
        needle: "An import reports `passed` only when the capped engine output parses without errors",
        because: "`project.import.run` must combine parsed diagnostics, asserted exit status and \
                  nominated artifacts rather than treating a pre-existing artifact as success",
    },
    RequiredStatement {
        file: "docs/operator/profiles-and-outcomes.md",
        needle: "A `passed` verdict requires both a framework-documented clean exit status",
        because: "`project.test` must not infer a passing suite from exit status alone; it must \
                  also parse the framework's clean summary",
    },
    RequiredStatement {
        file: "docs/operator/profiles-and-outcomes.md",
        needle: "structured `unsupported` report and launches nothing.",
        because: "an editor build without the command-line contract required by `script.check` \
                  must fail closed without launching",
    },
    RequiredStatement {
        file: "docs/contributor/workflow.md",
        needle: "The runner never evaluates a descriptor's assertions.",
        because: "`qa-task` compares exit codes only; a descriptor's assertions are never \
                  machine-evaluated, so a passing task must not read as if they were confirmed",
    },
];

fn is_probably_file_name(token: &str) -> bool {
    token
        .split('.')
        .any(|segment| NON_NAME_SEGMENTS.contains(&segment))
}

/// Whether a token has the shape of a canonical dotted name.
fn is_dotted_name_shape(token: &str) -> bool {
    if !token.contains('.') || token.contains('/') || token.contains('-') || token.contains(' ') {
        return false;
    }
    let mut segments = token.split('.');
    let first = segments.next().unwrap_or_default();
    if first.is_empty() || !first.starts_with(|ch: char| ch.is_ascii_lowercase()) {
        return false;
    }
    token.split('.').all(|segment| {
        !segment.is_empty()
            && segment
                .chars()
                .all(|ch| ch.is_ascii_lowercase() || ch == '_' || ch.is_ascii_digit())
            && segment.starts_with(|ch: char| ch.is_ascii_lowercase())
    })
}

fn config_examples(scanned: &Scanned) -> Vec<(usize, String, Option<String>)> {
    scanned
        .fences
        .iter()
        .filter(|fence| fence.info == "toml")
        .map(|fence| {
            let verdict = fence.annotation(CONFIG_ANNOTATION).map(|annotation| {
                annotation
                    .rsplit_once('=')
                    .map_or(String::new(), |(_, value)| {
                        value.trim().trim_matches('"').to_owned()
                    })
            });
            (fence.body_start_line, fence.body_text(), verdict)
        })
        .collect()
}

/// Collect the canonical configuration key paths from every accepted example.
fn accepted_config_keys(pages: &[DocFile]) -> (BTreeSet<String>, Vec<Finding>) {
    let mut keys = BTreeSet::new();
    let mut findings = Vec::new();
    for page in pages {
        let scanned = scan(&page.text);
        for (line, body, verdict) in config_examples(&scanned) {
            match verdict.as_deref() {
                Some("accept") => match canonical::load_config_example(&body) {
                    Ok(_) => {
                        if let Err(error) = canonical::flatten_config_keys(&body, &mut keys) {
                            findings.push(Finding::at(
                                &page.relative,
                                line,
                                "config-example",
                                error,
                            ));
                        }
                    }
                    Err(error) => findings.push(Finding::at(
                        &page.relative,
                        line,
                        "config-example",
                        format!(
                            "the example is annotated accept but the production loader \
                             rejected it: {error}"
                        ),
                    )),
                },
                Some("reject") => {
                    if canonical::load_config_example(&body).is_ok() {
                        findings.push(Finding::at(
                            &page.relative,
                            line,
                            "config-example",
                            "the example is annotated reject but the production loader \
                             accepted it",
                        ));
                    }
                }
                Some(other) => findings.push(Finding::at(
                    &page.relative,
                    line,
                    "config-example",
                    format!("unknown expected verdict {other:?}; use accept or reject"),
                )),
                None => findings.push(Finding::at(
                    &page.relative,
                    line,
                    "config-example",
                    format!(
                        "a TOML example must be preceded by \
                         <!-- {CONFIG_ANNOTATION} expect=\"accept\" --> or expect=\"reject\""
                    ),
                )),
            }
        }
    }
    (keys, findings)
}

fn check_names(
    page: &DocFile,
    scanned: &Scanned,
    canonical: &Canonical,
    config_keys: &BTreeSet<String>,
) -> Vec<Finding> {
    let mut findings = Vec::new();
    for (index, line) in scanned.lines.iter().enumerate() {
        let line_number = index + 1;
        for span in inline_spans(line) {
            if let Some(uri) = span.strip_prefix("godot-mcp://")
                && !uri.is_empty()
            {
                let normalized = format!("godot-mcp://{uri}");
                if !canonical.resources.contains(&normalized) {
                    findings.push(Finding::at(
                        &page.relative,
                        line_number,
                        "stale-resource-uri",
                        format!("{normalized:?} is not a canonical resource URI"),
                    ));
                }
                continue;
            }
            if span.len() > canonical::ENV_PREFIX.len()
                && span.starts_with(canonical::ENV_PREFIX)
                && span
                    .chars()
                    .all(|ch| ch.is_ascii_uppercase() || ch == '_' || ch.is_ascii_digit())
                && !canonical::env_var_is_recognized(&span)
            {
                findings.push(Finding::at(
                    &page.relative,
                    line_number,
                    "stale-env-var",
                    format!("{span:?} is not recognized by the configuration loader"),
                ));
                continue;
            }
            let historical = page.relative.starts_with(HISTORICAL_REPORTS)
                && canonical.historical_tools.contains(&span);
            if is_dotted_name_shape(&span)
                && !is_probably_file_name(&span)
                && !canonical.is_known_dotted(&span, config_keys)
                && !historical
            {
                findings.push(Finding::at(
                    &page.relative,
                    line_number,
                    "stale-name",
                    format!(
                        "{span:?} is not a canonical tool, configuration key, error code, \
                         plane or capability"
                    ),
                ));
            }
        }
    }
    findings
}

fn check_protocol_names(page: &DocFile, scanned: &Scanned, canonical: &Canonical) -> Vec<Finding> {
    let mut findings = Vec::new();
    for (index, line) in scanned.lines.iter().enumerate() {
        let line_number = index + 1;
        for span in inline_spans(line) {
            if let Some(kind) = span.strip_prefix("bridge:")
                && !canonical.bridge_kinds.contains(kind)
            {
                findings.push(Finding::at(
                    &page.relative,
                    line_number,
                    "stale-protocol-name",
                    format!("{kind:?} is not one of the frozen bridge message kinds"),
                ));
            }
            if let Some(field) = span.strip_prefix("envelope:")
                && !canonical.bridge_fields.contains(field)
            {
                findings.push(Finding::at(
                    &page.relative,
                    line_number,
                    "stale-protocol-name",
                    format!("{field:?} is not a bridge envelope field"),
                ));
            }
            if let Some(status) = span.strip_prefix("mutation:")
                && !canonical.bridge_mutation_statuses.contains(status)
            {
                findings.push(Finding::at(
                    &page.relative,
                    line_number,
                    "stale-protocol-name",
                    format!("{status:?} is not a bridge mutation status"),
                ));
            }
            if let Some(constant) = span.strip_prefix("http-protocol:")
                && constant != canonical.http_path
                && constant != canonical.proxy_hop_header
            {
                findings.push(Finding::at(
                    &page.relative,
                    line_number,
                    "stale-protocol-name",
                    format!("{constant:?} is neither the served MCP path nor the proxy-hop header"),
                ));
            }
        }
    }
    findings
}

fn link_targets(line: &str) -> Vec<String> {
    let mut targets = Vec::new();
    let mut rest = line;
    while let Some(open) = rest.find("](") {
        let after = &rest[open + 2..];
        match after.find(')') {
            Some(close) => {
                targets.push(after[..close].to_owned());
                rest = &after[close + 1..];
            }
            None => break,
        }
    }
    targets
}

fn check_links(workspace_root: &Path, page: &DocFile, scanned: &Scanned) -> Vec<Finding> {
    let mut findings = Vec::new();
    let page_dir = page
        .absolute
        .parent()
        .map_or_else(|| workspace_root.to_path_buf(), Path::to_path_buf);
    for (index, line) in scanned.lines.iter().enumerate() {
        if scanned.in_fence[index] {
            continue;
        }
        for target in link_targets(line) {
            if target.is_empty()
                || target.starts_with('#')
                || target.contains("://")
                || target.starts_with("mailto:")
            {
                continue;
            }
            let path_part = target.split('#').next().unwrap_or(&target);
            if path_part.is_empty() {
                continue;
            }
            let resolved = if let Some(absolute) = path_part.strip_prefix('/') {
                workspace_root.join(absolute)
            } else {
                page_dir.join(path_part)
            };
            if !resolved.exists() {
                findings.push(Finding::at(
                    &page.relative,
                    index + 1,
                    "broken-link",
                    format!("{target:?} does not resolve to a file in this repository"),
                ));
            }
        }
    }
    findings
}

fn check_generated(workspace_root: &Path, canonical: &Canonical) -> Vec<Finding> {
    let mut findings = Vec::new();
    for page in generated::PAGES {
        let path = workspace_root.join(page.relative);
        let expected = match generated::render(page.generator, canonical) {
            Ok(text) => text,
            Err(error) => {
                findings.push(Finding::file_level(page.relative, "generated-page", error));
                continue;
            }
        };
        match std::fs::read_to_string(&path) {
            Ok(actual) if actual == expected => {}
            Ok(_) => findings.push(Finding::file_level(
                page.relative,
                "generated-page",
                "this page no longer matches what its canonical source renders; regenerate it \
                 instead of editing it",
            )),
            Err(error) => findings.push(Finding::file_level(
                page.relative,
                "generated-page",
                format!("cannot read the generated page: {error}"),
            )),
        }
    }
    findings
}

fn check_required_statements(pages: &[DocFile]) -> Vec<Finding> {
    let mut findings = Vec::new();
    for required in REQUIRED_STATEMENTS {
        match pages.iter().find(|page| page.relative == required.file) {
            Some(page) if page.text.contains(required.needle) => {}
            Some(_) => findings.push(Finding::file_level(
                required.file,
                "required-statement",
                format!(
                    "must state {:?} because {}",
                    required.needle, required.because
                ),
            )),
            None => findings.push(Finding::file_level(
                required.file,
                "required-statement",
                "this page is required but missing",
            )),
        }
    }
    findings
}

/// The complete documentation check.
///
/// # Errors
/// Returns an error only when the documentation tree or a canonical source cannot
/// be read at all. Every content problem is reported as a [`Finding`].
pub fn check(workspace_root: &Path) -> Result<Vec<Finding>, String> {
    let pages = super::load(workspace_root)?;
    if pages.is_empty() {
        return Err("the documentation tree contains no page".to_string());
    }
    let canonical = canonical::build(workspace_root)?;
    let mut findings = check_generated(workspace_root, &canonical);
    let (config_keys, config_findings) = accepted_config_keys(&pages);
    findings.extend(config_findings);
    for page in &pages {
        let scanned = scan(&page.text);
        findings.extend(check_names(page, &scanned, &canonical, &config_keys));
        findings.extend(check_protocol_names(page, &scanned, &canonical));
        findings.extend(check_links(workspace_root, page, &scanned));
    }
    findings.extend(check_required_statements(&pages));
    let (_scripts, script_findings) = script::extract_all(&pages);
    findings.extend(script_findings);
    findings.extend(security::check(&pages));
    findings.extend(security::self_check());
    findings.extend(secrets::check(&pages));
    findings.extend(secrets::prove_detects_planted_credential());
    findings.sort();
    findings.dedup();
    Ok(findings)
}

/// Regenerate every generated page on disk.
///
/// # Errors
/// Returns an error if a canonical source cannot be read or a page cannot be
/// written.
pub fn write_generated(workspace_root: &Path) -> Result<Vec<String>, String> {
    let canonical = canonical::build(workspace_root)?;
    let mut written = Vec::new();
    for page in generated::PAGES {
        let text = generated::render(page.generator, &canonical)?;
        let path = workspace_root.join(page.relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("create {}: {error}", parent.display()))?;
        }
        std::fs::write(&path, text.as_bytes())
            .map_err(|error| format!("write {}: {error}", path.display()))?;
        written.push(page.relative.to_owned());
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dotted_name_shape_accepts_tools_and_rejects_file_names() {
        assert!(is_dotted_name_shape("scene.tree.get"));
        assert!(is_dotted_name_shape("http.local_secret"));
        assert!(!is_dotted_name_shape("godot-mcp.toml"));
        assert!(!is_dotted_name_shape("qa/tasks/36.json"));
        assert!(!is_dotted_name_shape("Cargo.toml"));
        assert!(!is_dotted_name_shape("4.4.1"));
        assert!(is_probably_file_name("project.godot"));
        assert!(is_probably_file_name("example.invalid"));
        assert!(!is_probably_file_name("scene.tree.get"));
    }

    #[test]
    fn link_and_config_annotation_extraction_are_precise() {
        assert_eq!(
            link_targets("see [a](x/y.md) and [b](https://example.invalid)"),
            vec!["x/y.md".to_owned(), "https://example.invalid".to_owned()]
        );
        let scanned = scan(
            "<!-- docs-check:config expect=\"reject\" -->\n```toml\ntransport = \"http\"\n```\n",
        );
        let examples = config_examples(&scanned);
        assert_eq!(examples.len(), 1);
        assert_eq!(examples[0].2.as_deref(), Some("reject"));
    }
}
