//! `godot-mcp skill install|update|uninstall|status`: the Agent Skills this
//! binary carries, written into the skill directories of the agents in use.
//!
//! Each skill is one `SKILL.md`, embedded at build time from the repository's
//! `skills/<name>/SKILL.md`, so what is installed always matches this binary.
//! Beside each installed `SKILL.md` godot-mcp keeps a marker,
//! `.godot-mcp-skill.json`, recording `{sha256, version, installed_at}`: the
//! SHA-256 of the bytes it wrote, the package version and the Unix time. Only
//! the SHA-256 takes part in a decision.
//!
//! `install` and `update` decide per skill:
//!
//! 1. not installed: write it;
//! 2. installed and `--force`: overwrite it;
//! 3. installed content equals the embedded content: write no `SKILL.md`, and
//!    rewrite the marker when it is missing or names another SHA-256;
//! 4. content differs and the marker names the installed content's SHA-256, so
//!    godot-mcp wrote it and it is an older version: update it;
//! 5. content differs and the marker names another SHA-256, so someone changed
//!    it: skip it, and say `--force` overwrites it;
//! 6. content differs and there is no usable marker: skip it as in 5.
//!
//! A skill whose files cannot be read, or whose `SKILL.md` is not a regular
//! file, is an error and is left alone even with `--force`; see [`files`] for
//! the read rules. `uninstall` removes only a `SKILL.md` godot-mcp wrote — one
//! equal to the embedded content or to the marker's SHA-256 — with its marker
//! and the skill directory once empty, and keeps a changed one. `status`
//! reports each skill and changes nothing.
//!
//! Every command processes all of its targets and skills, then reports. The
//! exit status is non-zero exactly when an error occurred; a skipped or kept
//! file is reported but is not an error.

mod files;
pub mod targets;

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub use files::{MARKER_FILE, SKILL_FILE, sha256_hex};
pub use targets::{
    Location, ResolvedTargets, Target, TargetSelection, parse_target_selection, resolve,
};

use files::{Installed, Leftover, Marker, SkillState};

/// One skill this binary carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmbeddedSkill {
    /// The skill's name, which is also its directory name.
    pub name: &'static str,
    /// The exact `SKILL.md` text.
    pub content: &'static str,
}

/// Every skill this binary carries, in the order commands process them. A test
/// holds this list to the repository's `skills/` directory.
pub const SKILLS: &[EmbeddedSkill] = &[
    EmbeddedSkill {
        name: "godot-mcp",
        content: include_str!("../../../../skills/godot-mcp/SKILL.md"),
    },
    EmbeddedSkill {
        name: "godot-mcp-playtest",
        content: include_str!("../../../../skills/godot-mcp-playtest/SKILL.md"),
    },
    EmbeddedSkill {
        name: "godot-mcp-setup",
        content: include_str!("../../../../skills/godot-mcp-setup/SKILL.md"),
    },
];

/// The package version a marker records.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The directories the commands resolve paths against, passed in rather than
/// read from the process so a caller can point every one of them elsewhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillContext {
    /// The user's home directory.
    pub home: PathBuf,
    /// The current directory, which `--local` writes under.
    pub cwd: PathBuf,
    /// `CLAUDE_CONFIG_DIR`, which replaces `~/.claude` for Claude Code.
    pub claude_config_dir: Option<PathBuf>,
    /// `CODEX_HOME`, which replaces `~/.codex` for Codex.
    pub codex_home: Option<PathBuf>,
}

impl SkillContext {
    /// Read the context from this process: `HOME` (or `USERPROFILE`),
    /// `CLAUDE_CONFIG_DIR`, `CODEX_HOME` and the current directory.
    ///
    /// # Errors
    /// Returns a message when the current directory or the home directory
    /// cannot be determined.
    pub fn from_env() -> Result<Self, String> {
        let cwd = std::env::current_dir()
            .map_err(|error| format!("cannot read the current directory: {error}"))?;
        Self::from_variables(cwd, |name| std::env::var_os(name))
    }

    /// Build the context from `cwd` and a variable lookup. A variable that is
    /// unset or blank counts as unset.
    ///
    /// # Errors
    /// Returns a message when neither `HOME` nor `USERPROFILE` is set.
    pub fn from_variables(
        cwd: PathBuf,
        variable: impl Fn(&str) -> Option<OsString>,
    ) -> Result<Self, String> {
        let set = |name: &str| {
            variable(name)
                .filter(|value| !value.to_string_lossy().trim().is_empty())
                .map(PathBuf::from)
        };
        let home = set("HOME")
            .or_else(|| set("USERPROFILE"))
            .ok_or_else(|| "neither HOME nor USERPROFILE is set".to_owned())?;
        Ok(Self {
            home,
            cwd,
            claude_config_dir: set("CLAUDE_CONFIG_DIR"),
            codex_home: set("CODEX_HOME"),
        })
    }

