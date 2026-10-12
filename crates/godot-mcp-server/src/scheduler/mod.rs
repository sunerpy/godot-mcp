//! The actor/scheduler with separate read and effect lanes, class fairness,
//! deadlines, cancellation and cross-plane idempotency.
//!
//! Todo 6 froze the shape: one actor owns the effect lane so effects are
//! serialized per project, while reads run concurrently up to a bounded limit.
//! Todo 27 completes that skeleton *in place* — nothing Todo 6 shipped is
//! replaced. What is added:
//!
//! * a bounded read semaphore plus exclusive mutation, expressed as admission
//!   tickets whose `Drop` releases capacity even on panic or cancellation;
//! * queue priority classes with an explicit, assertable
//!   [`StarvationBound`](fairness::StarvationBound);
//! * separate [`QueueDeadline`](fairness::QueueDeadline) and
//!   [`OperationDeadline`](fairness::OperationDeadline), because "never started"
//!   and "started and did not finish" are different truths;
//! * two structurally distinct cancellation paths, because only one of them
//!   proves no effect happened;
//! * mode-aware idempotency over the *existing* Todo 6 journal.
//!
//! ## What this layer refuses to do
//!
//! It never infers success from dispatch. It never creates a second journal, a
//! competing schema or an in-memory ledger — every durable fact it reports is read
//! from Todo 6's [`TransactionStore`](crate::journal::TransactionStore). It never
//! holds a lock across I/O: the effect lane's exclusivity guard is released before
//! the durable store is touched by a different lane. It offers no force-overwrite
//! and no clear: a write-blocking quarantine or an unresolved process survivor is
//! lifted only by an operator repair that produces its own proof.
//!
//! ## Quarantine and survivors
//!
//! The scheduler owns the write-blocking gate: while any transaction is
//! quarantined the effect lane refuses admission, so a quarantined project cannot
//! be written even if a capability profile would allow it. An unresolved process
//! survivor blocks mutations the same way, and for the same reason — neither the
//! pre-state nor the post-state is proven, so any write would be built on a guess.

mod fairness;
mod plane;

pub use fairness::{
    CancellationOutcome, FairnessError, FairnessSnapshot, OperationDeadline, QueueDeadline,
    QueuePriority, ShutdownOutcome, StarvationBound, priority_for,
};
pub use plane::{EffectPlane, PLANE_DETAIL_KEY, PlaneAvailability, plane_of};

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::contracts::{OutcomeError, ProjectIdentity, SchedulerMode, SchedulerState};
use tokio::sync::{Mutex as AsyncMutex, Notify, OwnedSemaphorePermit, Semaphore};
use tokio::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

use crate::idempotency::gate::IdempotencyGate;
use crate::idempotency::{MutationIdentity, ReplayDecision};
use crate::journal::TransactionStore;
use crate::policy::SurvivorProbe;

/// Which lane an operation is admitted to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    /// Concurrent, side-effect-free work.
    Read,
    /// Serialized, journaled, permit-gated work.
    Effect,
}

/// Bounded lane limits, fairness bounds and deadline budgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedulerLimits {
    /// Maximum concurrent reads.
    pub read_concurrency: u32,
    /// Maximum queued operations across both lanes.
    pub queue_limit: u32,
    /// How many overtakes a waiter tolerates before it is promoted.
    pub starvation_bound: StarvationBound,
    /// How long queued work may wait before admission is refused.
    pub queue_deadline: QueueDeadline,
    /// How long admitted work may run before it is abandoned.
    pub operation_deadline: OperationDeadline,
    /// How long a clean shutdown waits for admitted work to settle.
    pub shutdown_budget_ms: u64,
}

impl Default for SchedulerLimits {
    fn default() -> Self {
        Self {
            read_concurrency: 8,
            queue_limit: 64,
            starvation_bound: StarvationBound::default(),
            queue_deadline: match QueueDeadline::new(30_000) {
                Ok(deadline) => deadline,
                Err(_) => unreachable!(),
            },
            operation_deadline: match OperationDeadline::new(120_000) {
                Ok(deadline) => deadline,
                Err(_) => unreachable!(),
            },
            shutdown_budget_ms: 30_000,
        }
    }
}

/// One admission request: which lane, which priority class, and the cancellation
/// token the caller may trip before or during execution.
#[derive(Debug, Clone)]
pub struct AdmissionRequest {
    lane: Lane,
    priority: QueuePriority,
    cancellation: CancellationToken,
    queued_at_unix_ms: u64,
}

impl AdmissionRequest {
    /// Build one admission request.
    #[must_use]
    pub fn new(
        lane: Lane,
        priority: QueuePriority,
        cancellation: CancellationToken,
        queued_at_unix_ms: u64,
    ) -> Self {
        Self {
            lane,
            priority,
            cancellation,
            queued_at_unix_ms,
        }
    }

    /// Which lane is being requested.
    #[must_use]
    pub const fn lane(&self) -> Lane {
        self.lane
    }

    /// Which priority class the work belongs to.
    #[must_use]
    pub const fn priority(&self) -> QueuePriority {
        self.priority
    }

