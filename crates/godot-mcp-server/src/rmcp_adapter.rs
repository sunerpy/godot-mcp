//! Exact `rmcp` 3.5.1 adapter for the transport-independent kernel.
//!
//! The adapter serves the `initialize` lifecycle of MCP revisions up to 2025-11-25.
//! rmcp 3 also speaks the 2026-07-28 revision's per-request lifecycle; the adapter
//! refuses it ([`refuse_inline_lifecycle`]) and answers its `server/discover` probe
//! as a server that predates it.

use std::sync::Arc;

use async_trait::async_trait;
use godot_mcp_domain::contracts::{ByteRange, ErrorKind, OutcomeError};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, DiscoverRequestMethod, DiscoverResult,
    ErrorCode, ErrorData, GetPromptRequestParams, GetPromptResponse, GetPromptResult,
    Implementation, ListPromptsResult, ListResourceTemplatesResult, ListResourcesResult,
    ListToolsResult, PaginatedRequestParams, ProgressNotificationParam, Prompt, PromptMessage,
    ProtocolVersion, ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult,
    RequestMetaObject, Resource, ResourceContents, ResourceTemplate, Role, ServerCapabilities,
    ServerConfig, Tool, ToolAnnotations,
};
use rmcp::service::RequestContext;
use rmcp::{Peer, RoleServer, ServerHandler};
use serde::Deserialize;
use serde_json::Value;

use crate::SERVER_VERSION;
#[cfg(feature = "http")]
use crate::binding::{BoundSubject, SessionLease};
use crate::kernel::{
    AccessContext, KernelPrompt, KernelResourceContents, ProgressReporter, ProgressSink,
    ServiceKernel,
};

/// Namespaced request `_meta` field used for inclusive artifact byte ranges.
pub const ARTIFACT_RANGE_META_KEY: &str = "io.godot-mcp/artifactRange";

/// Official rmcp adapter shared by every transport.
#[derive(Clone)]
pub struct RmcpAdapter {
    source: KernelSource,
}

/// Where the adapter takes the kernel for each request from.
#[derive(Clone)]
enum KernelSource {
    /// One kernel for the whole session: a server launched with a project.
    Fixed {
        kernel: Arc<ServiceKernel>,
        access: AccessContext,
    },
    /// The bootstrap slot, whose kernel changes once when a project is bound.
    #[cfg(feature = "stdio")]
    Slot(Arc<crate::bootstrap::ServerSlot>),
}

impl RmcpAdapter {
    /// Bind one established transport identity to the shared kernel.
    #[must_use]
    pub const fn new(kernel: Arc<ServiceKernel>, access: AccessContext) -> Self {
        Self {
            source: KernelSource::Fixed { kernel, access },
        }
    }

    /// Serve a bootstrap slot: every request is dispatched to the slot's
    /// current kernel, and binding a project announces the changed tool list.
    #[cfg(feature = "stdio")]
    #[must_use]
    pub fn switching(slot: Arc<crate::bootstrap::ServerSlot>) -> Self {
        Self {
            source: KernelSource::Slot(slot),
        }
    }

    /// The kernel and access context of the next request, and the slot
    /// generation they belong to.
    fn current(&self) -> (Arc<ServiceKernel>, AccessContext, u64) {
        match &self.source {
            KernelSource::Fixed { kernel, access } => (Arc::clone(kernel), access.clone(), 0),
            #[cfg(feature = "stdio")]
            KernelSource::Slot(slot) => {
                let generation = slot.generation();
                let (kernel, access) = slot.kernel_and_access();
                (kernel, access, generation)
            }
        }
    }

    fn generation(&self) -> u64 {
        match &self.source {
            KernelSource::Fixed { .. } => 0,
            #[cfg(feature = "stdio")]
            KernelSource::Slot(slot) => slot.generation(),
        }
    }

