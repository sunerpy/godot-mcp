//! The agents a skill can be installed for, where each one reads skills from,
//! and how `auto` detects them.
//!
//! The table is frozen: every row was checked against that client's own
//! documentation, and `docs/operator/skills.md` publishes it.
//!
//! | target | `--global` | `--local` | detected when |
//! | --- | --- | --- | --- |
//! | `claude` | `$CLAUDE_CONFIG_DIR/skills`, else `~/.claude/skills` | `.claude/skills` | `$CLAUDE_CONFIG_DIR` or `~/.claude` exists; `.claude` |
//! | `codex` | `~/.agents/skills` | `.agents/skills` | `$CODEX_HOME` or `~/.codex` exists; `.codex` |
//! | `cursor` | `~/.cursor/skills` | `.cursor/skills` | `~/.cursor`; `.cursor` |
//! | `gemini` | `~/.gemini/skills` | `.gemini/skills` | `~/.gemini`; `.gemini` |
//! | `kiro` | `~/.kiro/skills` | `.kiro/skills` | `~/.kiro`; `.kiro` |
//! | `opencode` | `~/.config/opencode/skills` | `.opencode/skills` | `~/.config/opencode`; `.opencode` |
//!
//! `--local` paths are relative to the current directory.

use std::path::PathBuf;

use super::SkillContext;

/// Whether skills go to the user's own directories or the current directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Location {
    /// The user's skill directories under the home directory (the default).
    Global,
    /// The current directory's skill directories.
    Local,
}

/// One agent whose skill directory the command writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// Claude Code.
    Claude,
    /// Codex, which reads `.agents/skills`.
    Codex,
    /// Cursor.
    Cursor,
    /// Gemini CLI.
    Gemini,
    /// Kiro.
    Kiro,
    /// opencode.
    Opencode,
}

impl Target {
    /// Every target, in the order `all` and `auto` process them.
    pub const ALL: [Self; 6] = [
        Self::Claude,
        Self::Codex,
        Self::Cursor,
        Self::Gemini,
        Self::Kiro,
        Self::Opencode,
    ];

    /// The id `--target` names it by.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Cursor => "cursor",
            Self::Gemini => "gemini",
            Self::Kiro => "kiro",
            Self::Opencode => "opencode",
        }
    }

    /// The target `id` names, if any.
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|target| target.id() == id)
    }

    /// The directory that holds one subdirectory per installed skill.
    #[must_use]
    pub fn skills_dir(self, context: &SkillContext, location: Location) -> PathBuf {
        match location {
            Location::Global => match self {
                Self::Claude => context
                    .claude_config_dir()
                    .unwrap_or_else(|| context.home.join(".claude"))
                    .join("skills"),
                Self::Codex => context.home.join(".agents").join("skills"),
                Self::Cursor => context.home.join(".cursor").join("skills"),
                Self::Gemini => context.home.join(".gemini").join("skills"),
                Self::Kiro => context.home.join(".kiro").join("skills"),
                Self::Opencode => context.home.join(".config").join("opencode").join("skills"),
            },
            Location::Local => {
                let base = match self {
                    Self::Claude => ".claude",
                    Self::Codex => ".agents",
                    Self::Cursor => ".cursor",
                    Self::Gemini => ".gemini",
                    Self::Kiro => ".kiro",
                    Self::Opencode => ".opencode",
                };
                context.cwd.join(base).join("skills")
            }
        }
    }

    /// The configuration directories whose presence means this agent is in use.
    fn detection_dirs(self, context: &SkillContext, location: Location) -> Vec<PathBuf> {
        match location {
            Location::Global => match self {
                Self::Claude => context
                    .claude_config_dir()
                    .into_iter()
                    .chain([context.home.join(".claude")])
                    .collect(),
                Self::Codex => context
                    .codex_home()
                    .into_iter()
                    .chain([context.home.join(".codex")])
                    .collect(),
                Self::Cursor => vec![context.home.join(".cursor")],
                Self::Gemini => vec![context.home.join(".gemini")],
                Self::Kiro => vec![context.home.join(".kiro")],
                Self::Opencode => vec![context.home.join(".config").join("opencode")],
            },
            Location::Local => {
                let base = match self {
                    Self::Claude => ".claude",
                    Self::Codex => ".codex",
                    Self::Cursor => ".cursor",
                    Self::Gemini => ".gemini",
                    Self::Kiro => ".kiro",
                    Self::Opencode => ".opencode",
                };
                vec![context.cwd.join(base)]
            }
        }
    }

    /// Whether `auto` selects this target.
    #[must_use]
    pub fn detected(self, context: &SkillContext, location: Location) -> bool {
        self.detection_dirs(context, location)
            .iter()
            .any(|dir| dir.is_dir())
    }
}

/// What `--target` selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetSelection {
    /// Every target whose configuration directory exists, or `claude` alone.
    Auto,
    /// Every target.
    All,
    /// These targets, in this order, without repeats.
    Only(Vec<Target>),
}

/// The values `--target` accepts, for an error message.
fn accepted_values() -> String {
    let ids: Vec<&str> = Target::ALL.iter().map(|target| target.id()).collect();
    format!("auto, all, or a comma-separated list of {}", ids.join(", "))
}

/// Parse a `--target` value: `auto`, `all`, or `id[,id...]`.
///
/// # Errors
/// Returns a message naming every accepted value when the value is empty,
/// names an unknown target, or combines `auto` or `all` with another name.
pub fn parse_target_selection(value: &str) -> Result<TargetSelection, String> {
    let value = value.trim();
    match value {
        "auto" => return Ok(TargetSelection::Auto),
        "all" => return Ok(TargetSelection::All),
        _ => {}
    }
    let mut targets = Vec::new();
    for id in value.split(',').map(str::trim) {
        if id.is_empty() {
            return Err(format!(
                "a target name is empty; expected {}",
                accepted_values()
            ));
        }
        if id == "auto" || id == "all" {
            return Err(format!(
                "{id} cannot be combined with other targets; expected {}",
                accepted_values()
            ));
        }
        let Some(target) = Target::from_id(id) else {
            return Err(format!(
                "unknown target {id}; expected {}",
                accepted_values()
            ));
        };
        if !targets.contains(&target) {
            targets.push(target);
        }
    }
    Ok(TargetSelection::Only(targets))
}

/// The targets one command processes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTargets {
    /// The targets, in processing order.
    pub targets: Vec<Target>,
    /// Whether `auto` found no agent configuration and fell back to `claude`.
    pub fell_back: bool,
}

/// Resolve a selection against the directories that exist now.
#[must_use]
pub fn resolve(
    selection: &TargetSelection,
    context: &SkillContext,
    location: Location,
) -> ResolvedTargets {
    match selection {
        TargetSelection::All => ResolvedTargets {
            targets: Target::ALL.to_vec(),
            fell_back: false,
        },
        TargetSelection::Only(targets) => ResolvedTargets {
            targets: targets.clone(),
            fell_back: false,
        },
        TargetSelection::Auto => {
            let detected: Vec<Target> = Target::ALL
                .into_iter()
                .filter(|target| target.detected(context, location))
                .collect();
            if detected.is_empty() {
                ResolvedTargets {
                    targets: vec![Target::Claude],
                    fell_back: true,
                }
            } else {
                ResolvedTargets {
                    targets: detected,
                    fell_back: false,
                }
            }
        }
    }
}