    /// The caller's cancellation token.
    #[must_use]
    pub const fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    /// When the request entered the queue.
    #[must_use]
    pub const fn queued_at_unix_ms(&self) -> u64 {
        self.queued_at_unix_ms
    }
}

/// Why admission was refused, in a form that separates "never started" from
/// "started and did not finish".
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AdmissionError {
    /// The scheduler's mode refuses this lane right now.
    #[error("{0}")]
    ModeRefused(String),
    /// The queue is full.
    #[error("scheduler queue limit reached")]
    QueueFull,
    /// The work waited past its queue deadline and was never started.
    #[error("the operation waited longer than its queue deadline and was never started")]
    QueueDeadlineExpired,
    /// The work was cancelled while still queued; no boundary was touched.
    #[error("the operation was cancelled before it started; no effect was attempted")]
    CancelledBeforeStart,
    /// A quarantined transaction or unresolved survivor blocks every mutation.
    #[error("{0}")]
    WriteBlocked(String),
    /// The requested plane has no boundary in this build.
    #[error("the {0} boundary is not available")]
    PlaneUnavailable(&'static str),
}

impl AdmissionError {
    /// Map to the stable domain error taxonomy.
    #[must_use]
    pub fn to_outcome(&self) -> OutcomeError {
        match self {
            Self::ModeRefused(message) => {
                OutcomeError::new(ErrorKind::Unavailable, message, false, BTreeMap::new())
            }
            Self::QueueFull => OutcomeError::new(
                ErrorKind::Unavailable,
                self.to_string(),
                true,
                BTreeMap::new(),
            ),
            Self::QueueDeadlineExpired => QueueDeadline::expired_error(),
            Self::CancelledBeforeStart => CancellationOutcome::CancelledBeforeStart.to_outcome(),
            Self::WriteBlocked(message) => {
                OutcomeError::new(ErrorKind::Conflict, message, false, BTreeMap::new())
            }
            Self::PlaneUnavailable(_) => OutcomeError::new(
                ErrorKind::Unavailable,
                self.to_string(),
                false,
                BTreeMap::new(),
            ),
        }
    }
}

/// Admission ticket. Holding it keeps the lane's slot reserved; dropping it
/// releases the slot, so a cancelled or panicking operation cannot leak capacity.
#[derive(Debug)]
pub struct LaneAdmission {
    lane: Lane,
    priority: QueuePriority,
    counters: Arc<Counters>,
    cancellation: CancellationToken,
    operation_deadline: OperationDeadline,
    started: bool,
    _read_permit: Option<OwnedSemaphorePermit>,
}

impl LaneAdmission {
    /// Which lane was admitted.
    #[must_use]
    pub const fn lane(&self) -> Lane {
        self.lane
    }

    /// Which priority class this admission was served in.
    #[must_use]
    pub const fn priority(&self) -> QueuePriority {
        self.priority
    }

    /// The run budget this admission must finish inside.
    #[must_use]
    pub const fn operation_deadline(&self) -> OperationDeadline {
        self.operation_deadline
    }

    /// Whether execution has been marked as begun.
    ///
    /// The distinction is load-bearing: a cancellation observed before this flips
    /// proves no boundary was touched, and after it does not.
    #[must_use]
    pub const fn has_started(&self) -> bool {
        self.started
    }

    /// Mark the point at which the boundary is about to be touched.
    ///
    /// After this call a cancellation can no longer be reported as
    /// [`CancellationOutcome::CancelledBeforeStart`].
    pub fn mark_started(&mut self) {
        self.started = true;
    }

    /// Classify a cancellation against this admission's execution state.
    ///
    /// Returns `None` when the token has not been tripped.
    #[must_use]
    pub fn cancellation_outcome(&self) -> Option<CancellationOutcome> {
        if !self.cancellation.is_cancelled() {
            return None;
        }
        Some(if self.started {
            CancellationOutcome::CancelledDuringExecution
        } else {
            CancellationOutcome::CancelledBeforeStart
        })
    }
}

impl Drop for LaneAdmission {
    fn drop(&mut self) {
        self.counters.running.fetch_sub(1, Ordering::AcqRel);
        self.counters.settled.notify_waiters();
    }
}

#[derive(Debug, Default)]
struct Counters {
    running: AtomicU32,
    queued: AtomicU32,
    settled: Notify,
}

/// Why every mutation is currently refused.
///
/// Both variants mean the same thing operationally — no write may proceed — but
/// they are distinct because they are cleared by different proofs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteBlock {
    /// One or more transactions are quarantined: neither their pre-state nor
    /// their post-state is proven.
    Quarantined(Vec<String>),
    /// A supervised process survived without a resolution, so the effect it was
    /// part of is not decidable from here.
    UnresolvedSurvivor(String),
    /// The block state itself could not be read. This is a refusal, never an
    /// assumed "clear": guessing "nothing is blocked" is how a write lands on top
    /// of an unproven state.
    Unreadable(String),
}

