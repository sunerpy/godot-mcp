//! The `test-hooks`-gated Todo 27 scheduler/idempotency harness.
//!
//! Like the Todo 26 policy harness, this is the **only** way a test can reach the
//! crate-private journal store, the effect gateway, the policy permit and the
//! operator-recovery authority. It exposes *observations* — did reads overlap, how
//! many times did a boundary actually run, what bytes did a conflict carry — and
//! never a permit value and never a hint about which tuple element mismatched.
//!
//! Two properties are deliberately unreachable from here:
//!
//! * a test cannot mint a [`RepairProof`](crate::scheduler::RepairProof) itself;
//!   only [`SchedulerIdempotencyHarness::repair`] can, and only by driving the
//!   real proof-producing recovery path. A harness that could hand out a proof
//!   would let a test "unblock" writes without proving anything, which is exactly
//!   the hole the write-block protocol exists to close.
//! * a test cannot ask which identity element differed. The conflict observation
//!   is bytes, and the bytes are the same for every mismatch.

use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use godot_mcp_domain::CapabilityProfile;
use godot_mcp_domain::contracts::{PrincipalId, ProjectIdentity, SessionId};
use godot_mcp_domain::lease::{Lease, LeaseManager};
use godot_mcp_domain::safe_root::SafeRootError;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::HarnessError;
use crate::effect::boundary::{EffectTicket, FileEffectBoundary, FileWrite};
use crate::effect::{
    EffectBoundaries, EffectGateway, FileEffectIntent, LocalLeaseEvidence,
    OperatorRecoveryAuthority, PlannedWrite, RepairOperation, SafeRootFileBoundary,
    UnavailableBoundaries,
};
use crate::idempotency::gate::IdempotencyGate;
use crate::idempotency::{
    ConnectionSession, IdempotencyId, ModePrincipal, MutationIdentity, OperationToolFamily,
    ProjectBinding, ProjectObjectIdentity, RecordedStatus, ReplayDecision, TransportAuthMode,
    TransportBinding, TupleDimension, canonical_request_hash,
};
use crate::journal::{
    EffectBoundaryKind, FileOperation, HeldProject, OperationFamily, Phase, SafeRootProofSource,
    TransactionStore, TransportMode, encode_object_identity, reconcile_open_transactions,
};
use crate::policy::{
    BoundIdentity, BoundaryAvailability, DispatchRequest, PolicyAuthority, SurvivorProbe,
};
use crate::scheduler::{
    AdmissionError, AdmissionRequest, CancellationOutcome, EffectPlane, FairnessSnapshot, Lane,
    PlaneAvailability, QueuePriority, RepairProof, Scheduler, SchedulerLimits, ShutdownOutcome,
    WriteBlock, plane_of, priority_for,
};
use crate::stdio::INITIAL_PROJECT_GENERATION;

/// The permit-authority epoch every harness component is bound to.
const EPOCH: u64 = 1;
/// The bound local-OS principal. It is an opaque Todo 3 identifier.
const BOUND_PRINCIPAL: &str = "principal_0123456789abcdef";
/// The bound MCP session.
const BOUND_SESSION: &str = "session_0123456789abcdef";

/// One complete request context a test can vary element by element.
#[derive(Debug, Clone, PartialEq)]
pub struct RequestSpec {
    /// Canonical tool name.
    pub tool: String,
    /// Operation/tool family, in its stable snake_case name.
    pub family: String,
    /// Transport/auth mode, in its stable snake_case name.
    pub transport_mode: String,
    /// Mode-specific principal, already in the shape that mode requires.
    pub principal: String,
    /// Secondary principal component; only `proxied_oidc` uses it (`sub`).
    pub principal_subject: Option<String>,
    /// MCP connection/session identifier.
    pub session: String,
    /// Caller-supplied idempotency identifier.
    pub idempotency_id: String,
    /// Complete request arguments; hashed canonically, never truncated.
    pub arguments: Value,
    /// Fixed transaction ID for the journaled record.
    pub transaction_id: String,
    /// Project-relative path the mutation writes.
    pub path: String,
    /// Bytes the mutation publishes.
    pub bytes: Vec<u8>,
    /// Project object identity to present, when it must differ from the held one.
    pub project_object: Option<String>,
    /// Project generation to present, when it must differ from the held one.
    pub project_generation: Option<u64>,
}

