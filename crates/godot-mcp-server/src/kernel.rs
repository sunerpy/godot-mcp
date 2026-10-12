//! Transport-independent MCP service kernel.
//!
//! This module owns discovery, dispatch, resource authorization, range
//! enforcement, progress and cancellation semantics. Protocol-specific types
//! are intentionally confined to `rmcp_adapter`.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use godot_mcp_domain::CapabilityProfile;
use godot_mcp_domain::catalog::{
    HandlerKey, KernelOperation, ServerState, TOOL_CATALOG, ToolAnnotations, ToolDescriptor,
    descriptor,
};
use godot_mcp_domain::contracts::{
    ArtifactContract, ArtifactResourceUri, ByteRange, Capability, ErrorKind, GodotValue,
    OutcomeError, PrincipalId, ProjectIdentity, SessionId,
};
use godot_mcp_domain::manifests::{ProfileManifest, canonical_manifest};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::artifacts::ArtifactStore;
use crate::audit::{
    AuditDigest, AuditEvent, AuditEventDraft, AuditLane, AuditLog, AuditPriority, AuditQueueFacts,
    AuditStatus, AuditTransport, RequestAudit,
};
use crate::core::CoreLimits;
use crate::diagnostics::{DiagnosticsContext, MetricName};
use crate::idempotency::{IdempotencyId, ModePrincipal, TransportAuthMode};
use crate::limits::LimitEnforcer;
use crate::observability::{CorrelationId, RequestId, SafeField};
use crate::policy::{BoundIdentity, row_for};
use crate::scheduler::Scheduler;

mod redaction;

pub use redaction::{PRIVATE_STATE_MARKER, PrivatePaths};

const PROFILE_GUIDE_PROMPT: &str = "godot-mcp.profile-guide";
pub(crate) const IDEMPOTENCY_ARGUMENT: &str = "idempotencyId";

/// Identity and project-generation facts supplied by a transport after its
/// local or authenticated boundary has established them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessContext {
    project: ProjectIdentity,
    principal: PrincipalId,
    session: Option<SessionId>,
    transport_mode: TransportAuthMode,
    mode_principal: ModePrincipal,
    connection_session: String,
    observed_at_unix_ms: u64,
}

impl AccessContext {
    /// Construct a complete access context for one request boundary.
    #[must_use]
    pub fn new(
        project: ProjectIdentity,
        principal: PrincipalId,
        session: Option<SessionId>,
        observed_at_unix_ms: u64,
    ) -> Self {
        let connection_session = match &session {
            Some(session) => session.as_str().to_owned(),
            None => "stdio".to_owned(),
        };
        let mode_principal = ModePrincipal::LocalOs {
            principal: principal.as_str().to_owned(),
        };
        Self {
            project,
            principal,
            session,
            transport_mode: TransportAuthMode::Stdio,
            mode_principal,
            connection_session,
            observed_at_unix_ms,
        }
    }

    /// Replace the default stdio identity with facts established by an authenticated
    /// transport boundary.
    #[must_use]
    pub fn with_transport_identity(
        mut self,
        transport_mode: TransportAuthMode,
        mode_principal: ModePrincipal,
        connection_session: impl Into<String>,
    ) -> Self {
        let connection_session = connection_session.into();
        let canonical_principal = canonical_principal_id(transport_mode, &mode_principal);
        let canonical_session = Some(canonical_session_id(transport_mode, &connection_session));
        self.transport_mode = transport_mode;
        self.mode_principal = mode_principal;
        self.connection_session = connection_session;
        self.principal = canonical_principal;
        self.session = canonical_session;
        self
    }

    pub(crate) const fn with_observed_at_unix_ms(mut self, observed_at_unix_ms: u64) -> Self {
        self.observed_at_unix_ms = observed_at_unix_ms;
        self
    }

    /// Project object and generation established by the transport boundary.
    #[must_use]
    pub const fn project(&self) -> &ProjectIdentity {
        &self.project
    }

    /// Local or authenticated principal established by the transport boundary.
    #[must_use]
    pub const fn principal(&self) -> &PrincipalId {
        &self.principal
    }

    /// Optional session established by the transport boundary.
    #[must_use]
    pub const fn session(&self) -> Option<&SessionId> {
        self.session.as_ref()
    }

    /// Transport authentication mode established for this request.
    #[must_use]
    pub const fn transport_mode(&self) -> TransportAuthMode {
        self.transport_mode
    }

    /// Mode-specific principal established from the request credential.
    #[must_use]
    pub const fn mode_principal(&self) -> &ModePrincipal {
        &self.mode_principal
    }

    /// Connection/session identifier used by the durable mutation identity.
    #[must_use]
    pub fn connection_session(&self) -> &str {
        &self.connection_session
    }

    /// Trusted current time used for retention checks.
    #[must_use]
    pub const fn observed_at_unix_ms(&self) -> u64 {
        self.observed_at_unix_ms
    }
}

/// Answers, before a handler runs, whether a listed tool needs a plane that has
/// no boundary compiled into this build.
///
/// A tool whose required plane is absent is still a real, implemented method;
/// it answers `unavailable` immediately, naming the plane, instead of spending a
/// Godot launch only to be refused at the effect boundary.
pub trait PlanePreflight: Send + Sync {
    /// The first plane `tool` requires that this build cannot provide.
    fn missing_plane(&self, tool: &str) -> Option<crate::scheduler::EffectPlane>;

    /// The refusal for a call that needs `plane`, saying why when it can.
    fn unavailable_error(&self, plane: crate::scheduler::EffectPlane) -> OutcomeError {
        crate::scheduler::PlaneAvailability::unavailable_error(plane)
    }
}

