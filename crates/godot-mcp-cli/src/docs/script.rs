//! Executable documentation: the quickstart's shell steps, extracted verbatim.
//!
//! A quickstart that only a human can follow is a quickstart nobody can verify.
//! Each runnable page therefore marks its steps with explicit delimiters and
//! declares every variable it expands, so a test can execute the exact commands a
//! reader sees, in order, in a clean temporary environment — with no interpretation
//! step in between.
//!
//! Verbatim means verbatim: the extractor never rewrites a command. If a step
//! needs a value that only the test environment knows, the page must declare that
//! variable with [`VAR_MARKER`], and an undeclared expansion is an error rather
//! than something the harness silently fills in.

use super::scan::{html_comment, scan};
use super::{DocFile, Finding};

/// Opens a runnable block and names it.
pub const BEGIN_MARKER: &str = "docs-script:begin";

/// Closes a runnable block.
pub const END_MARKER: &str = "docs-script:end";

/// Declares a variable the block's steps may expand.
pub const VAR_MARKER: &str = "docs-script:var";

/// Fence info strings whose body is a runnable step.
pub const RUNNABLE_INFO: &[&str] = &["sh", "bash", "console"];

/// One extracted step: the exact command text and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptStep {
    /// 1-based line of the step's opening fence.
    pub line: usize,
    /// The step body, verbatim, newline-joined without a trailing newline.
    pub command: String,
}

/// One runnable block on a page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocScript {
    /// Block name from the begin marker.
    pub name: String,
    /// Page the block came from.
    pub file: String,
    /// Variables the block declared.
    pub variables: Vec<String>,
    /// Steps in source order.
    pub steps: Vec<ScriptStep>,
}

fn marker_argument(comment: &str, marker: &str) -> Option<String> {
    let rest = comment.strip_prefix(marker)?.trim();
    let value = rest
        .strip_prefix("name=")
        .or_else(|| rest.strip_prefix("name ="))
        .unwrap_or(rest)
        .trim();
    Some(value.trim_matches('"').to_owned())
}

/// Every identifier expanded as `$NAME` or `${NAME}` in `command`.
#[must_use]
pub fn expansions(command: &str) -> Vec<String> {
    let mut names = Vec::new();
    let bytes = command.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] != b'$' {
            index += 1;
            continue;
        }
        let mut cursor = index + 1;
        let braced = bytes.get(cursor) == Some(&b'{');
        if braced {
            cursor += 1;
        }
        let start = cursor;
        while cursor < bytes.len()
            && (bytes[cursor].is_ascii_alphanumeric() || bytes[cursor] == b'_')
        {
            cursor += 1;
        }
        if cursor > start {
            names.push(String::from_utf8_lossy(&bytes[start..cursor]).into_owned());
        }
        index = if braced && bytes.get(cursor) == Some(&b'}') {
            cursor + 1
        } else {
            cursor.max(index + 1)
        };
    }
    names.sort();
    names.dedup();
    names
}

