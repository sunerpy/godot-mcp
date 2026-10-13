use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use godot_mcp_domain::CapabilityProfile;
use godot_mcp_domain::contracts::{
    ArtifactContract, ArtifactId, ArtifactKind, ArtifactMetadata, ArtifactOwner,
    ArtifactResourceUri, ErrorKind, OutcomeError, PrincipalId, ProjectIdentity, RangeSemantics,
    SessionId,
};
use godot_mcp_domain::manifests::canonical_manifest;
use godot_mcp_domain::objid::{LeaseKey, StableObjectId};
use godot_mcp_server::kernel::{AccessContext, KernelToolCall, KernelToolHandler, ServiceKernel};
use godot_mcp_server::rmcp_adapter::{ARTIFACT_RANGE_META_KEY, RmcpAdapter};
use rmcp::model::{
    CallToolRequestParams, ClientRequest, ErrorCode, GetPromptRequestParams,
    ProgressNotificationParam, ProgressToken, ReadResourceRequestParams, Request,
    RequestMetaObject, ResourceContents,
};
use rmcp::service::PeerRequestOptions;
use rmcp::{ClientHandler, ServiceError, ServiceExt};
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;

const LIVE_ARTIFACT_EXPIRY_UNIX_MS: u64 = u64::MAX;

/// Keeps every progress notification until the test reads it. The probe reports
/// progress as soon as its call starts, which can be before
/// `send_cancellable_request` has handed the call's token back, and rmcp's
/// `ProgressDispatcher` drops a notification nobody has subscribed to yet.
#[derive(Clone)]
struct TestClient {
    progress_tx: mpsc::UnboundedSender<ProgressNotificationParam>,
    progress_rx: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<ProgressNotificationParam>>>,
}

impl Default for TestClient {
    fn default() -> Self {
        let (progress_tx, progress_rx) = mpsc::unbounded_channel();
        Self {
            progress_tx,
            progress_rx: Arc::new(tokio::sync::Mutex::new(progress_rx)),
        }
    }
}

impl TestClient {
    /// The next progress notification for `token`, passing over any other call's.
    async fn next_progress(&self, token: &ProgressToken) -> Option<ProgressNotificationParam> {
        let mut received = self.progress_rx.lock().await;
        while let Some(notification) = received.recv().await {
            if &notification.progress_token == token {
                return Some(notification);
            }
        }
        None
    }
}

impl ClientHandler for TestClient {
    async fn on_progress(
        &self,
        notification: ProgressNotificationParam,
        _context: rmcp::service::NotificationContext<rmcp::RoleClient>,
    ) {
        let _ = self.progress_tx.send(notification);
    }
}

struct CancellationProbe {
    cancelled: mpsc::UnboundedSender<()>,
}

#[async_trait]
impl KernelToolHandler for CancellationProbe {
    async fn call(&self, call: KernelToolCall) -> Result<Value, OutcomeError> {
        call.progress
            .report(0.0, Some(1.0), Some("waiting for cancellation"))
            .await;
        call.cancellation.cancelled().await;
        let _ = self.cancelled.send(());
        Err(OutcomeError::new(
            ErrorKind::Cancelled,
            "request cancelled",
            false,
            BTreeMap::new(),
        ))
    }
}

fn fixture_project() -> ProjectIdentity {
    ProjectIdentity::from_lease_key(
        LeaseKey {
            root: StableObjectId::Unix { dev: 7, ino: 11 },
            project_file: StableObjectId::Unix { dev: 7, ino: 13 },
        },
        3,
    )
}

fn fixture_access() -> (AccessContext, PrincipalId, SessionId) {
    let principal = PrincipalId::parse("principal_0123456789abcdef").expect("principal");
    let session = SessionId::parse("session_0123456789abcdef").expect("session");
    (
        AccessContext::new(
            fixture_project(),
            principal.clone(),
            Some(session.clone()),
            2_000,
        ),
        principal,
        session,
    )
}

fn fixture_artifact(principal: PrincipalId, session: SessionId) -> ArtifactContract {
    let artifact_id = ArtifactId::parse(format!("artifact_{}", "a".repeat(64))).expect("id");
    ArtifactContract::new(
        artifact_id.clone(),
        ArtifactResourceUri::for_artifact(&artifact_id),
        fixture_project(),
        Some(ArtifactOwner {
            principal: Some(principal),
            session: Some(session),
        }),
        ArtifactMetadata {
            kind: ArtifactKind::Report,
            media_type: "application/octet-stream".to_owned(),
            byte_length: 5,
            sha256: "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824".to_owned(),
            created_at_unix_ms: 1_000,
        },
        LIVE_ARTIFACT_EXPIRY_UNIX_MS,
        RangeSemantics::new(5, 3, true).expect("range semantics"),
    )
    .expect("artifact contract")
}

