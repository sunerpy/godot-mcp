//! The staged-authentication handler shell.
//!
//! `StagedHandler` wraps the one shared [`RmcpAdapter`] and does exactly two
//! things: it counts real kernel dispatches (so a test can prove a refusal never
//! reached the kernel), and it refuses every *protected* dispatch with
//! `unavailable` in `proxied_oidc` mode unless that listener has both authentication
//! layers armed.
//!
//! Nothing here decides whether *one request* is authenticated. That decision
//! belongs to [`RequestGuard`](crate::http::RequestGuard), which runs ahead of the
//! MCP service, so an unauthenticated request is refused before a handler exists to
//! reach — and that is why a request arriving here has already passed both layers.
//! Neither this shell nor the guard consults who connected or any forwarded header,
//! because neither a loopback peer address nor a header a proxy asserted is an
//! identity.
//!
//! A `proxied_oidc` listener built *without* a boundary keeps refusing
//! unconditionally, so misconfiguration fails closed rather than opening the mode.
//!
//! `initialize` and the `list_*` metadata methods stay available so a client can
//! complete a handshake and discover that the server exists — nothing about the
//! project is exposed by the frozen capability manifest, and refusing the handshake
//! instead would make the mode indistinguishable from an unreachable port.
//!
//! There is no tool logic here: every non-refused method delegates to the wrapped
//! adapter, which is the same adapter stdio serves.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use godot_mcp_domain::config::HttpMode;
use godot_mcp_domain::contracts::{ErrorKind, OutcomeError};

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, DiscoverResult, ErrorCode, ErrorData,
    GetPromptRequestParams, GetPromptResponse, ListPromptsResult, ListResourceTemplatesResult,
    ListResourcesResult, ListToolsResult, PaginatedRequestParams, ReadResourceRequestParams,
    ReadResourceResponse, ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::{RoleServer, ServerHandler};
use serde_json::{Value, json};

use crate::binding::{
    BindingRefusal, BindingRegistry, BoundSubject, SessionLease, binding_refusal,
};
use crate::kernel::IDEMPOTENCY_ARGUMENT;
use crate::rmcp_adapter::{RmcpAdapter, parse_range, refuse_inline_lifecycle};

/// The URI prefix every opaque artifact resource carries.
///
/// Only these reads are range-authorized: the other advertised resources are the
/// frozen capability, project, health and scheduler documents, which carry no bytes a
/// range could select and are already bounded by the kernel.
const ARTIFACT_URI_PREFIX: &str = "godot-mcp://artifacts/";

/// The shared adapter plus this mode's staged dispatch boundary.
#[derive(Clone)]
pub struct StagedHandler {
    adapter: RmcpAdapter,
    mode: HttpMode,
    dispatches: Arc<AtomicUsize>,
    boundary_armed: bool,
    bindings: Option<Arc<BindingRegistry>>,
}

impl StagedHandler {
    /// Wrap one adapter for one HTTP mode.
    #[must_use]
    pub const fn new(adapter: RmcpAdapter, mode: HttpMode, dispatches: Arc<AtomicUsize>) -> Self {
        Self {
            adapter,
            mode,
            dispatches,
            boundary_armed: false,
            bindings: None,
        }
    }

    /// Authorize every session-scoped dispatch and artifact read against `bindings`.
    ///
    /// Without one this shell behaves exactly as Todo 28/29 left it. With one, a
    /// `tools/call` is admitted as a task before it dispatches — so a replayed
    /// destructive mutation is refused — and *every* artifact range read is
    /// authorized individually against the subject the request re-proved.
    #[must_use]
    pub fn with_bindings(mut self, bindings: Arc<BindingRegistry>) -> Self {
        self.bindings = Some(bindings);
        self
    }

