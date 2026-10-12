//! The `test-hooks`-gated audit/limits harness.
//!
//! This harness is the **only** way a test can drive the audit sink and the limit
//! enforcer through the *production* policy authority, effect gateway and durable
//! journal. It deliberately exposes observations, never authority: no method
//! returns a permit, and the audit event it records is built from facts the
//! production layers produced.
//!
//! ## What is real, and what stands in
//!
//! Real: the project lease, the durable transaction journal, the effect gateway
//! and its `SafeRoot`-backed `file` boundary, the policy authority that mints the
//! permit, the decision identifier (derived by Todo 26's own function), the
//! terminal transaction phase, the append-only audit sink and every limit.
//!
//! Stand-in, and deliberately declared rather than hidden:
//!
//! * Every high-risk mutation executes through the canonical `resource.update`
//!   `file` row because this harness isolates file-limit and audit behavior. The
//!   audit event therefore reports `file` — the boundary the effect really reached
//!   — rather than the eventual boundary of the caller's tool.
//! * The other four boundaries are harness-local `UnavailableBoundaries`, which
//!   cannot execute anything, so the `file` counter is the single downstream touch
//!   counter a test needs to assert that a refused attempt dispatched nothing.
//! * The project generation is a fixed non-initial value, standing for a transport
//!   boundary that has already re-established the held project several times.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use async_trait::async_trait;
use godot_mcp_domain::CapabilityProfile;
use godot_mcp_domain::contracts::{
    Capability, ErrorKind, OutcomeError, PolicyReason, PolicyVerdict, PrincipalId, ProjectIdentity,
    SessionId,
};
use godot_mcp_domain::lease::{Lease, LeaseManager};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use super::HarnessError;
use crate::audit::{
    AuditBoundary, AuditDigest, AuditEvent, AuditEventDraft, AuditFailurePolicy, AuditLane,
    AuditLog, AuditPhase, AuditPriority, AuditQueueFacts, AuditSinkConfig, AuditStatus,
    AuditTransport,
};
use crate::effect::boundary::{EffectTicket, FileEffectBoundary, FileWrite};
use crate::effect::{
    EffectBoundaries, EffectGateway, FileEffectIntent, PlannedWrite, SafeRootFileBoundary,
    UnavailableBoundaries,
};
use crate::journal::{
    FileOperation, HeldProject, OperationFamily, Phase, SafeRootProofSource, TransactionStore,
    TransportMode, encode_object_identity,
};
use crate::kernel::{
    AccessContext, KernelToolCall, KernelToolHandler, ProgressReporter, ServiceKernel,
};
use crate::limits::{LimitEnforcer, LimitPolicy, LimitRejection};
use crate::observability::{CorrelationId, RequestId, SafeField};
use crate::policy::{
    BoundIdentity, BoundaryAvailability, DispatchRequest, PolicyAuthority, PolicyDispatchError,
    PolicyRefusal, SurvivorProbe, row_for,
};

/// The authority epoch every harness is bound to.
const EPOCH: u64 = 1;
/// The held project generation the harness reports.
const GENERATION: u64 = 7;
/// The opaque principal the harness binds.
const BOUND_PRINCIPAL: &str = "principal_0123456789abcdef";
/// The opaque session the harness binds.
const BOUND_SESSION: &str = "session_0123456789abcdef";
/// The canonical `file`-boundary row every high-risk mutation executes through.
const COMMIT_TOOL: &str = "resource.update";

/// How the harness bounds its audit sink and its limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditLimitsConfig {
    /// Rotate before a record would grow the live audit file past this size.
    pub max_audit_bytes: u64,
    /// How many rotated audit files to retain.
    pub max_audit_files: u8,
    /// Requests one subject may start inside the rate window.
    pub requests_per_window: u32,
    /// Width of the sliding rate window.
    pub rate_window_ms: u64,
    /// Concurrent in-flight requests one subject may hold.
    pub subject_concurrency: u32,
    /// Whether an unrecordable high-risk mutation must be refused.
    pub fail_closed: bool,
    /// Whether record writes fail after readiness probes have succeeded.
    pub record_failure_after_probe: bool,
}