impl RequestSpec {
    /// A `stdio` project-mutation request with a fixed, complete context.
    #[must_use]
    pub fn stdio_project_mutation(transaction_id: &str, idempotency_id: &str, path: &str) -> Self {
        Self {
            tool: "resource.update".to_owned(),
            family: OperationToolFamily::ProjectMutation.as_str().to_owned(),
            transport_mode: TransportAuthMode::Stdio.as_str().to_owned(),
            principal: BOUND_PRINCIPAL.to_owned(),
            principal_subject: None,
            session: BOUND_SESSION.to_owned(),
            idempotency_id: idempotency_id.to_owned(),
            arguments: serde_json::json!({ "contents": "todo-27", "path": path }),
            transaction_id: transaction_id.to_owned(),
            path: path.to_owned(),
            bytes: b"todo-27".to_vec(),
            project_object: None,
            project_generation: None,
        }
    }
}

/// What one mutation attempt actually did.
#[derive(Debug, Clone, PartialEq)]
pub struct MutationReport {
    /// Journaled transaction the attempt settled.
    pub transaction_id: String,
    /// Recorded status, in its stable snake_case name.
    pub status: String,
    /// Recorded result, where the status carries one.
    pub result: Option<Value>,
    /// Whether the attempt returned a recorded result instead of executing.
    pub replayed: bool,
    /// Whether the boundary ran during this attempt.
    pub executed: bool,
}

/// A refused mutation attempt, as a test observes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefusalReport {
    /// Stable domain error code from the ten-item taxonomy.
    pub code: &'static str,
    /// Caller-safe message.
    pub message: String,
    /// The refusal's canonical bytes, when it is an idempotency conflict.
    ///
    /// This is what the acceptance suite compares across all eight tuple
    /// dimensions; it must be byte-identical every time.
    pub conflict_bytes: Option<Vec<u8>>,
}

/// Observation of one concurrent-read burst.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadOverlap {
    /// Reads that were admitted.
    pub admitted: u32,
    /// The largest number of reads observed running at the same instant.
    pub max_concurrent: u32,
}

impl ReadOverlap {
    /// Whether more than one read was in flight at once.
    #[must_use]
    pub const fn overlapped(self) -> bool {
        self.max_concurrent > 1
    }
}

/// The `file` boundary mock: it counts, then delegates to the real
/// `SafeRoot`-backed boundary, so a permitted write really lands on disk.
struct RecordingFileBoundary {
    inner: SafeRootFileBoundary,
    touches: Arc<AtomicUsize>,
}

impl FileEffectBoundary for RecordingFileBoundary {
    fn available(&self) -> bool {
        self.inner.available()
    }

    fn publish(&self, ticket: &EffectTicket, write: &FileWrite) -> Result<(), SafeRootError> {
        self.touches.fetch_add(1, Ordering::Relaxed);
        self.inner.publish(ticket, write)
    }

    fn delete(
        &self,
        ticket: &EffectTicket,
        path: &str,
        expected: &[u8],
    ) -> Result<(), SafeRootError> {
        self.touches.fetch_add(1, Ordering::Relaxed);
        self.inner.delete(ticket, path, expected)
    }
}

/// The survivor state a test drives, standing in for Todo 17's launch journal.
#[derive(Debug, Default)]
struct MutableSurvivors {
    blocked: Mutex<Option<String>>,
    unreadable: Mutex<bool>,
}

impl SurvivorProbe for MutableSurvivors {
    fn unresolved_survivor(&self) -> Result<Option<String>, String> {
        if *self
            .unreadable
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
        {
            return Err("launch survivor state cannot be read".to_owned());
        }
        Ok(self
            .blocked
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone())
    }
}

/// The Todo 27 acceptance harness over one held project and its Todo 6 journal.
pub struct SchedulerIdempotencyHarness {
    lease: Lease,
    project: ProjectIdentity,
    scheduler: Arc<Scheduler>,
    store: Arc<TransactionStore>,
    gateway: EffectGateway,
    policy: PolicyAuthority,
    held: HeldProject,
    survivors: Arc<MutableSurvivors>,
    touches: Arc<AtomicUsize>,
    read_concurrency: u32,
    queue_limit: u32,
}