/// Extract every runnable block on one page.
///
/// # Errors
/// Returns an error if a block is unterminated, unnamed, empty, or expands a
/// variable it never declared.
pub fn extract(page: &DocFile) -> Result<Vec<DocScript>, String> {
    let scanned = scan(&page.text);
    let mut scripts: Vec<DocScript> = Vec::new();
    let mut open: Option<DocScript> = None;
    for (index, line) in scanned.lines.iter().enumerate() {
        let opening = scanned
            .fences
            .iter()
            .find(|fence| fence.open_line == index + 1);
        // A marker inside a fence body is a page showing the markup, not declaring a
        // block, so the page that documents this mechanism does not accidentally
        // declare an empty one. A fence's own opening line is not "inside" it.
        if opening.is_none() && scanned.in_fence[index] {
            continue;
        }
        if let Some(comment) = html_comment(line) {
            if let Some(name) = marker_argument(comment, BEGIN_MARKER) {
                if open.is_some() {
                    return Err(format!(
                        "{}:{}: a runnable block opened inside another",
                        page.relative,
                        index + 1
                    ));
                }
                if name.is_empty() {
                    return Err(format!(
                        "{}:{}: a runnable block must be named",
                        page.relative,
                        index + 1
                    ));
                }
                open = Some(DocScript {
                    name,
                    file: page.relative.clone(),
                    variables: Vec::new(),
                    steps: Vec::new(),
                });
                continue;
            }
            if comment.starts_with(END_MARKER) {
                let script = open.take().ok_or_else(|| {
                    format!(
                        "{}:{}: a runnable block closed without opening",
                        page.relative,
                        index + 1
                    )
                })?;
                if script.steps.is_empty() {
                    return Err(format!(
                        "{}: runnable block {:?} has no step",
                        page.relative, script.name
                    ));
                }
                scripts.push(script);
                continue;
            }
            if let Some(script) = open.as_mut()
                && let Some(declared) = comment.strip_prefix(VAR_MARKER)
            {
                let name = declared
                    .trim()
                    .split_once('=')
                    .map_or(declared.trim(), |(name, _)| name)
                    .trim()
                    .to_owned();
                if name.is_empty() {
                    return Err(format!(
                        "{}:{}: a declared variable must be named",
                        page.relative,
                        index + 1
                    ));
                }
                script.variables.push(name);
            }
            continue;
        }
        if let Some(script) = open.as_mut()
            && let Some(fence) = opening
            && RUNNABLE_INFO.contains(&fence.info.as_str())
        {
            let command = fence.body.join("\n");
            for name in expansions(&command) {
                if !script.variables.contains(&name) {
                    return Err(format!(
                        "{}:{}: step expands ${name} but the block never declared it",
                        page.relative, fence.body_start_line
                    ));
                }
            }
            script.steps.push(ScriptStep {
                line: fence.open_line,
                command,
            });
        }
    }
    if let Some(script) = open {
        return Err(format!(
            "{}: runnable block {:?} was never closed",
            page.relative, script.name
        ));
    }
    Ok(scripts)
}

/// Extract every runnable block in the tree, turning an extraction error into a
/// finding so one malformed page does not hide the rest.
#[must_use]
pub fn extract_all(pages: &[DocFile]) -> (Vec<DocScript>, Vec<Finding>) {
    let mut scripts = Vec::new();
    let mut findings = Vec::new();
    for page in pages {
        match extract(page) {
            Ok(found) => scripts.extend(found),
            Err(error) => {
                findings.push(Finding::file_level(&page.relative, "runnable-block", error))
            }
        }
    }
    (scripts, findings)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(text: &str) -> DocFile {
        DocFile {
            relative: "docs/operator/quickstart.md".to_owned(),
            absolute: std::path::PathBuf::from("docs/operator/quickstart.md"),
            text: text.to_owned(),
        }
    }

    #[test]
    fn a_block_extracts_its_declared_variables_and_steps_in_order() {
        let text = "\
<!-- docs-script:begin name=\"smoke\" -->\n\
<!-- docs-script:var PROJECT = the project root -->\n\
```sh\nmkdir -p \"$PROJECT\"\n```\n\
```sh\nls \"$PROJECT\"\n```\n\
<!-- docs-script:end -->\n";
        let scripts = extract(&page(text)).expect("the block extracts");
        assert_eq!(scripts.len(), 1);
        assert_eq!(scripts[0].name, "smoke");
        assert_eq!(scripts[0].variables, vec!["PROJECT".to_owned()]);
        assert_eq!(
            scripts[0]
                .steps
                .iter()
                .map(|step| step.command.clone())
                .collect::<Vec<_>>(),
            vec![
                "mkdir -p \"$PROJECT\"".to_owned(),
                "ls \"$PROJECT\"".to_owned()
            ]
        );
    }

    #[test]
    fn an_undeclared_expansion_is_an_error() {
        let text = "\
<!-- docs-script:begin name=\"smoke\" -->\n\
```sh\nls \"$UNDECLARED\"\n```\n\
<!-- docs-script:end -->\n";
        let error = extract(&page(text)).expect_err("an undeclared expansion is refused");
        assert!(error.contains("UNDECLARED"), "{error}");
    }

    #[test]
    fn an_unclosed_or_empty_block_is_an_error() {
        let unclosed = "<!-- docs-script:begin name=\"a\" -->\n```sh\nls\n```\n";
        assert!(extract(&page(unclosed)).is_err());
        let empty = "<!-- docs-script:begin name=\"a\" -->\n<!-- docs-script:end -->\n";
        assert!(extract(&page(empty)).is_err());
    }

    #[test]
    fn expansions_cover_both_spellings() {
        assert_eq!(
            expansions("cp \"$A\" \"${B}\" $A"),
            vec!["A".to_owned(), "B".to_owned()]
        );
    }
}