impl Default for AuditLimitsConfig {
    fn default() -> Self {
        Self {
            max_audit_bytes: 8 * 1024 * 1024,
            max_audit_files: 5,
            requests_per_window: 256,
            rate_window_ms: 60_000,
            subject_concurrency: 8,
            fail_closed: true,
            record_failure_after_probe: false,
        }
    }
}

/// One attempted request through the audit/limit gates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttemptSpec<'spec> {
    /// Canonical tool the caller asked for.
    pub tool: &'spec str,
    /// Trusted current time, supplied instead of a global clock.
    pub now_ms: u64,
    /// Declared request-body size.
    pub request_bytes: u64,
    /// Declared response size.
    pub response_bytes: u64,
}

/// What an admitted attempt did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptReport {
    /// Whether the attempt passed every gate and reached dispatch.
    pub dispatched: bool,
    /// Whether the audit record was durably written.
    pub audit_recorded: bool,
    /// Terminal durable phase, for a high-risk mutation.
    pub phase: Option<String>,
    /// Non-sensitive decision identifier the policy layer produced.
    pub decision_id: Option<String>,
}

/// Why an attempt was refused before it reached dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptRefusal {
    /// Stable domain error code from the ten-item taxonomy.
    pub code: &'static str,
    /// Whether retrying may succeed.
    pub retryable: bool,
    /// HTTP status the optional HTTP transport would return.
    pub http_status: u16,
    /// Whether an audit record was written for the refusal.
    pub audit_recorded: bool,
    /// Which bound refused the attempt, when a bound did.
    pub limit: Option<&'static str>,
}

/// What a negative tool call observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegativeCallReport {
    /// Stable domain error code the kernel returned.
    pub code: &'static str,
    /// Whether any downstream handler was invoked.
    pub dispatched: bool,
}

/// What a production-kernel call observed when audit recording was controlled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelAuditCallReport {
    /// Stable domain error code, or `ok` when the call succeeded.
    pub code: &'static str,
    /// Caller-facing outcome message, absent on success.
    pub message: Option<String>,
    /// Whether the returned outcome permits a retry.
    pub retryable: bool,
    /// Whether the registered handler completed.
    pub dispatched: bool,
}

/// The limit gate a test inspects.
#[derive(Debug, Clone)]
pub struct LimitGate {
    enforcer: Arc<LimitEnforcer>,
}

impl LimitGate {
    /// The single enforcer the harness consults before dispatch.
    #[must_use]
    pub fn enforcer(&self) -> Arc<LimitEnforcer> {
        Arc::clone(&self.enforcer)
    }
}

#[derive(Debug, Default)]
struct TouchCounter(AtomicUsize);

struct RecordingFileBoundary {
    inner: SafeRootFileBoundary,
    counter: Arc<TouchCounter>,
}

impl FileEffectBoundary for RecordingFileBoundary {
    fn available(&self) -> bool {
        self.inner.available()
    }

    fn publish(
        &self,
        ticket: &EffectTicket,
        write: &FileWrite,
    ) -> Result<(), godot_mcp_domain::safe_root::SafeRootError> {
        self.counter.0.fetch_add(1, Ordering::Relaxed);
        self.inner.publish(ticket, write)
    }

    fn delete(
        &self,
        ticket: &EffectTicket,
        path: &str,
        expected: &[u8],
    ) -> Result<(), godot_mcp_domain::safe_root::SafeRootError> {
        self.counter.0.fetch_add(1, Ordering::Relaxed);
        self.inner.delete(ticket, path, expected)
    }
}

struct ClearSurvivors;

impl SurvivorProbe for ClearSurvivors {
    fn unresolved_survivor(&self) -> Result<Option<String>, String> {
        Ok(None)
    }
}

struct RefusedHandler;