impl SchedulerIdempotencyHarness {
    /// Acquire the project, open the *existing* Todo 6 journal, reconcile, and
    /// assemble a scheduler bound to it.
    ///
    /// # Errors
    /// Returns the lease or store failure that prevented assembly.
    pub fn open(project_root: &Path, limits: SchedulerLimits) -> Result<Self, HarnessError> {
        let state_dir = crate::stdio::state_dir_for(project_root);
        let manager = LeaseManager::new(&state_dir);
        let lease = manager
            .acquire(project_root)
            .map_err(|error| HarnessError::Lease(error.to_string()))?;
        let key = lease.lease_key();
        let project = ProjectIdentity::from_lease_key(key, INITIAL_PROJECT_GENERATION);
        let held = HeldProject {
            object_identity: project.object_id().to_string(),
            root_object: encode_object_identity(key.root),
            file_object: encode_object_identity(key.project_file),
            generation: INITIAL_PROJECT_GENERATION,
        };
        let store = Arc::new(
            TransactionStore::open(&state_dir.join("journal"))
                .map_err(|error| HarnessError::Store(error.to_string()))?,
        );
        reconcile_open_transactions(
            store.as_ref(),
            &held,
            &SafeRootProofSource::new(lease.safe_root()),
        )
        .map_err(|error| HarnessError::Store(error.to_string()))?;

        let touches = Arc::new(AtomicUsize::new(0));
        let gateway = EffectGateway::new(
            Arc::clone(&store),
            held.clone(),
            EPOCH,
            Box::new(SafeRootProofSource::new(lease.safe_root())),
            EffectBoundaries {
                file: Box::new(RecordingFileBoundary {
                    inner: SafeRootFileBoundary::new(lease.safe_root()),
                    touches: Arc::clone(&touches),
                }),
                process: Box::new(UnavailableBoundaries),
                lsp_dap: Box::new(UnavailableBoundaries),
                editor: Box::new(UnavailableBoundaries),
                runtime: Box::new(UnavailableBoundaries),
            },
        );
        let availability = BoundaryAvailability::from_gateway(&gateway);
        let survivors = Arc::new(MutableSurvivors::default());
        let principal = PrincipalId::parse(BOUND_PRINCIPAL)
            .map_err(|error| HarnessError::Permit(error.to_string()))?;
        let session = SessionId::parse(BOUND_SESSION)
            .map_err(|error| HarnessError::Permit(error.to_string()))?;
        let policy = PolicyAuthority::new(
            CapabilityProfile::Full,
            BoundIdentity {
                project: project.clone(),
                principal: Some(principal),
                session: Some(session),
            },
            Arc::clone(&store),
            availability,
            Box::new(SharedSurvivors(Arc::clone(&survivors))),
            EPOCH,
        );

        let scheduler = Arc::new(Scheduler::new(project.clone(), limits));
        scheduler.set_plane_availability(PlaneAvailability::new(
            availability.of(EffectBoundaryKind::File),
            availability.of(EffectBoundaryKind::Process),
            availability.of(EffectBoundaryKind::LspDap),
            availability.of(EffectBoundaryKind::Editor),
            availability.of(EffectBoundaryKind::Runtime),
        ));
        scheduler.bind_journal(
            Arc::new(IdempotencyGate::new(
                Arc::clone(&store),
                held.object_identity.clone(),
                held.generation,
            )),
            Arc::clone(&store),
            Arc::clone(&survivors) as Arc<dyn SurvivorProbe>,
        );

        Ok(Self {
            lease,
            project,
            scheduler,
            store,
            gateway,
            policy,
            held,
            survivors,
            touches,
            read_concurrency: limits.read_concurrency.max(1),
            queue_limit: limits.queue_limit.max(1),
        })
    }

    /// The scheduler under test.
    #[must_use]
    pub fn scheduler(&self) -> Arc<Scheduler> {
        Arc::clone(&self.scheduler)
    }

    /// The held project identity.
    #[must_use]
    pub fn project(&self) -> ProjectIdentity {
        self.project.clone()
    }

    /// The held lease's project generation.
    #[must_use]
    pub fn project_generation(&self) -> u64 {
        let _held = self.lease.lease_key();
        self.held.generation
    }

    /// Build the eight-element identity for one request spec.
    ///
    /// # Errors
    /// Returns the identity failure when the spec is not a representable context.
    pub fn identity(&self, spec: &RequestSpec) -> Result<MutationIdentity, HarnessError> {
        let mode = mode_of(&spec.transport_mode)?;
        let principal = principal_of(mode, spec)?;
        let hash = canonical_request_hash(&spec.tool, &spec.arguments)
            .map_err(|error| HarnessError::Permit(error.to_string()))?;
        let object_identity = spec
            .project_object
            .clone()
            .unwrap_or_else(|| self.held.object_identity.clone());
        let transport = TransportBinding::new(
            mode,
            principal,
            ConnectionSession::new(spec.session.clone())
                .map_err(|error| HarnessError::Permit(error.to_string()))?,
        )
        .map_err(|error| HarnessError::Permit(error.to_string()))?;
        Ok(MutationIdentity::new(
            ProjectBinding::new(
                ProjectObjectIdentity::new(object_identity)
                    .map_err(|error| HarnessError::Permit(error.to_string()))?,
                spec.project_generation.unwrap_or(self.held.generation),
            ),
            transport,
            family_of_name(&spec.family)?,
            IdempotencyId::new(spec.idempotency_id.clone())
                .map_err(|error| HarnessError::Permit(error.to_string()))?,
            hash,
        ))
    }