/// Protocol-neutral progress destination supplied by an adapter.
#[async_trait]
pub trait ProgressSink: Send + Sync {
    /// Publish one monotonically advancing progress observation.
    async fn report(&self, progress: f64, total: Option<f64>, message: Option<&str>);
}

#[derive(Debug)]
struct NoopProgress;

#[async_trait]
impl ProgressSink for NoopProgress {
    async fn report(&self, _progress: f64, _total: Option<f64>, _message: Option<&str>) {}
}

/// Cloneable progress handle passed to kernel tool implementations.
#[derive(Clone)]
pub struct ProgressReporter(Arc<dyn ProgressSink>);

impl ProgressReporter {
    /// Construct a reporter around an adapter-provided sink.
    #[must_use]
    pub fn new(sink: Arc<dyn ProgressSink>) -> Self {
        Self(sink)
    }

    /// Construct a reporter that intentionally discards progress.
    #[must_use]
    pub fn noop() -> Self {
        Self(Arc::new(NoopProgress))
    }

    /// Publish one progress observation.
    pub async fn report(&self, progress: f64, total: Option<f64>, message: Option<&str>) {
        self.0.report(progress, total, message).await;
    }
}

/// Protocol-neutral request passed to a registered tool handler.
pub struct KernelToolCall {
    pub name: String,
    pub arguments: Map<String, Value>,
    pub idempotency_id: Option<String>,
    pub access: AccessContext,
    pub cancellation: CancellationToken,
    pub progress: ProgressReporter,
    pub(crate) diagnostics: Option<Arc<DiagnosticsContext>>,
    pub(crate) audit: RequestAudit,
}

/// Extension point for later outcome implementations. Todo 5 only owns the
/// shared dispatch boundary; concrete Godot operations arrive in later todos.
#[async_trait]
pub trait KernelToolHandler: Send + Sync {
    async fn call(&self, call: KernelToolCall) -> Result<Value, OutcomeError>;
}

/// Protocol-neutral advertised tool definition, rendered from the catalog.
#[derive(Debug, Clone, PartialEq)]
pub struct KernelTool {
    pub name: String,
    pub title: String,
    pub description: String,
    pub input_schema: Map<String, Value>,
    pub annotations: ToolAnnotations,
}

/// Protocol-neutral advertised resource definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelResource {
    pub uri: String,
    pub name: String,
    pub description: String,
    pub media_type: String,
    pub size: Option<u64>,
}

/// Protocol-neutral advertised resource template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelResourceTemplate {
    pub uri_template: String,
    pub name: String,
    pub description: String,
    pub media_type: String,
}

/// Protocol-neutral prompt definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelPrompt {
    pub name: String,
    pub description: String,
}

/// Protocol-neutral prompt response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelPromptResult {
    pub description: String,
    pub text: String,
}

/// Resource payload after kernel authorization and limit enforcement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KernelResourceContents {
    Text {
        uri: String,
        media_type: String,
        text: String,
    },
    Blob {
        uri: String,
        media_type: String,
        bytes: Vec<u8>,
    },
}

#[derive(Clone)]
struct OperationalControls {
    limits: Arc<LimitEnforcer>,
    audit: Arc<AuditLog>,
}

struct OperationalRequest {
    controls: OperationalControls,
    audit: RequestAudit,
    _slot: crate::limits::ConcurrencyGuard,
}

/// The one shared service implementation used by every transport adapter.
pub struct ServiceKernel {
    profile: CapabilityProfile,
    state: ServerState,
    core_limits: CoreLimits,
    handlers: BTreeMap<String, Arc<dyn KernelToolHandler>>,
    artifacts: Arc<ArtifactStore>,
    scheduler: Option<Arc<Scheduler>>,
    diagnostics: Option<Arc<DiagnosticsContext>>,
    operational: Option<OperationalControls>,
    private_paths: PrivatePaths,
    plane_preflight: Option<Arc<dyn PlanePreflight>>,
}

impl ServiceKernel {
    /// Create a kernel exposing exactly the selected canonical profile.
    #[must_use]
    pub fn new(profile: CapabilityProfile) -> Self {
        Self::in_state(profile, ServerState::ProjectBound)
    }

    /// Create a kernel exposing the selected profile's bootstrap tools: the
    /// state of a server that holds a workspace authority and has not bound a
    /// project yet.
    #[must_use]
    pub fn bootstrap(profile: CapabilityProfile) -> Self {
        Self::in_state(profile, ServerState::Bootstrap)
    }

    fn in_state(profile: CapabilityProfile, state: ServerState) -> Self {
        Self {
            profile,
            state,
            core_limits: CoreLimits::default(),
            handlers: BTreeMap::new(),
            artifacts: Arc::new(ArtifactStore::in_memory()),
            scheduler: None,
            diagnostics: None,
            operational: None,
            private_paths: PrivatePaths::default(),
            plane_preflight: None,
        }
    }

    /// Refuse a listed tool whose required plane has no compiled-in boundary
    /// before its handler runs.
    #[must_use]
    pub fn with_plane_preflight(mut self, preflight: Arc<dyn PlanePreflight>) -> Self {
        self.plane_preflight = Some(preflight);
        self
    }

    /// Install the local spellings no public result, error or resource may carry.
    #[must_use]
    pub fn with_private_paths(mut self, private_paths: PrivatePaths) -> Self {
        self.private_paths = private_paths;
        self
    }

    /// Attach the per-project scheduler so `resources/scheduler` reports the real
    /// bounded lane state instead of an unavailable placeholder.
    #[must_use]
    pub fn with_scheduler(mut self, scheduler: Arc<Scheduler>) -> Self {
        self.scheduler = Some(scheduler);
        self
    }