#[async_trait]
impl KernelToolHandler for RefusedHandler {
    async fn call(&self, _call: KernelToolCall) -> Result<Value, OutcomeError> {
        Err(OutcomeError::new(
            ErrorKind::Internal,
            "the harness never expects this handler to run",
            false,
            std::collections::BTreeMap::new(),
        ))
    }
}

struct CountingHandler(Arc<AtomicUsize>);

#[async_trait]
impl KernelToolHandler for CountingHandler {
    async fn call(&self, _call: KernelToolCall) -> Result<Value, OutcomeError> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok(Value::Object(Map::new()))
    }
}

/// The Todo 31 acceptance harness over one held project.
pub struct AuditLimitsHarness {
    _lease: Lease,
    gateway: EffectGateway,
    authority: PolicyAuthority,
    identity: BoundIdentity,
    project: ProjectIdentity,
    principal: PrincipalId,
    session: SessionId,
    profile: CapabilityProfile,
    counter: Arc<TouchCounter>,
    audit: Arc<AuditLog>,
    gate: LimitGate,
    subject: SafeField,
    fail_closed: bool,
    next_id: AtomicU64,
}

impl AuditLimitsHarness {
    /// Establish a project, journal, gateway, policy authority, audit sink and
    /// limit enforcer below one parent directory.
    ///
    /// # Errors
    /// Returns [`HarnessError`] when the project cannot be established, the
    /// durable store cannot be opened, or the audit sink cannot be created.
    pub fn open(parent: &Path, config: AuditLimitsConfig) -> Result<Self, HarnessError> {
        let root = prepare_project(parent)?;
        let state_dir = crate::stdio::state_dir_for(&root);
        let lease = LeaseManager::new(&state_dir)
            .acquire(&root)
            .map_err(|error| HarnessError::Lease(error.to_string()))?;
        let key = lease.lease_key();
        let project = ProjectIdentity::from_lease_key(key, GENERATION);
        let held = HeldProject {
            object_identity: project.object_id().to_string(),
            root_object: encode_object_identity(key.root),
            file_object: encode_object_identity(key.project_file),
            generation: GENERATION,
        };
        let store = Arc::new(
            TransactionStore::open(&state_dir.join("journal"))
                .map_err(|error| HarnessError::Store(error.to_string()))?,
        );
        let counter = Arc::new(TouchCounter::default());
        let gateway = EffectGateway::new(
            Arc::clone(&store),
            held,
            EPOCH,
            Box::new(SafeRootProofSource::new(lease.safe_root())),
            EffectBoundaries {
                file: Box::new(RecordingFileBoundary {
                    inner: SafeRootFileBoundary::new(lease.safe_root()),
                    counter: Arc::clone(&counter),
                }),
                process: Box::new(UnavailableBoundaries),
                lsp_dap: Box::new(UnavailableBoundaries),
                editor: Box::new(UnavailableBoundaries),
                runtime: Box::new(UnavailableBoundaries),
            },
        );
        let availability = BoundaryAvailability::from_gateway(&gateway);
        let principal = PrincipalId::parse(BOUND_PRINCIPAL)
            .map_err(|error| HarnessError::Permit(error.to_string()))?;
        let session = SessionId::parse(BOUND_SESSION)
            .map_err(|error| HarnessError::Permit(error.to_string()))?;
        let identity = BoundIdentity {
            project: project.clone(),
            principal: Some(principal.clone()),
            session: Some(session.clone()),
        };
        let authority = PolicyAuthority::new(
            CapabilityProfile::Full,
            identity.clone(),
            store,
            availability,
            Box::new(ClearSurvivors),
            EPOCH,
        );
        let audit = Arc::new(
            AuditLog::open(AuditSinkConfig {
                path: state_dir.join("audit").join("audit.jsonl"),
                max_bytes: config.max_audit_bytes,
                max_files: config.max_audit_files,
                failure_policy: if config.fail_closed {
                    AuditFailurePolicy::FailClosed
                } else {
                    AuditFailurePolicy::FailOpen
                },
            })
            .map_err(|error| HarnessError::Store(error.to_string()))?,
        );
        audit.set_record_failure(config.record_failure_after_probe);
        let enforcer = Arc::new(
            LimitEnforcer::new(LimitPolicy {
                requests_per_window: config.requests_per_window,
                rate_window_ms: config.rate_window_ms,
                max_subject_concurrency: config.subject_concurrency,
                ..LimitPolicy::default()
            })
            .map_err(|error| HarnessError::Store(error.to_string()))?,
        );
        Ok(Self {
            _lease: lease,
            gateway,
            authority,
            identity,
            project,
            principal,
            session,
            profile: CapabilityProfile::Full,
            counter,
            audit,
            gate: LimitGate { enforcer },
            subject: SafeField::new("subject_task31")
                .map_err(|error| HarnessError::Permit(error.to_string()))?,
            fail_closed: config.fail_closed,
            next_id: AtomicU64::new(1),
        })
    }