impl WriteBlock {
    /// The caller-facing refusal for this block.
    #[must_use]
    pub fn to_outcome(&self) -> OutcomeError {
        let (kind, message) = match self {
            Self::Quarantined(ids) => (
                ErrorKind::Conflict,
                format!("project has quarantined transactions ({ids:?}); writes are blocked"),
            ),
            Self::UnresolvedSurvivor(reason) => (
                ErrorKind::Conflict,
                format!("an unresolved process survivor blocks writes: {reason}"),
            ),
            Self::Unreadable(reason) => (
                ErrorKind::Unavailable,
                format!("the write-block state could not be read: {reason}"),
            ),
        };
        OutcomeError::new(kind, message, false, BTreeMap::new())
    }
}

/// Proof that an operator repair resolved a write block.
///
/// The type has a private unit seal, mirroring Todo 6's `AuthoritySeal`, so the
/// only way to obtain one is through the operator-recovery path that re-derived a
/// typed proof. There is deliberately no "force" or "clear" constructor: a write
/// block that could be waved away by an argument-free call would make the whole
/// quarantine protocol decorative.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairProof {
    transaction_id: String,
    operation: &'static str,
    seal: (),
}

impl RepairProof {
    /// The quarantined transaction this proof resolved.
    #[must_use]
    pub fn transaction_id(&self) -> &str {
        &self.transaction_id
    }

    /// The stable name of the repair operation that produced the proof.
    #[must_use]
    pub const fn operation(&self) -> &'static str {
        self.operation
    }

    /// The single mint point, reachable only from inside this crate's
    /// operator-recovery path after a typed proof was re-derived.
    ///
    /// That path is [`router::recovery`](crate::router), which mints a proof only
    /// when the repair produced a terminal, proof-backed journal record.
    pub(crate) const fn sealed(transaction_id: String, operation: &'static str) -> Self {
        Self {
            transaction_id,
            operation,
            seal: (),
        }
    }
}

/// Which durable components a scheduler currently has bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalBinding {
    /// The cross-plane idempotency gate.
    pub idempotency: bool,
    /// The durable transaction store.
    pub store: bool,
    /// The launch survivor probe.
    pub survivors: bool,
}

impl JournalBinding {
    /// Whether all three are bound.
    ///
    /// A partially bound scheduler is never "ready": each component gates a
    /// different refusal, so any missing one silently removes a guarantee.
    #[must_use]
    pub const fn is_complete(self) -> bool {
        self.idempotency && self.store && self.survivors
    }
}

/// The per-project scheduler.
pub struct Scheduler {
    project: ProjectIdentity,
    limits: SchedulerLimits,
    reads: Arc<Semaphore>,
    effect_lane: AsyncMutex<()>,
    counters: Arc<Counters>,
    mode: Mutex<SchedulerMode>,
    planes: Mutex<PlaneAvailability>,
    fairness: Mutex<FairnessLedger>,
    turnstile: Arc<Notify>,
    idempotency: Mutex<Option<Arc<IdempotencyGate>>>,
    store: Mutex<Option<Arc<TransactionStore>>>,
    survivors: Mutex<Option<Arc<dyn SurvivorProbe>>>,
}

/// One waiter's position in the fairness turnstile.
#[derive(Debug, Clone, Copy)]
struct Waiter {
    sequence: u64,
    priority: QueuePriority,
    overtakes: u32,
    promoted: bool,
}

impl Waiter {
    /// Ordering rank. A promoted waiter outranks every unpromoted class, which is
    /// what turns the starvation bound into a hard guarantee rather than a hint.
    const fn rank(self) -> u8 {
        if self.promoted {
            u8::MAX
        } else {
            match self.priority {
                QueuePriority::Bulk => 0,
                QueuePriority::Normal => 1,
                QueuePriority::Interactive => 2,
                QueuePriority::Operator => 3,
            }
        }
    }
}

#[derive(Debug, Default)]
struct FairnessLedger {
    next_sequence: u64,
    waiters: Vec<Waiter>,
    queued: BTreeMap<&'static str, u32>,
    admitted: BTreeMap<&'static str, u64>,
    max_observed_overtakes: u32,
    promotions: u64,
}

impl FairnessLedger {
    fn enqueue(&mut self, priority: QueuePriority) -> u64 {
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        self.waiters.push(Waiter {
            sequence,
            priority,
            overtakes: 0,
            promoted: false,
        });
        *self.queued.entry(priority.as_str()).or_insert(0) += 1;
        sequence
    }

    /// Whether `sequence` is the waiter the turnstile must serve next.
    fn is_head(&self, sequence: u64) -> bool {
        self.waiters
            .iter()
            .max_by_key(|waiter| (waiter.rank(), u64::MAX - waiter.sequence))
            .is_some_and(|head| head.sequence == sequence)
    }

    fn remove(&mut self, sequence: u64) -> Option<Waiter> {
        let index = self
            .waiters
            .iter()
            .position(|waiter| waiter.sequence == sequence)?;
        let waiter = self.waiters.remove(index);
        if let Some(count) = self.queued.get_mut(waiter.priority.as_str()) {
            *count = count.saturating_sub(1);
        }
        Some(waiter)
    }

