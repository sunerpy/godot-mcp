//! The single policy authority: the only place a production effect permit exists.
//!
//! Every dispatch — read or mutation — is evaluated here before anything is
//! queued and before any boundary is touched. The evaluation is a conjunction of
//! six independent gates, in this fixed order:
//!
//! 1. the operation must have a row in the canonical [matrix](super::matrix);
//! 2. the row's required capability must be advertised by the active profile;
//! 3. the caller's local or authenticated identity must match the identity the
//!    transport boundary bound to this authority;
//! 4. the request's project object identity *and* generation must match the held
//!    project;
//! 5. no transaction may be quarantined; and
//! 6. no unresolved launch survivor may be blocking mutations.
//!
//! Only after all six hold, and only for a row that actually produces a side
//! effect, is a permit minted — through Todo 6's [`PermitAuthority`], which is
//! the sole constructor of [`EffectPermit`]. The policy layer never assembles a
//! permit itself, and no crate outside this one can: [`EffectPermit`] is
//! crate-private and carries an `AuthoritySeal` whose only field is private to
//! the permit module.
//!
//! The permit is then re-checked at the final boundary by
//! [`PolicyAuthority::dispatch_file_effect`] /
//! [`PolicyAuthority::dispatch_boundary_effect`], which re-derive the decision ID
//! from the permit's own facts, refuse a mismatch, and write the non-sensitive
//! decision ID to the durable audit log *before* the boundary is reached.

use std::collections::BTreeMap;
use std::sync::Arc;

use godot_mcp_domain::contracts::{
    Capability, OutcomeError, PolicyDecision, PolicyReason, PolicyVerdict, PrincipalId,
    ProjectIdentity, SessionId,
};
use godot_mcp_domain::manifests::canonical_manifest;
use godot_mcp_domain::{CapabilityProfile, ErrorKind};

use super::decision::{
    BoundIdentity, build_decision, capability_name, decision_id, reason_error_kind, reason_name,
};
use super::matrix::{OperationPolicy, row_of};
use godot_mcp_domain::catalog::ToolDescriptor;
use godot_mcp_engine::supervision::journal::LaunchJournal;
use godot_mcp_engine::supervision::reconcile::mutation_block;

use crate::audit::PolicyAuditDecision;
use crate::effect::{
    EffectError, EffectGateway, EffectOutcome, EffectPermit, PermitAuthority, PermitError,
    PermitRequest, ProcessLaunchRequest,
};
use crate::journal::{EffectBoundaryKind, TransactionStore, TypedProof};

/// Which effect boundaries have a real implementation in this build.
///
/// The editor bridge is not shipped in this release, so `editor` is `false` in
/// production and every editor-plane row resolves to `unavailable` — the matrix
/// row still exists and is still evaluated, so the refusal is honest rather than
/// the operation being pretended out of existence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BoundaryAvailability {
    file: bool,
    process: bool,
    lsp_dap: bool,
    editor: bool,
    runtime: bool,
}

impl BoundaryAvailability {
    /// Build availability from the gateway's own report, so policy and health
    /// reporting can never disagree about which boundary is real.
    pub(crate) fn from_gateway(gateway: &EffectGateway) -> Self {
        let mut availability = Self {
            file: false,
            process: false,
            lsp_dap: false,
            editor: false,
            runtime: false,
        };
        for (kind, available) in gateway.boundary_availability() {
            match kind {
                EffectBoundaryKind::File => availability.file = available,
                EffectBoundaryKind::Process => availability.process = available,
                EffectBoundaryKind::LspDap => availability.lsp_dap = available,
                EffectBoundaryKind::Editor => availability.editor = available,
                EffectBoundaryKind::Runtime => availability.runtime = available,
            }
        }
        availability
    }

