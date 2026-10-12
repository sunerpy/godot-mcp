//! The index layer's failure type, mapped onto Todo 3's stable [`ErrorKind`].

use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::contracts::ContractViolation;

use crate::gdscript::GdParseError;
use crate::graph::GraphError;
use crate::project::ProjectError;
use crate::scene::SceneError;

/// A hard failure of the index layer.
///
/// A *stale* or *unreadable* store is not an error here: it is a
/// [`Coherence`](super::Coherence) verdict that forces a safe rescan. This type
/// is reserved for a rejected input, a refused read, or a store this layer
/// cannot honestly use at all.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    /// The state directory is not a usable private per-user location.
    #[error("insecure index state directory: {reason}")]
    InsecureState {
        /// Explanation of the rejection.
        reason: String,
    },
    /// Another owner already holds this project's index store.
    #[error("the project index is already owned by another process")]
    Conflict,
    /// A project-relative path was rejected: absolute, escaping, symlinked, or
    /// inside the `.godot/` engine cache. The refusal comes from Todo 2/8.
    #[error("the project refused the read: {0}")]
    Refused(#[source] ProjectError),
    /// The persisted store is present but cannot be decoded at this version.
    #[error("the persisted index cannot be read: {reason}")]
    StoreUnreadable {
        /// Explanation of the rejection.
        reason: String,
    },
    /// A supplied argument is not usable — an empty relative path, a duplicate
    /// inventory entry, or a zero debounce window.
    #[error("invalid index input: {reason}")]
    InvalidInput {
        /// Explanation of the violated invariant.
        reason: String,
    },
    /// A source document could not be modelled by Todos 7–9.
    #[error("a source document could not be modelled: {reason}")]
    Source {
        /// Stable outcome category inherited from the underlying layer.
        kind: ErrorKind,
        /// Explanation of the violated invariant.
        reason: String,
    },
    /// Todo 10 rejected the derived graph.
    #[error("the semantic graph was rejected: {0}")]
    Graph(#[source] GraphError),
    /// A genuine filesystem or SQLite failure.
    #[error("index storage error: {0}")]
    Io(String),
}

impl IndexError {
    /// Stable outcome category for this failure.
    #[must_use]
    pub fn error_kind(&self) -> ErrorKind {
        match self {
            Self::InsecureState { .. } => ErrorKind::PermissionDenied,
            Self::Conflict => ErrorKind::Conflict,
            Self::Refused(error) => error.error_kind(),
            Self::StoreUnreadable { .. } | Self::InvalidInput { .. } => ErrorKind::InvalidInput,
            Self::Source { kind, .. } => *kind,
            Self::Graph(error) => error.kind(),
            Self::Io(_) => ErrorKind::Internal,
        }
    }
}

impl From<ProjectError> for IndexError {
    fn from(error: ProjectError) -> Self {
        Self::Refused(error)
    }
}

impl From<GraphError> for IndexError {
    fn from(error: GraphError) -> Self {
        Self::Graph(error)
    }
}

impl From<SceneError> for IndexError {
    fn from(error: SceneError) -> Self {
        Self::Source {
            kind: error.error_kind(),
            reason: error.to_string(),
        }
    }
}

impl From<GdParseError> for IndexError {
    fn from(error: GdParseError) -> Self {
        Self::Source {
            kind: error.kind(),
            reason: error.message().to_owned(),
        }
    }
}

impl From<ContractViolation> for IndexError {
    fn from(violation: ContractViolation) -> Self {
        Self::Source {
            kind: violation.kind(),
            reason: violation.to_string(),
        }
    }
}

impl From<rusqlite::Error> for IndexError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Io(error.to_string())
    }
}
