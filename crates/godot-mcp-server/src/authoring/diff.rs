//! Bounded unified diffs for authoring results and dry runs.
//!
//! The diff is a reviewing aid, not the change itself: the change is the exact
//! bytes whose SHA-256 the result reports. It is therefore bounded twice — the
//! line comparison runs only over the region between the common prefix and
//! suffix, and only while that region is small enough to compare exactly, and
//! the rendered text stops at a fixed number of lines and bytes, saying so.

use serde_json::{Value, json};

/// Context lines around each change.
const CONTEXT: usize = 3;
/// Largest middle region compared line by line (old lines × new lines).
const MAX_COMPARED_CELLS: usize = 4_000_000;
/// Most rendered diff lines.
const MAX_RENDERED_LINES: usize = 400;
/// Most rendered diff bytes.
const MAX_RENDERED_BYTES: usize = 32 * 1024;

/// One rendered, bounded diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BoundedDiff {
    pub(super) text: String,
    pub(super) added: usize,
    pub(super) removed: usize,
    pub(super) truncated: bool,
}

impl BoundedDiff {
    pub(super) fn to_value(&self) -> Value {
        json!({
            "format": "unified",
            "text": self.text,
            "added_lines": self.added,
            "removed_lines": self.removed,
            "truncated": self.truncated,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Keep,
    Remove,
    Add,
}

/// Render a unified diff from `before` to `after`. `None` stands for an absent
/// file on that side.
pub(super) fn unified(
    old_path: Option<&str>,
    new_path: Option<&str>,
    before: &str,
    after: &str,
) -> BoundedDiff {
    let old: Vec<&str> = before.split_inclusive('\n').collect();
    let new: Vec<&str> = after.split_inclusive('\n').collect();
    let script = edit_script(&old, &new);
    let added = script.iter().filter(|(op, _, _)| *op == Op::Add).count();
    let removed = script.iter().filter(|(op, _, _)| *op == Op::Remove).count();

    let mut rendered = Renderer::default();
    rendered.line(&format!(
        "--- {}",
        old_path.map_or_else(|| "/dev/null".to_owned(), |path| format!("a/{path}"))
    ));
    rendered.line(&format!(
        "+++ {}",
        new_path.map_or_else(|| "/dev/null".to_owned(), |path| format!("b/{path}"))
    ));
    for hunk in hunks(&script) {
        let (old_start, old_count, new_start, new_count) = hunk_range(&script[hunk.clone()]);
        rendered.line(&format!(
            "@@ -{old_start},{old_count} +{new_start},{new_count} @@"
        ));
        for (op, old_index, new_index) in &script[hunk] {
            let (marker, text) = match op {
                Op::Keep => (' ', old[*old_index]),
                Op::Remove => ('-', old[*old_index]),
                Op::Add => ('+', new[*new_index]),
            };
            let body = text.strip_suffix('\n').unwrap_or(text);
            let body = body.strip_suffix('\r').unwrap_or(body);
            rendered.line(&format!("{marker}{body}"));
            if !text.ends_with('\n') {
                rendered.line("\\ No newline at end of file");
            }
        }
    }
    BoundedDiff {
        text: rendered.text,
        added,
        removed,
        truncated: rendered.truncated,
    }
}

#[derive(Default)]
struct Renderer {
    text: String,
    lines: usize,
    truncated: bool,
}

impl Renderer {
    fn line(&mut self, line: &str) {
        if self.truncated {
            return;
        }
        if self.lines >= MAX_RENDERED_LINES || self.text.len() + line.len() + 1 > MAX_RENDERED_BYTES
        {
            self.truncated = true;
            return;
        }
        self.text.push_str(line);
        self.text.push('\n');
        self.lines += 1;
    }
}

/// The edit script as `(op, old index, new index)` triples in order.
fn edit_script(old: &[&str], new: &[&str]) -> Vec<(Op, usize, usize)> {
    let prefix = old
        .iter()
        .zip(new.iter())
        .take_while(|(left, right)| left == right)
        .count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(left, right)| left == right)
        .count();
    let old_middle = &old[prefix..old.len() - suffix];
    let new_middle = &new[prefix..new.len() - suffix];

    let mut script: Vec<(Op, usize, usize)> = (0..prefix).map(|i| (Op::Keep, i, i)).collect();
    if old_middle
        .len()
        .saturating_mul(new_middle.len())
        .le(&MAX_COMPARED_CELLS)
    {
        script.extend(
            lcs_script(old_middle, new_middle)
                .into_iter()
                .map(|(op, i, j)| (op, i + prefix, j + prefix)),
        );
    } else {
        // Too large to compare exactly: the middle is shown as replaced.
        script.extend((0..old_middle.len()).map(|i| (Op::Remove, i + prefix, prefix)));
        script.extend((0..new_middle.len()).map(|j| (Op::Add, prefix, j + prefix)));
    }
    let old_tail = old.len() - suffix;
    let new_tail = new.len() - suffix;
    script.extend((0..suffix).map(|k| (Op::Keep, old_tail + k, new_tail + k)));
    script
}

fn lcs_script(old: &[&str], new: &[&str]) -> Vec<(Op, usize, usize)> {
    let width = new.len() + 1;
    let mut table = vec![0_u32; (old.len() + 1) * width];
    for i in (0..old.len()).rev() {
        for j in (0..new.len()).rev() {
            table[i * width + j] = if old[i] == new[j] {
                table[(i + 1) * width + j + 1] + 1
            } else {
                table[(i + 1) * width + j].max(table[i * width + j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut script = Vec::with_capacity(old.len() + new.len());
    while i < old.len() || j < new.len() {
        if i < old.len() && j < new.len() && old[i] == new[j] {
            script.push((Op::Keep, i, j));
            i += 1;
            j += 1;
        } else if i < old.len()
            && (j == new.len() || table[(i + 1) * width + j] >= table[i * width + j + 1])
        {
            // Removals come before additions, as unified diffs show them.
            script.push((Op::Remove, i, j));
            i += 1;
        } else {
            script.push((Op::Add, i, j));
            j += 1;
        }
    }
    script
}

/// Group changed script entries with their context into hunk ranges.
fn hunks(script: &[(Op, usize, usize)]) -> Vec<std::ops::Range<usize>> {
    let changes: Vec<usize> = script
        .iter()
        .enumerate()
        .filter(|(_, (op, _, _))| *op != Op::Keep)
        .map(|(index, _)| index)
        .collect();
    let mut ranges: Vec<std::ops::Range<usize>> = Vec::new();
    for index in changes {
        let start = index.saturating_sub(CONTEXT);
        let end = (index + CONTEXT + 1).min(script.len());
        match ranges.last_mut() {
            Some(last) if start <= last.end => last.end = last.end.max(end),
            _ => ranges.push(start..end),
        }
    }
    ranges
}

/// The `@@` header numbers of one hunk.
fn hunk_range(hunk: &[(Op, usize, usize)]) -> (usize, usize, usize, usize) {
    let old_count = hunk.iter().filter(|(op, _, _)| *op != Op::Add).count();
    let new_count = hunk.iter().filter(|(op, _, _)| *op != Op::Remove).count();
    let (_, first_old, first_new) = hunk[0];
    // A zero-length side is numbered by the line *before* it, as unified diffs
    // do; otherwise by its first line, 1-based.
    let old_start = if old_count == 0 {
        first_old
    } else {
        first_old + 1
    };
    let new_start = if new_count == 0 {
        first_new
    } else {
        first_new + 1
    };
    (old_start, old_count, new_start, new_count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_one_line_change_renders_one_hunk_with_context() {
        let before = "a\nb\nc\nd\ne\nf\ng\nh\n";
        let after = "a\nb\nc\nd\nE\nf\ng\nh\n";
        let diff = unified(Some("x.txt"), Some("x.txt"), before, after);
        assert_eq!(
            diff.text,
            "--- a/x.txt\n+++ b/x.txt\n@@ -2,7 +2,7 @@\n b\n c\n d\n-e\n+E\n f\n g\n h\n"
        );
        assert_eq!((diff.added, diff.removed, diff.truncated), (1, 1, false));
    }

    #[test]
    fn a_created_file_diffs_against_dev_null() {
        let diff = unified(None, Some("new.gd"), "", "extends Node\n");
        assert_eq!(
            diff.text,
            "--- /dev/null\n+++ b/new.gd\n@@ -0,0 +1,1 @@\n+extends Node\n"
        );
    }

    #[test]
    fn a_missing_final_newline_is_marked() {
        let diff = unified(Some("x"), Some("x"), "a\n", "a\nb");
        assert!(
            diff.text.ends_with("+b\n\\ No newline at end of file\n"),
            "{}",
            diff.text
        );
    }

    #[test]
    fn output_is_bounded_and_says_so() {
        let after: String = (0..2_000).map(|n| format!("line {n}\n")).collect();
        let diff = unified(None, Some("big.txt"), "", &after);
        assert!(diff.truncated);
        assert_eq!(diff.added, 2_000);
        assert!(diff.text.lines().count() <= MAX_RENDERED_LINES);
    }
}