    /// Attach the live diagnostics owners shared by the health tool and resource.
    #[must_use]
    pub(crate) fn with_diagnostics(mut self, diagnostics: Arc<DiagnosticsContext>) -> Self {
        self.diagnostics = Some(diagnostics);
        self
    }

    #[cfg(feature = "http")]
    pub(crate) fn declare_http_diagnostics(
        &self,
        source: crate::diagnostics::HttpDiagnosticsSource,
    ) {
        if let Some(diagnostics) = &self.diagnostics {
            diagnostics.declare_http(source);
        }
    }

    /// Attach the single operational limit gate and durable audit sink shared by
    /// every transport.
    #[must_use]
    pub fn with_operational_controls(
        mut self,
        limits: Arc<LimitEnforcer>,
        audit: Arc<AuditLog>,
    ) -> Self {
        self.artifacts.attach_limits(Arc::clone(&limits));
        self.operational = Some(OperationalControls { limits, audit });
        self
    }

    /// Selected canonical profile.
    #[must_use]
    pub const fn profile(&self) -> CapabilityProfile {
        self.profile
    }

    /// The server state whose tools this kernel lists.
    #[must_use]
    pub const fn state(&self) -> ServerState {
        self.state
    }

    pub(crate) const fn set_core_limits(&mut self, limits: CoreLimits) {
        self.core_limits = limits;
    }

