//! Path globs for selecting project files, and Godot's own wildcard match.
//!
//! Two matchers live here because two different contracts need them:
//!
//! * [`PathGlob`] is the selection language of this server's tools
//!   (`project.files.list`, `project.search`). It has path semantics: `*` and
//!   `?` never cross a `/`, `**` does, and a pattern without a `/` matches the
//!   file name at any depth, the way a `.gitignore` line does.
//! * [`godot_wildcard_match`] reproduces Godot's `String::match` /
//!   `String::matchn`, which an export preset's `include_filter` is evaluated
//!   with: `*` matches any run of characters, `/` included, and `?` matches
//!   one character that is not a `.`. Using the path glob there would mispredict
//!   which files an export bundles.
//!
//! Both are linear in the input: [`PathGlob`] compiles to a [`regex::Regex`],
//! whose matching never backtracks, and the wildcard match is the classic
//! two-pointer algorithm, bounded by the product of the two lengths.

use regex::Regex;

/// Longest glob a caller may supply, in characters: Unicode scalar values,
/// as a schema's `maxLength` counts them.
pub const MAX_GLOB_LENGTH: usize = 1024;

/// A rejected glob, with a message a caller can act on.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct GlobError {
    message: String,
}

impl GlobError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// Human-readable explanation.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// One compiled path glob.
///
/// | Syntax | Matches |
/// | --- | --- |
/// | `*` | any run of characters except `/` |
/// | `**` | any run of characters, `/` included (`**/` also matches nothing) |
/// | `?` | one character except `/` |
/// | `[abc]`, `[a-z]`, `[!a]` | one character of (or not of) the class |
/// | `{a,b}` | either alternative; alternatives do not nest |
///
/// A pattern containing no `/` is matched against the file name at any depth;
/// a pattern containing one is matched against the whole project-relative path
/// (a leading `/` or `res://` is ignored). Matching is case-sensitive, as
/// Godot's own resource paths are.
#[derive(Debug, Clone)]
pub struct PathGlob {
    source: String,
    regex: Regex,
    basename_only: bool,
}

impl PathGlob {
    /// Compile one glob.
    ///
    /// # Errors
    /// Returns [`GlobError`] for an empty or overlong pattern, an unterminated
    /// class or alternation, a nested alternation, or a stray `}`.
    pub fn new(pattern: &str) -> Result<Self, GlobError> {
        let trimmed = pattern.trim();
        if trimmed.is_empty() {
            return Err(GlobError::new("a glob must not be empty"));
        }
        if trimmed.chars().count() > MAX_GLOB_LENGTH {
            return Err(GlobError::new(format!(
                "a glob must not exceed {MAX_GLOB_LENGTH} characters"
            )));
        }
        let unprefixed = trimmed.strip_prefix("res://").unwrap_or(trimmed);
        let body = unprefixed.trim_start_matches('/');
        if body.is_empty() {
            return Err(GlobError::new(
                "a glob must name something below the project root",
            ));
        }
        let basename_only = !body.contains('/');
        let translated = translate(body)?;
        let regex = Regex::new(&format!("^{translated}$"))
            .map_err(|error| GlobError::new(format!("the glob cannot be compiled: {error}")))?;
        Ok(Self {
            source: trimmed.to_owned(),
            regex,
            basename_only,
        })
    }

    /// The pattern exactly as supplied (trimmed).
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.source
    }

    /// Whether `path`, a project-relative path with `/` separators, matches.
    #[must_use]
    pub fn matches(&self, path: &str) -> bool {
        let path = path.strip_prefix("res://").unwrap_or(path);
        if self.basename_only {
            let name = path.rsplit('/').next().unwrap_or(path);
            self.regex.is_match(name)
        } else {
            self.regex.is_match(path)
        }
    }
}

/// A set of include and exclude globs.
///
/// A path is selected when it matches at least one include (or there are no
/// includes) and no exclude.
#[derive(Debug, Clone, Default)]
pub struct GlobSet {
    include: Vec<PathGlob>,
    exclude: Vec<PathGlob>,
}

