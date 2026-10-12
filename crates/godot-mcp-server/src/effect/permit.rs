//! Permit types and their two — and only two — issuing authorities.
//!
//! There is no public constructor for either permit type: both live in this
//! crate-private module, both carry an unforgeable private seal field, and both
//! are only reachable through an authority value. An adapter, a plugin or any
//! external crate therefore cannot construct a permit at all, which is what makes
//! the effect gateway's authorization non-bypassable.
//!
//! Two authorities exist:
//!
//! * [`PermitAuthority`] — the ordinary, capability-profile-driven authority. Its
//!   sole authorized caller is the Todo 26 policy layer. It refuses to issue any
//!   permit while a transaction is `quarantined`, unconditionally.
//! * [`OperatorRecoveryAuthority`] — a distinct, non-profile-driven authority
//!   whose only product is a [`RepairPermit`]. Its sole authorized caller is the
//!   Todo 34 local-only `transaction repair` subcommand, which must first acquire
//!   the Todo 2 project-lease advisory OS lock. It is never reachable from an
//!   MCP/HTTP/network handler and has no capability-profile mapping.

use std::sync::Arc;

use godot_mcp_domain::contracts::Capability;

use crate::journal::{
    EffectBoundaryKind, EffectKind, OperationFamily, Phase, StoreError, TransactionStore,
};

/// Unforgeable proof that a value was produced inside this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AuthoritySeal(());

/// An ordinary, capability-profile-driven effect permit.
///
/// A permit is scoped to exactly one journaled transaction, one capability, one
/// operation family, one effect boundary and one project generation. It also
/// carries the issuing authority's epoch, so a permit minted before a
/// project-generation change or a quarantine event is *stale* and rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EffectPermit {
    transaction_id: String,
    capability: Capability,
    operation_family: OperationFamily,
    boundary: EffectBoundaryKind,
    project_object_identity: String,
    project_generation: u64,
    epoch: u64,
    seal: AuthoritySeal,
}

impl EffectPermit {
    pub(crate) fn transaction_id(&self) -> &str {
        &self.transaction_id
    }

    pub(crate) const fn capability(&self) -> Capability {
        self.capability
    }

    pub(crate) const fn operation_family(&self) -> OperationFamily {
        self.operation_family
    }

    pub(crate) const fn boundary(&self) -> EffectBoundaryKind {
        self.boundary
    }

    pub(crate) fn project_object_identity(&self) -> &str {
        &self.project_object_identity
    }

    pub(crate) const fn project_generation(&self) -> u64 {
        self.project_generation
    }

    pub(crate) const fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The authority seal. Its type has a private unit field, so a permit-shaped
    /// value cannot be assembled outside this module even within the crate.
    pub(crate) const fn seal(&self) -> AuthoritySeal {
        self.seal
    }
}

/// The complete, immutable request for an ordinary permit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PermitRequest {
    pub(crate) transaction_id: String,
    pub(crate) capability: Capability,
    pub(crate) operation_family: OperationFamily,
    pub(crate) boundary: EffectBoundaryKind,
    pub(crate) project_object_identity: String,
    pub(crate) project_generation: u64,
}

/// Why a permit was refused.
#[derive(Debug, thiserror::Error)]
pub(crate) enum PermitError {
    /// One or more transactions are quarantined, so no ordinary permit is legal.
    #[error("project has quarantined transactions ({0:?}); no ordinary permit can be issued")]
    Quarantined(Vec<String>),
    /// The permit's transaction is not journaled, or not in an issuable phase.
    #[error("transaction {transaction_id} is not in an issuable phase (currently {phase})")]
    NotIssuable {
        transaction_id: String,
        phase: Phase,
    },
    /// The repair authority was asked for a transaction that is not quarantined.
    #[error(
        "transaction {transaction_id} is not quarantined (currently {phase}); the recovery authority refuses it"
    )]
    NotQuarantined {
        transaction_id: String,
        phase: Phase,
    },
    /// The durable store could not answer.
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// The ordinary permit authority. Todo 26's capability-policy tables are its only
/// authorized caller.
pub(crate) struct PermitAuthority {
    store: Arc<TransactionStore>,
    epoch: u64,
}

impl PermitAuthority {
    pub(crate) const fn new(store: Arc<TransactionStore>, epoch: u64) -> Self {
        Self { store, epoch }
    }

    /// Whether the authority is currently able to issue any ordinary permit.
    ///
    /// This constructs nothing: it reports only the unconditional quarantine gate,
    /// so health reporting can state that writes are blocked without minting a
    /// permit as a probe.
    pub(crate) fn can_issue(&self) -> Result<(), PermitError> {
        let quarantined = self.store.quarantined_transactions()?;
        if quarantined.is_empty() {
            Ok(())
        } else {
            Err(PermitError::Quarantined(quarantined))
        }
    }

    /// Issue one permit, or refuse.
    ///
    /// Quarantine is checked first and unconditionally: while any transaction is
    /// quarantined no ordinary permit exists, so no capability profile — however
    /// permissive — can produce a write.
    pub(crate) fn issue(&self, request: PermitRequest) -> Result<EffectPermit, PermitError> {
        let quarantined = self.store.quarantined_transactions()?;
        if !quarantined.is_empty() {
            return Err(PermitError::Quarantined(quarantined));
        }
        let record = self.store.load(&request.transaction_id)?;
        if record.phase.is_terminal() {
            return Err(PermitError::NotIssuable {
                transaction_id: request.transaction_id,
                phase: record.phase,
            });
        }
        Ok(EffectPermit {
            transaction_id: request.transaction_id,
            capability: request.capability,
            operation_family: request.operation_family,
            boundary: request.boundary,
            project_object_identity: request.project_object_identity,
            project_generation: request.project_generation,
            epoch: self.epoch,
            seal: AuthoritySeal(()),
        })
    }
}