    /// Record one admission and charge an overtake to every waiter it passed.
    fn admit(&mut self, sequence: u64, bound: StarvationBound) {
        let Some(admitted) = self.remove(sequence) else {
            return;
        };
        *self.admitted.entry(admitted.priority.as_str()).or_insert(0) += 1;
        let admitted_rank = admitted.rank();
        for waiter in &mut self.waiters {
            if waiter.rank() >= admitted_rank {
                continue;
            }
            waiter.overtakes += 1;
            self.max_observed_overtakes = self.max_observed_overtakes.max(waiter.overtakes);
            if waiter.overtakes >= bound.overtakes() && !waiter.promoted {
                waiter.promoted = true;
                self.promotions += 1;
            }
        }
    }
}

impl Scheduler {
    /// Create a scheduler for one held project.
    ///
    /// Every plane starts unavailable. Availability is an *observation* the
    /// assembling runtime supplies from the effect gateway's own boundaries, so a
    /// freshly built scheduler can never claim a boundary that is not there.
    #[must_use]
    pub fn new(project: ProjectIdentity, limits: SchedulerLimits) -> Self {
        Self {
            project,
            limits,
            reads: Arc::new(Semaphore::new(limits.read_concurrency.max(1) as usize)),
            effect_lane: AsyncMutex::new(()),
            counters: Arc::new(Counters::default()),
            mode: Mutex::new(SchedulerMode::Accepting),
            planes: Mutex::new(PlaneAvailability::new(false, false, false, false, false)),
            fairness: Mutex::new(FairnessLedger::default()),
            turnstile: Arc::new(Notify::new()),
            idempotency: Mutex::new(None),
            store: Mutex::new(None),
            survivors: Mutex::new(None),
        }
    }

    /// Record which planes have a real boundary compiled in.
    pub fn set_plane_availability(&self, availability: PlaneAvailability) {
        *self
            .planes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = availability;
    }