impl GlobSet {
    /// Compile both lists.
    ///
    /// # Errors
    /// Returns the first [`GlobError`], naming the offending pattern.
    pub fn new<I, E, S, T>(include: I, exclude: E) -> Result<Self, GlobError>
    where
        I: IntoIterator<Item = S>,
        E: IntoIterator<Item = T>,
        S: AsRef<str>,
        T: AsRef<str>,
    {
        let compile = |pattern: &str| {
            PathGlob::new(pattern)
                .map_err(|error| GlobError::new(format!("`{pattern}`: {}", error.message())))
        };
        Ok(Self {
            include: include
                .into_iter()
                .map(|pattern| compile(pattern.as_ref()))
                .collect::<Result<_, _>>()?,
            exclude: exclude
                .into_iter()
                .map(|pattern| compile(pattern.as_ref()))
                .collect::<Result<_, _>>()?,
        })
    }

    /// Whether `path` is selected.
    #[must_use]
    pub fn selects(&self, path: &str) -> bool {
        (self.include.is_empty() || self.include.iter().any(|glob| glob.matches(path)))
            && !self.exclude.iter().any(|glob| glob.matches(path))
    }

    /// Whether the set selects everything (no include and no exclude).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.include.is_empty() && self.exclude.is_empty()
    }
}

fn translate(body: &str) -> Result<String, GlobError> {
    let chars: Vec<char> = body.chars().collect();
    let mut out = String::with_capacity(body.len() * 2);
    let mut index = 0;
    let mut in_alternation = false;
    while index < chars.len() {
        let current = chars[index];
        match current {
            '*' => {
                if chars.get(index + 1) == Some(&'*') {
                    // `**/` also matches zero directories; a bare `**` any run.
                    if chars.get(index + 2) == Some(&'/') {
                        out.push_str("(?:.*/)?");
                        index += 3;
                    } else {
                        out.push_str(".*");
                        index += 2;
                    }
                } else {
                    out.push_str("[^/]*");
                    index += 1;
                }
            }
            '?' => {
                out.push_str("[^/]");
                index += 1;
            }
            '[' => {
                let (class, next) = translate_class(&chars, index)?;
                out.push_str(&class);
                index = next;
            }
            '{' => {
                if in_alternation {
                    return Err(GlobError::new("alternations `{...}` must not nest"));
                }
                in_alternation = true;
                out.push_str("(?:");
                index += 1;
            }
            ',' if in_alternation => {
                out.push('|');
                index += 1;
            }
            '}' => {
                if !in_alternation {
                    return Err(GlobError::new("`}` closes no alternation"));
                }
                in_alternation = false;
                out.push(')');
                index += 1;
            }
            '\\' => {
                // `\x` escapes the next character literally.
                let Some(next) = chars.get(index + 1) else {
                    return Err(GlobError::new("a glob must not end with `\\`"));
                };
                out.push_str(&regex::escape(&next.to_string()));
                index += 2;
            }
            other => {
                out.push_str(&regex::escape(&other.to_string()));
                index += 1;
            }
        }
    }
    if in_alternation {
        return Err(GlobError::new("an alternation `{...}` is never closed"));
    }
    Ok(out)
}

fn translate_class(chars: &[char], start: usize) -> Result<(String, usize), GlobError> {
    let mut index = start + 1;
    let mut out = String::from("[");
    if matches!(chars.get(index), Some('!' | '^')) {
        out.push('^');
        index += 1;
    }
    let mut members = 0usize;
    while let Some(&current) = chars.get(index) {
        if current == ']' && members > 0 {
            // A class never matches the separator, whatever it lists.
            out.push_str("&&[^/]]");
            return Ok((out, index + 1));
        }
        match current {
            '\\' | '[' | ']' | '&' | '~' | '^' => {
                out.push('\\');
                out.push(current);
            }
            '-' => out.push('-'),
            other => out.push(other),
        }
        members += 1;
        index += 1;
    }
    Err(GlobError::new("a character class `[...]` is never closed"))
}