    /// The catalog descriptor of `name`, when the selected profile lists it in
    /// this kernel's server state.
    fn listed(&self, name: &str) -> Option<&'static ToolDescriptor> {
        descriptor(name).filter(|descriptor| self.lists(descriptor))
    }

    fn lists(&self, descriptor: &ToolDescriptor) -> bool {
        descriptor.in_profile(self.profile) && descriptor.in_state(self.state)
    }

    /// Register the production handler of one catalog operation.
    ///
    /// The tool it serves is the descriptor that names `key`; its schema comes
    /// from the same key, so a handler can serve no tool but its own.
    pub fn register_tool_handler(
        &mut self,
        key: HandlerKey,
        handler: Arc<dyn KernelToolHandler>,
    ) -> Result<(), OutcomeError> {
        let descriptor = key.descriptor();
        if !self.lists(descriptor) {
            return Err(outcome_error(
                ErrorKind::NotFound,
                "tool is not present in the selected canonical profile",
            ));
        }
        if matches!(key, HandlerKey::Kernel(_)) {
            return Err(outcome_error(
                ErrorKind::Conflict,
                "the kernel serves this tool itself",
            ));
        }
        if self
            .handlers
            .insert(descriptor.name.to_owned(), handler)
            .is_some()
        {
            return Err(outcome_error(
                ErrorKind::Conflict,
                "tool handler is already registered",
            ));
        }
        Ok(())
    }

    /// Register opaque artifact bytes. Storage locations are not accepted by
    /// this API and therefore cannot leak through the public contract.
    pub fn register_artifact(
        &mut self,
        contract: ArtifactContract,
        bytes: Vec<u8>,
        access: &AccessContext,
    ) -> Result<(), OutcomeError> {
        let request_bytes = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        let operational =
            self.begin_operational_request("artifacts.register", access, request_bytes)?;
        let mut result = (|| {
            if contract.project() != access.project()
                || contract.owner().is_some_and(|owner| {
                    owner
                        .principal
                        .as_ref()
                        .is_some_and(|principal| principal != access.principal())
                        || owner
                            .session
                            .as_ref()
                            .is_some_and(|session| Some(session) != access.session())
                })
            {
                return Err(outcome_error(
                    ErrorKind::PermissionDenied,
                    "artifact access context does not match",
                ));
            }
            if request_bytes != contract.metadata().byte_length {
                return Err(outcome_error(
                    ErrorKind::InvalidInput,
                    "artifact bytes do not match the declared length",
                ));
            }
            self.artifacts.register_declared(contract, bytes)
        })();
        self.record_audit(
            "artifacts.register",
            access,
            operational.as_ref().map(|request| &request.audit),
            operational.is_some(),
            false,
            &mut result,
        );
        result
    }

    /// Every registered artifact's public contract, in URI order.
    ///
    /// The contract is structurally incapable of carrying a storage locator, so
    /// handing it out cannot leak one. The transport binding layer needs it to
    /// decide which artifacts a newly authorized session may read.
    #[must_use]
    pub fn artifact_contracts(&self) -> Vec<ArtifactContract> {
        self.artifacts.contracts()
    }

    /// The artifact store every handler and transport of this kernel shares.
    #[must_use]
    pub fn artifact_store(&self) -> Arc<ArtifactStore> {
        Arc::clone(&self.artifacts)
    }

    /// Serve artifacts from `store` instead of a private in-memory one.
    #[must_use]
    pub fn with_artifact_store(mut self, store: Arc<ArtifactStore>) -> Self {
        if let Some(operational) = &self.operational {
            store.attach_limits(Arc::clone(&operational.limits));
        }
        self.artifacts = store;
        self
    }

    /// Deterministic tool discovery: every catalog tool the selected profile
    /// authorizes, in name order, with its strict schema and annotations.
    #[must_use]
    pub fn tools(&self) -> Vec<KernelTool> {
        let mut tools: Vec<KernelTool> = TOOL_CATALOG
            .iter()
            .filter(|descriptor| self.lists(descriptor))
            .map(|descriptor| KernelTool {
                name: descriptor.name.to_owned(),
                title: descriptor.title.to_owned(),
                description: descriptor.description.to_owned(),
                input_schema: tool_schema(descriptor.name, self.core_limits),
                annotations: descriptor.annotations,
            })
            .collect();
        tools.sort_by(|left, right| left.name.cmp(&right.name));
        tools
    }

    /// Catalog tools the selected profile lists for which no production handler
    /// is registered. A production assembly refuses to serve while this is not
    /// empty, so discovery can never advertise a method without a handler.
    #[must_use]
    pub fn missing_handlers(&self) -> Vec<&'static str> {
        TOOL_CATALOG
            .iter()
            .filter(|descriptor| {
                self.lists(descriptor)
                    && !matches!(descriptor.handler, Some(HandlerKey::Kernel(_)))
                    && !self.handlers.contains_key(descriptor.name)
            })
            .map(|descriptor| descriptor.name)
            .collect()
    }

    /// Deterministic concrete-resource discovery. Artifact templates are
    /// advertised separately; only registered opaque artifacts are concrete,
    /// and only the ones `access` could read are named: its project's, its own
    /// or unowned, and not expired at its observation time.
    #[must_use]
    pub fn resources(&self, access: &AccessContext) -> Vec<KernelResource> {
        let mut resources: Vec<_> = canonical_manifest(self.profile)
            .resources
            .into_iter()
            .filter(|uri| !uri.contains('{'))
            .map(|uri| KernelResource {
                name: resource_name(&uri),
                description: format!("Godot MCP resource: {uri}"),
                media_type: "application/json".to_owned(),
                size: None,
                uri,
            })
            .collect();
        resources.extend(
            self.artifacts
                .contracts()
                .into_iter()
                .filter(|contract| {
                    owned_by(contract, access)
                        && access.observed_at_unix_ms() < contract.expires_at_unix_ms()
                })
                .map(|contract| KernelResource {
                    uri: contract.resource_uri().as_str().to_owned(),
                    name: contract.artifact_id().to_string(),
                    description: "Opaque generated artifact".to_owned(),
                    media_type: contract.metadata().media_type.clone(),
                    size: Some(contract.metadata().byte_length),
                }),
        );
        resources.sort_by(|left, right| left.uri.cmp(&right.uri));
        resources
    }

    /// Deterministic resource-template discovery.
    #[must_use]
    pub fn resource_templates(&self) -> Vec<KernelResourceTemplate> {
        canonical_manifest(self.profile)
            .resources
            .into_iter()
            .filter(|uri| uri.contains('{'))
            .map(|uri_template| KernelResourceTemplate {
                name: "artifact".to_owned(),
                description: "Opaque generated artifact".to_owned(),
                media_type: "application/octet-stream".to_owned(),
                uri_template,
            })
            .collect()
    }

    /// Prompt discovery remains independent from sampling, roots and protocol
    /// logging; none is required for kernel correctness.
    #[must_use]
    pub fn prompts(&self) -> Vec<KernelPrompt> {
        vec![KernelPrompt {
            name: PROFILE_GUIDE_PROMPT.to_owned(),
            description: "Explain the active Godot MCP capability profile".to_owned(),
        }]
    }

    /// Resolve one protocol-neutral prompt.
    pub fn get_prompt(
        &self,
        name: &str,
        arguments: Option<&Map<String, Value>>,
    ) -> Result<KernelPromptResult, OutcomeError> {
        if name != PROFILE_GUIDE_PROMPT {
            return Err(outcome_error(ErrorKind::NotFound, "prompt was not found"));
        }
        if arguments.is_some_and(|arguments| !arguments.is_empty()) {
            return Err(outcome_error(
                ErrorKind::InvalidInput,
                "profile guide prompt accepts no arguments",
            ));
        }
        Ok(KernelPromptResult {
            description: "Active profile guide".to_owned(),
            text: format!(
                "The active profile is `{}`. Discovery is derived from its canonical manifest.",
                self.profile.manifest_name()
            ),
        })
    }

    /// Dispatch one tool call through the shared kernel.
    pub async fn call_tool(
        &self,
        name: &str,
        mut arguments: Map<String, Value>,
        access: AccessContext,
        cancellation: CancellationToken,
        progress: ProgressReporter,
    ) -> Result<Value, OutcomeError> {
        if self.listed(name).is_none() {
            self.note_tool(MetricName::RequestsRefused, name);
            return Err(outcome_error(ErrorKind::NotFound, "tool was not found"));
        }
        let request_bytes = serialized_object_bytes(&arguments)?;
        let operational = match self.begin_operational_request(name, &access, request_bytes) {
            Ok(operational) => {
                self.note_tool(MetricName::RequestsAdmitted, name);
                operational
            }
            Err(error) => {
                self.note_tool(MetricName::RequestsRefused, name);
                return Err(error);
            }
        };
        let idempotency_result = match arguments.remove(IDEMPOTENCY_ARGUMENT) {
            None => Ok(None),
            Some(Value::String(value)) => IdempotencyId::new(value)
                .map(|id| Some(id.as_str().to_owned()))
                .map_err(|error| outcome_error(ErrorKind::InvalidInput, error.to_string())),
            Some(_) => Err(outcome_error(
                ErrorKind::InvalidInput,
                "idempotencyId must be a non-empty string",
            )),
        };
        let idempotency_id = match idempotency_result {
            Ok(idempotency_id) => idempotency_id,
            Err(error) => {
                let mut result = Err(error);
                self.record_audit(
                    name,
                    &access,
                    operational.as_ref().map(|request| &request.audit),
                    operational.is_some(),
                    false,
                    &mut result,
                );
                return result;
            }
        };
        if let Some(request) = &operational
            && row_for(name).is_some_and(|row| row.requires_permit())
            && request
                .controls
                .audit
                .failure_policy()
                .refuses_high_risk_mutation()
            && request.controls.audit.probe_writable().is_err()
        {
            let error = outcome_error(ErrorKind::Unavailable, "durable audit sink is unavailable");
            let mut result = Err(error);
            self.record_audit(
                name,
                &access,
                operational.as_ref().map(|request| &request.audit),
                true,
                false,
                &mut result,
            );
            return result;
        }

        if let Err(error) = reject_undeclared_arguments(name, &arguments, self.core_limits) {
            let mut result = Err(error);
            self.record_audit(
                name,
                &access,
                operational.as_ref().map(|request| &request.audit),
                operational.is_some(),
                false,
                &mut result,
            );
            return result;
        }

        let replay_safe = idempotency_id.is_some();
        let missing_plane = self
            .plane_preflight
            .as_ref()
            .and_then(|preflight| preflight.missing_plane(name));
        let result = if let Some(operation) = kernel_operation(name) {
            match operation {
                KernelOperation::Capabilities => capabilities_result(arguments),
            }
        } else if let Some(plane) = missing_plane {
            Err(self.plane_preflight.as_ref().map_or_else(
                || crate::scheduler::PlaneAvailability::unavailable_error(plane),
                |preflight| preflight.unavailable_error(plane),
            ))
        } else {
            match self.handlers.get(name) {
                Some(handler) => {
                    handler
                        .call(KernelToolCall {
                            name: name.to_owned(),
                            arguments,
                            idempotency_id,
                            access: access.clone(),
                            cancellation,
                            progress,
                            diagnostics: self.diagnostics.clone(),
                            audit: operational
                                .as_ref()
                                .map(|request| request.audit.clone())
                                .unwrap_or_default(),
                        })
                        .await
                }
                None => {
                    if let Some(diagnostics) = &self.diagnostics {
                        diagnostics.note_plane_refusal(name);
                    }
                    Err(outcome_error(
                        ErrorKind::Unavailable,
                        "tool implementation is not available yet",
                    ))
                }
            }
        };
        if let (Err(error), Some(diagnostics)) = (&result, &self.diagnostics)
            && error.kind() == ErrorKind::Unavailable
            && let Some(GodotValue::String(plane)) =
                error.details().get(crate::scheduler::PLANE_DETAIL_KEY)
            && let Some(plane) = crate::scheduler::EffectPlane::ALL
                .into_iter()
                .find(|candidate| candidate.as_str() == plane)
        {
            diagnostics.note_plane_refusal_on(plane);
        }
        let mut result = match (&operational, result) {
            (Some(request), Ok(value)) => {
                let bytes = serde_json::to_vec(&value).map_err(|_| {
                    outcome_error(ErrorKind::Internal, "response serialization failed")
                })?;
                request
                    .controls
                    .limits
                    .check_response_bytes(bytes.len() as u64)
                    .map_err(|rejection| rejection.to_outcome())
                    .map(|()| value)
            }
            (_, result) => result,
        };
        self.record_audit(
            name,
            &access,
            operational.as_ref().map(|request| &request.audit),
            operational.is_some(),
            replay_safe,
            &mut result,
        );
        result
            .map(|mut value| {
                self.private_paths.redact_value(&mut value);
                value
            })
            .map_err(|error| self.private_paths.redact_error(error))
    }

    fn note_tool(&self, metric: MetricName, tool: &str) {
        if let Some(diagnostics) = &self.diagnostics {
            diagnostics.note_tool(metric, tool);
        }
    }

    fn begin_operational_request(
        &self,
        name: &str,
        access: &AccessContext,
        request_bytes: u64,
    ) -> Result<Option<OperationalRequest>, OutcomeError> {
        let Some(controls) = self.operational.clone() else {
            return Ok(None);
        };
        let audit = self.request_audit(name, access);
        match admit_request(&controls, access, request_bytes) {
            Ok(slot) => Ok(Some(OperationalRequest {
                controls,
                audit,
                _slot: slot,
            })),
            Err(rejection) => {
                let mut result = Err(rejection);
                self.record_audit(name, access, Some(&audit), false, false, &mut result);
                result.map(|()| None)
            }
        }
    }

    fn request_audit(&self, name: &str, access: &AccessContext) -> RequestAudit {
        let audit = RequestAudit::default();
        let capability = row_for(name).map_or(Capability::Read, |row| row.capability);
        let identity = BoundIdentity {
            project: access.project().clone(),
            principal: Some(access.principal().clone()),
            session: access.session().cloned(),
        };
        audit.record_policy_decision(crate::policy::decision::admission_audit_decision(
            self.profile,
            &identity,
            name,
            capability,
        ));
        audit
    }

    fn record_audit<T>(
        &self,
        name: &str,
        access: &AccessContext,
        audit: Option<&RequestAudit>,
        admitted: bool,
        replay_safe: bool,
        result: &mut Result<T, OutcomeError>,
    ) {
        let Some(controls) = &self.operational else {
            return;
        };
        let row = row_for(name);
        let Some(audit) = audit else {
            return;
        };
        let Some(policy) = audit.policy_decision() else {
            return;
        };
        let effect = audit.effect_facts();
        let sequence = controls.audit.next_sequence();
        let Ok(correlation) = CorrelationId::new(format!("corr_kernel_{sequence}")) else {
            return;
        };
        let Ok(request) = RequestId::new(format!("req_kernel_{sequence}")) else {
            return;
        };
        let (status, error_kind, error_retryable) = match result {
            Ok(_) => (AuditStatus::Ok, None, false),
            Err(error) if admitted => (AuditStatus::Failed, Some(error.kind()), error.retryable()),
            Err(error) => (AuditStatus::Refused, Some(error.kind()), error.retryable()),
        };
        let event = AuditEvent::seal(AuditEventDraft {
            sequence,
            recorded_at_unix_ms: access.observed_at_unix_ms(),
            transport: audit_transport(access.transport_mode()),
            profile: self.profile,
            tool: name,
            principal_digest: AuditDigest::of_str(
                "principal",
                &access.mode_principal().to_journal_string(),
            ),
            session_digest: Some(AuditDigest::of_str("session", access.connection_session())),
            credential_digest: None,
            project_digest: AuditDigest::of_str("project", access.project().object_id().as_str()),
            project_generation: access.project().generation(),
            capability: policy.capability,
            verdict: policy.verdict,
            reason: policy.reason,
            decision_id: &policy.decision_id,
            queue: AuditQueueFacts {
                lane: if row.is_some_and(|row| row.requires_permit()) {
                    AuditLane::Mutation
                } else {
                    AuditLane::Read
                },
                priority: AuditPriority::Interactive,
                queued_depth: 0,
                admitted,
            },
            boundary: effect.as_ref().map(|effect| effect.boundary),
            phase: effect.as_ref().map(|effect| effect.phase),
            content_digest: effect.and_then(|effect| effect.content_digest),
            status,
            error_kind,
            error_retryable,
            correlation,
            request,
        });
        if let Ok(event) = event
            && controls.audit.record(&event).is_err()
            && result.is_ok()
            && row.is_some_and(|row| row.requires_permit())
            && controls.audit.failure_policy().refuses_high_risk_mutation()
        {
            let message = if replay_safe {
                "mutation committed but durable audit record failed; retry with the same \
                 idempotencyId to read the recorded result"
            } else {
                "mutation committed but durable audit record failed; retry is unsafe without an \
                 idempotencyId"
            };
            *result = Err(OutcomeError::new(
                ErrorKind::Unavailable,
                message,
                replay_safe,
                BTreeMap::new(),
            ));
        }
    }

    /// Read one resource after applying project, principal, session, retention
    /// and byte-range checks in the kernel rather than in a transport.
    pub fn read_resource(
        &self,
        uri: &str,
        requested_range: Option<ByteRange>,
        access: &AccessContext,
    ) -> Result<KernelResourceContents, OutcomeError> {
        if uri.starts_with("godot-mcp://artifacts/") {
            return self.read_artifact(uri, requested_range, access);
        }
        let request_bytes = u64::try_from(uri.len()).unwrap_or(u64::MAX);
        let operational =
            self.begin_operational_request("resources.read", access, request_bytes)?;
        let mut result = (|| {
            if requested_range.is_some() {
                return Err(outcome_error(
                    ErrorKind::Unsupported,
                    "byte ranges are supported only for artifact resources",
                ));
            }
            let manifest = canonical_manifest(self.profile);
            if !manifest.resources.contains(uri) || uri.contains('{') {
                return Err(outcome_error(ErrorKind::NotFound, "resource was not found"));
            }
            let value = match uri {
                "godot-mcp://resources/capabilities" => all_manifests_value(),
                "godot-mcp://resources/project" => json!({ "project": access.project() }),
                "godot-mcp://resources/health" => match &self.diagnostics {
                    Some(diagnostics) => diagnostics.health_value()?,
                    None => json!({
                        "status": "ready",
                        "project": access.project(),
                    }),
                },
                "godot-mcp://resources/scheduler" => match &self.scheduler {
                    Some(scheduler) => {
                        serde_json::to_value(scheduler.state(access.observed_at_unix_ms())?)
                            .map_err(|_| {
                                outcome_error(ErrorKind::Internal, "scheduler serialization failed")
                            })?
                    }
                    None => {
                        return Err(outcome_error(
                            ErrorKind::Unavailable,
                            "no scheduler is attached to this kernel",
                        ));
                    }
                },
                _ => return Err(outcome_error(ErrorKind::NotFound, "resource was not found")),
            };
            let mut value = value;
            self.private_paths.redact_value(&mut value);
            let text = serde_json::to_string(&value)
                .map_err(|_| outcome_error(ErrorKind::Internal, "resource serialization failed"))?;
            if let Some(request) = &operational {
                request
                    .controls
                    .limits
                    .check_response_bytes(text.len() as u64)
                    .map_err(|rejection| rejection.to_outcome())?;
            }
            Ok(KernelResourceContents::Text {
                uri: uri.to_owned(),
                media_type: "application/json".to_owned(),
                text,
            })
        })();
        self.record_audit(
            "resources.read",
            access,
            operational.as_ref().map(|request| &request.audit),
            operational.is_some(),
            false,
            &mut result,
        );
        result
    }

    fn read_artifact(
        &self,
        uri: &str,
        requested_range: Option<ByteRange>,
        access: &AccessContext,
    ) -> Result<KernelResourceContents, OutcomeError> {
        let request_bytes = u64::try_from(uri.len()).unwrap_or(u64::MAX);
        let operational =
            self.begin_operational_request("artifacts.read", access, request_bytes)?;
        let mut result = (|| {
            ArtifactResourceUri::parse(uri).map_err(contract_error)?;
            let contract = self
                .artifacts
                .contract(uri)
                .ok_or_else(|| outcome_error(ErrorKind::NotFound, "artifact was not found"))?;
            if !owned_by(&contract, access) {
                return Err(outcome_error(
                    ErrorKind::PermissionDenied,
                    "artifact access context does not match",
                ));
            }
            if access.observed_at_unix_ms() >= contract.expires_at_unix_ms() {
                return Err(outcome_error(ErrorKind::NotFound, "artifact was not found"));
            }
            if let Some(request) = &operational {
                request
                    .controls
                    .limits
                    .check_retention(
                        contract.metadata().created_at_unix_ms,
                        access.observed_at_unix_ms(),
                    )
                    .map_err(|rejection| rejection.to_outcome())?;
            }
            let range = contract
                .ranges()
                .resolve(requested_range)
                .map_err(contract_error)?;
            let range_bytes = range.end_inclusive - range.start + 1;
            if let Some(request) = &operational {
                request
                    .controls
                    .limits
                    .check_range_bytes(range_bytes)
                    .and_then(|()| request.controls.limits.check_response_bytes(range_bytes))
                    .map_err(|rejection| rejection.to_outcome())?;
            }
            let bytes = self.artifacts.read_range(uri, &range)?;
            Ok(KernelResourceContents::Blob {
                uri: uri.to_owned(),
                media_type: contract.metadata().media_type.clone(),
                bytes,
            })
        })();
        self.record_audit(
            "artifacts.read",
            access,
            operational.as_ref().map(|request| &request.audit),
            operational.is_some(),
            false,
            &mut result,
        );
        result
    }
}