    /// Which planes can currently execute.
    #[must_use]
    pub fn plane_availability(&self) -> PlaneAvailability {
        *self
            .planes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Bind the cross-plane idempotency gate, the durable store it reads and the
    /// survivor probe, all over the *existing* Todo 6 and Todo 17 machinery.
    ///
    /// The production assembly point is
    /// [`OutcomeRouter::assemble`](crate::router::OutcomeRouter), so a transport
    /// that routes cannot forget to bind: building the router binds all three.
    /// Until a scheduler is bound, [`Self::write_block`] reports
    /// [`WriteBlock::Unreadable`] and [`Self::replay_decision`] refuses — an
    /// unbound scheduler fails closed rather than pretending nothing is recorded.
    pub(crate) fn bind_journal(
        &self,
        gate: Arc<IdempotencyGate>,
        store: Arc<TransactionStore>,
        survivors: Arc<dyn SurvivorProbe>,
    ) {
        *self
            .idempotency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(gate);
        *self
            .store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(store);
        *self
            .survivors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(survivors);
    }

    /// Which durable components are bound right now.
    ///
    /// This exists so the *shipped* build can be asserted to have them, not just
    /// a test build: three `None`s here mean cross-plane idempotency, exact
    /// same-context replay and unresolved-survivor write blocking are all inert.
    #[must_use]
    pub fn journal_binding(&self) -> JournalBinding {
        JournalBinding {
            idempotency: self
                .idempotency
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_some(),
            store: self
                .store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_some(),
            survivors: self
                .survivors
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_some(),
        }
    }

    /// Decide whether a presented identity may replay, must conflict, or is new.
    ///
    /// # Errors
    /// Returns [`ErrorKind::Unavailable`] when the durable journal cannot be read.
    /// It never falls back to "assume new": guessing would authorize a duplicate
    /// effect.
    pub fn replay_decision(
        &self,
        identity: &MutationIdentity,
    ) -> Result<ReplayDecision, OutcomeError> {
        let gate = self
            .idempotency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .ok_or_else(|| {
                OutcomeError::new(
                    ErrorKind::Unavailable,
                    "no durable journal is bound to this scheduler",
                    false,
                    BTreeMap::new(),
                )
            })?;
        gate.decide(identity)
    }

    /// Why every mutation is currently blocked, if it is.
    ///
    /// # Errors
    /// Returns [`ErrorKind::Unavailable`] when the block state cannot be read.
    pub fn write_block(&self) -> Result<Option<WriteBlock>, OutcomeError> {
        match self.quarantined_transactions() {
            Ok(quarantined) if !quarantined.is_empty() => {
                return Ok(Some(WriteBlock::Quarantined(quarantined)));
            }
            Ok(_) => {}
            Err(reason) => return Ok(Some(WriteBlock::Unreadable(reason))),
        }
        // An unreadable survivor state is a refusal, never an assumed "clear":
        // that assumption is exactly how a write lands on top of an unproven state.
        match self.unresolved_survivor() {
            Ok(Some(reason)) => Ok(Some(WriteBlock::UnresolvedSurvivor(reason))),
            Ok(None) => Ok(None),
            Err(reason) => Ok(Some(WriteBlock::Unreadable(reason))),
        }
    }

    /// Read every quarantined transaction independently of survivor state.
    pub(crate) fn quarantined_transactions(&self) -> Result<Vec<String>, String> {
        let store = self
            .store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let Some(store) = store else {
            return Err("no durable journal is bound to this scheduler".to_owned());
        };
        store
            .quarantined_transactions()
            .map_err(|error| error.to_string())
    }

    /// Read the unresolved survivor independently of quarantine state.
    pub(crate) fn unresolved_survivor(&self) -> Result<Option<String>, String> {
        let survivors = self
            .survivors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let Some(survivors) = survivors else {
            return Err("no launch survivor probe is bound to this scheduler".to_owned());
        };
        survivors.unresolved_survivor()
    }

    /// Lift a write block, and only with a proof an operator repair produced.
    ///
    /// # Errors
    /// Returns [`ErrorKind::Conflict`] when the proof does not resolve the
    /// transaction that is actually blocking writes.
    pub fn resolve_write_block(&self, proof: &RepairProof) -> Result<(), OutcomeError> {
        // The proof is a precondition, not the answer: the block state is
        // re-read from the journal afterwards, so a proof for the wrong
        // transaction — or a second still-quarantined transaction — leaves writes
        // blocked. Nothing here can force or clear a block.
        let still_blocked = self.write_block()?;
        match still_blocked {
            None => Ok(()),
            Some(WriteBlock::Quarantined(ids))
                if ids.iter().any(|id| id == proof.transaction_id()) =>
            {
                Err(OutcomeError::new(
                    ErrorKind::Conflict,
                    format!(
                        "the {} repair did not settle transaction {}; writes remain blocked",
                        proof.operation(),
                        proof.transaction_id()
                    ),
                    false,
                    BTreeMap::new(),
                ))
            }
            Some(block) => Err(block.to_outcome()),
        }
    }

    /// Move the scheduler into a new admission mode.
    pub fn set_mode(&self, mode: SchedulerMode) {
        *self
            .mode
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = mode;
    }

    /// Current admission mode.
    #[must_use]
    pub fn mode(&self) -> SchedulerMode {
        *self
            .mode
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Admit one read in a named priority class, honouring the queue deadline and
    /// the caller's cancellation token.
    ///
    /// A token already tripped when the wait ends yields
    /// [`AdmissionError::CancelledBeforeStart`], which is a *proof* that no work
    /// began — not merely a report that the caller stopped waiting.
    ///
    /// # Errors
    /// Returns the admission refusal; none of them touch a boundary.
    pub async fn admit_read_classed(
        &self,
        request: &AdmissionRequest,
    ) -> Result<LaneAdmission, AdmissionError> {
        if matches!(self.mode(), SchedulerMode::Stopped) {
            return Err(AdmissionError::ModeRefused(
                "scheduler is stopped".to_owned(),
            ));
        }
        let deadline = queue_deadline_from_now(self.limits.queue_deadline);
        let turn = self.wait_for_turn(request, deadline).await?;
        let permit = tokio::select! {
            permit = Arc::clone(&self.reads).acquire_owned() => permit
                .map_err(|_| AdmissionError::ModeRefused(
                    "scheduler read lane is closed".to_owned(),
                ))?,
            () = request.cancellation.cancelled() => {
                return Err(AdmissionError::CancelledBeforeStart);
            }
            () = tokio::time::sleep_until(deadline) => {
                return Err(AdmissionError::QueueDeadlineExpired);
            }
        };
        turn.commit();
        Ok(LaneAdmission {
            lane: Lane::Read,
            priority: request.priority,
            counters: Arc::clone(&self.counters),
            cancellation: request.cancellation.clone(),
            operation_deadline: self.limits.operation_deadline,
            started: false,
            _read_permit: Some(permit),
        })
    }

    /// Wait for this request's turn at the fairness turnstile.
    ///
    /// The returned [`Turn`] holds the waiter's queue slot: dropping it without
    /// [`Turn::commit`] releases the slot, so a refused or cancelled waiter can
    /// never leak queue capacity or block the class behind it.
    async fn wait_for_turn(
        &self,
        request: &AdmissionRequest,
        deadline: Instant,
    ) -> Result<Turn<'_>, AdmissionError> {
        self.enter_queue().map_err(|_| AdmissionError::QueueFull)?;
        let sequence = self
            .fairness
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .enqueue(request.priority);
        let turn = Turn {
            scheduler: self,
            sequence,
            committed: false,
        };
        loop {
            // The wakeup is registered *before* the head test. Registering after it
            // would lose a notification published in between and wedge the waiter.
            let notified = self.turnstile.notified();
            let mut notified = std::pin::pin!(notified);
            notified.as_mut().enable();
            // Cancellation is checked before the head test, so work whose caller
            // already gave up never starts even if it happens to be next.
            if request.cancellation.is_cancelled() {
                return Err(AdmissionError::CancelledBeforeStart);
            }
            if self
                .fairness
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_head(sequence)
            {
                return Ok(turn);
            }
            tokio::select! {
                () = notified => {}
                () = request.cancellation.cancelled() => {
                    return Err(AdmissionError::CancelledBeforeStart);
                }
                () = tokio::time::sleep_until(deadline) => {
                    return Err(AdmissionError::QueueDeadlineExpired);
                }
            }
        }
    }

    /// Admit one exclusive mutation on a named plane.
    ///
    /// The order of the gates is itself normative: the plane's availability and
    /// the write block are checked *before* the work is queued, so an operation
    /// that can never execute is refused immediately instead of occupying queue
    /// capacity and then failing. The editor and runtime planes keep their branch
    /// here even in this release, where their bridges are absent: they are refused
    /// with `unavailable`, not silently removed.
    ///
    /// # Errors
    /// Returns the admission refusal; none of them touch a boundary.
    pub async fn admit_mutation(
        &self,
        request: &AdmissionRequest,
        plane: EffectPlane,
    ) -> Result<EffectAdmission<'_>, AdmissionError> {
        match self.mode() {
            SchedulerMode::Accepting => {}
            SchedulerMode::Draining => {
                return Err(AdmissionError::ModeRefused(
                    "scheduler is draining".to_owned(),
                ));
            }
            SchedulerMode::Quarantined => {
                return Err(AdmissionError::WriteBlocked(
                    "project has a quarantined transaction; writes are blocked".to_owned(),
                ));
            }
            SchedulerMode::Stopped => {
                return Err(AdmissionError::ModeRefused(
                    "scheduler is stopped".to_owned(),
                ));
            }
        }
        if !self.plane_availability().of(plane) {
            return Err(AdmissionError::PlaneUnavailable(plane.as_str()));
        }
        match self.write_block() {
            Ok(None) => {}
            Ok(Some(block)) => {
                return Err(AdmissionError::WriteBlocked(
                    block.to_outcome().message().to_owned(),
                ));
            }
            Err(error) => {
                return Err(AdmissionError::WriteBlocked(error.message().to_owned()));
            }
        }

        let deadline = queue_deadline_from_now(self.limits.queue_deadline);
        let turn = self.wait_for_turn(request, deadline).await?;
        let guard = tokio::select! {
            guard = self.effect_lane.lock() => guard,
            () = request.cancellation.cancelled() => {
                return Err(AdmissionError::CancelledBeforeStart);
            }
            () = tokio::time::sleep_until(deadline) => {
                return Err(AdmissionError::QueueDeadlineExpired);
            }
        };
        // A token tripped while this waiter held the lane guard still proves no
        // boundary ran: the guard is released by the early return and nothing was
        // dispatched.
        if request.cancellation.is_cancelled() {
            return Err(AdmissionError::CancelledBeforeStart);
        }
        turn.commit();
        Ok(EffectAdmission {
            admission: LaneAdmission {
                lane: Lane::Effect,
                priority: request.priority,
                counters: Arc::clone(&self.counters),
                cancellation: request.cancellation.clone(),
                operation_deadline: self.limits.operation_deadline,
                started: false,
                _read_permit: None,
            },
            _guard: guard,
        })
    }