    /// Derive a spec that differs in exactly one tuple dimension.
    ///
    /// Only the named element moves; every other element, and the mode/principal
    /// pairing invariant, is preserved, so the resulting refusal is attributable
    /// to that dimension alone.
    #[must_use]
    pub fn vary(&self, spec: &RequestSpec, dimension: TupleDimension) -> RequestSpec {
        let mut varied = spec.clone();
        match dimension {
            TupleDimension::ProjectObject => {
                varied.project_object = Some(format!("project_{}", "b".repeat(64)));
            }
            TupleDimension::ProjectGeneration => {
                varied.project_generation = Some(self.held.generation + 1);
            }
            TupleDimension::TransportAuthMode => {
                varied.transport_mode = TransportAuthMode::TrustedLocal.as_str().to_owned();
            }
            TupleDimension::Principal => {
                varied.principal = "principal_fedcba9876543210".to_owned();
            }
            TupleDimension::Session => {
                varied.session = "session_fedcba9876543210".to_owned();
            }
            TupleDimension::OperationFamily => {
                varied.family = OperationToolFamily::DestructiveExport.as_str().to_owned();
            }
            TupleDimension::IdempotencyId => {
                varied.idempotency_id = format!("{}-other", spec.idempotency_id);
            }
            TupleDimension::RequestHash => {
                varied.arguments =
                    serde_json::json!({ "contents": "todo-27-other", "path": spec.path });
            }
        }
        varied
    }

    /// Admit `count` reads and report the peak number running at once.
    ///
    /// Every admission is held until the burst finishes, so the peak is read from
    /// the scheduler's own running counter rather than inferred from timing.
    pub async fn concurrent_reads(&self, count: u32, priority: QueuePriority) -> ReadOverlap {
        let mut held = Vec::new();
        let mut max_concurrent = 0;
        for index in 0..count {
            let request =
                AdmissionRequest::new(Lane::Read, priority, CancellationToken::new(), index.into());
            match self.scheduler.admit_read_classed(&request).await {
                Ok(admission) => {
                    held.push(admission);
                    max_concurrent = max_concurrent.max(self.running());
                }
                Err(_) => break,
            }
        }
        let admitted = u32::try_from(held.len()).unwrap_or(u32::MAX);
        drop(held);
        ReadOverlap {
            admitted,
            max_concurrent,
        }
    }