    /// Whether one boundary has a real implementation.
    pub(crate) const fn of(self, boundary: EffectBoundaryKind) -> bool {
        match boundary {
            EffectBoundaryKind::File => self.file,
            EffectBoundaryKind::Process => self.process,
            EffectBoundaryKind::LspDap => self.lsp_dap,
            EffectBoundaryKind::Editor => self.editor,
            EffectBoundaryKind::Runtime => self.runtime,
        }
    }
}

/// The unresolved-survivor state the policy layer must consult before it lets any
/// work be queued.
///
/// This is Todo 17's mutation block, surfaced as a trait so the policy layer
/// consults a *real* launch journal in production instead of assuming there is
/// nothing to block on.
pub(crate) trait SurvivorProbe: Send + Sync {
    /// `Some(reason)` when an unresolved survivor blocks mutations.
    ///
    /// # Errors
    /// Returns a caller-safe message when the survivor state cannot be read. An
    /// unreadable survivor state is a refusal, never an assumed "clear".
    fn unresolved_survivor(&self) -> Result<Option<String>, String>;
}

/// The production probe over Todo 17's launch journal.
///
/// It calls Todo 17's own `mutation_block`; the policy layer does not reimplement
/// survivor classification and cannot disagree with the engine about it.
pub(crate) struct LaunchJournalSurvivors<Journal> {
    journal: Journal,
}

impl<Journal> LaunchJournalSurvivors<Journal>
where
    Journal: LaunchJournal + Send + Sync,
{
    pub(crate) const fn new(journal: Journal) -> Self {
        Self { journal }
    }
}

impl<Journal> SurvivorProbe for LaunchJournalSurvivors<Journal>
where
    Journal: LaunchJournal + Send + Sync,
{
    fn unresolved_survivor(&self) -> Result<Option<String>, String> {
        mutation_block(&self.journal).map_err(|error| error.to_string())
    }
}

/// A build with no process plane attached: there is no launch journal to consult,
/// so there is provably no launch survivor to block on.
///
/// This is *not* a stand-in for an unreadable journal: it states that no launch
/// has ever been journaled through this authority, which is exactly the case for
/// a file-only dispatch lane.
pub(crate) struct NoLaunchPlane;

impl SurvivorProbe for NoLaunchPlane {
    fn unresolved_survivor(&self) -> Result<Option<String>, String> {
        Ok(None)
    }
}

/// One dispatch the policy layer must evaluate.
#[derive(Debug, Clone)]
pub(crate) struct DispatchRequest {
    /// Canonical tool name.
    pub(crate) tool: String,
    /// Project identity the transport boundary presented.
    pub(crate) project: ProjectIdentity,
    /// Local or authenticated principal the transport boundary presented.
    pub(crate) principal: Option<PrincipalId>,
    /// Session the transport boundary presented.
    pub(crate) session: Option<SessionId>,
    /// The already-journaled transaction a mutating dispatch will execute.
    pub(crate) transaction_id: Option<String>,
}

/// A production permit plus the decision that authorized it.
///
/// The struct is crate-private and has no public constructor: the only way to
/// obtain one is [`PolicyAuthority::authorize`]. Its permit field is Todo 6's
/// sealed [`EffectPermit`], so even inside this crate no other module can
/// assemble one.
pub(crate) struct PolicyPermit {
    permit: EffectPermit,
    decision_id: String,
    tool: String,
    capability: Capability,
    boundary: EffectBoundaryKind,
}

impl PolicyPermit {
    /// Non-sensitive decision ID this permit was issued under.
    pub(crate) fn decision_id(&self) -> &str {
        &self.decision_id
    }

    /// The boundary this permit authorizes.
    pub(crate) const fn boundary(&self) -> EffectBoundaryKind {
        self.boundary
    }

    /// Canonical tool the permit was issued for.
    pub(crate) fn tool(&self) -> &str {
        &self.tool
    }
}

