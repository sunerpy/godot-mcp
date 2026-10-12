//! Versioned, transport-independent public domain contracts.

mod artifact;
mod ids;
mod outcomes;
mod provenance;
mod schema;
mod values;

pub use crate::{Confidence, ErrorKind};
pub use artifact::{
    ArtifactContract, ArtifactKind, ArtifactMetadata, ArtifactOwner, ByteRange, RangeSemantics,
};
pub use ids::{
    ArtifactId, ArtifactResourceUri, OpaqueObjectId, PrincipalId, ProjectObjectId, ProjectResource,
    ResourceUri, SessionId,
};
pub use outcomes::{
    CapabilitiesReport, Capability, ComponentHealth, ComponentName, EffectDisposition,
    HealthReport, HealthStatus, OperationClass, OperationEffect, OperationPlane, OutcomeError,
    PolicyDecision, PolicyReason, PolicyVerdict, ProjectIdentity, SchedulerMode, SchedulerState,
    StaticResult,
};
pub use provenance::{Provenance, ProvenanceSource};
pub use schema::canonical_contract_schema;
pub use values::{
    Aabb, Basis, Color, GodotValue, Matrix4, ObjectReference, Plane, Projection, Quaternion, Rect2,
    Rect2i, ResourceReference, Transform2D, Transform3D, Vector2, Vector2i, Vector3, Vector3i,
    Vector4, Vector4i,
};

use crate::CONTRACT_VERSION;

/// Exact version marker carried by every top-level public contract.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
pub enum ContractVersion {
    /// Initial frozen contract.
    #[serde(rename = "0.1.0")]
    V0_1_0,
    /// Implementation-backed discovery: a profile lists only implemented methods.
    #[default]
    #[serde(rename = "0.2.0")]
    V0_2_0,
}

impl ContractVersion {
    /// Current version marker.
    #[must_use]
    pub const fn current() -> Self {
        Self::V0_2_0
    }

    /// Stable textual representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::V0_1_0 => "0.1.0",
            Self::V0_2_0 => CONTRACT_VERSION,
        }
    }
}

/// A rejected domain-contract invariant.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ContractViolation {
    kind: ErrorKind,
    message: String,
}

impl ContractViolation {
    pub(crate) fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// Stable outcome category for the violation.
    #[must_use]
    pub const fn kind(&self) -> ErrorKind {
        self.kind
    }
}

pub(crate) fn require_non_empty(value: &str, field: &'static str) -> Result<(), ContractViolation> {
    if value.trim().is_empty() {
        return Err(ContractViolation::new(
            ErrorKind::InvalidInput,
            format!("{field} must not be empty"),
        ));
    }
    Ok(())
}