    /// Journal, authorize and execute one mutation end to end.
    ///
    /// The order is normative: the replay decision is taken *first*, so a reused
    /// identity never reaches the queue, the policy layer or a boundary.
    ///
    /// # Errors
    /// Returns the refusal, including its byte-stable conflict envelope when the
    /// identity was reused from a different context.
    pub async fn execute_mutation(
        &self,
        spec: &RequestSpec,
    ) -> Result<MutationReport, RefusalReport> {
        let identity = self
            .identity(spec)
            .map_err(|error| internal_refusal(&error.to_string()))?;
        match self.scheduler.replay_decision(&identity) {
            Ok(ReplayDecision::Fresh) => {}
            Ok(ReplayDecision::RecordedResult(recorded)) => {
                return Ok(MutationReport {
                    transaction_id: recorded.transaction_id().to_owned(),
                    status: recorded.status().as_str().to_owned(),
                    result: recorded.result().cloned(),
                    replayed: true,
                    executed: false,
                });
            }
            Ok(ReplayDecision::Conflict(envelope)) => {
                return Err(RefusalReport {
                    code: envelope.to_outcome().code(),
                    message: envelope.message().to_owned(),
                    conflict_bytes: Some(envelope.as_bytes()),
                });
            }
            Err(error) => {
                return Err(RefusalReport {
                    code: error.code(),
                    message: error.message().to_owned(),
                    conflict_bytes: None,
                });
            }
        }

        let plane = family_of_name(&spec.family)
            .map_err(|error| internal_refusal(&error.to_string()))
            .and_then(|family| {
                plane_of(family).ok_or_else(|| internal_refusal("a read family has no plane"))
            })?;
        let request = AdmissionRequest::new(
            Lane::Effect,
            QueuePriority::Normal,
            CancellationToken::new(),
            0,
        );
        let mut admission = self
            .scheduler
            .admit_mutation(&request, plane)
            .await
            .map_err(admission_refusal)?;

        let intent = self.intent(spec);
        self.gateway
            .open_file_transaction(&intent)
            .map_err(|error| internal_refusal(error.to_outcome().message()))?;
        self.gateway
            .stage_file_transaction(&intent)
            .map_err(|error| internal_refusal(error.to_outcome().message()))?;
        let grant = self
            .policy
            .authorize(&DispatchRequest {
                tool: spec.tool.clone(),
                project: self.project.clone(),
                principal: PrincipalId::parse(BOUND_PRINCIPAL).ok(),
                session: SessionId::parse(BOUND_SESSION).ok(),
                transaction_id: Some(spec.transaction_id.clone()),
            })
            .map_err(|refusal| RefusalReport {
                code: refusal.error_kind().code(),
                message: refusal.to_string(),
                conflict_bytes: None,
            })?;
        let permit = grant
            .permit
            .ok_or_else(|| internal_refusal("a mutating dispatch produced no permit"))?;

        admission.mark_started();
        let outcome = self
            .policy
            .dispatch_file_effect(&self.gateway, &permit)
            .map_err(|error| RefusalReport {
                code: error.error_kind().code(),
                message: error.to_string(),
                conflict_bytes: None,
            })?;
        drop(admission);

        let recorded = self
            .recorded_of(&outcome.transaction_id)
            .map_err(|error| internal_refusal(&error.to_string()))?;
        Ok(MutationReport {
            transaction_id: outcome.transaction_id,
            status: recorded.0,
            result: recorded.1,
            replayed: false,
            executed: true,
        })
    }

    /// Cancel one mutation while it is queued behind a running mutation.
    ///
    /// # Errors
    /// Returns the refusal the queued work received. A
    /// [`AdmissionError::CancelledBeforeStart`] refusal is the proof that no
    /// boundary was touched.
    pub async fn cancel_while_queued(
        &self,
        spec: &RequestSpec,
        token: &CancellationToken,
    ) -> Result<CancellationOutcome, RefusalReport> {
        let blocker = AdmissionRequest::new(
            Lane::Effect,
            QueuePriority::Normal,
            CancellationToken::new(),
            0,
        );
        let held = self
            .scheduler
            .admit_mutation(&blocker, EffectPlane::File)
            .await;
        let queued = AdmissionRequest::new(Lane::Effect, QueuePriority::Normal, token.clone(), 0);
        let (result, ()) = tokio::join!(
            self.scheduler.admit_mutation(&queued, EffectPlane::File),
            async {
                for _ in 0..8 {
                    tokio::task::yield_now().await;
                }
                token.cancel();
            }
        );
        drop(held);
        let _ = spec;
        match result {
            Ok(_admission) => Err(internal_refusal("cancelled queued work was admitted")),
            Err(AdmissionError::CancelledBeforeStart) => {
                Ok(CancellationOutcome::CancelledBeforeStart)
            }
            Err(error) => Err(admission_refusal(error)),
        }
    }

    /// Cancel one mutation *after* it began, and classify the outcome.
    pub async fn cancel_during_execution(&self) -> Option<CancellationOutcome> {
        let token = CancellationToken::new();
        let request = AdmissionRequest::new(Lane::Effect, QueuePriority::Normal, token.clone(), 0);
        let mut admission = self
            .scheduler
            .admit_mutation(&request, EffectPlane::File)
            .await
            .ok()?;
        admission.mark_started();
        token.cancel();
        admission.cancellation_outcome()
    }

    /// Let one mutation wait past its queue deadline behind a running mutation.
    pub async fn queue_deadline_race(&self) -> RefusalReport {
        let blocker = AdmissionRequest::new(
            Lane::Effect,
            QueuePriority::Normal,
            CancellationToken::new(),
            0,
        );
        let held = self
            .scheduler
            .admit_mutation(&blocker, EffectPlane::File)
            .await;
        let waiter = AdmissionRequest::new(
            Lane::Effect,
            QueuePriority::Normal,
            CancellationToken::new(),
            0,
        );
        let result = self
            .scheduler
            .admit_mutation(&waiter, EffectPlane::File)
            .await;
        drop(held);
        match result {
            Ok(_admission) => internal_refusal("deadline-expired work was admitted"),
            Err(error) => admission_refusal(error),
        }
    }