    #[cfg(feature = "http")]
    #[must_use]
    pub(crate) fn for_bound_request(
        &self,
        subject: &BoundSubject,
        lease: &SessionLease,
        observed_at_unix_ms: u64,
    ) -> Self {
        let (kernel, access, _generation) = self.current();
        Self {
            source: KernelSource::Fixed {
                kernel,
                access: access
                    .with_transport_identity(
                        subject.mode(),
                        subject.principal().clone(),
                        lease.handle().as_str(),
                    )
                    .with_observed_at_unix_ms(observed_at_unix_ms),
            },
        }
    }
}

impl ServerHandler for RmcpAdapter {
    fn get_info(&self) -> ServerConfig {
        let tools = ServerCapabilities::builder()
            .enable_prompts()
            .enable_resources()
            .enable_tools();
        let capabilities = match &self.source {
            KernelSource::Fixed { .. } => tools.build(),
            #[cfg(feature = "stdio")]
            KernelSource::Slot(_) => tools.enable_tool_list_changed().build(),
        };
        ServerConfig::new(capabilities)
            .with_protocol_version(ProtocolVersion::LATEST_WITH_INITIALIZE)
            .with_server_info(Implementation::new("godot-mcp-server", SERVER_VERSION))
            .with_instructions(
                "Use canonical Godot MCP tools and opaque resources; storage paths are never \
                 public.",
            )
    }

