//! The one path from a validation request to a running engine.
//!
//! Every engine invocation in this module goes through Todo 14's
//! [`LaunchProtocol`]: the boundary is pre-created, the identity receipt is
//! durable before any user code runs, output is byte-capped, and the deadline is
//! enforced by the supervisor rather than by the engine. There is deliberately no
//! second, bare `Command::spawn` path here — a `Command` this module built itself
//! would be a process outside the containment boundary and outside the journal.
//!
//! Arguments are always an array. The engine's program path and every flag travel
//! as separate `Vec<String>` elements from here down through
//! [`SpawnPlan`](crate::supervision::boundary::SpawnPlan) into the barrier's
//! `execvp`, so there is no point at which a shell could re-interpret them.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use godot_mcp_domain::ErrorKind;

use crate::supervision::boundary::{BoundaryProvider, TrustLevel};
use crate::supervision::journal::{LaunchJournal, OperationFamily, TransactionEnvelope};
use crate::supervision::protocol::{
    EnvironmentAllowlist, FaultInjection, LaunchError, LaunchProtocol, LaunchRequest, OutputCaps,
    SilentObserver, TerminationKind,
};
use crate::validation::framework::SelectionSeparatorError;
use crate::validation::outcome::Observed;

/// Where a canonical transaction envelope comes from.
///
/// The engine plane never mints transaction identity itself: Todo 6 owns the
/// journal and the server layer owns envelope construction. This port is how a
/// validation operation asks for the envelope its one launch will be recorded
/// under.
pub trait TransactionSource {
    /// One fresh envelope for one engine invocation.
    fn next(&self, tool_name: &str, family: OperationFamily) -> TransactionEnvelope;
}

/// Everything about *which* engine runs and *how* it is contained. Assembled by
/// the caller from Todo 13's discovery/probe results; this module never discovers
/// or re-probes an executable.
#[derive(Debug, Clone)]
pub struct EngineBinding {
    /// The executable, already discovered and version-negotiated by Todo 13.
    pub program: PathBuf,
    /// The executable's expected SHA-256, revalidated by the launch protocol
    /// immediately before use.
    pub binary_sha256: String,
    /// The bundled barrier/supervisor binary.
    pub supervisor_binary: PathBuf,
    /// The held project root. Also the engine's working directory.
    pub project_root: PathBuf,
    /// Scratch space for launch artifacts (gate, captured output).
    pub work_dir: PathBuf,
    /// Exactly what the engine's environment may contain.
    pub environment: EnvironmentAllowlist,
    /// Byte budgets for captured output.
    pub caps: OutputCaps,
    /// Whether the project's code is trusted. Untrusted code never runs under a
    /// weak boundary — the launch protocol enforces that, not this module.
    pub trust: TrustLevel,
    /// The flags this build advertises, as parsed from its `--help` by Todo 13.
    ///
    /// This is what makes "the build cannot do this" answerable *before* a
    /// launch. It matters because the engine silently ignores a flag it does not
    /// support: without this set, an unsupported operation would exit 0 and look
    /// like a success.
    pub capabilities: BTreeSet<String>,
}

/// One completed engine invocation.
#[derive(Debug, Clone)]
pub struct EngineRun {
    /// The exact argument array issued, program path first.
    pub argv: Vec<String>,
    /// Captured stdout, decoded lossily and capped.
    pub stdout: String,
    /// Captured stderr, decoded lossily and capped.
    pub stderr: String,
    /// Whether either stream hit its byte cap.
    pub truncated: bool,
    /// Exit/signal/timeout facts, taken from the launch protocol's own
    /// structured diagnostics rather than re-derived.
    pub observed: Observed,
    /// The transaction this invocation was journaled under.
    pub transaction_id: String,
}

impl EngineRun {
    /// Both captured streams, concatenated for diagnostic parsing.
    ///
    /// Godot writes diagnostics to stderr, but a build wrapper, a `-s` framework
    /// runner or a redirecting launcher can move them, so both streams are always
    /// scanned.
    #[must_use]
    pub fn combined_output(&self) -> String {
        let mut combined = String::with_capacity(self.stdout.len() + self.stderr.len() + 1);
        combined.push_str(&self.stderr);
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(&self.stdout);
        combined
    }

    /// Whether the deadline, not the engine, ended this run.
    #[must_use]
    pub const fn timed_out(&self) -> bool {
        self.observed.timed_out
    }
}