/// Godot's `String::match` (case-sensitive) or `String::matchn`
/// (case-insensitive): `*` matches any run of characters, `/` included, and
/// `?` matches exactly one character that is not `.`.
#[must_use]
pub fn godot_wildcard_match(pattern: &str, text: &str, case_sensitive: bool) -> bool {
    let fold = |c: char| {
        if case_sensitive {
            c
        } else {
            c.to_uppercase().next().unwrap_or(c)
        }
    };
    let pattern: Vec<char> = pattern.chars().map(fold).collect();
    let text: Vec<char> = text.chars().map(fold).collect();
    let (mut p, mut t) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        match pattern.get(p) {
            Some('*') => {
                star = Some((p, t));
                p += 1;
            }
            Some('?') if text[t] != '.' => {
                p += 1;
                t += 1;
            }
            Some(&literal) if literal != '?' && literal == text[t] => {
                p += 1;
                t += 1;
            }
            _ => match star {
                Some((star_p, star_t)) => {
                    p = star_p + 1;
                    t = star_t + 1;
                    star = Some((star_p, star_t + 1));
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slashless_glob_matches_the_file_name_at_any_depth() {
        let glob = PathGlob::new("*.tscn").expect("glob");
        assert!(glob.matches("main.tscn"));
        assert!(glob.matches("scenes/levels/one.tscn"));
        assert!(!glob.matches("scenes/one.tres"));
    }

    #[test]
    fn a_glob_with_a_slash_matches_the_whole_path() {
        let glob = PathGlob::new("scenes/*.tscn").expect("glob");
        assert!(glob.matches("scenes/main.tscn"));
        assert!(!glob.matches("scenes/levels/one.tscn"));
        assert!(!glob.matches("other/scenes/main.tscn"));
        let deep = PathGlob::new("res://scenes/**/*.tscn").expect("glob");
        assert!(deep.matches("scenes/main.tscn"));
        assert!(deep.matches("scenes/levels/one.tscn"));
    }

    #[test]
    fn classes_alternations_and_escapes_translate() {
        let glob = PathGlob::new("*.{gd,tscn}").expect("glob");
        assert!(glob.matches("a/b.gd") && glob.matches("b.tscn") && !glob.matches("b.tres"));
        let class = PathGlob::new("level_[0-9].tscn").expect("glob");
        assert!(class.matches("level_3.tscn") && !class.matches("level_x.tscn"));
        let negated = PathGlob::new("[!a]*.gd").expect("glob");
        assert!(negated.matches("b.gd") && !negated.matches("a.gd"));
        let literal = PathGlob::new("a\\*b").expect("glob");
        assert!(literal.matches("a*b") && !literal.matches("axb"));
        let dot = PathGlob::new("a.b").expect("glob");
        assert!(
            !dot.matches("aXb"),
            "a dot is literal, not a regex wildcard"
        );
    }

    #[test]
    fn malformed_globs_are_refused() {
        for bad in ["", "   ", "{a,b", "a}", "{a,{b}}", "[abc", "a\\", "res://"] {
            assert!(PathGlob::new(bad).is_err(), "{bad:?} must be refused");
        }
        assert!(PathGlob::new(&"a".repeat(MAX_GLOB_LENGTH + 1)).is_err());
    }

    #[test]
    fn a_glob_set_needs_one_include_and_no_exclude() {
        let set = GlobSet::new(["*.gd"], ["addons/**"]).expect("set");
        assert!(set.selects("scripts/player.gd"));
        assert!(!set.selects("addons/tool/plugin.gd"));
        assert!(!set.selects("scenes/main.tscn"));
        let everything = GlobSet::new(Vec::<String>::new(), Vec::<String>::new()).expect("set");
        assert!(everything.is_empty() && everything.selects("anything"));
    }

    #[test]
    fn godot_wildcard_star_crosses_separators_and_question_mark_skips_dots() {
        assert!(godot_wildcard_match(
            "*.json",
            "res://data/levels.json",
            false
        ));
        assert!(godot_wildcard_match("data/*", "data/a/b.txt", true));
        assert!(godot_wildcard_match("*.JSON", "res://x.json", false));
        assert!(!godot_wildcard_match("*.JSON", "res://x.json", true));
        assert!(godot_wildcard_match("file?.txt", "file1.txt", true));
        assert!(!godot_wildcard_match("file?txt", "file.txt", true));
        assert!(!godot_wildcard_match("*.json", "res://x.jsonx", true));
        assert!(godot_wildcard_match("**", "", true));
    }
}