    /// `server/discover` opens the 2026-07-28 lifecycle, which this server does not
    /// serve, so it answers as a server that predates the method: "method not found".
    /// A client that probes before it connects then falls back to `initialize`.
    async fn discover(
        &self,
        _context: RequestContext<RoleServer>,
    ) -> Result<DiscoverResult, ErrorData> {
        Err(ErrorData::method_not_found::<DiscoverRequestMethod>())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        refuse_inline_lifecycle(&context.meta)?;
        let (kernel, _access, _generation) = self.current();
        Ok(ListToolsResult::with_all_items(
            kernel
                .tools()
                .into_iter()
                .map(|tool| {
                    Tool::new(tool.name, tool.description, Arc::new(tool.input_schema))
                        .with_title(tool.title.clone())
                        .with_annotations(
                            ToolAnnotations::with_title(tool.title)
                                .read_only(tool.annotations.read_only)
                                .destructive(tool.annotations.destructive)
                                .idempotent(tool.annotations.idempotent)
                                .open_world(tool.annotations.open_world),
                        )
                })
                .collect(),
        ))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        refuse_inline_lifecycle(&context.meta)?;
        let progress = context
            .meta
            .get_progress_token()
            .map(|token| {
                ProgressReporter::new(Arc::new(RmcpProgressSink {
                    peer: context.peer.clone(),
                    token,
                }))
            })
            .unwrap_or_else(ProgressReporter::noop);
        let name = request.name.into_owned();
        let (kernel, access, generation) = self.current();
        let access = access.with_observed_at_unix_ms(current_unix_ms());
        let result = kernel
            .call_tool(
                &name,
                request.arguments.unwrap_or_default(),
                access,
                context.ct,
                progress,
            )
            .await;
        if self.generation() != generation {
            // The call bound a project: the listed tools are now the project's.
            let _ = context.peer.notify_tool_list_changed().await;
        }
        match result {
            Ok(value) => Ok(CallToolResult::structured(value).into()),
            Err(error) if error.kind() == ErrorKind::InvalidInput => {
                Err(protocol_error(ErrorCode::INVALID_PARAMS, error))
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                Err(protocol_error(ErrorCode::METHOD_NOT_FOUND, error))
            }
            Err(error) if error.kind() == ErrorKind::Internal => {
                Err(protocol_error(ErrorCode::INTERNAL_ERROR, error))
            }
            Err(error) => Ok(CallToolResult::structured_error(error_value(&error)).into()),
        }
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        refuse_inline_lifecycle(&context.meta)?;
        let (kernel, access, _generation) = self.current();
        let access = access.with_observed_at_unix_ms(current_unix_ms());
        Ok(ListResourcesResult::with_all_items(
            kernel
                .resources(&access)
                .into_iter()
                .map(|resource| {
                    let mut result = Resource::new(resource.uri, resource.name)
                        .with_description(resource.description)
                        .with_mime_type(resource.media_type);
                    if let Some(size) = resource.size {
                        result = result.with_size(size);
                    }
                    result
                })
                .collect(),
        ))
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        refuse_inline_lifecycle(&context.meta)?;
        let (kernel, _access, _generation) = self.current();
        Ok(ListResourceTemplatesResult::with_all_items(
            kernel
                .resource_templates()
                .into_iter()
                .map(|template| {
                    ResourceTemplate::new(template.uri_template, template.name)
                        .with_description(template.description)
                        .with_mime_type(template.media_type)
                })
                .collect(),
        ))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        refuse_inline_lifecycle(&context.meta)?;
        // rmcp moves request `_meta` into RequestContext before handler
        // dispatch, so protocol extensions must be read from the context.
        let range = parse_range(Some(&context.meta))?;
        let (kernel, access, _generation) = self.current();
        let access = access.with_observed_at_unix_ms(current_unix_ms());
        match kernel.read_resource(&request.uri, range, &access) {
            Ok(KernelResourceContents::Text {
                uri,
                media_type,
                text,
            }) => Ok(ReadResourceResult::new(vec![
                ResourceContents::text(text, uri).with_mime_type(media_type),
            ])
            .into()),
            Ok(KernelResourceContents::Blob {
                uri,
                media_type,
                bytes,
            }) => Ok(ReadResourceResult::new(vec![
                ResourceContents::blob(encode_base64(&bytes), uri).with_mime_type(media_type),
            ])
            .into()),
            Err(error) if error.kind() == ErrorKind::InvalidInput => {
                Err(protocol_error(ErrorCode::INVALID_PARAMS, error))
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                Err(protocol_error(ErrorCode::RESOURCE_NOT_FOUND, error))
            }
            Err(error) if error.kind() == ErrorKind::Internal => {
                Err(protocol_error(ErrorCode::INTERNAL_ERROR, error))
            }
            Err(error) => Err(protocol_error(ErrorCode::INVALID_REQUEST, error)),
        }
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        refuse_inline_lifecycle(&context.meta)?;
        let (kernel, _access, _generation) = self.current();
        Ok(ListPromptsResult::with_all_items(
            kernel.prompts().into_iter().map(prompt_to_rmcp).collect(),
        ))
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, ErrorData> {
        refuse_inline_lifecycle(&context.meta)?;
        let (kernel, _access, _generation) = self.current();
        kernel
            .get_prompt(&request.name, request.arguments.as_ref())
            .map(|result| {
                GetPromptResult::new(vec![PromptMessage::new_text(Role::User, result.text)])
                    .with_description(result.description)
                    .into()
            })
            .map_err(|error| match error.kind() {
                ErrorKind::InvalidInput => protocol_error(ErrorCode::INVALID_PARAMS, error),
                ErrorKind::NotFound => protocol_error(ErrorCode::METHOD_NOT_FOUND, error),
                _ => protocol_error(ErrorCode::INTERNAL_ERROR, error),
            })
    }
}

fn current_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

struct RmcpProgressSink {
    peer: Peer<RoleServer>,
    token: rmcp::model::ProgressToken,
}

#[async_trait]
impl ProgressSink for RmcpProgressSink {
    async fn report(&self, progress: f64, total: Option<f64>, message: Option<&str>) {
        let mut notification = ProgressNotificationParam::new(self.token.clone(), progress);
        if let Some(total) = total {
            notification = notification.with_total(total);
        }
        if let Some(message) = message {
            notification = notification.with_message(message);
        }
        let _ = self.peer.notify_progress(notification).await;
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ArtifactRangeWire {
    start: u64,
    end_inclusive: u64,
}

/// Refuse a request sent in the 2026-07-28 lifecycle.
///
/// That revision moves the client context `initialize` negotiates into each
/// request's own `_meta`, and rmcp answers such a request without a handshake on
/// stdio and without a session over HTTP. This server serves the `initialize`
/// lifecycle only, because the HTTP session binding and the stdio bootstrap rest on
/// it. The test is rmcp's own: a request is in that lifecycle when its `_meta` holds
/// every key the revision requires, or names a revision that has no handshake.
pub(crate) fn refuse_inline_lifecycle(meta: &RequestMetaObject) -> Result<(), ErrorData> {
    let inline = meta
        .missing_required_keys(&ProtocolVersion::V_2026_07_28)
        .is_empty()
        || meta
            .protocol_version()
            .is_some_and(|version| !version.has_initialize());
    if !inline {
        return Ok(());
    }
    Err(protocol_error(
        ErrorCode::INVALID_REQUEST,
        OutcomeError::new(
            ErrorKind::Unsupported,
            "this server serves MCP over the initialize handshake, at revision 2025-11-25 or \
             earlier; a request that carries its client context in _meta is not served",
            false,
            Default::default(),
        ),
    ))
}

/// Read the inclusive artifact byte range out of a request's `_meta`.
///
/// Visible to the crate so the HTTP binding shell can authorize the *same* range the
/// kernel will then enforce, rather than parsing the wire form a second time and
/// risking a disagreement between what was authorized and what was read.
pub(crate) fn parse_range(
    meta: Option<&RequestMetaObject>,
) -> Result<Option<ByteRange>, ErrorData> {
    let Some(value) = meta.and_then(|meta| meta.get(ARTIFACT_RANGE_META_KEY)) else {
        return Ok(None);
    };
    let wire: ArtifactRangeWire = serde_json::from_value(value.clone()).map_err(|_| {
        protocol_error(
            ErrorCode::INVALID_PARAMS,
            OutcomeError::new(
                ErrorKind::InvalidInput,
                "artifact range must contain integer start and endInclusive fields",
                false,
                Default::default(),
            ),
        )
    })?;
    ByteRange::new(wire.start, wire.end_inclusive)
        .map(Some)
        .map_err(|violation| {
            protocol_error(
                ErrorCode::INVALID_PARAMS,
                OutcomeError::new(
                    violation.kind(),
                    violation.to_string(),
                    false,
                    Default::default(),
                ),
            )
        })
}

fn prompt_to_rmcp(prompt: KernelPrompt) -> Prompt {
    Prompt::new(prompt.name, Some(prompt.description), None)
}

fn protocol_error(code: ErrorCode, error: OutcomeError) -> ErrorData {
    let message = error.message().to_owned();
    ErrorData::new(code, message, Some(error_value(&error)))
}

fn error_value(error: &OutcomeError) -> Value {
    serde_json::json!({
        "code": error.code(),
        "message": error.message(),
        "retryable": error.retryable(),
        "details": error.details(),
    })
}

fn encode_base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        output.push(ALPHABET[(first >> 2) as usize] as char);
        output.push(ALPHABET[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
        if chunk.len() > 1 {
            output.push(ALPHABET[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char);
        } else {
            output.push('=');
        }
        if chunk.len() > 2 {
            output.push(ALPHABET[(third & 0x3f) as usize] as char);
        } else {
            output.push('=');
        }
    }
    output
}

// API drift probe: rmcp 3 exposes request cancellation directly on
// RequestContext and progress through Peer<RoleServer>, and the handler answers
// with the `*Response` enums and reads `_meta` as a `RequestMetaObject`. Keeping
// these symbols in the adapter makes an accidental downgrade fail compile.
const _: fn(RequestContext<RoleServer>) -> tokio_util::sync::CancellationToken =
    |context| context.ct;
