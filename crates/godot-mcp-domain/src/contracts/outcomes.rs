//! Outcome-oriented result, health, scheduler, effect, and policy contracts.

use std::collections::{BTreeMap, BTreeSet};

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};

use super::{
    ArtifactId, ContractVersion, ContractViolation, GodotValue, PrincipalId, ProjectObjectId,
    Provenance, ResourceUri, SessionId, require_non_empty,
};
use crate::manifests::ProfileManifest;
use crate::objid::LeaseKey;
use crate::{CONTRACT_VERSION, ErrorKind};

/// Public project identity derived from Todo 2's held-object lease identity.
///
/// The public form is a one-way opaque digest plus generation. A path spelling
/// is deliberately not a component of identity and cannot be serialized here.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct ProjectIdentity {
    schema_version: ContractVersion,
    object_id: ProjectObjectId,
    generation: u64,
}

impl ProjectIdentity {
    /// Adapt Todo 2's root + project-file held-object identity into a public ID.
    #[must_use]
    pub fn from_lease_key(key: LeaseKey, generation: u64) -> Self {
        Self {
            schema_version: ContractVersion::current(),
            object_id: ProjectObjectId::from_lease_key(key),
            generation,
        }
    }

    /// Opaque stable object identity.
    #[must_use]
    pub fn object_id(&self) -> &ProjectObjectId {
        &self.object_id
    }

    /// Current project generation.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }
}

/// A public static result. Provenance and confidence cannot be omitted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StaticResult<T> {
    schema_version: ContractVersion,
    project: ProjectIdentity,
    data: T,
    provenance: Provenance,
}

impl<T> StaticResult<T> {
    /// Construct a public static result.
    #[must_use]
    pub fn new(project: ProjectIdentity, data: T, provenance: Provenance) -> Self {
        Self {
            schema_version: ContractVersion::current(),
            project,
            data,
            provenance,
        }
    }

    /// Evidence and confidence supporting the answer.
    #[must_use]
    pub const fn provenance(&self) -> &Provenance {
        &self.provenance
    }
}

/// Stable public capability identifiers.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Read,
    ProjectWrite,
    EditorMutate,
    RuntimeExecute,
    ExportDestructive,
}

/// Public operation plane, expressed by outcome rather than implementation.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum OperationPlane {
    Static,
    Project,
    Editor,
    Runtime,
}

/// Public operation class used for effects and policy.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum OperationClass {
    Read,
    ProjectMutation,
    EditorMutation,
    RuntimeExecution,
    DestructiveExport,
}

/// Stable disposition of an attempted effect.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum EffectDisposition {
    NoChange,
    Committed,
    Compensated,
    Quarantined,
    Cancelled,
}

/// Public summary of an operation's effects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OperationEffect {
    schema_version: ContractVersion,
    operation: String,
    plane: OperationPlane,
    class: OperationClass,
    disposition: EffectDisposition,
    affected_resources: BTreeSet<ResourceUri>,
    artifacts: BTreeSet<ArtifactId>,
}

impl OperationEffect {
    /// Construct a deterministic operation-effect summary.
    #[must_use]
    pub fn new(
        operation: impl Into<String>,
        plane: OperationPlane,
        class: OperationClass,
        disposition: EffectDisposition,
        affected_resources: BTreeSet<ResourceUri>,
        artifacts: BTreeSet<ArtifactId>,
    ) -> Self {
        Self {
            schema_version: ContractVersion::current(),
            operation: operation.into(),
            plane,
            class,
            disposition,
            affected_resources,
            artifacts,
        }
    }
}

/// Overall or component health state.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    Ready,
    Degraded,
    Unavailable,
}

/// Named public health component.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ComponentName {
    Project,
    Index,
    Godot,
    Editor,
    Runtime,
    Scheduler,
    Authorization,
}

/// One bounded health component result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ComponentHealth {
    component: ComponentName,
    status: HealthStatus,
    message: Option<String>,
}

impl ComponentHealth {
    #[must_use]
    pub fn new(component: ComponentName, status: HealthStatus, message: Option<String>) -> Self {
        Self {
            component,
            status,
            message,
        }
    }
}

/// Public health result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HealthReport {
    schema_version: ContractVersion,
    project: ProjectIdentity,
    status: HealthStatus,
    components: Vec<ComponentHealth>,
    observed_at_unix_ms: u64,
}

impl HealthReport {
    #[must_use]
    pub fn new(
        project: ProjectIdentity,
        status: HealthStatus,
        components: Vec<ComponentHealth>,
        observed_at_unix_ms: u64,
    ) -> Self {
        Self {
            schema_version: ContractVersion::current(),
            project,
            status,
            components,
            observed_at_unix_ms,
        }
    }
}

/// Public capability result with source and confidence evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CapabilitiesReport {
    schema_version: ContractVersion,
    project: ProjectIdentity,
    manifest: ProfileManifest,
    available: BTreeSet<Capability>,
    provenance: Provenance,
}

impl CapabilitiesReport {
    #[must_use]
    pub fn new(
        project: ProjectIdentity,
        manifest: ProfileManifest,
        available: BTreeSet<Capability>,
        provenance: Provenance,
    ) -> Self {
        Self {
            schema_version: ContractVersion::current(),
            project,
            manifest,
            available,
            provenance,
        }
    }
}

