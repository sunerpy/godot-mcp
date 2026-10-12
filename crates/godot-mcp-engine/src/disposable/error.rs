//! What can go wrong on the disposable path, as a closed set.
//!
//! Each variant maps to exactly one of Todo 3's frozen error kinds. The mapping
//! matters most for [`DisposableError::IsolationUnavailable`]: it is
//! `unsupported`, not `unavailable` and not `internal`, because the honest answer
//! is "this host cannot contain untrusted code the way this operation requires" —
//! and the operation is refused rather than retried or downgraded.

use std::path::PathBuf;

use godot_mcp_domain::ErrorKind;

use crate::validation::runner::RunError;

/// A failure of a disposable-copy operation.
#[derive(Debug, thiserror::Error)]
pub enum DisposableError {
    /// No boundary of sufficient strength is available, so nothing was created
    /// and nothing was launched. Untrusted code is never run under a weak
    /// boundary, and the isolation level is never silently downgraded.
    #[error("untrusted execution refused before anything was created: {reason}")]
    IsolationUnavailable {
        /// Why each candidate mechanism was declined, from Todo 14's own
        /// fail-closed provider selection.
        reason: String,
    },
    /// A symlink was found in the region being copied. It is refused, never
    /// followed: following it would pull material from outside the project into a
    /// tree untrusted code is about to be handed.
    #[error("refusing to copy `{path}`: a symlink is never followed into a disposable copy")]
    SymlinkComponent {
        /// The project-relative path of the offending entry.
        path: String,
    },
    /// Something that is neither a regular file nor a directory.
    #[error("refusing to copy `{path}`: {detail}")]
    UnsupportedEntry {
        /// The project-relative path of the offending entry.
        path: String,
        /// What was found instead.
        detail: String,
    },
    /// The source has no `project.godot`, so it is not a Godot project.
    #[error("`{path}` is not a Godot project: no project.godot was found at its root")]
    NotAProject {
        /// The rejected source root.
        path: PathBuf,
    },
    /// A filesystem operation failed.
    #[error("filesystem failure at `{path}`: {reason}")]
    Filesystem {
        /// Which path the operation concerned.
        path: String,
        /// What the OS reported.
        reason: String,
    },
    /// The copy could not be removed, so mutation containment can no longer be
    /// claimed for it. Deliberately not folded into `Filesystem`: a copy that
    /// outlives its session is a fact a caller must be told.
    #[error("the disposable copy at `{path}` could not be discarded: {reason}")]
    Cleanup {
        /// The disposable root that is still present.
        path: String,
        /// What the OS reported.
        reason: String,
    },
    /// The engine could not be launched or contained.
    #[error(transparent)]
    Launch(#[from] RunError),
}

impl DisposableError {
    /// Map to the stable domain error taxonomy frozen by Todo 3.
    #[must_use]
    pub fn error_kind(&self) -> ErrorKind {
        match self {
            Self::IsolationUnavailable { .. } => ErrorKind::Unsupported,
            Self::SymlinkComponent { .. } => ErrorKind::PermissionDenied,
            Self::UnsupportedEntry { .. } => ErrorKind::InvalidInput,
            Self::NotAProject { .. } => ErrorKind::NotFound,
            Self::Filesystem { .. } => ErrorKind::Internal,
            Self::Cleanup { .. } => ErrorKind::Uncertain,
            Self::Launch(error) => error.error_kind(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_isolation_is_unsupported_not_internal() {
        let error = DisposableError::IsolationUnavailable {
            reason: "unix_locator: weak containment is never used for untrusted code".to_owned(),
        };
        assert_eq!(error.error_kind(), ErrorKind::Unsupported);
        assert!(error.to_string().contains("weak"));
        assert!(
            error.to_string().contains("before anything was created"),
            "the message says the refusal preceded any effect: {error}"
        );
    }

    #[test]
    fn a_symlink_refusal_is_permission_denied_and_names_the_entry() {
        let error = DisposableError::SymlinkComponent {
            path: "escape.txt".to_owned(),
        };
        assert_eq!(error.error_kind(), ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("escape.txt"));
        assert!(error.to_string().contains("never followed"));
    }

    #[test]
    fn a_surviving_copy_is_uncertain_rather_than_internal() {
        let error = DisposableError::Cleanup {
            path: "/tmp/copy".to_owned(),
            reason: "device or resource busy".to_owned(),
        };
        assert_eq!(error.error_kind(), ErrorKind::Uncertain);
    }

    #[test]
    fn a_missing_project_marker_is_not_found() {
        let error = DisposableError::NotAProject {
            path: PathBuf::from("/tmp/bare"),
        };
        assert_eq!(error.error_kind(), ErrorKind::NotFound);
    }
}