    fn against_cwd(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd.join(path)
        }
    }

    /// `CLAUDE_CONFIG_DIR`, a relative value taken from the current directory.
    pub(crate) fn claude_config_dir(&self) -> Option<PathBuf> {
        self.claude_config_dir
            .as_deref()
            .map(|path| self.against_cwd(path))
    }

    /// `CODEX_HOME`, a relative value taken from the current directory.
    pub(crate) fn codex_home(&self) -> Option<PathBuf> {
        self.codex_home
            .as_deref()
            .map(|path| self.against_cwd(path))
    }
}

/// The four commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// `skill install`.
    Install,
    /// `skill update`.
    Update,
    /// `skill uninstall`.
    Uninstall,
    /// `skill status`.
    Status,
}

impl Action {
    const fn name(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Update => "update",
            Self::Uninstall => "uninstall",
            Self::Status => "status",
        }
    }
}

/// One command and its flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// What to do.
    pub action: Action,
    /// Which agents.
    pub targets: TargetSelection,
    /// The user's directories or the current directory's.
    pub location: Location,
    /// Overwrite a changed or unrecognised `SKILL.md` (`install`, `update`).
    pub force: bool,
    /// Report what `update` would do and write nothing.
    pub dry_run: bool,
    /// Print the diff from each installed `SKILL.md` to the embedded one.
    pub diff: bool,
}

impl Request {
    /// A request with every flag off.
    #[must_use]
    pub fn new(action: Action, targets: TargetSelection, location: Location) -> Self {
        Self {
            action,
            targets,
            location,
            force: false,
            dry_run: false,
            diff: false,
        }
    }
}

/// What one command did, counted per skill and target.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    /// `SKILL.md` files written, or that a dry run would write.
    pub written: usize,
    /// Skills already up to date.
    pub unchanged: usize,
    /// Skills left alone because their `SKILL.md` was changed or unrecognised.
    pub skipped: usize,
    /// Skills `uninstall` removed.
    pub removed: usize,
    /// Changed `SKILL.md` files `uninstall` kept.
    pub kept: usize,
    /// Skills that were not installed.
    pub absent: usize,
    /// Skills `status` found at an older version godot-mcp wrote.
    pub outdated: usize,
    /// Skills `status` found changed, or not recognisably written by godot-mcp.
    pub modified: usize,
    /// Skills that could not be processed.
    pub errors: usize,
}

impl Summary {
    /// The process exit status: 1 when an error occurred, otherwise 0.
    #[must_use]
    pub const fn exit_status(&self) -> u8 {
        if self.errors > 0 { 1 } else { 0 }
    }
}

/// What `install` and `update` do with one skill.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Decision {
    /// Branch 1: not installed.
    Install,
    /// Branch 2: installed, and `--force`.
    Overwrite,
    /// Branch 3: up to date; the marker is rewritten when this names why.
    Current { marker_rewrite: Option<String> },
    /// Branch 4: godot-mcp wrote an older version.
    Update { from: Option<String> },
    /// Branch 5: the marker names other content.
    SkipChanged,
    /// Branch 6: there is no usable marker, for this reason.
    SkipUnrecognised(String),
}

fn decide(state: &SkillState, embedded: &str, force: bool) -> Decision {
    let Installed::Present(bytes) = &state.installed else {
        return Decision::Install;
    };
    if force {
        return Decision::Overwrite;
    }
    let installed_sha = sha256_hex(bytes);
    let embedded_sha = sha256_hex(embedded.as_bytes());
    if installed_sha == embedded_sha {
        let marker_rewrite = match &state.marker {
            Marker::Usable { sha256, .. } if *sha256 == embedded_sha => None,
            Marker::Usable { .. } => Some("its marker named another SHA-256".to_owned()),
            other => other.absence_reason(),
        };
        return Decision::Current { marker_rewrite };
    }
    match &state.marker {
        Marker::Usable { sha256, version } if *sha256 == installed_sha => Decision::Update {
            from: version.clone(),
        },
        Marker::Usable { .. } => Decision::SkipChanged,
        other => Decision::SkipUnrecognised(other.absence_reason().unwrap_or_default()),
    }
}