    fn enter_queue(&self) -> Result<(), OutcomeError> {
        let queued = self.counters.queued.fetch_add(1, Ordering::AcqRel) + 1;
        if queued > self.limits.queue_limit {
            self.counters.queued.fetch_sub(1, Ordering::AcqRel);
            return Err(OutcomeError::new(
                ErrorKind::Unavailable,
                "scheduler queue limit reached",
                true,
                BTreeMap::new(),
            ));
        }
        Ok(())
    }

    /// The public bounded scheduler state for `resources/scheduler`.
    pub fn state(&self, observed_at_unix_ms: u64) -> Result<SchedulerState, OutcomeError> {
        SchedulerState::new(
            self.project.clone(),
            self.mode(),
            self.counters.running.load(Ordering::Acquire),
            self.counters.queued.load(Ordering::Acquire),
            self.limits.read_concurrency.max(1),
            self.limits.queue_limit,
            observed_at_unix_ms,
        )
        .map_err(|violation| {
            OutcomeError::new(
                violation.kind(),
                violation.to_string(),
                false,
                BTreeMap::new(),
            )
        })
    }

    /// Per-class occupancy plus the largest overtake count any waiter observed.
    #[must_use]
    pub fn fairness(&self) -> FairnessSnapshot {
        let ledger = self
            .fairness
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        FairnessSnapshot::new(
            ledger.queued.clone(),
            ledger.admitted.clone(),
            ledger.max_observed_overtakes,
            ledger.promotions,
        )
    }

    /// The configured starvation bound the fairness snapshot must respect.
    #[must_use]
    pub const fn starvation_bound(&self) -> StarvationBound {
        self.limits.starvation_bound
    }

    /// How many admitted operations are currently running.
    #[must_use]
    pub fn running(&self) -> u32 {
        self.counters.running.load(Ordering::Acquire)
    }

    /// How many operations are currently waiting for admission.
    #[must_use]
    pub fn queued(&self) -> u32 {
        self.counters.queued.load(Ordering::Acquire)
    }