/// The four proof-driven operator-recovery operations. Every one is proof-backed;
/// there is deliberately no force, overwrite, guess or quarantine-clear variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RepairOperation {
    /// Re-derive and compare the typed proof; record nothing else.
    Reconcile,
    /// `quarantined → compensated`, only with a proven pre-state.
    Compensate,
    /// Kill a still-contained `process_launch` survivor, then record `compensated`.
    Terminate,
    /// Record `committed`, only when live inspection independently proves the
    /// post-state through one of the three typed proof forms.
    AcknowledgeSafeState,
}

impl RepairOperation {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Reconcile => "reconcile",
            Self::Compensate => "compensate",
            Self::Terminate => "terminate",
            Self::AcknowledgeSafeState => "acknowledge-safe-state",
        }
    }
}

/// A narrowly scoped, quarantine-exempt operator-recovery permit.
///
/// It is scoped to exactly one already-`quarantined` transaction and exactly one
/// proof-driven operation. It grants no capability, is not derived from any
/// capability profile, and is structurally incapable of authorizing a commit
/// without an independent typed proof.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepairPermit {
    transaction_id: String,
    operation: RepairOperation,
    project_object_identity: String,
    project_generation: u64,
    seal: AuthoritySeal,
}

impl RepairPermit {
    pub(crate) fn transaction_id(&self) -> &str {
        &self.transaction_id
    }

    pub(crate) const fn operation(&self) -> RepairOperation {
        self.operation
    }

    pub(crate) fn project_object_identity(&self) -> &str {
        &self.project_object_identity
    }

    pub(crate) const fn project_generation(&self) -> u64 {
        self.project_generation
    }
}

/// Evidence that the caller holds the Todo 2 project-lease advisory OS lock.
///
/// The recovery authority cannot be constructed without it, which is what keeps
/// the authority local-only: a network handler has no lease guard to present.
pub(crate) struct LocalLeaseEvidence {
    project_object_identity: String,
    project_generation: u64,
}

impl LocalLeaseEvidence {
    /// Build lease evidence from a *held* lease. The caller must already own the
    /// advisory lock; the identity is derived from the same held-handle facts the
    /// journal records.
    pub(crate) const fn new(project_object_identity: String, project_generation: u64) -> Self {
        Self {
            project_object_identity,
            project_generation,
        }
    }
}

/// The operator-recovery authority: the sole issuer of [`RepairPermit`].
pub(crate) struct OperatorRecoveryAuthority {
    store: Arc<TransactionStore>,
    evidence: LocalLeaseEvidence,
}

impl OperatorRecoveryAuthority {
    pub(crate) const fn new(store: Arc<TransactionStore>, evidence: LocalLeaseEvidence) -> Self {
        Self { store, evidence }
    }

    /// Issue one repair permit for one quarantined transaction.
    ///
    /// The requested operation and its proof outcome are written to the audit log
    /// *before* the permit is returned, so no state transition can ever be
    /// journaled without a preceding audit record.
    pub(crate) fn issue(
        &self,
        transaction_id: &str,
        operation: RepairOperation,
    ) -> Result<RepairPermit, PermitError> {
        let record = self.store.load(transaction_id)?;
        if record.phase != Phase::Quarantined {
            return Err(PermitError::NotQuarantined {
                transaction_id: transaction_id.to_owned(),
                phase: record.phase,
            });
        }
        self.store.audit(
            transaction_id,
            "repair_permit_issued",
            &format!(
                "effect_kind={} operation={} proof_outcome=pending",
                record.effect_kind.as_str(),
                operation.as_str()
            ),
        )?;
        Ok(RepairPermit {
            transaction_id: transaction_id.to_owned(),
            operation,
            project_object_identity: self.evidence.project_object_identity.clone(),
            project_generation: self.evidence.project_generation,
            seal: AuthoritySeal(()),
        })
    }

    /// Issue one `acknowledge-safe-state` permit for the single documented
    /// non-quarantined exception: a `process_launch` reconciliation left in Todo
    /// 14's case-4/5 ambiguous live-survivor state.
    ///
    /// Cases 4 and 5 are exactly the `effect_started_fsynced` and
    /// `committed_fsynced` marks, so this refuses every other phase and every
    /// other effect kind, and it can never issue any operation other than
    /// `acknowledge-safe-state`. [`Self::issue`] is untouched and still refuses
    /// anything that is not `quarantined`.
    pub(crate) fn issue_ambiguous_survivor(
        &self,
        transaction_id: &str,
    ) -> Result<RepairPermit, PermitError> {
        let record = self.store.load(transaction_id)?;
        let ambiguous = record.effect_kind == EffectKind::ProcessLaunch
            && matches!(
                record.phase,
                Phase::EffectStartedFsynced | Phase::CommittedFsynced
            );
        if !ambiguous {
            return Err(PermitError::NotQuarantined {
                transaction_id: transaction_id.to_owned(),
                phase: record.phase,
            });
        }
        self.store.audit(
            transaction_id,
            "repair_permit_issued",
            &format!(
                "effect_kind={} operation={} ambiguous_survivor_case=todo14-4-5 \
                 proof_outcome=pending",
                record.effect_kind.as_str(),
                RepairOperation::AcknowledgeSafeState.as_str()
            ),
        )?;
        Ok(RepairPermit {
            transaction_id: transaction_id.to_owned(),
            operation: RepairOperation::AcknowledgeSafeState,
            project_object_identity: self.evidence.project_object_identity.clone(),
            project_generation: self.evidence.project_generation,
            seal: AuthoritySeal(()),
        })
    }
}