/// Whether godot-mcp wrote this `SKILL.md`: it equals the embedded content, or
/// the marker names its SHA-256.
fn written_by_godot_mcp(bytes: &[u8], marker: &Marker, embedded: &str) -> bool {
    let sha = sha256_hex(bytes);
    sha == sha256_hex(embedded.as_bytes())
        || matches!(marker, Marker::Usable { sha256, .. } if *sha256 == sha)
}

/// The text of a report line for one skill of one target.
fn prefix(target: Target, skill: &EmbeddedSkill) -> String {
    format!("{}: {}:", target.id(), skill.name)
}

/// The installed file as a diff side, or the one line that replaces the diff
/// when it is not UTF-8 text.
enum DiffView {
    /// Nothing to show: the installed content is the embedded content.
    Same,
    /// A rendered diff, ending in a newline.
    Text(String),
    /// The installed bytes are not UTF-8, so there is no text diff.
    NotUtf8(String),
}

fn diff_view(skill: &EmbeddedSkill, installed: &Installed) -> DiffView {
    let label = format!("{}/{SKILL_FILE}", skill.name);
    let rendered = match installed {
        Installed::Absent => godot_mcp_server::diff::unified(None, Some(&label), "", skill.content),
        Installed::Present(bytes) if bytes.as_slice() == skill.content.as_bytes() => {
            return DiffView::Same;
        }
        Installed::Present(bytes) => match std::str::from_utf8(bytes) {
            Ok(text) => {
                godot_mcp_server::diff::unified(Some(&label), Some(&label), text, skill.content)
            }
            Err(_) => {
                return DiffView::NotUtf8(format!(
                    "the installed {SKILL_FILE} is not UTF-8 text, so there is no diff \
                     (installed: {} bytes, sha256 {}; embedded: {} bytes, sha256 {})",
                    bytes.len(),
                    sha256_hex(bytes),
                    skill.content.len(),
                    sha256_hex(skill.content.as_bytes())
                ));
            }
        },
    };
    let mut text = rendered.text;
    if rendered.truncated {
        text.push_str("(diff truncated)\n");
    }
    DiffView::Text(text)
}

/// Run one command and write its report to `out`.
///
/// # Errors
/// Returns an error only when the report cannot be written; every problem with
/// a skill is reported and counted in [`Summary::errors`].
pub fn run(request: &Request, context: &SkillContext, out: &mut dyn Write) -> io::Result<Summary> {
    let resolved = resolve(&request.targets, context, request.location);
    if resolved.fell_back {
        writeln!(
            out,
            "auto: no agent configuration directory was found; using claude"
        )?;
    }
    let mut summary = Summary::default();
    for target in &resolved.targets {
        let skills_dir = target.skills_dir(context, request.location);
        for skill in SKILLS {
            let skill_dir = skills_dir.join(skill.name);
            let line = prefix(*target, skill);
            match request.action {
                Action::Install | Action::Update => {
                    write_one(request, skill, &skill_dir, &line, out, &mut summary)?;
                }
                Action::Uninstall => uninstall_one(skill, &skill_dir, &line, out, &mut summary)?,
                Action::Status => status_one(skill, &skill_dir, &line, out, &mut summary)?,
            }
        }
    }
    write_summary(request, &summary, out)?;
    Ok(summary)
}

fn write_summary(request: &Request, summary: &Summary, out: &mut dyn Write) -> io::Result<()> {
    let name = request.action.name();
    match request.action {
        Action::Install | Action::Update if request.dry_run => writeln!(
            out,
            "{name} --dry-run wrote nothing: would write {}, up to date {}, would skip {}, \
             errors {}",
            summary.written, summary.unchanged, summary.skipped, summary.errors
        ),
        Action::Install | Action::Update => writeln!(
            out,
            "{name}: written {}, up to date {}, skipped {}, errors {}",
            summary.written, summary.unchanged, summary.skipped, summary.errors
        ),
        Action::Uninstall => writeln!(
            out,
            "{name}: removed {}, kept {}, not installed {}, errors {}",
            summary.removed, summary.kept, summary.absent, summary.errors
        ),
        Action::Status => writeln!(
            out,
            "{name}: not installed {}, up to date {}, update available {}, modified {}, \
             errors {}",
            summary.absent, summary.unchanged, summary.outdated, summary.modified, summary.errors
        ),
    }
}

