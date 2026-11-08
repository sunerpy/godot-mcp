//! Bounded content search over the project's text files.
//!
//! A query is a literal string (the default) or a regular expression in the
//! syntax of the `regex` crate. Both compile to the same linear-time matcher,
//! which never backtracks, and the compiled program's size is capped, so no
//! pattern can make a search run away. Matching is per line: a match never
//! spans a line break.
//!
//! Only files read as UTF-8 text are searched. Binary files, files larger
//! than the 8 MiB analysis bound and files the caller did not read are
//! counted as skipped, never silently passed over.

use regex::{Regex, RegexBuilder};
use serde::Serialize;

use super::glob::GlobSet;
use super::model::{Coverage, CoverageBuilder, FileContent, ProjectSources};

/// Longest pattern accepted, in characters: Unicode scalar values, as a
/// schema's `maxLength` counts them.
pub const MAX_PATTERN_LENGTH: usize = 1024;

/// Most context lines accepted on each side of a match.
pub const MAX_CONTEXT_LINES: usize = 10;

/// Most matches one search collects.
pub const MAX_MATCHES: usize = 10_000;

/// Matches collected when the caller names no bound.
pub const DEFAULT_MAX_MATCHES: usize = 1_000;

/// Longest line text returned, in characters; a longer line is cut to a
/// window around the match.
const MAX_LINE_CHARS: usize = 500;

/// Characters kept before the match when a long line is cut.
const WINDOW_LEAD: usize = 100;

/// Longest context line returned, in characters.
const MAX_CONTEXT_CHARS: usize = 200;

/// Compiled-program size bound for the matcher, in bytes.
const REGEX_SIZE_LIMIT: usize = 1 << 20;

/// Lazy-DFA cache bound for the matcher, in bytes.
const REGEX_DFA_LIMIT: usize = 1 << 21;

/// What to search for, and where.
#[derive(Debug, Clone)]
pub struct SearchQuery {
    /// The literal or regular expression.
    pub pattern: String,
    /// Whether [`Self::pattern`] is a regular expression.
    pub regex: bool,
    /// Whether letter case must match.
    pub case_sensitive: bool,
    /// Whether a match must start and end at word boundaries.
    pub whole_word: bool,
    /// Context lines returned on each side of a match.
    pub context: usize,
    /// Most matches to collect.
    pub max_matches: usize,
    /// Which files to search.
    pub selection: GlobSet,
}

/// A compiled query.
#[derive(Debug, Clone)]
pub struct Matcher(Regex);

impl Matcher {
    /// Compile `query`.
    ///
    /// # Errors
    /// Returns a message naming the problem for an empty or overlong pattern,
    /// an invalid regular expression, or one whose compiled form exceeds the
    /// size bound.
    pub fn new(query: &SearchQuery) -> Result<Self, String> {
        if query.pattern.is_empty() {
            return Err("the search pattern must not be empty".to_owned());
        }
        if query.pattern.chars().count() > MAX_PATTERN_LENGTH {
            return Err(format!(
                "the search pattern must not exceed {MAX_PATTERN_LENGTH} characters"
            ));
        }
        let body = if query.regex {
            query.pattern.clone()
        } else {
            regex::escape(&query.pattern)
        };
        let body = if query.whole_word {
            format!(r"\b(?:{body})\b")
        } else {
            body
        };
        RegexBuilder::new(&body)
            .case_insensitive(!query.case_sensitive)
            .size_limit(REGEX_SIZE_LIMIT)
            .dfa_size_limit(REGEX_DFA_LIMIT)
            .build()
            .map(Self)
            .map_err(|error| match error {
                regex::Error::CompiledTooBig(limit) => format!(
                    "the regular expression compiles to more than {limit} bytes; simplify it"
                ),
                other => format!("the regular expression is invalid: {other}"),
            })
    }
}

/// One line shown around a match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContextLine {
    /// 1-based line number.
    pub line: u32,
    /// The line (its first 200 characters).
    pub text: String,
    /// Whether the line was longer than what is shown.
    pub truncated: bool,
}

/// One match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SearchMatch {
    /// Project-relative path.
    pub path: String,
    /// 1-based line.
    pub line: u32,
    /// 1-based column of the match's first character.
    pub column: u32,
    /// Length of the match in characters.
    pub length: u32,
    /// The line, or a 500-character window of it around the match.
    pub text: String,
    /// 1-based column of [`Self::text`]'s first character.
    pub text_column: u32,
    /// Whether [`Self::text`] is a window of a longer line.
    pub truncated: bool,
    /// Lines before the match, nearest last.
    pub before: Vec<ContextLine>,
    /// Lines after the match, nearest first.
    pub after: Vec<ContextLine>,
}