    /// How many times the downstream `file` boundary has been invoked.
    #[must_use]
    pub fn touches(&self) -> usize {
        self.counter.0.load(Ordering::Relaxed)
    }

    /// The append-only audit sink.
    #[must_use]
    pub fn audit(&self) -> &AuditLog {
        self.audit.as_ref()
    }

    /// The limit gate every attempt passes through.
    #[must_use]
    pub const fn gate(&self) -> &LimitGate {
        &self.gate
    }

    /// The rate/concurrency subject the harness attributes work to.
    #[must_use]
    pub fn subject(&self) -> SafeField {
        self.subject.clone()
    }

    /// Every recorded audit event in the live file, oldest first.
    ///
    /// # Errors
    /// Returns [`HarnessError`] when the sink cannot be read or a line is not
    /// canonical JSON.
    pub fn audit_events(&self) -> Result<Vec<Value>, HarnessError> {
        let bytes = self
            .audit
            .current_bytes()
            .map_err(|error| HarnessError::Store(error.to_string()))?;
        let text = String::from_utf8(bytes)
            .map_err(|_| HarnessError::Store("audit sink is not valid UTF-8".to_owned()))?;
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                serde_json::from_str::<Value>(line)
                    .map_err(|_| HarnessError::Store("audit line is not canonical JSON".to_owned()))
            })
            .collect()
    }

    /// Run one attempt through the limit gates, the audit readiness probe, the
    /// production dispatch and the audit record, in that order.
    ///
    /// # Errors
    /// Returns [`AttemptRefusal`] when any gate refused the attempt, in which case
    /// no dispatch happened.
    pub fn attempt(&self, spec: AttemptSpec<'_>) -> Result<AttemptReport, AttemptRefusal> {
        let Some(row) = row_for(spec.tool) else {
            return Err(AttemptRefusal {
                code: ErrorKind::NotFound.code(),
                retryable: false,
                http_status: 404,
                audit_recorded: false,
                limit: None,
            });
        };
        let high_risk = row.requires_permit();
        let enforcer = self.gate.enforcer();

        enforcer
            .note_request()
            .and_then(|()| enforcer.check_body_bytes(spec.request_bytes))
            .and_then(|()| enforcer.check_response_bytes(spec.response_bytes))
            .and_then(|()| enforcer.check_rate(&self.subject, spec.now_ms))
            .map_err(refusal_from_limit)?;
        let _slot = Arc::clone(&enforcer)
            .admit(&self.subject, spec.now_ms)
            .map_err(refusal_from_limit)?;

        let sink_ready = self.audit.probe_writable().is_ok();
        if !sink_ready && high_risk && self.fail_closed {
            return Err(AttemptRefusal {
                code: ErrorKind::Unavailable.code(),
                retryable: true,
                http_status: 503,
                audit_recorded: false,
                limit: None,
            });
        }

        let sequence = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (phase, decision_id) = if high_risk {
            let outcome = self.commit_file_effect(sequence)?;
            (Some(outcome.0), outcome.1)
        } else {
            (None, self.authorize_read(spec.tool, row.capability)?)
        };

        let event = self
            .build_event(&spec, row.capability, sequence, phase, &decision_id)
            .map_err(|error| AttemptRefusal {
                code: ErrorKind::Internal.code(),
                retryable: false,
                http_status: 500,
                audit_recorded: false,
                limit: Some(error),
            })?;
        let audit_recorded = self.audit.record(&event).is_ok();
        Ok(AttemptReport {
            dispatched: true,
            audit_recorded,
            phase: phase.map(|phase| audit_phase_of(phase).audit_name().to_owned()),
            decision_id: Some(decision_id),
        })
    }

    /// Attempt to bind a handler to a name no canonical profile advertises.
    ///
    /// A handler registers under the handler key of a catalog descriptor, so a
    /// name the catalog does not hold has no key to register under and is
    /// refused as not found before any registry is reached.
    ///
    /// # Errors
    /// Returns [`HarnessError`] when the kernel *accepted* the registration, which
    /// would mean an execution surface exists.
    pub fn register_reserved_handler(&self, name: &str) -> Result<&'static str, HarnessError> {
        let Some(key) = godot_mcp_domain::catalog::descriptor(name).and_then(|tool| tool.handler)
        else {
            return Ok(ErrorKind::NotFound.code());
        };
        let mut kernel = ServiceKernel::new(self.profile);
        match kernel.register_tool_handler(key, Arc::new(RefusedHandler)) {
            Ok(()) => Err(HarnessError::Permit(
                "the kernel registry accepted a non-canonical handler".to_owned(),
            )),
            Err(error) => Ok(error.code()),
        }
    }

    /// Call one non-canonical tool through the production kernel and observe both
    /// the returned error class and whether anything downstream ran.
    pub async fn negative_tool_call(&self, name: &str) -> NegativeCallReport {
        let dispatches = Arc::new(AtomicUsize::new(0));
        let mut kernel = ServiceKernel::new(self.profile);
        if let Some(key) =
            godot_mcp_domain::catalog::descriptor(COMMIT_TOOL).and_then(|tool| tool.handler)
        {
            let _ = kernel
                .register_tool_handler(key, Arc::new(CountingHandler(Arc::clone(&dispatches))));
        }
        let outcome = kernel
            .call_tool(
                name,
                Map::new(),
                self.access(),
                CancellationToken::new(),
                ProgressReporter::noop(),
            )
            .await;
        NegativeCallReport {
            code: outcome.err().map_or("ok", |error| error.code()),
            dispatched: dispatches.load(Ordering::Relaxed) > 0,
        }
    }

    /// Call a canonical tool through the production kernel with this harness's
    /// configured audit sink and report both dispatch and caller outcome.
    pub async fn kernel_tool_call(
        &self,
        name: &str,
        idempotency_id: Option<&str>,
    ) -> KernelAuditCallReport {
        let dispatches = Arc::new(AtomicUsize::new(0));
        let mut kernel = ServiceKernel::new(self.profile)
            .with_operational_controls(self.gate.enforcer(), Arc::clone(&self.audit));
        let key = godot_mcp_domain::catalog::descriptor(name)
            .and_then(|tool| tool.handler)
            .expect("the harness calls only catalog tools");
        // The kernel answers its own tools; every other one gets the counting
        // handler.
        if !matches!(key, godot_mcp_domain::catalog::HandlerKey::Kernel(_)) {
            kernel
                .register_tool_handler(key, Arc::new(CountingHandler(Arc::clone(&dispatches))))
                .expect("the harness registers only canonical tools");
        }
        let mut arguments = Map::new();
        if let Some(idempotency_id) = idempotency_id {
            arguments.insert(
                "idempotencyId".to_owned(),
                Value::String(idempotency_id.to_owned()),
            );
        }
        let outcome = kernel
            .call_tool(
                name,
                arguments,
                self.access(),
                CancellationToken::new(),
                ProgressReporter::noop(),
            )
            .await;
        match outcome {
            Ok(_) => KernelAuditCallReport {
                code: "ok",
                message: None,
                retryable: false,
                dispatched: dispatches.load(Ordering::Relaxed) > 0,
            },
            Err(error) => KernelAuditCallReport {
                code: error.code(),
                message: Some(error.message().to_owned()),
                retryable: error.retryable(),
                dispatched: dispatches.load(Ordering::Relaxed) > 0,
            },
        }
    }

    fn access(&self) -> AccessContext {
        AccessContext::new(
            self.project.clone(),
            self.principal.clone(),
            Some(self.session.clone()),
            1_000,
        )
    }

    fn commit_file_effect(&self, sequence: u64) -> Result<(Phase, String), AttemptRefusal> {
        let transaction_id = format!("00000000-0000-4000-8000-{sequence:012}");
        let path = format!("scenes/audit-{sequence:04}.tres");
        let intent = FileEffectIntent {
            transaction_id: transaction_id.clone(),
            tool_name: COMMIT_TOOL.to_owned(),
            operation_family: OperationFamily::ProjectMutation,
            idempotency_id: format!("idem-{sequence:012}"),
            transport_mode: TransportMode::Stdio,
            principal: Some(self.principal.as_str().to_owned()),
            session: Some(self.session.as_str().to_owned()),
            arguments: serde_json::json!({ "sequence": sequence }),
            writes: vec![PlannedWrite {
                path,
                operation: FileOperation::Create,
                bytes: Some(format!("audit fixture {sequence}\n").into_bytes()),
                precondition: crate::effect::WritePrecondition::Observed,
            }],
            required_process_transaction: None,
            opened_at_unix_ms: 1_700_000_000_000,
            expected_revision: None,
        };
        self.gateway
            .open_file_transaction(&intent)
            .and_then(|_record| self.gateway.stage_file_transaction(&intent))
            .map_err(|error| refusal_from_outcome(&error.to_outcome()))?;
        let grant = self
            .authority
            .authorize(&DispatchRequest {
                tool: COMMIT_TOOL.to_owned(),
                project: self.project.clone(),
                principal: Some(self.principal.clone()),
                session: Some(self.session.clone()),
                transaction_id: Some(transaction_id),
            })
            .map_err(|refusal| refusal_from_policy(&refusal))?;
        let permit = grant.permit.ok_or_else(|| AttemptRefusal {
            code: ErrorKind::PermissionDenied.code(),
            retryable: false,
            http_status: 403,
            audit_recorded: false,
            limit: None,
        })?;
        let decision_id = permit.decision_id().to_owned();
        let outcome = self
            .authority
            .dispatch_file_effect(&self.gateway, &permit)
            .map_err(|error| match error {
                PolicyDispatchError::Refused(refusal) => refusal_from_policy(&refusal),
                PolicyDispatchError::Effect(effect) => refusal_from_outcome(&effect.to_outcome()),
            })?;
        Ok((outcome.phase, decision_id))
    }

    fn authorize_read(&self, tool: &str, capability: Capability) -> Result<String, AttemptRefusal> {
        self.authority
            .authorize(&DispatchRequest {
                tool: tool.to_owned(),
                project: self.project.clone(),
                principal: Some(self.principal.clone()),
                session: Some(self.session.clone()),
                transaction_id: None,
            })
            .map_err(|refusal| refusal_from_policy(&refusal))?;
        Ok(crate::policy::decision::decision_id(
            &self.identity,
            tool,
            capability,
            PolicyVerdict::Allow,
            PolicyReason::Granted,
            EPOCH,
        ))
    }

    fn build_event(
        &self,
        spec: &AttemptSpec<'_>,
        capability: Capability,
        sequence: u64,
        phase: Option<Phase>,
        decision_id: &str,
    ) -> Result<AuditEvent, &'static str> {
        let correlation = CorrelationId::new(format!("corr_31_{sequence}"))
            .map_err(|_| "correlation identifier")?;
        let request =
            RequestId::new(format!("req_31_{sequence}")).map_err(|_| "request identifier")?;
        AuditEvent::seal(AuditEventDraft {
            sequence: self.audit.next_sequence(),
            recorded_at_unix_ms: spec.now_ms,
            transport: AuditTransport::Stdio,
            profile: self.profile,
            tool: spec.tool,
            principal_digest: AuditDigest::of_str("principal", self.principal.as_str()),
            session_digest: Some(AuditDigest::of_str("session", self.session.as_str())),
            credential_digest: None,
            project_digest: AuditDigest::of_str("project", self.project.object_id().as_str()),
            project_generation: self.project.generation(),
            capability,
            verdict: PolicyVerdict::Allow,
            reason: PolicyReason::Granted,
            decision_id,
            queue: AuditQueueFacts {
                lane: if phase.is_some() {
                    AuditLane::Mutation
                } else {
                    AuditLane::Read
                },
                priority: AuditPriority::Interactive,
                queued_depth: 0,
                admitted: true,
            },
            boundary: phase.map(|_| AuditBoundary::File),
            phase: phase.map(audit_phase_of),
            content_digest: None,
            status: AuditStatus::Ok,
            error_kind: None,
            error_retryable: false,
            correlation,
            request,
        })
        .map_err(|_| "audit record")
    }
}