/// The result of one successful evaluation.
pub(crate) struct PolicyGrant {
    /// The public decision. It carries no permit and no credential.
    pub(crate) decision: PolicyDecision,
    /// The production permit, present only for a mutating operation.
    pub(crate) permit: Option<PolicyPermit>,
    /// The matrix row that was applied.
    pub(crate) row: OperationPolicy,
    pub(crate) audit_decision: PolicyAuditDecision,
}

/// Why a dispatch was refused, with its stable error class.
#[derive(Debug, thiserror::Error)]
pub(crate) enum PolicyRefusal {
    /// The tool is not a canonical operation, so no row grants it anything.
    #[error("operation {tool} is not a canonical capability-matrix operation")]
    UnknownOperation { tool: String },
    /// The active profile does not advertise the required capability.
    #[error("profile {profile} does not advertise capability {capability} required by {tool}")]
    MissingCapability {
        tool: String,
        profile: &'static str,
        capability: &'static str,
    },
    /// The presented identity is not the identity bound to this authority.
    #[error("presented {field} does not match the identity bound to this authority")]
    IdentityMismatch {
        field: &'static str,
        reason: PolicyReason,
    },
    /// The presented project object identity or generation is not the held one.
    #[error("presented project object identity or generation does not match the held project")]
    ProjectGenerationMismatch,
    /// A quarantined transaction blocks all work.
    #[error("project has quarantined transactions ({0:?}); every dispatch is refused")]
    Quarantined(Vec<String>),
    /// An unresolved launch survivor blocks all mutations.
    #[error("{0}")]
    UnresolvedSurvivor(String),
    /// The boundary this operation needs has no implementation in this build.
    #[error("the {boundary} effect boundary is not available in this build")]
    BoundaryUnavailable { boundary: &'static str },
    /// The permit authority refused.
    #[error("permit was refused: {0}")]
    Permit(String),
    /// The decision could not be published.
    #[error("policy decision could not be published: {0}")]
    Decision(String),
    /// A permit reached the final boundary without matching its own decision.
    #[error("permit does not match its policy decision at the final boundary: {0}")]
    PermitMismatch(String),
}

impl PolicyRefusal {
    /// Stable domain error class. Never `internal` for an authorization refusal.
    pub(crate) fn error_kind(&self) -> ErrorKind {
        match self {
            Self::UnknownOperation { .. } => ErrorKind::NotFound,
            Self::MissingCapability { .. } | Self::PermitMismatch(_) => ErrorKind::PermissionDenied,
            Self::IdentityMismatch { reason, .. } => reason_error_kind(*reason),
            Self::ProjectGenerationMismatch
            | Self::Quarantined(_)
            | Self::UnresolvedSurvivor(_) => ErrorKind::Conflict,
            Self::BoundaryUnavailable { .. } => ErrorKind::Unavailable,
            Self::Permit(_) => ErrorKind::PermissionDenied,
            Self::Decision(_) => ErrorKind::Internal,
        }
    }

    /// The frozen public reason this refusal reports.
    pub(crate) const fn policy_reason(&self) -> PolicyReason {
        match self {
            // A tool with no row has no capability to be missing; it is reported
            // as a missing capability because that is the only frozen reason that
            // says "this caller may not have this operation".
            Self::UnknownOperation { .. }
            | Self::MissingCapability { .. }
            | Self::Permit(_)
            | Self::Decision(_)
            | Self::PermitMismatch(_) => PolicyReason::MissingCapability,
            Self::IdentityMismatch { reason, .. } => *reason,
            Self::ProjectGenerationMismatch => PolicyReason::ProjectGenerationMismatch,
            // An unresolved survivor is the same class of write block as a
            // quarantined transaction and is reported with the same frozen
            // reason; the message distinguishes them.
            Self::Quarantined(_) | Self::UnresolvedSurvivor(_) => PolicyReason::ProjectQuarantined,
            Self::BoundaryUnavailable { .. } => PolicyReason::SchedulerUnavailable,
        }
    }
}

/// The one policy authority for one held project.
pub(crate) struct PolicyAuthority {
    profile: CapabilityProfile,
    identity: BoundIdentity,
    permits: PermitAuthority,
    store: Arc<TransactionStore>,
    availability: BoundaryAvailability,
    survivors: Box<dyn SurvivorProbe>,
    epoch: u64,
}

impl PolicyAuthority {
    /// Assemble the authority over one held project and one bound identity.
    pub(crate) fn new(
        profile: CapabilityProfile,
        identity: BoundIdentity,
        store: Arc<TransactionStore>,
        availability: BoundaryAvailability,
        survivors: Box<dyn SurvivorProbe>,
        epoch: u64,
    ) -> Self {
        Self {
            profile,
            identity,
            permits: PermitAuthority::new(Arc::clone(&store), epoch),
            store,
            availability,
            survivors,
            epoch,
        }
    }