/// Files a search did not look into.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct SkippedFiles {
    /// Files outside the include/exclude selection.
    pub excluded: usize,
    /// Binary or non-UTF-8 files.
    pub binary: usize,
    /// Files over the 8 MiB analysis bound.
    pub too_large: usize,
    /// Files the caller did not read.
    pub not_read: usize,
    /// Selected text files left unsearched after the match bound was reached.
    pub after_limit: usize,
}

/// The content-search view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SearchReport {
    /// Every collected match, ordered by path, line and column.
    pub matches: Vec<SearchMatch>,
    /// Text files searched.
    pub files_searched: usize,
    /// Searched files with at least one match.
    pub files_matched: usize,
    /// Whether the search stopped at [`Self::max_matches`]; more matches may
    /// exist.
    pub match_limit_reached: bool,
    /// The match bound that applied.
    pub max_matches: usize,
    /// Files not searched, by reason.
    pub skipped: SkippedFiles,
    /// What the search could not see.
    pub coverage: Coverage,
}

/// Search every selected text file. `cancelled` is polled between files; when
/// it returns `true` the search stops and `None` is returned.
#[must_use]
pub fn search(
    sources: &ProjectSources,
    query: &SearchQuery,
    matcher: &Matcher,
    cancelled: &dyn Fn() -> bool,
) -> Option<SearchReport> {
    let mut matches = Vec::new();
    let mut files_searched = 0;
    let mut files_matched = 0;
    let mut skipped = SkippedFiles::default();
    let mut coverage = CoverageBuilder::default();
    let mut limit_reached = false;
    for (path, file) in sources.files() {
        if cancelled() {
            return None;
        }
        if !query.selection.selects(path) {
            skipped.excluded += 1;
            continue;
        }
        let text = match file.content() {
            FileContent::Text(text) => text,
            FileContent::Binary => {
                skipped.binary += 1;
                continue;
            }
            FileContent::TooLarge => {
                skipped.too_large += 1;
                coverage.note(
                    "files_too_large",
                    "these files exceed the 8 MiB analysis bound and were not searched",
                    path.to_owned(),
                );
                continue;
            }
            FileContent::Unread(_) => {
                skipped.not_read += 1;
                coverage.note(
                    "files_not_read",
                    "these files were not read and were not searched",
                    path.to_owned(),
                );
                continue;
            }
        };
        if limit_reached {
            skipped.after_limit += 1;
            continue;
        }
        files_searched += 1;
        let lines: Vec<&str> = text.lines().collect();
        let mut matched = false;
        'lines: for (index, line) in lines.iter().enumerate() {
            for found in matcher.0.find_iter(line) {
                if matches.len() >= query.max_matches {
                    limit_reached = true;
                    break 'lines;
                }
                matched = true;
                matches.push(build_match(
                    path,
                    &lines,
                    index,
                    line,
                    found.start(),
                    found.end(),
                    query.context,
                ));
            }
        }
        if matched {
            files_matched += 1;
        }
    }
    if limit_reached {
        coverage.note(
            "match_limit_reached",
            "the search stopped at its match bound; raise max_matches or narrow the query to see the rest",
            format!("{} matches", query.max_matches),
        );
    }
    Some(SearchReport {
        matches,
        files_searched,
        files_matched,
        match_limit_reached: limit_reached,
        max_matches: query.max_matches,
        skipped,
        coverage: coverage.finish(),
    })
}

fn build_match(
    path: &str,
    lines: &[&str],
    index: usize,
    line: &str,
    start: usize,
    end: usize,
    context: usize,
) -> SearchMatch {
    let column = line[..start].chars().count();
    let length = line[start..end].chars().count();
    let total = line.chars().count();
    let (text, text_column, truncated) = if total <= MAX_LINE_CHARS {
        (line.to_owned(), 0, false)
    } else {
        let window_start = column
            .saturating_sub(WINDOW_LEAD)
            .min(total.saturating_sub(MAX_LINE_CHARS));
        (
            line.chars()
                .skip(window_start)
                .take(MAX_LINE_CHARS)
                .collect(),
            window_start,
            true,
        )
    };
    let context_line = |position: usize| {
        let text = lines[position];
        let truncated = text.chars().count() > MAX_CONTEXT_CHARS;
        ContextLine {
            line: clamp(position + 1),
            text: text.chars().take(MAX_CONTEXT_CHARS).collect(),
            truncated,
        }
    };
    let before = (index.saturating_sub(context)..index)
        .map(context_line)
        .collect();
    let after = (index + 1..lines.len().min(index + 1 + context))
        .map(context_line)
        .collect();
    SearchMatch {
        path: path.to_owned(),
        line: clamp(index + 1),
        column: clamp(column + 1),
        length: clamp(length),
        text,
        text_column: clamp(text_column + 1),
        truncated,
        before,
        after,
    }
}