/// A five-byte artifact of the fixture project, owned by `owner`.
fn owned_artifact(fill: char, owner: Option<ArtifactOwner>, expires: u64) -> ArtifactContract {
    let artifact_id =
        ArtifactId::parse(format!("artifact_{}", fill.to_string().repeat(64))).expect("id");
    ArtifactContract::new(
        artifact_id.clone(),
        ArtifactResourceUri::for_artifact(&artifact_id),
        fixture_project(),
        owner,
        ArtifactMetadata {
            kind: ArtifactKind::Report,
            media_type: "application/octet-stream".to_owned(),
            byte_length: 5,
            sha256: "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824".to_owned(),
            created_at_unix_ms: 1_000,
        },
        expires,
        RangeSemantics::new(5, 3, true).expect("range semantics"),
    )
    .expect("artifact contract")
}

async fn start_pair(
    kernel: ServiceKernel,
    access: AccessContext,
) -> anyhow::Result<(
    rmcp::service::RunningService<rmcp::RoleClient, TestClient>,
    tokio::task::JoinHandle<anyhow::Result<()>>,
)> {
    let (server_transport, client_transport) = tokio::io::duplex(16 * 1024);
    let server = RmcpAdapter::new(Arc::new(kernel), access);
    let server_task = tokio::spawn(async move {
        server.serve(server_transport).await?.waiting().await?;
        Ok(())
    });
    let client = TestClient::default().serve(client_transport).await?;
    Ok((client, server_task))
}