    /// The active profile.
    pub(crate) const fn profile(&self) -> CapabilityProfile {
        self.profile
    }

    /// Evaluate one dispatch. Nothing is queued and no boundary is touched until
    /// this returns `Ok`.
    pub(crate) fn authorize(
        &self,
        request: &DispatchRequest,
    ) -> Result<PolicyGrant, PolicyRefusal> {
        let descriptor = godot_mcp_domain::catalog::descriptor(&request.tool).ok_or_else(|| {
            PolicyRefusal::UnknownOperation {
                tool: request.tool.clone(),
            }
        })?;
        self.authorize_descriptor(request, descriptor)
    }

    /// Evaluate one request against an already-resolved catalog descriptor.
    ///
    /// The name lookup happens exactly once, at the entry point; every gate below
    /// reads the descriptor's own row, so a request cannot be authorized under
    /// one operation shape and executed under another.
    pub(crate) fn authorize_descriptor(
        &self,
        request: &DispatchRequest,
        descriptor: &ToolDescriptor,
    ) -> Result<PolicyGrant, PolicyRefusal> {
        if descriptor.name != request.tool {
            return Err(PolicyRefusal::UnknownOperation {
                tool: request.tool.clone(),
            });
        }
        let row = row_of(descriptor);

        // 2. Capability. A `core` profile advertises only `read`, so every
        //    mutation row fails here before any other work happens.
        if !canonical_manifest(self.profile)
            .capabilities
            .contains(&row.capability)
        {
            return Err(PolicyRefusal::MissingCapability {
                tool: request.tool.clone(),
                profile: self.profile.manifest_name(),
                capability: capability_name(row.capability),
            });
        }

        // 3. Identity. The bound identity comes from the transport's local or
        //    authenticated boundary; a profile name and a same-host location are
        //    never accepted as identity.
        if request.principal.as_ref() != self.identity.principal.as_ref() {
            return Err(PolicyRefusal::IdentityMismatch {
                field: "principal",
                reason: PolicyReason::PrincipalMismatch,
            });
        }
        if request.session.as_ref() != self.identity.session.as_ref() {
            return Err(PolicyRefusal::IdentityMismatch {
                field: "session",
                reason: PolicyReason::SessionMismatch,
            });
        }

        // 4. Project object identity and generation.
        if request.project.object_id() != self.identity.project.object_id()
            || request.project.generation() != self.identity.project.generation()
        {
            return Err(PolicyRefusal::ProjectGenerationMismatch);
        }

        // 5. Quarantine.
        let quarantined = self
            .store
            .quarantined_transactions()
            .map_err(|error| PolicyRefusal::Permit(error.to_string()))?;
        if !quarantined.is_empty() {
            return Err(PolicyRefusal::Quarantined(quarantined));
        }

        // 6. Unresolved launch survivor.
        if let Some(reason) = self
            .survivors
            .unresolved_survivor()
            .map_err(PolicyRefusal::UnresolvedSurvivor)?
        {
            return Err(PolicyRefusal::UnresolvedSurvivor(reason));
        }

        // The boundary the row needs must actually exist in this build. The
        // editor plane is mapped completely and honestly, and refuses here.
        if let Some(boundary) = row.boundary
            && !self.availability.of(boundary)
        {
            return Err(PolicyRefusal::BoundaryUnavailable {
                boundary: boundary_name(boundary),
            });
        }

        let decision = build_decision(
            &self.identity,
            &request.tool,
            row.capability,
            PolicyReason::Granted,
            self.epoch,
        )
        .map_err(|error| PolicyRefusal::Decision(error.to_string()))?;
        let granted_decision_id = decision_id(
            &self.identity,
            &request.tool,
            row.capability,
            PolicyVerdict::Allow,
            PolicyReason::Granted,
            self.epoch,
        );

        let permit = match (row.family, row.boundary) {
            (Some(family), Some(boundary)) => {
                let transaction_id = request.transaction_id.clone().ok_or_else(|| {
                    PolicyRefusal::Permit(
                        "a mutating dispatch must name its already-journaled transaction"
                            .to_owned(),
                    )
                })?;
                let permit = self
                    .permits
                    .issue(PermitRequest {
                        transaction_id,
                        capability: row.capability,
                        operation_family: family,
                        boundary,
                        project_object_identity: self.identity.project.object_id().to_string(),
                        project_generation: self.identity.project.generation(),
                    })
                    .map_err(permit_refusal)?;
                Some(PolicyPermit {
                    permit,
                    decision_id: granted_decision_id.clone(),
                    tool: request.tool.clone(),
                    capability: row.capability,
                    boundary,
                })
            }
            // A read-only row mints nothing: there is no permit to leak and no
            // boundary write to authorize.
            _ => None,
        };

        Ok(PolicyGrant {
            decision,
            permit,
            row,
            audit_decision: PolicyAuditDecision {
                capability: row.capability,
                verdict: PolicyVerdict::Allow,
                reason: PolicyReason::Granted,
                decision_id: granted_decision_id,
            },
        })
    }