    /// The subject and session binding this request arrived with.
    ///
    /// Both come from the request's own extensions, which the transport middleware
    /// filled in *after* revalidating the credential. Neither is read from the
    /// protocol payload, so a client cannot assert either.
    fn bound_subject(
        context: &RequestContext<RoleServer>,
    ) -> (Option<BoundSubject>, Option<SessionLease>) {
        let Some(parts) = context.extensions.get::<axum::http::request::Parts>() else {
            return (None, None);
        };
        (
            parts.extensions.get::<BoundSubject>().cloned(),
            parts.extensions.get::<SessionLease>().cloned(),
        )
    }

    /// The binding refusal, in the protocol shape a genuine absence also takes.
    fn binding_error(refusal: &BindingRefusal) -> ErrorData {
        let error = refusal.to_outcome();
        ErrorData::new(
            ErrorCode::METHOD_NOT_FOUND,
            error.message().to_owned(),
            Some(json!({
                "code": error.code(),
                "message": error.message(),
                "retryable": error.retryable(),
                "details": error.details(),
            })),
        )
    }

    /// Record that a `proxied_oidc` listener has both authentication layers armed.
    ///
    /// Until a boundary is attached the refusal below stays unconditional, so a
    /// partially configured `proxied_oidc` listener fails closed. The per-request
    /// decision is *not* made here: it is made by
    /// [`RequestGuard`](crate::http::RequestGuard), which runs ahead of the MCP
    /// service and refuses an unauthenticated request before it can reach a handler.
    /// This flag only records that the guard is enforcing.
    #[must_use]
    pub fn with_armed_boundary(mut self) -> Self {
        self.boundary_armed = true;
        self
    }

    /// Whether protected dispatch is available in this mode.
    #[must_use]
    pub const fn dispatch_available(&self) -> bool {
        match self.mode {
            HttpMode::TrustedLocal => true,
            HttpMode::ProxiedOidc => self.boundary_armed,
        }
    }

    fn record_dispatch(&self) {
        self.dispatches.fetch_add(1, Ordering::SeqCst);
    }

    fn staged_refusal() -> OutcomeError {
        OutcomeError::new(
            ErrorKind::Unavailable,
            "the proxied_oidc authentication boundary is not configured",
            false,
            Default::default(),
        )
    }

    fn refusal_value() -> Value {
        let error = Self::staged_refusal();
        json!({
            "code": error.code(),
            "message": error.message(),
            "retryable": error.retryable(),
            "details": error.details(),
        })
    }

    fn refusal_error() -> ErrorData {
        let error = Self::staged_refusal();
        ErrorData::new(
            ErrorCode::INVALID_REQUEST,
            error.message().to_owned(),
            Some(Self::refusal_value()),
        )
    }
}

impl ServerHandler for StagedHandler {
    fn get_info(&self) -> ServerConfig {
        self.adapter.get_info()
    }

    async fn discover(
        &self,
        context: RequestContext<RoleServer>,
    ) -> Result<DiscoverResult, ErrorData> {
        self.adapter.discover(context).await
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        self.adapter.list_tools(request, context).await
    }

    async fn list_resources(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        refuse_inline_lifecycle(&context.meta)?;
        // The listing names the artifacts the subject this request re-proved
        // could read, so it is answered as that subject.
        let (subject, lease) = match self.bindings.as_ref() {
            Some(_) => Self::bound_subject(&context),
            None => (None, None),
        };
        let request_adapter = match (subject.as_ref(), lease.as_ref()) {
            (Some(subject), Some(lease)) => {
                self.adapter
                    .for_bound_request(subject, lease, crate::http::now_unix_ms())
            }
            _ => self.adapter.clone(),
        };
        request_adapter.list_resources(request, context).await
    }

    async fn list_resource_templates(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        self.adapter.list_resource_templates(request, context).await
    }