fn write_one(
    request: &Request,
    skill: &EmbeddedSkill,
    skill_dir: &Path,
    line: &str,
    out: &mut dyn Write,
    summary: &mut Summary,
) -> io::Result<()> {
    let dir = skill_dir.display();
    let state = match files::read_state(skill_dir) {
        Ok(state) => state,
        Err(reason) => {
            summary.errors += 1;
            return writeln!(out, "{line} error {dir}: {reason}");
        }
    };
    let decision = decide(&state, skill.content, request.force);
    let dry = request.dry_run;
    let (diff_text, not_utf8) = if request.diff {
        match diff_view(skill, &state.installed) {
            DiffView::Same => (None, None),
            DiffView::Text(text) => (Some(text), None),
            DiffView::NotUtf8(note) => (None, Some(note)),
        }
    } else {
        (None, None)
    };
    let mut message = match &decision {
        Decision::Install | Decision::Overwrite | Decision::Update { .. } => {
            let verb = match (&decision, dry) {
                (Decision::Install, true) => "would install".to_owned(),
                (Decision::Install, false) => "installed".to_owned(),
                (Decision::Overwrite, true) => "would overwrite".to_owned(),
                (Decision::Overwrite, false) => "overwrote".to_owned(),
                (_, true) => "would update".to_owned(),
                (_, false) => "updated".to_owned(),
            };
            let detail = match &decision {
                Decision::Overwrite => " (--force)".to_owned(),
                Decision::Update { from } => format!(
                    " ({} -> {VERSION})",
                    from.as_deref().unwrap_or("unknown version")
                ),
                _ => String::new(),
            };
            if dry {
                summary.written += 1;
                format!("{verb} {dir}{detail}")
            } else {
                match files::write_skill(skill_dir, skill.content, VERSION) {
                    Ok(()) => {
                        summary.written += 1;
                        format!("{verb} {dir}{detail}")
                    }
                    Err(reason) => {
                        summary.errors += 1;
                        format!("error {dir}: {reason}")
                    }
                }
            }
        }
        Decision::Current { marker_rewrite } => match marker_rewrite {
            None => {
                summary.unchanged += 1;
                format!("up to date {dir}")
            }
            Some(reason) if dry => {
                summary.unchanged += 1;
                format!("up to date {dir}; would rewrite its marker ({reason})")
            }
            Some(reason) => {
                let sha = sha256_hex(skill.content.as_bytes());
                match files::write_marker(skill_dir, &sha, VERSION) {
                    Ok(()) => {
                        summary.unchanged += 1;
                        format!("up to date {dir}; rewrote its marker ({reason})")
                    }
                    Err(error) => {
                        summary.errors += 1;
                        format!("error {dir}: {error}")
                    }
                }
            }
        },
        Decision::SkipChanged => {
            summary.skipped += 1;
            let verb = if dry { "would skip" } else { "skipped" };
            format!(
                "{verb} {dir}: its {SKILL_FILE} was changed after godot-mcp wrote it; \
                 --force overwrites it"
            )
        }
        Decision::SkipUnrecognised(reason) => {
            summary.skipped += 1;
            let verb = if dry { "would skip" } else { "skipped" };
            format!(
                "{verb} {dir}: its {SKILL_FILE} differs and {reason}, so godot-mcp cannot \
                 tell it wrote it; --force overwrites it"
            )
        }
    };
    if let Some(note) = not_utf8 {
        message.push_str("; ");
        message.push_str(&note);
    }
    writeln!(out, "{line} {message}")?;
    if let Some(text) = diff_text {
        out.write_all(text.as_bytes())?;
    }
    Ok(())
}

fn uninstall_one(
    skill: &EmbeddedSkill,
    skill_dir: &Path,
    line: &str,
    out: &mut dyn Write,
    summary: &mut Summary,
) -> io::Result<()> {
    let dir = skill_dir.display();
    let state = match files::read_state(skill_dir) {
        Ok(state) => state,
        Err(reason) => {
            summary.errors += 1;
            return writeln!(out, "{line} error {dir}: {reason}");
        }
    };
    let bytes = match &state.installed {
        Installed::Absent => {
            summary.absent += 1;
            if !files::has_marker_file(skill_dir) {
                return writeln!(out, "{line} not installed {dir}");
            }
            return match files::remove_skill(skill_dir, false) {
                Ok(_) => writeln!(
                    out,
                    "{line} not installed {dir}; removed its leftover marker"
                ),
                Err(reason) => {
                    summary.errors += 1;
                    writeln!(out, "{line} error {dir}: {reason}")
                }
            };
        }
        Installed::Present(bytes) => bytes,
    };
    if !written_by_godot_mcp(bytes, &state.marker, skill.content) {
        summary.kept += 1;
        let why = match state.marker.absence_reason() {
            None => format!("its {SKILL_FILE} was changed after godot-mcp wrote it"),
            Some(reason) => format!(
                "its {SKILL_FILE} differs and {reason}, so godot-mcp cannot tell it wrote it"
            ),
        };
        return writeln!(out, "{line} kept {dir}: {why}");
    }
    match files::remove_skill(skill_dir, true) {
        Ok(leftover) => {
            summary.removed += 1;
            match leftover {
                Leftover::Nothing => writeln!(out, "{line} removed {dir}"),
                Leftover::Directory => writeln!(
                    out,
                    "{line} removed {dir}; kept the directory, which holds other files"
                ),
                Leftover::Link => writeln!(
                    out,
                    "{line} removed {dir}; kept the symbolic link it is reached through"
                ),
            }
        }
        Err(reason) => {
            summary.errors += 1;
            writeln!(out, "{line} error {dir}: {reason}")
        }
    }
}