    /// Publish one refusal as a public decision, for audit and for the caller.
    pub(crate) fn refusal_decision(
        &self,
        tool: &str,
        capability: Capability,
        refusal: &PolicyRefusal,
    ) -> Result<PolicyDecision, PolicyRefusal> {
        build_decision(
            &self.identity,
            tool,
            capability,
            refusal.policy_reason(),
            self.epoch,
        )
        .map_err(|error| PolicyRefusal::Decision(error.to_string()))
    }

    pub(crate) fn refusal_audit_decision(
        &self,
        tool: &str,
        capability: Capability,
        refusal: &PolicyRefusal,
    ) -> PolicyAuditDecision {
        let reason = refusal.policy_reason();
        PolicyAuditDecision {
            capability,
            verdict: PolicyVerdict::Deny,
            reason,
            decision_id: decision_id(
                &self.identity,
                tool,
                capability,
                PolicyVerdict::Deny,
                reason,
                self.epoch,
            ),
        }
    }

    /// Re-check one permit at the final boundary and record its decision ID.
    ///
    /// This runs *before* the gateway is entered, so a permit that fails here
    /// never reaches a boundary. Three facts are re-checked against live state
    /// rather than trusted from the permit: the authority epoch, the held project
    /// object identity and generation, and the decision ID re-derived from the
    /// permit's own facts.
    fn enforce_at_boundary(
        &self,
        permit: &PolicyPermit,
        boundary: EffectBoundaryKind,
    ) -> Result<(), PolicyRefusal> {
        if permit.permit.epoch() != self.epoch {
            return Err(PolicyRefusal::PermitMismatch(
                "permit was issued by a superseded authority epoch".to_owned(),
            ));
        }
        if permit.permit.project_object_identity() != self.identity.project.object_id().as_str()
            || permit.permit.project_generation() != self.identity.project.generation()
        {
            return Err(PolicyRefusal::PermitMismatch(
                "permit does not match the held project object identity or generation".to_owned(),
            ));
        }
        if permit.boundary != boundary || permit.permit.boundary() != boundary {
            return Err(PolicyRefusal::PermitMismatch(
                "permit authorizes a different effect boundary".to_owned(),
            ));
        }
        if !self.availability.of(boundary) {
            return Err(PolicyRefusal::BoundaryUnavailable {
                boundary: boundary_name(boundary),
            });
        }
        let expected = decision_id(
            &self.identity,
            &permit.tool,
            permit.capability,
            PolicyVerdict::Allow,
            PolicyReason::Granted,
            self.epoch,
        );
        if expected != permit.decision_id {
            return Err(PolicyRefusal::PermitMismatch(
                "decision ID re-derived at the boundary does not match the permit".to_owned(),
            ));
        }
        let quarantined = self
            .store
            .quarantined_transactions()
            .map_err(|error| PolicyRefusal::Permit(error.to_string()))?;
        if !quarantined.is_empty() {
            return Err(PolicyRefusal::Quarantined(quarantined));
        }
        if let Some(reason) = self
            .survivors
            .unresolved_survivor()
            .map_err(PolicyRefusal::UnresolvedSurvivor)?
        {
            return Err(PolicyRefusal::UnresolvedSurvivor(reason));
        }
        // The non-sensitive decision ID is written before the boundary is
        // touched, so no effect can exist without a preceding audit record
        // naming the decision that authorized it.
        self.store
            .audit(
                permit.permit.transaction_id(),
                "policy_decision_enforced",
                &format!(
                    "decision_id={} tool={} capability={} boundary={} verdict=allow reason={}",
                    permit.decision_id,
                    permit.tool,
                    capability_name(permit.capability),
                    boundary_name(boundary),
                    reason_name(PolicyReason::Granted)
                ),
            )
            .map_err(|error| PolicyRefusal::Permit(error.to_string()))?;
        Ok(())
    }