/// Scheduler admission state.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SchedulerMode {
    Accepting,
    Draining,
    Quarantined,
    Stopped,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SchedulerWire {
    schema_version: ContractVersion,
    project: ProjectIdentity,
    mode: SchedulerMode,
    running: u32,
    queued: u32,
    concurrency_limit: u32,
    queue_limit: u32,
    observed_at_unix_ms: u64,
}

/// Public bounded scheduler state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SchedulerState {
    schema_version: ContractVersion,
    project: ProjectIdentity,
    mode: SchedulerMode,
    running: u32,
    queued: u32,
    concurrency_limit: u32,
    queue_limit: u32,
    observed_at_unix_ms: u64,
}

impl SchedulerState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        project: ProjectIdentity,
        mode: SchedulerMode,
        running: u32,
        queued: u32,
        concurrency_limit: u32,
        queue_limit: u32,
        observed_at_unix_ms: u64,
    ) -> Result<Self, ContractViolation> {
        if concurrency_limit == 0 || queue_limit == 0 {
            return Err(ContractViolation::new(
                ErrorKind::InvalidInput,
                "scheduler limits must be positive",
            ));
        }
        if running > concurrency_limit || queued > queue_limit {
            return Err(ContractViolation::new(
                ErrorKind::InvalidInput,
                "scheduler counters exceed their declared limits",
            ));
        }
        Ok(Self {
            schema_version: ContractVersion::current(),
            project,
            mode,
            running,
            queued,
            concurrency_limit,
            queue_limit,
            observed_at_unix_ms,
        })
    }
}

impl<'de> Deserialize<'de> for SchedulerState {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = SchedulerWire::deserialize(deserializer)?;
        if wire.schema_version != ContractVersion::current() {
            return Err(D::Error::custom("unsupported scheduler schema version"));
        }
        Self::new(
            wire.project,
            wire.mode,
            wire.running,
            wire.queued,
            wire.concurrency_limit,
            wire.queue_limit,
            wire.observed_at_unix_ms,
        )
        .map_err(D::Error::custom)
    }
}

/// Public policy verdict.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum PolicyVerdict {
    Allow,
    Deny,
}

/// Stable, non-sensitive reason for a policy verdict.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum PolicyReason {
    Granted,
    MissingCapability,
    PrincipalMismatch,
    SessionMismatch,
    ProjectGenerationMismatch,
    ProjectQuarantined,
    SchedulerUnavailable,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyWire {
    schema_version: ContractVersion,
    decision_id: String,
    project: ProjectIdentity,
    principal: Option<PrincipalId>,
    session: Option<SessionId>,
    required_capability: Capability,
    verdict: PolicyVerdict,
    reason: PolicyReason,
}

/// Public policy decision without credentials, permits, or implementation data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PolicyDecision {
    schema_version: ContractVersion,
    decision_id: String,
    project: ProjectIdentity,
    principal: Option<PrincipalId>,
    session: Option<SessionId>,
    required_capability: Capability,
    verdict: PolicyVerdict,
    reason: PolicyReason,
}

impl PolicyDecision {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        decision_id: impl Into<String>,
        project: ProjectIdentity,
        principal: Option<PrincipalId>,
        session: Option<SessionId>,
        required_capability: Capability,
        verdict: PolicyVerdict,
        reason: PolicyReason,
    ) -> Result<Self, ContractViolation> {
        let decision_id = decision_id.into();
        require_non_empty(&decision_id, "policy decision ID")?;
        let reason_is_granted = reason == PolicyReason::Granted;
        if (verdict == PolicyVerdict::Allow) != reason_is_granted {
            return Err(ContractViolation::new(
                ErrorKind::InvalidInput,
                "allow requires granted and deny requires a denial reason",
            ));
        }
        Ok(Self {
            schema_version: ContractVersion::current(),
            decision_id,
            project,
            principal,
            session,
            required_capability,
            verdict,
            reason,
        })
    }
}

impl<'de> Deserialize<'de> for PolicyDecision {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = PolicyWire::deserialize(deserializer)?;
        if wire.schema_version != ContractVersion::current() {
            return Err(D::Error::custom("unsupported policy schema version"));
        }
        Self::new(
            wire.decision_id,
            wire.project,
            wire.principal,
            wire.session,
            wire.required_capability,
            wire.verdict,
            wire.reason,
        )
        .map_err(D::Error::custom)
    }
}

/// Stable public outcome error.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OutcomeError {
    schema_version: ContractVersion,
    kind: ErrorKind,
    message: String,
    retryable: bool,
    details: BTreeMap<String, GodotValue>,
}

impl OutcomeError {
    #[must_use]
    pub fn new(
        kind: ErrorKind,
        message: impl Into<String>,
        retryable: bool,
        details: BTreeMap<String, GodotValue>,
    ) -> Self {
        Self {
            schema_version: ContractVersion::current(),
            kind,
            message: message.into(),
            retryable,
            details,
        }
    }

    /// Stable wire code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        self.kind.code()
    }

    /// Stable domain error category.
    #[must_use]
    pub const fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// Caller-safe error message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Whether retrying the same operation may succeed.
    #[must_use]
    pub const fn retryable(&self) -> bool {
        self.retryable
    }

    /// Structured caller-safe details.
    #[must_use]
    pub const fn details(&self) -> &BTreeMap<String, GodotValue> {
        &self.details
    }
}

impl std::fmt::Display for OutcomeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for OutcomeError {}

const _: () = assert!(CONTRACT_VERSION.len() == 5);