/// A failure to *run* the engine at all. This is deliberately narrow: a run that
/// completed and reported a problem is not an error here, it is a classification.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    /// A caller-supplied engine path is not portable and project-relative.
    #[error("invalid {0} project-relative path: {1:?}")]
    InvalidProjectPath(&'static str, String),
    /// Artifact evidence must come from the same project the engine runs against.
    #[error("the import evidence root is not bound to the engine project root")]
    EvidenceRootMismatch,
    /// A framework would reinterpret one validated element as multiple paths.
    #[error(transparent)]
    InvalidSelection(#[from] SelectionSeparatorError),
    /// The launch protocol refused or aborted the launch.
    #[error(transparent)]
    Launch(#[from] LaunchError),
}

impl RunError {
    /// Map to the stable domain error taxonomy.
    #[must_use]
    pub fn error_kind(&self) -> ErrorKind {
        match self {
            Self::InvalidProjectPath(_, _) | Self::InvalidSelection(_) => ErrorKind::InvalidInput,
            Self::EvidenceRootMismatch => ErrorKind::Conflict,
            Self::Launch(error) => error.error_kind(),
        }
    }
}

/// Runs the engine, once per operation, inside a containment boundary.
pub struct EngineRunner<'r> {
    journal: &'r dyn LaunchJournal,
    provider: &'r dyn BoundaryProvider,
    transactions: &'r dyn TransactionSource,
    binding: EngineBinding,
}

impl<'r> EngineRunner<'r> {
    /// Bind a runner to one journal, one boundary provider and one engine.
    #[must_use]
    pub const fn new(
        journal: &'r dyn LaunchJournal,
        provider: &'r dyn BoundaryProvider,
        transactions: &'r dyn TransactionSource,
        binding: EngineBinding,
    ) -> Self {
        Self {
            journal,
            provider,
            transactions,
            binding,
        }
    }

    /// The engine this runner drives.
    #[must_use]
    pub const fn binding(&self) -> &EngineBinding {
        &self.binding
    }

    /// The project root this runner is bound to.
    #[must_use]
    pub fn project_root(&self) -> &std::path::Path {
        &self.binding.project_root
    }

    /// The first of `required` that this build does not advertise.
    ///
    /// A caller checks this *before* invoking, because a Godot build silently
    /// ignores an unsupported flag: it would exit 0 having done nothing, and no
    /// amount of output parsing afterwards could distinguish that from success.
    #[must_use]
    pub fn missing_capability(&self, required: &[&str]) -> Option<String> {
        required
            .iter()
            .find(|flag| !self.binding.capabilities.contains(**flag))
            .map(|flag| (*flag).to_owned())
    }

    /// Run the engine once with `arguments`.
    ///
    /// # Errors
    /// Returns [`RunError`] only when the engine could not be launched or
    /// contained. A deadline is **not** an error: it comes back as a completed
    /// run whose [`Observed::timed_out`] is set, so the caller can classify it as
    /// its own category.
    pub fn invoke(
        &self,
        tool_name: &str,
        family: OperationFamily,
        arguments: Vec<String>,
        deadline: Duration,
    ) -> Result<EngineRun, RunError> {
        let envelope = self.transactions.next(tool_name, family);
        let transaction_id = envelope.transaction_id.clone();
        let request = LaunchRequest {
            envelope,
            trust: self.binding.trust,
            program: self.binding.program.clone(),
            arguments,
            binary_sha256: self.binding.binary_sha256.clone(),
            supervisor_binary: self.binding.supervisor_binary.clone(),
            project_root_display: self.binding.project_root.clone(),
            environment: self.binding.environment.clone(),
            caps: self.binding.caps,
            deadline,
            work_dir: self.binding.work_dir.clone(),
            evidence_path: None,
        };
        let protocol = LaunchProtocol::new(self.journal);
        let outcome = protocol.launch(
            &request,
            self.provider,
            &FaultInjection::none(),
            &SilentObserver,
        )?;
        let diagnostics = &outcome.diagnostics;
        Ok(EngineRun {
            argv: outcome.argv.clone(),
            stdout: String::from_utf8_lossy(&diagnostics.stdout.bytes).into_owned(),
            stderr: String::from_utf8_lossy(&diagnostics.stderr.bytes).into_owned(),
            truncated: diagnostics.stdout.truncated || diagnostics.stderr.truncated,
            observed: Observed {
                exit_code: diagnostics.exit_code,
                signal: diagnostics.signal.clone(),
                timed_out: diagnostics.termination == TerminationKind::TimedOut,
            },
            transaction_id,
        })
    }
}

/// The flags every headless engine invocation carries.
///
/// `--headless` selects the dummy display/audio drivers (it does *not* imply
/// anything about which work is requested), and `--path` points the engine at the
/// held project root. Both are passed as separate array elements.
#[must_use]
pub fn headless_project_args(project_root: &std::path::Path) -> Vec<String> {
    vec![
        "--headless".to_owned(),
        "--path".to_owned(),
        project_root.display().to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(capabilities: &[&str]) -> EngineBinding {
        EngineBinding {
            program: PathBuf::from("/nonexistent/godot"),
            binary_sha256: "0".repeat(64),
            supervisor_binary: PathBuf::from("/nonexistent/godot-mcp-supervisor"),
            project_root: PathBuf::from("/tmp/project"),
            work_dir: PathBuf::from("/tmp/work"),
            environment: EnvironmentAllowlist::default(),
            caps: OutputCaps {
                stdout_bytes: 16,
                stderr_bytes: 16,
            },
            trust: TrustLevel::Untrusted,
            capabilities: capabilities.iter().map(|flag| (*flag).to_owned()).collect(),
        }
    }

    struct Nothing;

    impl TransactionSource for Nothing {
        fn next(&self, _tool_name: &str, _family: OperationFamily) -> TransactionEnvelope {
            unreachable!("these tests never launch");
        }
    }

    #[test]
    fn a_missing_capability_is_reported_by_name_before_any_launch() {
        let journal = NoJournal;
        let provider = NoProvider;
        let runner = EngineRunner::new(
            &journal,
            &provider,
            &Nothing,
            binding(&["--headless", "--path", "--script"]),
        );
        assert_eq!(
            runner.missing_capability(&["--headless", "--check-only", "--script"]),
            Some("--check-only".to_owned())
        );
        assert_eq!(runner.missing_capability(&["--headless"]), None);
    }

    #[test]
    fn headless_arguments_are_separate_array_elements() {
        let args = headless_project_args(std::path::Path::new("/tmp/a project"));
        assert_eq!(
            args,
            vec![
                "--headless".to_owned(),
                "--path".to_owned(),
                "/tmp/a project".to_owned()
            ],
            "a path with a space is one element, never quoted into a string"
        );
    }

    #[test]
    fn combined_output_scans_both_streams() {
        let run = EngineRun {
            argv: Vec::new(),
            stdout: "banner\n".to_owned(),
            stderr: "SCRIPT ERROR: Parse Error: x".to_owned(),
            truncated: false,
            observed: Observed {
                exit_code: Some(1),
                signal: None,
                timed_out: false,
            },
            transaction_id: "t".to_owned(),
        };
        let combined = run.combined_output();
        assert!(combined.contains("SCRIPT ERROR"));
        assert!(combined.contains("banner"));
        assert!(!run.timed_out());
    }

    struct NoJournal;

    impl LaunchJournal for NoJournal {
        fn write_phase(
            &self,
            _record: &crate::supervision::journal::LaunchRecord,
        ) -> Result<(), crate::supervision::journal::JournalError> {
            unreachable!("these tests never launch");
        }

        fn settle(
            &self,
            _record: &crate::supervision::journal::LaunchRecord,
            _disposition: crate::supervision::journal::Disposition,
            _reason: &str,
            _proof: Option<&crate::supervision::journal::ProcessLaunchProof>,
        ) -> Result<(), crate::supervision::journal::JournalError> {
            unreachable!("these tests never launch");
        }

        fn open_launches(
            &self,
        ) -> Result<
            Vec<crate::supervision::journal::LaunchRecord>,
            crate::supervision::journal::JournalError,
        > {
            Ok(Vec::new())
        }

        fn committed_launches(
            &self,
        ) -> Result<
            Vec<crate::supervision::journal::LaunchRecord>,
            crate::supervision::journal::JournalError,
        > {
            Ok(Vec::new())
        }

        fn quarantined(&self) -> Result<Vec<String>, crate::supervision::journal::JournalError> {
            Ok(Vec::new())
        }
    }

    struct NoProvider;

    impl BoundaryProvider for NoProvider {
        fn kind(&self) -> crate::supervision::boundary::BoundaryKind {
            crate::supervision::boundary::BoundaryKind::ContainerAdapter
        }

        fn available(&self) -> Result<(), crate::supervision::boundary::BoundaryError> {
            unreachable!("these tests never launch");
        }

        fn create(
            &self,
            _transaction_id: &str,
        ) -> Result<
            Box<dyn crate::supervision::boundary::ContainmentBoundary>,
            crate::supervision::boundary::BoundaryError,
        > {
            unreachable!("these tests never launch");
        }

        fn reopen(
            &self,
            _identity: &crate::supervision::boundary::BoundaryIdentity,
        ) -> Result<
            Option<Box<dyn crate::supervision::boundary::ContainmentBoundary>>,
            crate::supervision::boundary::BoundaryError,
        > {
            Ok(None)
        }
    }
}