    /// Execute one staged `file` effect under a policy permit.
    pub(crate) fn dispatch_file_effect(
        &self,
        gateway: &EffectGateway,
        permit: &PolicyPermit,
    ) -> Result<EffectOutcome, PolicyDispatchError> {
        self.dispatch_file_effect_removing(gateway, permit, &[])
    }

    /// Execute one staged `file` effect under a policy permit, removing the
    /// proven delete paths in `removals`.
    pub(crate) fn dispatch_file_effect_removing(
        &self,
        gateway: &EffectGateway,
        permit: &PolicyPermit,
        removals: &[String],
    ) -> Result<EffectOutcome, PolicyDispatchError> {
        self.enforce_at_boundary(permit, EffectBoundaryKind::File)
            .map_err(PolicyDispatchError::Refused)?;
        gateway
            .execute_file_effect_removing(&permit.permit, removals)
            .map_err(PolicyDispatchError::Effect)
    }

    /// Attempt one non-`file` boundary under a policy permit.
    pub(crate) fn dispatch_boundary_effect(
        &self,
        gateway: &EffectGateway,
        permit: &PolicyPermit,
    ) -> Result<TypedProof, PolicyDispatchError> {
        let boundary = permit.boundary;
        self.enforce_at_boundary(permit, boundary)
            .map_err(PolicyDispatchError::Refused)?;
        gateway
            .attempt_boundary_effect(&permit.permit, boundary)
            .map_err(PolicyDispatchError::Effect)
    }

    pub(crate) fn dispatch_process_effect(
        &self,
        gateway: &EffectGateway,
        permit: &PolicyPermit,
        request: &ProcessLaunchRequest,
    ) -> Result<crate::effect::ProcessEffectOutcome, PolicyDispatchError> {
        self.enforce_at_boundary(permit, EffectBoundaryKind::Process)
            .map_err(PolicyDispatchError::Refused)?;
        gateway
            .execute_process_effect(&permit.permit, request)
            .map_err(PolicyDispatchError::Effect)
    }