/// Whether `access` is the context `contract` was produced for: the same
/// project, and its owner's principal and session where the contract names one.
fn owned_by(contract: &ArtifactContract, access: &AccessContext) -> bool {
    contract.project() == access.project()
        && contract.owner().is_none_or(|owner| {
            owner
                .principal
                .as_ref()
                .is_none_or(|principal| principal == access.principal())
                && owner
                    .session
                    .as_ref()
                    .is_none_or(|session| Some(session) == access.session())
        })
}

pub(crate) fn canonical_principal_id(
    mode: TransportAuthMode,
    principal: &ModePrincipal,
) -> PrincipalId {
    let identity = format!("{}:{}", mode.as_str(), principal.to_journal_string());
    PrincipalId::parse(format!(
        "principal_{}",
        AuditDigest::of_str("transport-principal", &identity).as_str()
    ))
    .expect("an audit digest is a valid canonical principal payload")
}

pub(crate) fn canonical_session_id(mode: TransportAuthMode, connection_session: &str) -> SessionId {
    let identity = format!("{}:{connection_session}", mode.as_str());
    SessionId::parse(format!(
        "session_{}",
        AuditDigest::of_str("transport-session", &identity).as_str()
    ))
    .expect("an audit digest is a valid canonical session payload")
}

fn serialized_object_bytes(arguments: &Map<String, Value>) -> Result<u64, OutcomeError> {
    serde_json::to_vec(arguments)
        .map(|bytes| bytes.len() as u64)
        .map_err(|_| outcome_error(ErrorKind::Internal, "request serialization failed"))
}