#[tokio::test]
async fn in_memory_client_initialize_list_call_read_cancel() -> anyhow::Result<()> {
    let (access, principal, session) = fixture_access();
    let artifact = fixture_artifact(principal, session);
    let artifact_uri = artifact.resource_uri().as_str().to_owned();
    let (cancelled_tx, mut cancelled_rx) = mpsc::unbounded_channel();

    let mut kernel = ServiceKernel::new(CapabilityProfile::Full);
    kernel.register_artifact(artifact, b"hello".to_vec(), &access)?;
    kernel.register_tool_handler(
        godot_mcp_domain::catalog::descriptor("project.validate")
            .and_then(|tool| tool.handler)
            .expect("a catalog tool"),
        Arc::new(CancellationProbe {
            cancelled: cancelled_tx,
        }),
    )?;

    let (client, server_task) = start_pair(kernel, access).await?;
    let server_info = client.peer_info().expect("initialize returned server info");
    assert_eq!(
        server_info
            .server_info
            .as_ref()
            .expect("initialize names the server")
            .name,
        "godot-mcp-server"
    );
    assert_eq!(
        server_info
            .server_info
            .as_ref()
            .expect("initialize names the server")
            .version,
        env!("CARGO_PKG_VERSION"),
        "serverInfo carries the release's version, the one `godot-mcp --version` prints"
    );

    let expected_tools = canonical_manifest(CapabilityProfile::Full).tools;
    let listed_tools: BTreeSet<_> = client
        .list_all_tools()
        .await?
        .into_iter()
        .map(|tool| tool.name.into_owned())
        .collect();
    assert_eq!(listed_tools, expected_tools);

    let listed_resources: BTreeSet<_> = client
        .list_all_resources()
        .await?
        .into_iter()
        .map(|resource| resource.uri)
        .collect();
    assert!(listed_resources.contains("godot-mcp://resources/capabilities"));
    assert!(listed_resources.contains(&artifact_uri));
    let templates = client.list_all_resource_templates().await?;
    assert!(
        templates
            .iter()
            .any(|template| { template.uri_template == "godot-mcp://artifacts/{artifact_id}" })
    );

    let prompts = client.list_all_prompts().await?;
    assert_eq!(prompts.len(), 1);
    let prompt = client
        .get_prompt(GetPromptRequestParams::new(prompts[0].name.clone()))
        .await?;
    assert_eq!(prompt.messages.len(), 1);

    let manifests = client
        .call_tool(CallToolRequestParams::new("capabilities.get"))
        .await?;
    let manifest_keys: BTreeSet<_> = manifests
        .structured_content
        .as_ref()
        .and_then(Value::as_object)
        .expect("structured profile manifests")
        .keys()
        .cloned()
        .collect();
    assert_eq!(
        manifest_keys,
        ["core", "editor", "runtime", "full"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    );

    let capabilities = client
        .read_resource(ReadResourceRequestParams::new(
            "godot-mcp://resources/capabilities",
        ))
        .await?;
    assert_eq!(capabilities.contents.len(), 1);

    let mut range_meta = RequestMetaObject::new();
    range_meta.0.insert(
        ARTIFACT_RANGE_META_KEY.to_owned(),
        json!({ "start": 1, "endInclusive": 3 }),
    );
    let artifact_read = client
        .read_resource(ReadResourceRequestParams::new(&artifact_uri).with_meta(range_meta))
        .await?;
    assert_eq!(artifact_read.contents.len(), 1);
    match &artifact_read.contents[0] {
        ResourceContents::BlobResourceContents { blob, .. } => assert_eq!(blob, "ZWxs"),
        other => panic!("expected bounded artifact blob, got {other:?}"),
    }

    let request = ClientRequest::CallToolRequest(Request::new(CallToolRequestParams::new(
        "project.validate",
    )));
    let handle = client
        .send_cancellable_request(request, PeerRequestOptions::no_options())
        .await?;
    let update = tokio::time::timeout(
        Duration::from_secs(2),
        client.service().next_progress(&handle.progress_token),
    )
    .await?
    .expect("progress notification");
    assert_eq!(update.message.as_deref(), Some("waiting for cancellation"));
    handle.cancel(Some("test cancellation".to_owned())).await?;
    tokio::time::timeout(Duration::from_secs(2), cancelled_rx.recv())
        .await?
        .expect("kernel observed cancellation");

    client.cancel().await?;
    server_task.await??;
    Ok(())
}

#[tokio::test]
async fn malformed_params_returns_invalid_params() -> anyhow::Result<()> {
    let (access, _, _) = fixture_access();
    let (client, server_task) =
        start_pair(ServiceKernel::new(CapabilityProfile::Core), access).await?;
    let mut arguments = Map::new();
    arguments.insert("profile".to_owned(), json!(42));

    let error = client
        .call_tool(CallToolRequestParams::new("capabilities.get").with_arguments(arguments))
        .await
        .expect_err("malformed profile must be a protocol error");
    match error {
        ServiceError::McpError(error) => {
            assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
            assert_eq!(
                error
                    .data
                    .as_ref()
                    .and_then(|data| data.get("code"))
                    .and_then(Value::as_str),
                Some("invalid_input")
            );
        }
        other => panic!("expected MCP invalid params, got {other:?}"),
    }

    client.cancel().await?;
    server_task.await??;
    Ok(())
}

/// `resources/list` names only the artifacts the caller could read: its own
/// and its project's unowned ones, while they have not expired.
#[tokio::test]
async fn the_resource_listing_names_only_artifacts_the_caller_may_read() -> anyhow::Result<()> {
    let (access, principal, session) = fixture_access();
    let stranger = PrincipalId::parse("principal_fedcba9876543210").expect("principal");
    let stranger_access = AccessContext::new(
        fixture_project(),
        stranger.clone(),
        Some(session.clone()),
        2_000,
    );
    let owner = |principal: PrincipalId| ArtifactOwner {
        principal: Some(principal),
        session: Some(session.clone()),
    };
    let own = owned_artifact(
        'a',
        Some(owner(principal.clone())),
        LIVE_ARTIFACT_EXPIRY_UNIX_MS,
    );
    let foreign = owned_artifact('b', Some(owner(stranger)), LIVE_ARTIFACT_EXPIRY_UNIX_MS);
    let expired = owned_artifact('c', Some(owner(principal)), 1_500);
    let unowned = owned_artifact('d', None, LIVE_ARTIFACT_EXPIRY_UNIX_MS);

    let mut kernel = ServiceKernel::new(CapabilityProfile::Full);
    kernel.register_artifact(own.clone(), b"hello".to_vec(), &access)?;
    kernel.register_artifact(foreign, b"hello".to_vec(), &stranger_access)?;
    kernel.register_artifact(expired, b"hello".to_vec(), &access)?;
    kernel.register_artifact(unowned.clone(), b"hello".to_vec(), &access)?;

    let (client, server_task) = start_pair(kernel, access).await?;
    let listed: BTreeSet<String> = client
        .list_all_resources()
        .await?
        .into_iter()
        .map(|resource| resource.uri)
        .filter(|uri| uri.starts_with("godot-mcp://artifacts/"))
        .collect();
    let expected: BTreeSet<String> = [own, unowned]
        .iter()
        .map(|contract| contract.resource_uri().as_str().to_owned())
        .collect();
    assert_eq!(
        listed, expected,
        "a listing never names another caller's or an expired artifact"
    );
    client.cancel().await?;
    server_task.await??;
    Ok(())
}