    /// Re-check `permit` at the editor boundary and send one journaled
    /// live-editor mutation.
    pub(crate) fn dispatch_editor_effect(
        &self,
        gateway: &EffectGateway,
        permit: &PolicyPermit,
        mutation: &crate::effect::boundary::EditorMutation,
    ) -> Result<crate::effect::boundary::EditorEffectOutcome, PolicyDispatchError> {
        self.enforce_at_boundary(permit, EffectBoundaryKind::Editor)
            .map_err(PolicyDispatchError::Refused)?;
        gateway
            .execute_editor_effect(&permit.permit, mutation)
            .map_err(PolicyDispatchError::Effect)
    }

    /// Re-check `permit` at the process boundary and start one managed
    /// session.
    pub(crate) fn dispatch_session_start(
        &self,
        gateway: &EffectGateway,
        permit: &PolicyPermit,
        request: &crate::effect::boundary::SessionStartRequest,
    ) -> Result<crate::effect::boundary::SessionStartOutcome, PolicyDispatchError> {
        self.enforce_at_boundary(permit, EffectBoundaryKind::Process)
            .map_err(PolicyDispatchError::Refused)?;
        gateway
            .execute_session_start(&permit.permit, request)
            .map_err(PolicyDispatchError::Effect)
    }

    /// Re-check `permit` at the process boundary and stop one managed session
    /// whose stop is a process effect.
    pub(crate) fn dispatch_session_stop(
        &self,
        gateway: &EffectGateway,
        permit: &PolicyPermit,
        command: &crate::effect::boundary::EditorMutation,
    ) -> Result<crate::effect::boundary::EditorEffectOutcome, PolicyDispatchError> {
        self.enforce_at_boundary(permit, EffectBoundaryKind::Process)
            .map_err(PolicyDispatchError::Refused)?;
        gateway
            .execute_session_stop(&permit.permit, command)
            .map_err(PolicyDispatchError::Effect)
    }

    /// Re-check `permit` at the LSP/DAP boundary and carry out one journaled
    /// command to a managed editor session's language server or debug
    /// adapter.
    pub(crate) fn dispatch_debug_effect(
        &self,
        gateway: &EffectGateway,
        permit: &PolicyPermit,
        command: &crate::effect::boundary::EditorMutation,
    ) -> Result<crate::effect::boundary::EditorEffectOutcome, PolicyDispatchError> {
        self.enforce_at_boundary(permit, EffectBoundaryKind::LspDap)
            .map_err(PolicyDispatchError::Refused)?;
        gateway
            .execute_debug_effect(&permit.permit, command)
            .map_err(PolicyDispatchError::Effect)
    }

    /// Re-check `permit` at the runtime boundary and carry out one journaled
    /// command to a managed game session.
    pub(crate) fn dispatch_runtime_effect(
        &self,
        gateway: &EffectGateway,
        permit: &PolicyPermit,
        command: &crate::effect::boundary::EditorMutation,
    ) -> Result<crate::effect::boundary::EditorEffectOutcome, PolicyDispatchError> {
        self.enforce_at_boundary(permit, EffectBoundaryKind::Runtime)
            .map_err(PolicyDispatchError::Refused)?;
        gateway
            .execute_runtime_effect(&permit.permit, command)
            .map_err(PolicyDispatchError::Effect)
    }
}

/// A dispatch failed either at the policy re-check or inside the gateway.
#[derive(Debug, thiserror::Error)]
pub(crate) enum PolicyDispatchError {
    /// The policy layer refused before the boundary was reached.
    #[error(transparent)]
    Refused(PolicyRefusal),
    /// The gateway or a boundary refused.
    #[error(transparent)]
    Effect(EffectError),
}

impl PolicyDispatchError {
    /// Stable domain error class.
    pub(crate) fn error_kind(&self) -> ErrorKind {
        match self {
            Self::Refused(refusal) => refusal.error_kind(),
            Self::Effect(error) => error.to_outcome().kind(),
        }
    }