    /// Fill the bounded queue behind a saturated read lane and report the refusal
    /// the one request past the limit receives.
    ///
    /// The read permits are all held first, so every later request genuinely
    /// occupies a queue slot rather than being admitted immediately.
    pub async fn queue_full(&self) -> RefusalReport {
        let mut holders = Vec::new();
        for index in 0..self.read_concurrency {
            let request = AdmissionRequest::new(
                Lane::Read,
                QueuePriority::Normal,
                CancellationToken::new(),
                index.into(),
            );
            if let Ok(admission) = self.scheduler.admit_read_classed(&request).await {
                holders.push(admission);
            }
        }

        let waiters: Vec<AdmissionRequest> = (0..self.queue_limit)
            .map(|index| {
                AdmissionRequest::new(
                    Lane::Read,
                    priority_for(OperationToolFamily::ProjectMutation),
                    CancellationToken::new(),
                    index.into(),
                )
            })
            .collect();
        let overflow = AdmissionRequest::new(
            Lane::Read,
            QueuePriority::Normal,
            CancellationToken::new(),
            0,
        );
        let waits = futures::future::join_all(
            waiters
                .iter()
                .map(|request| self.scheduler.admit_read_classed(request)),
        );
        let overflow_wait = async {
            // The queue must be full before the overflow request arrives, so it
            // yields until every waiter above has taken its slot.
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            self.scheduler.admit_read_classed(&overflow).await
        };
        let release = async {
            for _ in 0..16 {
                tokio::task::yield_now().await;
            }
            drop(holders);
        };
        let (admitted, overflow_result, ()) = tokio::join!(waits, overflow_wait, release);
        drop(admitted);
        match overflow_result {
            Ok(_admission) => internal_refusal("queue overflow was admitted"),
            Err(error) => admission_refusal(error),
        }
    }

    /// Drive a burst that would starve the low class and report the ledger.
    ///
    /// The read lane is saturated *first*, so every burst member really queues.
    /// Without that step each waiter would find a free permit at its first poll
    /// and be admitted before the next one enqueued, and the run would observe no
    /// contention at all.
    pub async fn saturate(&self, bulk: u32, interactive: u32) -> FairnessSnapshot {
        let holders_requests: Vec<AdmissionRequest> = (0..self.read_concurrency)
            .map(|index| {
                AdmissionRequest::new(
                    Lane::Read,
                    QueuePriority::Normal,
                    CancellationToken::new(),
                    index.into(),
                )
            })
            .collect();
        let mut holders = Vec::new();
        for request in &holders_requests {
            if let Ok(admission) = self.scheduler.admit_read_classed(request).await {
                holders.push(admission);
            }
        }

        // The two classes are interleaved as they enter the queue. Enqueueing one
        // class first would fill the bounded queue with it, and the other class
        // would only ever see `QueueFull` — that tests backpressure, not fairness.
        let mut requests: Vec<AdmissionRequest> = Vec::new();
        for index in 0..bulk.max(interactive) {
            if index < interactive {
                requests.push(AdmissionRequest::new(
                    Lane::Read,
                    priority_for(OperationToolFamily::StaticAnalysis),
                    CancellationToken::new(),
                    index.into(),
                ));
            }
            if index < bulk {
                requests.push(AdmissionRequest::new(
                    Lane::Read,
                    priority_for(OperationToolFamily::ProjectImport),
                    CancellationToken::new(),
                    index.into(),
                ));
            }
        }
        let waits = futures::future::join_all(
            requests
                .iter()
                .map(|request| self.scheduler.admit_read_classed(request)),
        );
        let release = async {
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            drop(holders);
        };
        let (admissions, ()) = tokio::join!(waits, release);
        drop(admissions);
        self.scheduler.fairness()
    }

    /// Attempt admission for one plane without executing anything.
    ///
    /// # Errors
    /// Returns the plane's honest refusal.
    pub async fn probe_plane(
        &self,
        plane: EffectPlane,
        priority: QueuePriority,
    ) -> Result<(), RefusalReport> {
        let request = AdmissionRequest::new(Lane::Effect, priority, CancellationToken::new(), 0);
        self.scheduler
            .admit_mutation(&request, plane)
            .await
            .map(|_admission| ())
            .map_err(admission_refusal)
    }

    /// Declare (or clear) an unresolved supervised-process survivor.
    pub fn declare_survivor(&self, reason: Option<&str>) {
        *self
            .survivors
            .blocked
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = reason.map(str::to_owned);
    }

