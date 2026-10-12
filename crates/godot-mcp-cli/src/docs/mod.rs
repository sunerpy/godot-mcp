//! Documentation verification: the docs checker, the security lint, the
//! credential scanner and the executable-quickstart extractor.
//!
//! ## Why this lives in code rather than in a review checklist
//!
//! Every operator-facing name in `docs/` — a tool name, a configuration key, an
//! environment variable, a protocol constant, a Godot version — has exactly one
//! canonical owner elsewhere in this repository. Prose copies of those names rot
//! silently. So the pages that enumerate them are *generated* from the canonical
//! owner and byte-compared ([`generated`]), and every other page's names are
//! *checked against* the canonical owner ([`checker`]).
//!
//! ## Scope of the scanners
//!
//! [`security`] and [`secrets`] read **only** files under `docs/`. That is a
//! deliberate structural property, not an oversight: this module's own source
//! necessarily contains the very terms the lint forbids in documentation, and a
//! scanner that scanned its own implementation would flag itself. Keeping the scan
//! surface to `docs/` means the exemption machinery is needed only for genuine
//! in-documentation quotations, and each such exemption is narrow and verified
//! against a canonical constant rather than being a free-text opt-out.

pub mod canonical;
pub mod checker;
pub mod generated;
pub mod scan;
pub mod script;
pub mod secrets;
pub mod security;

use std::path::{Path, PathBuf};

/// The documentation root, relative to the workspace root.
pub const DOCS_DIR: &str = "docs";

/// One documentation page loaded from disk.
#[derive(Debug, Clone)]
pub struct DocFile {
    /// Workspace-relative, `/`-separated path (`docs/operator/quickstart.md`).
    pub relative: String,
    /// Absolute path on disk.
    pub absolute: PathBuf,
    /// Full file text.
    pub text: String,
}

/// One problem found in the documentation tree.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Finding {
    /// Workspace-relative page, or the empty string for a tree-level finding.
    pub file: String,
    /// 1-based line number, or 0 when the finding is not line-scoped.
    pub line: usize,
    /// Stable rule identifier.
    pub rule: String,
    /// Human-readable explanation.
    pub detail: String,
}

impl Finding {
    /// Build a line-scoped finding.
    #[must_use]
    pub fn at(file: &str, line: usize, rule: &str, detail: impl Into<String>) -> Self {
        Self {
            file: file.to_owned(),
            line,
            rule: rule.to_owned(),
            detail: detail.into(),
        }
    }

    /// Build a file-scoped finding.
    #[must_use]
    pub fn file_level(file: &str, rule: &str, detail: impl Into<String>) -> Self {
        Self::at(file, 0, rule, detail)
    }

    /// One stable single-line rendering.
    #[must_use]
    pub fn to_line(&self) -> String {
        let location = if self.file.is_empty() {
            "docs".to_owned()
        } else if self.line == 0 {
            self.file.clone()
        } else {
            format!("{}:{}", self.file, self.line)
        };
        format!("{location}: [{}] {}", self.rule, self.detail)
    }
}

/// Render a finding list as a stable, sorted report.
#[must_use]
pub fn render(findings: &[Finding]) -> String {
    let mut sorted = findings.to_vec();
    sorted.sort();
    let mut text = String::new();
    for finding in &sorted {
        text.push_str(&finding.to_line());
        text.push('\n');
    }
    text
}

/// Load every Markdown page under `docs/`, sorted by relative path.
///
/// # Errors
/// Returns an error if `docs/` is missing or a page is not valid UTF-8.
pub fn load(workspace_root: &Path) -> Result<Vec<DocFile>, String> {
    let root = workspace_root.join(DOCS_DIR);
    if !root.is_dir() {
        return Err(format!("{} is not a directory", root.display()));
    }
    let mut pages = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let entries =
            std::fs::read_dir(&dir).map_err(|error| format!("read {}: {error}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|error| format!("read {}: {error}", dir.display()))?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(std::ffi::OsStr::to_str) != Some("md") {
                continue;
            }
            let text = std::fs::read_to_string(&path)
                .map_err(|error| format!("read {}: {error}", path.display()))?;
            let relative = path
                .strip_prefix(workspace_root)
                .map_err(|_| format!("{} is outside the workspace", path.display()))?
                .to_string_lossy()
                .replace('\\', "/");
            pages.push(DocFile {
                relative,
                absolute: path,
                text,
            });
        }
    }
    pages.sort_by(|left, right| left.relative.cmp(&right.relative));
    Ok(pages)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn findings_render_stably_with_and_without_a_line() {
        let findings = vec![
            Finding::at("docs/b.md", 7, "rule-b", "second"),
            Finding::file_level("docs/a.md", "rule-a", "first"),
        ];
        assert_eq!(
            render(&findings),
            "docs/a.md: [rule-a] first\ndocs/b.md:7: [rule-b] second\n"
        );
    }
}