    pub(crate) fn to_outcome(&self) -> OutcomeError {
        match self {
            Self::Refused(refusal) => OutcomeError::new(
                refusal.error_kind(),
                refusal.to_string(),
                false,
                BTreeMap::new(),
            ),
            Self::Effect(error) => error.to_outcome(),
        }
    }
}

fn permit_refusal(error: PermitError) -> PolicyRefusal {
    match error {
        PermitError::Quarantined(ids) => PolicyRefusal::Quarantined(ids),
        other => PolicyRefusal::Permit(other.to_string()),
    }
}

/// Stable snake_case name of one effect boundary.
pub(crate) const fn boundary_name(boundary: EffectBoundaryKind) -> &'static str {
    match boundary {
        EffectBoundaryKind::File => "file",
        EffectBoundaryKind::Process => "process",
        EffectBoundaryKind::LspDap => "lsp_dap",
        EffectBoundaryKind::Editor => "editor",
        EffectBoundaryKind::Runtime => "runtime",
    }
}

#[cfg(test)]
mod tests {
    use godot_mcp_engine::supervision::journal::{
        Disposition, JournalError, LaunchRecord, ProcessLaunchProof,
    };

    use super::*;

    struct StubLaunchJournal {
        quarantined: Vec<String>,
    }

    impl LaunchJournal for StubLaunchJournal {
        fn write_phase(&self, _record: &LaunchRecord) -> Result<(), JournalError> {
            Ok(())
        }

        fn settle(
            &self,
            _record: &LaunchRecord,
            _disposition: Disposition,
            _reason: &str,
            _proof: Option<&ProcessLaunchProof>,
        ) -> Result<(), JournalError> {
            Ok(())
        }

        fn open_launches(&self) -> Result<Vec<LaunchRecord>, JournalError> {
            Ok(Vec::new())
        }

        fn committed_launches(&self) -> Result<Vec<LaunchRecord>, JournalError> {
            Ok(Vec::new())
        }

        fn quarantined(&self) -> Result<Vec<String>, JournalError> {
            Ok(self.quarantined.clone())
        }
    }

    #[test]
    fn a_file_only_lane_has_provably_no_launch_survivor() {
        assert_eq!(NoLaunchPlane.unresolved_survivor(), Ok(None));
    }

    #[test]
    fn the_production_probe_surfaces_the_engines_own_mutation_block() {
        let clear = LaunchJournalSurvivors::new(StubLaunchJournal {
            quarantined: Vec::new(),
        });
        assert_eq!(clear.unresolved_survivor(), Ok(None));

        let blocked = LaunchJournalSurvivors::new(StubLaunchJournal {
            quarantined: vec!["00000000-0000-4000-8000-000000002601".to_owned()],
        });
        let reason = blocked
            .unresolved_survivor()
            .expect("the stub journal is readable")
            .expect("a quarantined launch blocks mutations");
        assert!(
            reason.contains("unresolved launch survivor"),
            "the engine's own wording must be surfaced unchanged: {reason}"
        );
        assert!(
            reason.contains("00000000-0000-4000-8000-000000002601"),
            "the blocking transaction must be named: {reason}"
        );
    }

    #[test]
    fn process_dispatch_preserves_gateway_timeout_outcome() {
        let error = PolicyDispatchError::Effect(EffectError::OperationDeadline(
            crate::scheduler::OperationDeadline::expired_error(),
        ));

        let outcome = error.to_outcome();

        assert_eq!(outcome.code(), "timeout");
        assert!(!outcome.retryable());
        assert_eq!(
            outcome.message(),
            "the operation exceeded its execution deadline; its effect is not yet proven"
        );
    }
}