fn clamp(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(pattern: &str) -> SearchQuery {
        SearchQuery {
            pattern: pattern.to_owned(),
            regex: false,
            case_sensitive: false,
            whole_word: false,
            context: 0,
            max_matches: DEFAULT_MAX_MATCHES,
            selection: GlobSet::default(),
        }
    }

    fn sources() -> ProjectSources {
        let mut sources = ProjectSources::new();
        sources.insert_text(
            "a.gd",
            "var Speed = 1\nfunc speedy():\n\tspeed = 2 # speed\n",
        );
        sources.insert_text("b/c.tscn", "[node name=\"Speed\"]\n");
        sources.insert_bytes("d.png", b"\x89PNG\0speed");
        sources.insert_unread("e.txt", "budget");
        sources
    }

    fn run(query: &SearchQuery) -> SearchReport {
        let matcher = Matcher::new(query).expect("compiles");
        search(&sources(), query, &matcher, &|| false).expect("not cancelled")
    }

    #[test]
    fn literal_search_is_case_insensitive_by_default() {
        let report = run(&query("speed"));
        let found: Vec<(&str, u32, u32)> = report
            .matches
            .iter()
            .map(|found| (found.path.as_str(), found.line, found.column))
            .collect();
        assert_eq!(
            found,
            vec![
                ("a.gd", 1, 5),
                ("a.gd", 2, 6),
                ("a.gd", 3, 2),
                ("a.gd", 3, 14),
                ("b/c.tscn", 1, 13)
            ]
        );
        assert_eq!(report.files_searched, 2);
        assert_eq!(report.files_matched, 2);
        assert_eq!(report.skipped.binary, 1);
        assert_eq!(report.skipped.not_read, 1);
        assert!(!report.coverage.complete);
    }

    #[test]
    fn case_whole_word_regex_and_selection_narrow_the_search() {
        let mut exact = query("Speed");
        exact.case_sensitive = true;
        assert_eq!(run(&exact).matches.len(), 2);
        let mut word = query("speed");
        word.whole_word = true;
        assert_eq!(run(&word).matches.len(), 4);
        let mut pattern = query(r"speed\w+");
        pattern.regex = true;
        let report = run(&pattern);
        assert_eq!(report.matches.len(), 1);
        assert_eq!(report.matches[0].length, 6);
        let mut scoped = query("speed");
        scoped.selection = GlobSet::new(["*.tscn"], Vec::<String>::new()).expect("globs");
        let report = run(&scoped);
        assert_eq!(report.matches.len(), 1);
        assert_eq!(report.skipped.excluded, 3);
    }

    #[test]
    fn the_match_bound_and_context_are_honoured() {
        let mut bounded = query("speed");
        bounded.max_matches = 2;
        bounded.context = 1;
        let report = run(&bounded);
        assert_eq!(report.matches.len(), 2);
        assert!(report.match_limit_reached);
        assert_eq!(report.skipped.after_limit, 1);
        assert_eq!(report.matches[1].before[0].line, 1);
        assert_eq!(report.matches[1].after[0].line, 3);
    }

    #[test]
    fn long_lines_are_cut_to_a_window_around_the_match() {
        let mut sources = ProjectSources::new();
        let line = format!("{}needle{}", "x".repeat(900), "y".repeat(900));
        sources.insert_text("long.txt", &line);
        let query = query("needle");
        let matcher = Matcher::new(&query).expect("compiles");
        let report = search(&sources, &query, &matcher, &|| false).expect("searched");
        let found = &report.matches[0];
        assert!(found.truncated);
        assert_eq!(found.column, 901);
        assert_eq!(found.text_column, 801);
        assert_eq!(found.text.chars().count(), 500);
        assert!(found.text.contains("needle"));
    }

    #[test]
    fn bad_patterns_are_refused() {
        assert!(Matcher::new(&query("")).is_err());
        let mut invalid = query("(");
        invalid.regex = true;
        assert!(Matcher::new(&invalid).is_err());
        let mut huge = query(r"\w{1000}\w{1000}\w{1000}");
        huge.regex = true;
        assert!(Matcher::new(&huge).is_err());
        assert!(Matcher::new(&query(&"a".repeat(MAX_PATTERN_LENGTH + 1))).is_err());
        let cancelled = search(
            &sources(),
            &query("a"),
            &Matcher::new(&query("a")).expect("ok"),
            &|| true,
        );
        assert!(cancelled.is_none());
    }
}