const fn audit_phase_of(phase: Phase) -> AuditPhase {
    match phase {
        Phase::IntentFsynced => AuditPhase::Opened,
        Phase::StagedFsynced => AuditPhase::Staged,
        Phase::EffectStartedFsynced => AuditPhase::EffectStarted,
        Phase::CommittedFsynced => AuditPhase::Committed,
        Phase::Compensated => AuditPhase::Compensated,
        Phase::Quarantined => AuditPhase::Quarantined,
    }
}

fn refusal_from_limit(rejection: LimitRejection) -> AttemptRefusal {
    AttemptRefusal {
        code: rejection.error_kind().code(),
        retryable: rejection.is_retryable(),
        http_status: rejection.http_status(),
        audit_recorded: false,
        limit: Some(rejection.kind().label()),
    }
}

fn refusal_from_policy(refusal: &PolicyRefusal) -> AttemptRefusal {
    let kind = refusal.error_kind();
    AttemptRefusal {
        code: kind.code(),
        retryable: matches!(kind, ErrorKind::Unavailable | ErrorKind::Timeout),
        http_status: status_for(kind),
        audit_recorded: false,
        limit: None,
    }
}

fn refusal_from_outcome(outcome: &OutcomeError) -> AttemptRefusal {
    AttemptRefusal {
        code: outcome.code(),
        retryable: outcome.retryable(),
        http_status: status_for(outcome.kind()),
        audit_recorded: false,
        limit: None,
    }
}

