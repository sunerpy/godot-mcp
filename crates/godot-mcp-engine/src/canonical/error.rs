//! The closed failure set for canonical text-resource writes and UID operations.
//!
//! Every variant maps to Todo 3's stable [`ErrorKind`] taxonomy, and the mapping
//! is the point rather than an afterthought: a caller must be able to tell a
//! *refusal* (this operation is not permitted on this input) from a *conflict*
//! (the world moved under us) from an *uncertain* outcome (we cannot prove what
//! happened) without parsing prose.
//!
//! Two variants deserve their own note:
//!
//! * [`CanonicalError::Quarantined`] is [`ErrorKind::Uncertain`], never
//!   `Internal`. It is the honest answer when a mutation's own file evidence and
//!   its linked `process_launch` proof do not together establish either the exact
//!   pre-state or the exact post-state. It also **blocks the next mutation**, so
//!   it is not a failure a caller may retry past.
//! * [`CanonicalError::UnprovenComposite`] is what a matching post-hash *alone*
//!   gets you. It exists so that "the bytes look right" can never be spelled as
//!   a success anywhere in this module.

use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::safe_root::SafeRootError;

use crate::supervision::journal::JournalError;
use crate::validation::runner::RunError;

/// A canonical-write failure.
#[derive(Debug, thiserror::Error)]
pub enum CanonicalError {
    /// The target is a binary resource (`.scn`/`.res`). Never mutated, never
    /// rewritten as text, and there is no override.
    #[error(
        "{path} is a binary resource; godot-mcp only writes canonical text resources (.tscn/.tres)"
    )]
    BinaryResource {
        /// The refused project-relative path.
        path: String,
    },

    /// The target is not a text resource this module knows how to ask the engine
    /// to produce.
    #[error("{path} is not a canonical text resource (.tscn/.tres)")]
    NotATextResource {
        /// The refused project-relative path.
        path: String,
    },

    /// The target path is malformed, absolute, escapes the project root, or names
    /// the helper's own versioned area.
    #[error("{path} is not a writable in-project resource path: {reason}")]
    UnusablePath {
        /// The refused path, as given.
        path: String,
        /// Exactly what is wrong with it.
        reason: String,
    },

    /// The resource is open in a live editor session, so writing it from outside
    /// would race an unsaved buffer. Fails closed; there is no force path.
    #[error("{path} is open in an editor session and is never overwritten from outside")]
    ResourceOpen {
        /// The refused project-relative path.
        path: String,
    },

    /// The on-disk bytes are not the preimage this transaction recorded, so
    /// something else changed the resource. Fails closed; there is no force path.
    #[error("{path} changed outside this transaction and is never overwritten")]
    ExternallyChanged {
        /// The conflicted project-relative path.
        path: String,
    },

    /// The requested UID is already bound to a different resource, or the engine
    /// minted a UID this project already binds elsewhere.
    #[error("UID {uid} is already bound to {existing}, not {requested}")]
    UidConflict {
        /// The engine-minted UID. Never derived in this crate.
        uid: String,
        /// The path the UID is already bound to.
        existing: String,
        /// The path the request wanted to bind.
        requested: String,
    },

    /// This build cannot run the helper at all, so nothing was attempted.
    #[error("the configured Godot build cannot perform canonical writes: {detection}")]
    Unsupported {
        /// Exactly what was inspected and what it showed.
        detection: String,
    },

    /// The bundled helper refused or failed the request.
    #[error("the bundled canonical-write helper reported {status}: {reason}")]
    HelperRefused {
        /// The helper's own status spelling.
        status: String,
        /// The helper's own reason.
        reason: String,
    },

    /// The helper ran but produced nothing this module can interpret, so the
    /// outcome cannot be established either way.
    #[error("the canonical-write helper produced no interpretable result: {reason}")]
    HelperInconclusive {
        /// Why the run cannot be interpreted.
        reason: String,
    },

    /// The on-disk bytes match the intended post-state but the linked
    /// `process_launch` proof does not independently establish a clean terminal
    /// helper process. Matching bytes are never sufficient on their own.
    #[error(
        "the file bytes match the intended post-state but the linked process_launch proof does \
         not establish a clean terminal helper process: {reason}"
    )]
    UnprovenComposite {
        /// Which half of the composite requirement is missing.
        reason: String,
    },

    /// Neither the exact pre-state nor the exact post-state could be established.
    /// Blocks the next mutation until an operator repairs it.
    #[error("transaction {transaction_id} is quarantined and blocks further mutation: {reason}")]
    Quarantined {
        /// The quarantined transaction.
        transaction_id: String,
        /// Why it could not be resolved.
        reason: String,
    },

    /// A previously quarantined transaction still blocks mutation.
    #[error("mutation is blocked: {count} quarantined transaction(s) await repair: {transactions}")]
    MutationBlocked {
        /// How many transactions are quarantined.
        count: usize,
        /// Their IDs, comma separated.
        transactions: String,
    },

    /// The requested repair is refused because it would assert a state that is
    /// not proven.
    #[error("repair refused for {transaction_id}: {reason}")]
    RepairRefused {
        /// The transaction the repair was attempted on.
        transaction_id: String,
        /// Why the repair cannot be granted.
        reason: String,
    },

    /// The engine could not be launched or contained.
    #[error(transparent)]
    Launch(#[from] RunError),

    /// A durable journal write failed or a record was not canonical.
    #[error(transparent)]
    Journal(#[from] JournalError),

    /// A leased-root read or compare-and-swap failed.
    #[error("leased-root access failed: {0}")]
    Root(String),

    /// Scratch/staging I/O failed.
    #[error("filesystem I/O failed at {path}: {reason}")]
    Io {
        /// The path involved.
        path: String,
        /// The underlying failure.
        reason: String,
    },
}

impl CanonicalError {
    /// The stable domain error kind.
    #[must_use]
    pub fn error_kind(&self) -> ErrorKind {
        match self {
            Self::BinaryResource { .. }
            | Self::NotATextResource { .. }
            | Self::UnusablePath { .. } => ErrorKind::InvalidInput,
            Self::ResourceOpen { .. }
            | Self::ExternallyChanged { .. }
            | Self::UidConflict { .. }
            | Self::MutationBlocked { .. } => ErrorKind::Conflict,
            Self::Unsupported { .. } => ErrorKind::Unsupported,
            Self::HelperRefused { .. } => ErrorKind::InvalidInput,
            Self::HelperInconclusive { .. }
            | Self::UnprovenComposite { .. }
            | Self::Quarantined { .. }
            | Self::RepairRefused { .. } => ErrorKind::Uncertain,
            Self::Launch(error) => error.error_kind(),
            Self::Journal(_) | Self::Io { .. } | Self::Root(_) => ErrorKind::Internal,
        }
    }

    /// Whether this failure blocks the next mutation until an operator repairs it.
    #[must_use]
    pub const fn blocks_mutation(&self) -> bool {
        matches!(
            self,
            Self::Quarantined { .. } | Self::MutationBlocked { .. }
        )
    }
}

impl From<SafeRootError> for CanonicalError {
    fn from(error: SafeRootError) -> Self {
        match error {
            SafeRootError::StalePreimage { path } | SafeRootError::Traversal { path } => {
                // A stale preimage is exactly "something else changed it"; a
                // traversal attempt is a refused path. Both fail closed.
                if matches!(error_is_traversal(&path), IsTraversal::Yes) {
                    Self::UnusablePath {
                        path: path.display().to_string(),
                        reason: "path traversal (`..`) is not permitted inside the root".to_owned(),
                    }
                } else {
                    Self::ExternallyChanged {
                        path: path.display().to_string(),
                    }
                }
            }
            SafeRootError::OutsideRoot { ref path } | SafeRootError::Symlink { ref path } => {
                Self::UnusablePath {
                    path: path.display().to_string(),
                    reason: error.to_string(),
                }
            }
            SafeRootError::IdentityChanged => Self::ExternallyChanged {
                path: "project root".to_owned(),
            },
            SafeRootError::Io(reason) => Self::Root(reason),
        }
    }
}

enum IsTraversal {
    Yes,
    No,
}

fn error_is_traversal(path: &std::path::Path) -> IsTraversal {
    if path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        IsTraversal::Yes
    } else {
        IsTraversal::No
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quarantine_is_uncertain_and_blocks_mutation() {
        let error = CanonicalError::Quarantined {
            transaction_id: "00000000-0000-4000-8000-000000000001".to_owned(),
            reason: "the linked process_launch proof is missing".to_owned(),
        };
        assert_eq!(error.error_kind(), ErrorKind::Uncertain);
        assert!(error.blocks_mutation());
    }

    #[test]
    fn matching_bytes_without_a_clean_linked_proof_is_never_success() {
        let error = CanonicalError::UnprovenComposite {
            reason: "the linked helper is still live inside its boundary".to_owned(),
        };
        assert_eq!(error.error_kind(), ErrorKind::Uncertain);
        assert!(
            error.to_string().contains("match the intended post-state"),
            "the message states plainly that matching bytes were not enough: {error}"
        );
    }

    #[test]
    fn a_binary_resource_is_invalid_input_and_has_no_override() {
        let error = CanonicalError::BinaryResource {
            path: "scenes/main.scn".to_owned(),
        };
        assert_eq!(error.error_kind(), ErrorKind::InvalidInput);
        assert!(!error.blocks_mutation());
    }

    #[test]
    fn an_external_change_and_an_open_resource_are_both_conflicts() {
        for error in [
            CanonicalError::ExternallyChanged {
                path: "scenes/main.tscn".to_owned(),
            },
            CanonicalError::ResourceOpen {
                path: "scenes/main.tscn".to_owned(),
            },
        ] {
            assert_eq!(error.error_kind(), ErrorKind::Conflict);
        }
    }

    #[test]
    fn a_stale_preimage_becomes_an_external_change_and_a_traversal_stays_a_refusal() {
        let stale: CanonicalError = SafeRootError::StalePreimage {
            path: std::path::PathBuf::from("scenes/main.tscn"),
        }
        .into();
        assert!(matches!(stale, CanonicalError::ExternallyChanged { .. }));
        let traversal: CanonicalError = SafeRootError::Traversal {
            path: std::path::PathBuf::from("../outside.tscn"),
        }
        .into();
        assert!(matches!(traversal, CanonicalError::UnusablePath { .. }));
    }
}