    /// Make the survivor state unreadable, proving the scheduler refuses rather
    /// than assuming "clear".
    pub fn set_survivor_state_unreadable(&self, unreadable: bool) {
        *self
            .survivors
            .unreadable
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = unreadable;
    }

    /// Drive one transaction to `quarantined`, so every later write is blocked.
    ///
    /// # Errors
    /// Returns the store failure that prevented the transition.
    pub fn quarantine(&self, transaction_id: &str, path: &str) -> Result<(), HarnessError> {
        let spec = RequestSpec::stdio_project_mutation(transaction_id, "idem-quarantine", path);
        self.gateway
            .open_file_transaction(&self.intent(&spec))
            .map_err(|error| HarnessError::Store(error.to_string()))?;
        self.store
            .advance(
                transaction_id,
                Phase::Quarantined,
                None,
                Some("harness-forced quarantine".to_owned()),
                Some(crate::journal::TerminalOutcome::failed(
                    godot_mcp_domain::ErrorKind::Uncertain,
                )),
            )
            .map(|_record| ())
            .map_err(|error| HarnessError::Store(error.to_string()))
    }

    /// Why writes are currently blocked, if they are.
    ///
    /// # Errors
    /// Returns the refusal when the block state cannot be read.
    pub fn write_block(&self) -> Result<Option<WriteBlock>, RefusalReport> {
        self.scheduler.write_block().map_err(|error| RefusalReport {
            code: error.code(),
            message: error.message().to_owned(),
            conflict_bytes: None,
        })
    }

    /// Run the real operator repair for one quarantined transaction and return the
    /// proof it produced.
    ///
    /// This is the only source of a [`RepairProof`] in a test build: it drives
    /// Todo 6's recovery authority, which re-derives a typed proof, and mints the
    /// proof only from a terminal, proof-backed result.
    ///
    /// # Errors
    /// Returns the recovery failure when no proof could be produced.
    pub fn repair(&self, transaction_id: &str) -> Result<RepairProof, HarnessError> {
        let authority = OperatorRecoveryAuthority::new(
            Arc::clone(&self.store),
            LocalLeaseEvidence::new(self.held.object_identity.clone(), self.held.generation),
        );
        let permit = authority
            .issue(transaction_id, RepairOperation::Compensate)
            .map_err(|error| HarnessError::Permit(error.to_string()))?;
        let outcome = self
            .gateway
            .apply_repair(&permit)
            .map_err(|error| HarnessError::Store(error.to_outcome().message().to_owned()))?;
        if outcome.phase == Phase::Quarantined {
            return Err(HarnessError::Store(format!(
                "the repair produced no proof: {}",
                outcome.reason
            )));
        }
        Ok(RepairProof::sealed(
            outcome.transaction_id,
            RepairOperation::Compensate.as_str(),
        ))
    }

    /// Ask the scheduler to lift a write block with one repair proof.
    ///
    /// # Errors
    /// Returns the refusal when the block still holds.
    pub fn resolve_write_block(&self, proof: &RepairProof) -> Result<(), RefusalReport> {
        self.scheduler
            .resolve_write_block(proof)
            .map_err(|error| RefusalReport {
                code: error.code(),
                message: error.message().to_owned(),
                conflict_bytes: None,
            })
    }

    /// How many times the file boundary actually ran.
    #[must_use]
    pub fn effect_touches(&self) -> usize {
        self.touches.load(Ordering::Relaxed)
    }

    /// Durable phase of one transaction, in its stable snake_case name.
    ///
    /// # Errors
    /// Returns the store failure that prevented the read.
    pub fn phase_of(&self, transaction_id: &str) -> Result<String, HarnessError> {
        self.store
            .load(transaction_id)
            .map(|record| record.phase.as_str().to_owned())
            .map_err(|error| HarnessError::Store(error.to_string()))
    }

    /// Recorded status of one transaction, as the owning context would see it.
    ///
    /// # Errors
    /// Returns the store failure that prevented the read.
    pub fn recorded_status(&self, transaction_id: &str) -> Result<RecordedStatus, HarnessError> {
        let gate = IdempotencyGate::new(
            Arc::clone(&self.store),
            self.held.object_identity.clone(),
            self.held.generation,
        );
        gate.recorded(transaction_id)
            .map(|recorded| recorded.status())
            .map_err(|error| HarnessError::Store(error.message().to_owned()))
    }

    /// Contents of one project-relative path, so a test can prove a write landed
    /// exactly once.
    ///
    /// # Errors
    /// Returns the read failure.
    pub fn read_path(&self, path: &str) -> Result<Option<Vec<u8>>, HarnessError> {
        crate::journal::ProofSource::read_bytes(
            &SafeRootProofSource::new(self.lease.safe_root()),
            path,
        )
        .map_err(|error| HarnessError::Store(error.to_string()))
    }