fn admit_request(
    controls: &OperationalControls,
    access: &AccessContext,
    request_bytes: u64,
) -> Result<crate::limits::ConcurrencyGuard, OutcomeError> {
    let digest = AuditDigest::of_str(
        "limit-subject",
        &access.mode_principal().to_journal_string(),
    );
    let subject = SafeField::new(format!("subject_{digest}"))
        .map_err(|_| outcome_error(ErrorKind::Internal, "limit subject could not be derived"))?;
    controls
        .limits
        .note_request()
        .and_then(|()| controls.limits.check_body_bytes(request_bytes))
        .and_then(|()| {
            controls
                .limits
                .check_rate(&subject, access.observed_at_unix_ms())
        })
        .map_err(|rejection| rejection.to_outcome())?;
    Arc::clone(&controls.limits)
        .admit(&subject, access.observed_at_unix_ms())
        .map_err(|rejection| rejection.to_outcome())
}

const fn audit_transport(mode: TransportAuthMode) -> AuditTransport {
    match mode {
        TransportAuthMode::Stdio => AuditTransport::Stdio,
        TransportAuthMode::TrustedLocal => AuditTransport::HttpTrustedLocal,
        TransportAuthMode::ProxiedOidc => AuditTransport::HttpProxiedOidc,
    }
}