const fn status_for(kind: ErrorKind) -> u16 {
    match kind {
        ErrorKind::InvalidInput => 400,
        ErrorKind::NotFound => 404,
        ErrorKind::Conflict => 409,
        ErrorKind::Unsupported => 501,
        ErrorKind::PermissionDenied => 403,
        ErrorKind::Timeout => 504,
        ErrorKind::Cancelled => 499,
        ErrorKind::Unavailable => 503,
        ErrorKind::Uncertain | ErrorKind::Internal => 500,
    }
}

/// Materialize a minimal Godot project marker and the destination directory the
/// permitted write lands in. `SafeRoot` publishes relative to an already-held
/// parent handle and owns no directory-creation primitive, so the destination
/// must exist before the effect runs.
fn prepare_project(parent: &Path) -> Result<PathBuf, HarnessError> {
    let root = parent.join("project");
    std::fs::create_dir_all(root.join("scenes"))
        .map_err(|error| HarnessError::Lease(error.to_string()))?;
    let marker = root.join("project.godot");
    if !marker.exists() {
        std::fs::write(
            &marker,
            b"; godot-mcp audit/limits harness project marker.\n\nconfig_version=5\n\n[application]\n\nconfig/name=\"godot-mcp task31 fixture\"\n",
        )
        .map_err(|error| HarnessError::Lease(error.to_string()))?;
    }
    Ok(root)
}