    async fn list_prompts(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        self.adapter.list_prompts(request, context).await
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        refuse_inline_lifecycle(&context.meta)?;
        if !self.dispatch_available() {
            return Ok(CallToolResult::structured_error(Self::refusal_value()).into());
        }
        let admitted = match self.bindings.as_ref() {
            None => None,
            Some(bindings) => {
                let (subject, lease) = Self::bound_subject(&context);
                let Some(lease) = lease else {
                    return Err(Self::binding_error(&binding_refusal()));
                };
                let idempotency = request
                    .arguments
                    .as_ref()
                    .and_then(|arguments| arguments.get(IDEMPOTENCY_ARGUMENT))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let task = bindings
                    .bind_task(
                        lease.handle().as_str(),
                        subject.as_ref(),
                        crate::http::now_unix_ms(),
                        &request.name,
                        idempotency.as_deref(),
                    )
                    .map_err(|refusal| Self::binding_error(&refusal))?;
                Some((Arc::clone(bindings), lease, subject, task))
            }
        };
        self.record_dispatch();
        let request_adapter = admitted
            .as_ref()
            .and_then(|(_, lease, subject, _)| {
                subject.as_ref().map(|subject| {
                    self.adapter
                        .for_bound_request(subject, lease, crate::http::now_unix_ms())
                })
            })
            .unwrap_or_else(|| self.adapter.clone());
        let result = request_adapter.call_tool(request, context).await;
        if let Some((bindings, lease, subject, task)) = admitted {
            // The mutation left this process, so the tuple can never be replayed —
            // which holds whether or not an answer came back.
            let _ = bindings.mark_task_dispatched(
                lease.handle().as_str(),
                subject.as_ref(),
                crate::http::now_unix_ms(),
                task.handle().as_str(),
            );
        }
        result
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        refuse_inline_lifecycle(&context.meta)?;
        if !self.dispatch_available() {
            return Err(Self::refusal_error());
        }
        let (subject, lease) = match self.bindings.as_ref() {
            Some(_) => Self::bound_subject(&context),
            None => (None, None),
        };
        if let Some(bindings) = self.bindings.as_ref()
            && request.uri.starts_with(ARTIFACT_URI_PREFIX)
        {
            let range = parse_range(Some(&context.meta))?;
            let Some(lease) = lease.as_ref() else {
                return Err(Self::binding_error(&binding_refusal()));
            };
            bindings
                .authorize_artifact_read(
                    lease.handle().as_str(),
                    subject.as_ref(),
                    crate::http::now_unix_ms(),
                    &request.uri,
                    range,
                )
                .map_err(|refusal| Self::binding_error(&refusal))?;
        }
        self.record_dispatch();
        let request_adapter = match (subject.as_ref(), lease.as_ref()) {
            (Some(subject), Some(lease)) => {
                self.adapter
                    .for_bound_request(subject, lease, crate::http::now_unix_ms())
            }
            _ => self.adapter.clone(),
        };
        request_adapter.read_resource(request, context).await
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, ErrorData> {
        refuse_inline_lifecycle(&context.meta)?;
        if !self.dispatch_available() {
            return Err(Self::refusal_error());
        }
        self.record_dispatch();
        let (subject, lease) = Self::bound_subject(&context);
        let request_adapter = match (subject.as_ref(), lease.as_ref()) {
            (Some(subject), Some(lease)) => {
                self.adapter
                    .for_bound_request(subject, lease, crate::http::now_unix_ms())
            }
            _ => self.adapter.clone(),
        };
        request_adapter.get_prompt(request, context).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unarmed_boundary_refuses_dispatch() {
        assert_eq!(
            StagedHandler::staged_refusal().kind(),
            ErrorKind::Unavailable
        );
        assert_eq!(
            StagedHandler::refusal_value()
                .get("code")
                .and_then(Value::as_str),
            Some("unavailable")
        );
        assert!(!StagedHandler::staged_refusal().retryable());
    }

    #[test]
    fn the_refusal_is_byte_stable() {
        assert_eq!(
            serde_json::to_string(&StagedHandler::refusal_value()).expect("serialize"),
            serde_json::to_string(&StagedHandler::refusal_value()).expect("serialize"),
        );
    }
}