fn capabilities_result(arguments: Map<String, Value>) -> Result<Value, OutcomeError> {
    if arguments.keys().any(|key| key != "profile") {
        return Err(outcome_error(
            ErrorKind::InvalidInput,
            "capabilities.get accepts only the optional profile argument",
        ));
    }
    match arguments.get("profile") {
        None => Ok(all_manifests_value()),
        Some(Value::String(profile)) => profile_from_name(profile)
            .map(canonical_manifest)
            .and_then(|manifest| {
                serde_json::to_value(manifest).map_err(|_| {
                    outcome_error(ErrorKind::Internal, "manifest serialization failed")
                })
            }),
        Some(_) => Err(outcome_error(
            ErrorKind::InvalidInput,
            "profile must be one of core, editor, runtime, or full",
        )),
    }
}

fn all_manifests_value() -> Value {
    let manifests = all_manifests();
    serde_json::to_value(manifests).expect("canonical manifests are always serializable")
}

fn all_manifests() -> BTreeMap<&'static str, ProfileManifest> {
    [
        ("core", CapabilityProfile::Core),
        ("editor", CapabilityProfile::Editor),
        ("runtime", CapabilityProfile::Runtime),
        ("full", CapabilityProfile::Full),
    ]
    .into_iter()
    .map(|(name, profile)| (name, canonical_manifest(profile)))
    .collect()
}

fn profile_from_name(name: &str) -> Result<CapabilityProfile, OutcomeError> {
    match name {
        "core" => Ok(CapabilityProfile::Core),
        "editor" => Ok(CapabilityProfile::Editor),
        "runtime" => Ok(CapabilityProfile::Runtime),
        "full" => Ok(CapabilityProfile::Full),
        _ => Err(outcome_error(
            ErrorKind::InvalidInput,
            "profile must be one of core, editor, runtime, or full",
        )),
    }
}

/// The kernel's own operation behind `name`, when the kernel serves it.
fn kernel_operation(name: &str) -> Option<KernelOperation> {
    match descriptor(name)?.handler? {
        HandlerKey::Kernel(operation) => Some(operation),
        _ => None,
    }
}