    /// Stop admitting, let admitted work settle, and report what actually
    /// happened.
    ///
    /// Shutdown never claims a clean drain it did not observe: work still running
    /// when the budget expires is counted and reported, because an operation whose
    /// effect may have landed must not be recorded as "did not happen".
    pub async fn shutdown(&self) -> ShutdownOutcome {
        self.set_mode(SchedulerMode::Draining);
        self.turnstile.notify_waiters();
        let deadline =
            Instant::now() + Duration::from_millis(self.limits.shutdown_budget_ms.max(1));
        loop {
            // Registered before the counter is read, so a completion published
            // between the read and the wait cannot be missed.
            let settled = self.counters.settled.notified();
            let mut settled = std::pin::pin!(settled);
            settled.as_mut().enable();
            if self.counters.running.load(Ordering::Acquire) == 0 {
                self.set_mode(SchedulerMode::Stopped);
                return ShutdownOutcome::Drained;
            }
            tokio::select! {
                () = settled => {}
                () = tokio::time::sleep_until(deadline) => {
                    // Work still in flight is reported, never assumed settled: its
                    // effect may already have landed.
                    let still_running = self.counters.running.load(Ordering::Acquire);
                    self.set_mode(SchedulerMode::Stopped);
                    return if still_running == 0 {
                        ShutdownOutcome::Drained
                    } else {
                        ShutdownOutcome::DeadlineExpired { still_running }
                    };
                }
            }
        }
    }
}

/// One waiter's held place in the fairness turnstile.
///
/// Dropping it without [`Self::commit`] releases the queue slot and wakes the
/// class behind it, so a cancelled, deadline-expired or refused waiter cannot
/// leak capacity or wedge the queue.
struct Turn<'scheduler> {
    scheduler: &'scheduler Scheduler,
    sequence: u64,
    committed: bool,
}

impl Turn<'_> {
    fn commit(mut self) {
        self.committed = true;
        {
            let mut ledger = self
                .scheduler
                .fairness
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            ledger.admit(self.sequence, self.scheduler.limits.starvation_bound);
        }
        self.scheduler
            .counters
            .queued
            .fetch_sub(1, Ordering::AcqRel);
        self.scheduler
            .counters
            .running
            .fetch_add(1, Ordering::AcqRel);
        self.scheduler.turnstile.notify_waiters();
    }
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        {
            let mut ledger = self
                .scheduler
                .fairness
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            ledger.remove(self.sequence);
        }
        self.scheduler
            .counters
            .queued
            .fetch_sub(1, Ordering::AcqRel);
        self.scheduler.turnstile.notify_waiters();
    }
}

/// The single instant at which one admission's whole wait expires.
///
/// It is computed once per admission and threaded through every wait, so the
/// budget bounds the *total* time queued rather than resetting at each stage.
fn queue_deadline_from_now(deadline: QueueDeadline) -> Instant {
    Instant::now() + Duration::from_millis(deadline.wait_budget_ms())
}

/// An admitted effect. The held lane guard keeps effects serialized per project.
pub struct EffectAdmission<'lane> {
    admission: LaneAdmission,
    _guard: tokio::sync::MutexGuard<'lane, ()>,
}

impl EffectAdmission<'_> {
    /// Which lane was admitted.
    #[must_use]
    pub const fn lane(&self) -> Lane {
        self.admission.lane()
    }

    /// Which priority class this admission was served in.
    #[must_use]
    pub const fn priority(&self) -> QueuePriority {
        self.admission.priority()
    }

    /// The run budget this mutation must finish inside.
    #[must_use]
    pub const fn operation_deadline(&self) -> OperationDeadline {
        self.admission.operation_deadline()
    }

    /// Whether the boundary has been marked as reached.
    #[must_use]
    pub const fn has_started(&self) -> bool {
        self.admission.has_started()
    }

    /// Mark the point at which the boundary is about to be touched.
    pub fn mark_started(&mut self) {
        self.admission.mark_started();
    }

    /// Classify a cancellation against this mutation's execution state.
    #[must_use]
    pub fn cancellation_outcome(&self) -> Option<CancellationOutcome> {
        self.admission.cancellation_outcome()
    }
}

#[cfg(test)]
mod self_audit {
    //! Source-scanning self-audit: the bans this layer must keep are asserted
    //! against its own bytes, so a future edit that reintroduces one fails a test
    //! rather than passing review.

    const SCHEDULER_SOURCE: &str = include_str!("mod.rs");
    const FAIRNESS_SOURCE: &str = include_str!("fairness.rs");
    const PLANE_SOURCE: &str = include_str!("plane.rs");

    fn production_source(source: &str) -> &str {
        source
            .find("#[cfg(test)]")
            .map_or(source, |index| &source[..index])
    }

    /// The scheduler defines no second journal, schema or in-memory ledger.
    #[test]
    fn the_scheduler_defines_no_second_persistence() {
        for (name, source) in [
            ("scheduler/mod.rs", SCHEDULER_SOURCE),
            ("scheduler/fairness.rs", FAIRNESS_SOURCE),
            ("scheduler/plane.rs", PLANE_SOURCE),
        ] {
            let source = production_source(source);
            for banned in [
                "rusqlite",
                "Connection::open",
                "CREATE TABLE",
                "struct TransactionStore",
                "fn open_transaction",
                "include_str!(\"../../../../schemas",
            ] {
                assert!(
                    !source.contains(banned),
                    "{name} must reuse the Todo 6 journal, never define another ({banned})"
                );
            }
        }
    }