fn status_one(
    skill: &EmbeddedSkill,
    skill_dir: &Path,
    line: &str,
    out: &mut dyn Write,
    summary: &mut Summary,
) -> io::Result<()> {
    let dir = skill_dir.display();
    let state = match files::read_state(skill_dir) {
        Ok(state) => state,
        Err(reason) => {
            summary.errors += 1;
            return writeln!(out, "{line} error {dir}: {reason}");
        }
    };
    let Installed::Present(bytes) = &state.installed else {
        summary.absent += 1;
        return writeln!(out, "{line} not installed {dir}");
    };
    let installed_sha = sha256_hex(bytes);
    let embedded_sha = sha256_hex(skill.content.as_bytes());
    if installed_sha == embedded_sha {
        summary.unchanged += 1;
        let note = match &state.marker {
            Marker::Usable { sha256, .. } if *sha256 == embedded_sha => String::new(),
            _ => "; install or update rewrites its marker".to_owned(),
        };
        return writeln!(out, "{line} up to date {dir} ({VERSION}){note}");
    }
    match &state.marker {
        Marker::Usable { sha256, version } if *sha256 == installed_sha => {
            summary.outdated += 1;
            writeln!(
                out,
                "{line} update available {dir} ({} -> {VERSION})",
                version.as_deref().unwrap_or("unknown version")
            )
        }
        Marker::Usable { .. } => {
            summary.modified += 1;
            writeln!(
                out,
                "{line} modified {dir}: its {SKILL_FILE} was changed after godot-mcp wrote it"
            )
        }
        other => {
            summary.modified += 1;
            writeln!(
                out,
                "{line} modified {dir}: its {SKILL_FILE} differs and {}, so godot-mcp cannot \
                 tell it wrote it",
                other.absence_reason().unwrap_or_default()
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(installed: Installed, marker: Marker) -> SkillState {
        SkillState { installed, marker }
    }

    fn usable(sha256: &str) -> Marker {
        Marker::Usable {
            sha256: sha256.to_owned(),
            version: Some("0.0.1".to_owned()),
        }
    }

    #[test]
    fn decide_covers_the_six_branches_and_the_marker_rewrite() {
        let embedded = "embedded\n";
        let old = b"old\n".to_vec();
        let old_sha = sha256_hex(&old);
        let embedded_sha = sha256_hex(embedded.as_bytes());
        assert_eq!(
            decide(&state(Installed::Absent, Marker::Missing), embedded, false),
            Decision::Install
        );
        assert_eq!(
            decide(
                &state(Installed::Present(old.clone()), usable("x")),
                embedded,
                true
            ),
            Decision::Overwrite
        );
        assert_eq!(
            decide(
                &state(
                    Installed::Present(embedded.as_bytes().to_vec()),
                    usable(&embedded_sha)
                ),
                embedded,
                false
            ),
            Decision::Current {
                marker_rewrite: None
            }
        );
        assert_eq!(
            decide(
                &state(
                    Installed::Present(embedded.as_bytes().to_vec()),
                    Marker::Missing
                ),
                embedded,
                false
            ),
            Decision::Current {
                marker_rewrite: Some("it has no marker".to_owned())
            }
        );
        assert_eq!(
            decide(
                &state(Installed::Present(old.clone()), usable(&old_sha)),
                embedded,
                false
            ),
            Decision::Update {
                from: Some("0.0.1".to_owned())
            }
        );
        assert_eq!(
            decide(
                &state(Installed::Present(old.clone()), usable(&embedded_sha)),
                embedded,
                false
            ),
            Decision::SkipChanged
        );
        assert_eq!(
            decide(
                &state(
                    Installed::Present(old),
                    Marker::Unusable("its marker is not valid JSON".to_owned())
                ),
                embedded,
                false
            ),
            Decision::SkipUnrecognised("its marker is not valid JSON".to_owned())
        );
    }
}