/// The strict input schema of one catalog operation, from the component that
/// serves it and from no other.
fn handler_schema(key: HandlerKey, core_limits: CoreLimits) -> Map<String, Value> {
    match key {
        HandlerKey::Kernel(KernelOperation::Capabilities) => json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "profile": {
                    "type": "string",
                    "enum": ["core", "editor", "runtime", "full"]
                }
            }
        })
        .as_object()
        .cloned()
        .expect("the capabilities schema is an object"),
        HandlerKey::Core(operation) => crate::core::input_schema(operation, core_limits),
        HandlerKey::Engine(operation) => crate::engine_plane::engine_input_schema(operation),
        HandlerKey::Authoring(operation) => crate::authoring::input_schema(operation),
        HandlerKey::Editor(operation) => crate::editor::input_schema(operation),
        HandlerKey::Session(operation) => crate::engine_plane::session_input_schema(operation),
        HandlerKey::Bootstrap(operation) => crate::bootstrap_schema::input_schema(operation),
    }
}

pub(crate) fn tool_schema(name: &str, core_limits: CoreLimits) -> Map<String, Value> {
    // A name the catalog does not hold gets a schema that accepts nothing
    // rather than a permissive object.
    let mut schema = descriptor(name)
        .and_then(|descriptor| descriptor.handler)
        .map_or_else(
            || {
                json!({ "type": "object", "additionalProperties": false, "properties": {} })
                    .as_object()
                    .cloned()
                    .expect("the empty schema is an object")
            },
            |key| handler_schema(key, core_limits),
        );
    let idempotency = json!({
        "type": "string",
        "minLength": IdempotencyId::MIN_LENGTH,
        "description": "Non-empty identifier that makes this call replay-safe."
    });
    schema
        .entry("properties".to_owned())
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .expect("tool schema properties are JSON objects")
        .insert(IDEMPOTENCY_ARGUMENT.to_owned(), idempotency);
    schema
}

/// Refuse an argument the tool's advertised schema does not declare.
///
/// Every advertised schema sets `additionalProperties: false`; this is the
/// kernel's own enforcement of that promise, applied before any handler sees the
/// arguments, so no tool can accept a field its schema does not describe.
fn reject_undeclared_arguments(
    name: &str,
    arguments: &Map<String, Value>,
    core_limits: CoreLimits,
) -> Result<(), OutcomeError> {
    let schema = tool_schema(name, core_limits);
    let declared = schema
        .get("properties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if let Some(undeclared) = arguments.keys().find(|key| !declared.contains_key(*key)) {
        return Err(outcome_error(
            ErrorKind::InvalidInput,
            format!("{name} does not accept the argument `{undeclared}`"),
        ));
    }
    Ok(())
}

fn resource_name(uri: &str) -> String {
    uri.rsplit('/').next().unwrap_or("resource").to_owned()
}

fn outcome_error(kind: ErrorKind, message: impl Into<String>) -> OutcomeError {
    OutcomeError::new(kind, message, false, BTreeMap::new())
}

fn contract_error(error: godot_mcp_domain::contracts::ContractViolation) -> OutcomeError {
    outcome_error(error.kind(), error.to_string())
}

#[cfg(test)]
mod catalog_binding_tests {
    use super::*;

    struct Answers;

    #[async_trait]
    impl KernelToolHandler for Answers {
        async fn call(&self, _call: KernelToolCall) -> Result<Value, OutcomeError> {
            Ok(json!({}))
        }
    }

    /// Every catalog tool's advertised schema is the schema of the operation
    /// its descriptor names, strict at the top level; the kernel adds only the
    /// idempotency argument every tool takes.
    #[test]
    fn every_catalog_tool_takes_its_schema_from_its_handler_key() {
        fn without_idempotency(mut schema: Value) -> Value {
            if let Some(Value::Object(fields)) = schema.get_mut("properties") {
                assert!(fields.remove(IDEMPOTENCY_ARGUMENT).is_some());
            }
            schema
        }
        for descriptor in TOOL_CATALOG {
            let key = descriptor
                .handler
                .expect("catalog tools name their handler");
            let operation = Value::Object(handler_schema(key, CoreLimits::default()));
            let advertised = Value::Object(tool_schema(descriptor.name, CoreLimits::default()));
            assert_eq!(
                advertised["additionalProperties"],
                json!(false),
                "{}",
                descriptor.name
            );
            assert_eq!(
                without_idempotency(advertised),
                operation,
                "{}",
                descriptor.name
            );
        }
    }

    /// A handler registers under its operation's key, once, and only for a
    /// tool the kernel lists; the kernel answers its own tool itself.
    #[test]
    fn a_handler_registers_under_its_operation_key_once() {
        use godot_mcp_domain::catalog::{AuthoringOperation, KernelOperation};

        let mut kernel = ServiceKernel::new(CapabilityProfile::Full);
        kernel
            .register_tool_handler(AuthoringOperation::FileGet.key(), Arc::new(Answers))
            .expect("the first registration");
        let twice = kernel
            .register_tool_handler(AuthoringOperation::FileGet.key(), Arc::new(Answers))
            .expect_err("a second handler for one operation");
        assert_eq!(twice.kind(), ErrorKind::Conflict);
        let own = kernel
            .register_tool_handler(KernelOperation::Capabilities.key(), Arc::new(Answers))
            .expect_err("the kernel serves capabilities.get");
        assert_eq!(own.kind(), ErrorKind::Conflict);
        let mut core = ServiceKernel::new(CapabilityProfile::Core);
        let unlisted = core
            .register_tool_handler(AuthoringOperation::FileCreate.key(), Arc::new(Answers))
            .expect_err("core lists no write");
        assert_eq!(unlisted.kind(), ErrorKind::NotFound);
    }
}