    /// The scheduler never touches the filesystem or reopens a raw path.
    #[test]
    fn the_scheduler_never_reopens_a_raw_path() {
        for (name, source) in [
            ("scheduler/mod.rs", SCHEDULER_SOURCE),
            ("scheduler/fairness.rs", FAIRNESS_SOURCE),
            ("scheduler/plane.rs", PLANE_SOURCE),
        ] {
            let source = production_source(source);
            for banned in [
                "std::fs::",
                "File::open",
                "File::create",
                "Path::new",
                "PathBuf::from",
                "canonicalize",
                "Command::new",
            ] {
                assert!(
                    !source.contains(banned),
                    "{name} must not reach the filesystem or spawn a process ({banned})"
                );
            }
        }
    }

    /// No lock is held across durable I/O.
    ///
    /// Every `Mutex` guard in this module is taken, read or written, and released
    /// inside one expression or block; the durable reads (`write_block`,
    /// `replay_decision`) clone the `Arc` out first and call the store afterwards.
    #[test]
    fn no_lock_is_held_over_durable_io() {
        let source = production_source(SCHEDULER_SOURCE);
        for guarded in [
            ".lock()\n            .unwrap_or_else(|poisoned| poisoned.into_inner())\n            .clone();\n        let Some(store)",
            ".lock()\n            .unwrap_or_else(|poisoned| poisoned.into_inner())\n            .clone();\n        let Some(survivors)",
        ] {
            assert!(
                source.contains(guarded),
                "the durable reads must clone the handle out of the lock before any I/O"
            );
        }
        assert!(
            !source.contains("quarantined_transactions()?;\n        }"),
            "the store must never be called while a guard is still in scope"
        );
    }

    /// There is exactly one write-block resolution path, and it takes a proof.
    #[test]
    fn a_write_block_is_lifted_only_by_a_proof() {
        let source = production_source(SCHEDULER_SOURCE);
        assert_eq!(
            source.matches("fn resolve_write_block").count(),
            1,
            "exactly one resolution path may exist"
        );
        assert!(
            source.contains("pub fn resolve_write_block(&self, proof: &RepairProof)"),
            "the resolution path must require a repair proof"
        );
        for banned in [
            "fn force_",
            "fn clear_write_block",
            "fn override_",
            "force: bool",
        ] {
            assert!(
                !source.contains(banned),
                "the scheduler must offer no force or clear path ({banned})"
            );
        }
    }

    /// Cancellation and deadline outcomes are never collapsed into one answer.
    #[test]
    fn the_two_cancellation_and_deadline_paths_stay_distinct() {
        let fairness = production_source(FAIRNESS_SOURCE);
        assert!(fairness.contains("CancelledBeforeStart"));
        assert!(fairness.contains("CancelledDuringExecution"));
        assert!(
            fairness.contains("ErrorKind::Uncertain"),
            "an in-flight cancellation must be uncertain, never cancelled"
        );
        assert!(
            fairness.contains("ErrorKind::Timeout"),
            "an expired run budget must be a timeout"
        );
        assert!(
            fairness.contains("ErrorKind::Unavailable"),
            "an expired queue budget must be unavailable"
        );
    }

    /// Every plane keeps its branch, including the ones this release cannot run.
    #[test]
    fn every_plane_keeps_its_branch() {
        let plane = production_source(PLANE_SOURCE);
        for named in ["File", "Process", "LspDap", "Editor", "Runtime"] {
            assert!(
                plane.contains(&format!("Self::{named}")),
                "the {named} plane must keep its branch even when its bridge is absent"
            );
        }
        assert!(
            plane.contains("EffectPlane::Editor"),
            "the editor plane must remain mapped rather than removed"
        );
    }

    #[test]
    fn a_promoted_waiter_outranks_every_unpromoted_class() {
        let promoted = super::Waiter {
            sequence: 9,
            priority: super::QueuePriority::Bulk,
            overtakes: 3,
            promoted: true,
        };
        for priority in super::QueuePriority::ALL {
            let unpromoted = super::Waiter {
                sequence: 0,
                priority,
                overtakes: 0,
                promoted: false,
            };
            assert!(
                promoted.rank() > unpromoted.rank(),
                "a promoted waiter must outrank the unpromoted {} class",
                priority.as_str()
            );
        }
    }

    #[test]
    fn the_starvation_bound_promotes_a_repeatedly_overtaken_waiter() {
        let bound = super::StarvationBound::new(2).expect("a positive bound");
        let mut ledger = super::FairnessLedger::default();
        let bulk = ledger.enqueue(super::QueuePriority::Bulk);
        for _ in 0..2 {
            let interactive = ledger.enqueue(super::QueuePriority::Interactive);
            assert!(
                ledger.is_head(interactive),
                "the higher class is served first while the bound allows it"
            );
            ledger.admit(interactive, bound);
        }
        assert_eq!(ledger.promotions, 1, "the starved waiter must be promoted");
        let interactive = ledger.enqueue(super::QueuePriority::Interactive);
        assert!(
            ledger.is_head(bulk),
            "a promoted waiter must now be served ahead of the higher class"
        );
        ledger.admit(bulk, bound);
        assert!(ledger.is_head(interactive));
    }
}
