//! A deliberately small, line-oriented Markdown scanner.
//!
//! The docs checker and the security lint both need the same three structural
//! facts about a documentation page, and both need them per line so a finding can
//! name an exact location:
//!
//! * which lines are inside a fenced code block, and with what info string;
//! * which inline back-ticked spans a prose line carries;
//! * which HTML-comment annotations precede a block.
//!
//! No Markdown renderer is pulled in for this: the workspace dependency baseline
//! is exact-pinned and adding a parser for a link check would be a new shipping
//! dependency. The scanner is therefore intentionally structural rather than
//! semantic — it never tries to understand emphasis, tables or nesting.

/// One fenced code block.
#[derive(Debug, Clone)]
pub struct Fence {
    /// The info string after the opening back-ticks, trimmed (`sh`, `toml`, ...).
    pub info: String,
    /// 1-based line number of the opening fence.
    pub open_line: usize,
    /// 1-based line number of the first body line.
    pub body_start_line: usize,
    /// Body lines, without the fence markers.
    pub body: Vec<String>,
    /// HTML-comment annotations on the contiguous comment lines immediately above
    /// the opening fence, in source order.
    pub annotations: Vec<String>,
}

impl Fence {
    /// The body rejoined with `\n` and a trailing newline.
    #[must_use]
    pub fn body_text(&self) -> String {
        let mut text = self.body.join("\n");
        text.push('\n');
        text
    }

    /// The first annotation whose text starts with `prefix`.
    #[must_use]
    pub fn annotation(&self, prefix: &str) -> Option<&str> {
        self.annotations
            .iter()
            .find(|annotation| annotation.starts_with(prefix))
            .map(String::as_str)
    }
}

/// One structural view of a Markdown page.
#[derive(Debug, Clone)]
pub struct Scanned {
    /// Every line, 0-indexed here and reported 1-based.
    pub lines: Vec<String>,
    /// For each line, whether it sits inside a fenced block (fence markers count
    /// as inside, so a lint never treats a fence marker as prose).
    pub in_fence: Vec<bool>,
    /// Every fenced block, in source order.
    pub fences: Vec<Fence>,
}

/// The back-tick run length and the text after it, for a line that could open or
/// close a fence.
///
/// The run length matters: a fence closes only on a run at least as long as the one
/// that opened it, which is what lets a page show fenced Markdown inside a longer
/// fence. Without that, the inner fence would close the outer block and the page's
/// own examples would be scanned as if they were live content.
fn fence_marker(line: &str) -> Option<(usize, &str)> {
    let trimmed = line.trim_start();
    let run = trimmed.chars().take_while(|ch| *ch == '`').count();
    if run < 3 {
        return None;
    }
    Some((run, &trimmed[run..]))
}

/// Extract the payload of an HTML comment line, or `None` when the line is not a
/// single self-contained comment.
#[must_use]
pub fn html_comment(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    let inner = trimmed.strip_prefix("<!--")?.strip_suffix("-->")?;
    Some(inner.trim())
}

/// Scan one page.
#[must_use]
pub fn scan(text: &str) -> Scanned {
    let lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let mut in_fence = vec![false; lines.len()];
    let mut fences = Vec::new();
    let mut open: Option<Fence> = None;
    let mut open_run = 0usize;
    for (index, line) in lines.iter().enumerate() {
        match (&mut open, fence_marker(line)) {
            (None, Some((run, info))) => {
                let mut annotations = Vec::new();
                let mut probe = index;
                while probe > 0 {
                    probe -= 1;
                    match html_comment(&lines[probe]) {
                        Some(comment) => annotations.push(comment.to_owned()),
                        None => break,
                    }
                }
                annotations.reverse();
                in_fence[index] = true;
                open_run = run;
                open = Some(Fence {
                    info: info.trim().to_owned(),
                    open_line: index + 1,
                    body_start_line: index + 2,
                    body: Vec::new(),
                    annotations,
                });
            }
            (Some(fence), Some((run, _))) => {
                in_fence[index] = true;
                if run >= open_run {
                    if let Some(fence) = open.take() {
                        fences.push(fence);
                    }
                } else {
                    fence.body.push(line.clone());
                }
            }
            (Some(fence), None) => {
                in_fence[index] = true;
                fence.body.push(line.clone());
            }
            (None, None) => {}
        }
    }
    // An unterminated fence still yields its collected body so a finding can name
    // it rather than the scanner silently dropping content.
    if let Some(fence) = open.take() {
        fences.push(fence);
    }
    Scanned {
        lines,
        in_fence,
        fences,
    }
}

/// Every back-ticked inline span on one line, without the back-ticks.
///
/// Only single-back-tick spans are recognized, which is the only form the
/// documentation uses for a canonical name.
#[must_use]
pub fn inline_spans(line: &str) -> Vec<String> {
    let mut spans = Vec::new();
    let mut rest = line;
    while let Some(start) = rest.find('`') {
        let after = &rest[start + 1..];
        match after.find('`') {
            Some(end) => {
                spans.push(after[..end].to_owned());
                rest = &after[end + 1..];
            }
            None => break,
        }
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fences_capture_info_body_and_preceding_annotations() {
        let scanned = scan("intro\n<!-- a:b -->\n```sh\necho hi\n```\ntail\n");
        assert_eq!(scanned.fences.len(), 1);
        let fence = &scanned.fences[0];
        assert_eq!(fence.info, "sh");
        assert_eq!(fence.body, vec!["echo hi".to_owned()]);
        assert_eq!(fence.annotations, vec!["a:b".to_owned()]);
        assert_eq!(fence.open_line, 3);
        assert_eq!(fence.body_start_line, 4);
        assert!(!scanned.in_fence[0]);
        assert!(scanned.in_fence[3]);
        assert!(!scanned.in_fence[5]);
    }

    #[test]
    fn a_longer_fence_contains_a_shorter_one_instead_of_being_closed_by_it() {
        let scanned = scan("````markdown\n```sh\nls\n```\n````\nafter\n");
        assert_eq!(scanned.fences.len(), 1);
        let fence = &scanned.fences[0];
        assert_eq!(fence.info, "markdown");
        assert_eq!(
            fence.body,
            vec!["```sh".to_owned(), "ls".to_owned(), "```".to_owned()]
        );
        assert!(!scanned.in_fence[5]);
    }

    #[test]
    fn inline_spans_are_single_backtick_only() {
        assert_eq!(
            inline_spans("use `scene.tree.get` and `health.get` here"),
            vec!["scene.tree.get".to_owned(), "health.get".to_owned()]
        );
        assert!(inline_spans("no spans here").is_empty());
        assert!(inline_spans("unclosed `span").is_empty());
    }

    #[test]
    fn html_comment_requires_a_whole_line() {
        assert_eq!(html_comment("<!-- x -->"), Some("x"));
        assert_eq!(html_comment("text <!-- x -->"), None);
    }
}