    /// Stop admitting and let admitted work settle.
    pub async fn shutdown(&self) -> ShutdownOutcome {
        self.scheduler.shutdown().await
    }

    /// The bound context this harness presents, so a test can assert the
    /// mode-specific pairing without hard-coding it twice.
    #[must_use]
    pub fn bound_context(&self) -> (TransportAuthMode, ModePrincipal, ConnectionSession) {
        (
            TransportAuthMode::Stdio,
            ModePrincipal::LocalOs {
                principal: BOUND_PRINCIPAL.to_owned(),
            },
            ConnectionSession::new(BOUND_SESSION).expect("the bound session is non-empty"),
        )
    }

    fn running(&self) -> u32 {
        self.scheduler.running()
    }

    fn recorded_of(&self, transaction_id: &str) -> Result<(String, Option<Value>), HarnessError> {
        let gate = IdempotencyGate::new(
            Arc::clone(&self.store),
            self.held.object_identity.clone(),
            self.held.generation,
        );
        gate.recorded(transaction_id)
            .map(|recorded| {
                (
                    recorded.status().as_str().to_owned(),
                    recorded.result().cloned(),
                )
            })
            .map_err(|error| HarnessError::Store(error.message().to_owned()))
    }

    fn intent(&self, spec: &RequestSpec) -> FileEffectIntent {
        FileEffectIntent {
            transaction_id: spec.transaction_id.clone(),
            tool_name: spec.tool.clone(),
            operation_family: OperationFamily::ProjectMutation,
            idempotency_id: spec.idempotency_id.clone(),
            transport_mode: journal_mode(&spec.transport_mode),
            principal: Some(spec.principal.clone()),
            session: Some(spec.session.clone()),
            arguments: spec.arguments.clone(),
            writes: vec![PlannedWrite {
                path: spec.path.clone(),
                operation: FileOperation::Create,
                bytes: Some(spec.bytes.clone()),
                precondition: crate::effect::WritePrecondition::Observed,
            }],
            required_process_transaction: None,
            opened_at_unix_ms: 1_700_000_000_000,
            expected_revision: None,
        }
    }
}

/// The shared survivor probe the policy layer consults.
struct SharedSurvivors(Arc<MutableSurvivors>);

impl SurvivorProbe for SharedSurvivors {
    fn unresolved_survivor(&self) -> Result<Option<String>, String> {
        self.0.unresolved_survivor()
    }
}

fn mode_of(name: &str) -> Result<TransportAuthMode, HarnessError> {
    TransportAuthMode::ALL
        .into_iter()
        .find(|mode| mode.as_str() == name)
        .ok_or_else(|| HarnessError::Permit(format!("unknown transport mode {name}")))
}

fn journal_mode(name: &str) -> TransportMode {
    match name {
        "trusted_local" => TransportMode::TrustedLocal,
        "proxied_oidc" => TransportMode::ProxiedOidc,
        _ => TransportMode::Stdio,
    }
}

fn principal_of(
    mode: TransportAuthMode,
    spec: &RequestSpec,
) -> Result<ModePrincipal, HarnessError> {
    Ok(match mode {
        TransportAuthMode::Stdio => ModePrincipal::LocalOs {
            principal: spec.principal.clone(),
        },
        TransportAuthMode::TrustedLocal => ModePrincipal::LaunchSecret {
            principal: spec.principal.clone(),
        },
        TransportAuthMode::ProxiedOidc => ModePrincipal::Oidc {
            issuer: spec.principal.clone(),
            subject: spec.principal_subject.clone().ok_or_else(|| {
                HarnessError::Permit("proxied_oidc requires an exact subject".to_owned())
            })?,
        },
    })
}

fn family_of_name(name: &str) -> Result<OperationToolFamily, HarnessError> {
    OperationToolFamily::ALL
        .into_iter()
        .find(|family| family.as_str() == name)
        .ok_or_else(|| HarnessError::Permit(format!("unknown operation family {name}")))
}

fn admission_refusal(error: AdmissionError) -> RefusalReport {
    let outcome = error.to_outcome();
    RefusalReport {
        code: outcome.code(),
        message: outcome.message().to_owned(),
        conflict_bytes: None,
    }
}

fn internal_refusal(message: &str) -> RefusalReport {
    RefusalReport {
        code: "internal",
        message: message.to_owned(),
        conflict_bytes: None,
    }
}
