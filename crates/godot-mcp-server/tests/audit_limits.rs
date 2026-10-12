//! Todo 31 acceptance suite: structured audit, enforced limits, and the
//! structural proof that no arbitrary-execution surface exists.
//!
//! Both descriptor-owned tests are exhaustive by design: `audit_limits.rs` is
//! the mechanical entry point behind `make qa-task TASK=31`, so every acceptance
//! criterion is asserted here rather than spread across ad-hoc tests.
//!
//! ## Why the absence proof is structural rather than a string scan
//!
//! The reference implementation this workspace studied guarded a scriptable
//! execution tool with a *string blacklist*, which any caller could sidestep by
//! spelling the payload differently. This suite therefore proves absence three
//! structural ways before it ever looks at bytes:
//!
//! 1. **Complete registry/schema enumeration.** Every tool and every resource of
//!    all four canonical profiles is walked, and no entry is the reserved
//!    identifier or any arbitrary-execution shape.
//! 2. **A real negative MCP call.** The reserved identifier is called through the
//!    production kernel *and* over a live `rmcp` client/server pair. Both return
//!    a stable `not_found` with zero downstream dispatch, because the kernel
//!    registry structurally refuses to bind a handler to a name the manifest does
//!    not contain — not because a blacklist matched.
//! 3. **Complete config-surface enumeration.** Every candidate switch key is
//!    rejected by the frozen `deny_unknown_fields` configuration shape, at the
//!    top level and inside every nested section.
//!
//! The binary/string scan is supplementary only, and it is proven *non-vacuous*
//! against a synthetic buffer before its zero-hit result on a real artifact is
//! trusted.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use godot_mcp_domain::CapabilityProfile;
use godot_mcp_domain::contracts::{
    ArtifactContract, ArtifactId, ArtifactKind, ArtifactMetadata, ArtifactOwner,
    ArtifactResourceUri, ByteRange, Capability, ErrorKind, OutcomeError, PolicyReason,
    PolicyVerdict, PrincipalId, ProjectIdentity, RangeSemantics, SessionId,
};
use godot_mcp_domain::manifests::canonical_manifest;
use godot_mcp_domain::objid::{LeaseKey, StableObjectId};
use godot_mcp_server::audit::{
    AuditBoundary, AuditDigest, AuditEvent, AuditEventDraft, AuditFailurePolicy, AuditLane,
    AuditLog, AuditPhase, AuditPriority, AuditQueueFacts, AuditSinkConfig, AuditStatus,
    AuditTransport, MAX_AUDIT_EVENT_BYTES, RESERVED_UNSAFE_SCRIPT_CAPABILITY,
    audit_execution_surface, config_surface_probe, scan_bytes_for_reserved_identifier,
};
use godot_mcp_server::idempotency::{ModePrincipal, TransportAuthMode};
use godot_mcp_server::kernel::{
    AccessContext, KernelToolCall, KernelToolHandler, ProgressReporter, ServiceKernel,
};
use godot_mcp_server::limits::{LimitEnforcer, LimitPolicy, LimitRejection};
use godot_mcp_server::observability::{
    CorrelationId, RequestId, SafeField, ScreenshotBytes, SecretValue, SourceText, TokenValue,
};
use godot_mcp_server::rmcp_adapter::RmcpAdapter;
use godot_mcp_server::test_support::{AttemptSpec, AuditLimitsConfig, AuditLimitsHarness};
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

/// The four canonical profiles, in manifest order.
const PROFILES: [CapabilityProfile; 4] = [
    CapabilityProfile::Core,
    CapabilityProfile::Editor,
    CapabilityProfile::Runtime,
    CapabilityProfile::Full,
];

/// Frozen Todo 3 profile capacities. A drift here means the enumeration below
/// silently stopped covering the real surface.
/// Exact contract 0.2 profile sizes. Version 0.1 listed planned editor and
/// runtime methods without handlers (25/39/49); contract 0.2 lists only the
/// implemented catalog, so a planned method cannot inflate a profile.
const PROFILE_TOOL_COUNTS: [(CapabilityProfile, usize); 4] = [
    (CapabilityProfile::Core, 15),
    (CapabilityProfile::Editor, 48),
    (CapabilityProfile::Runtime, 44),
    (CapabilityProfile::Full, 73),
];

/// Every shape that would denote an arbitrary-code-execution outcome. The
/// enumeration below asserts no manifest entry matches any of them.
const EXECUTION_SHAPES: &[&str] = &[
    "unsafe_script",
    "unsafe-script",
    "script.run",
    "script.eval",
    "script.exec",
    "script.execute",
    "gdscript.eval",
    "gdscript.exec",
    "gdscript.run",
    "gdscript.execute",
    "shell.exec",
    "shell.run",
    "process.exec",
    "command.run",
    "eval",
    "exec",
    "system",
];

/// JSON keys an audit record may never contain, because each would carry raw
/// arguments, results, content or a storage locator.
const FORBIDDEN_AUDIT_KEYS: &[&str] = &[
    "arguments",
    "args",
    "params",
    "parameters",
    "result",
    "results",
    "path",
    "paths",
    "file",
    "files",
    "filename",
    "bytes",
    "body",
    "token",
    "secret",
    "password",
    "credential",
    "source",
    "source_text",
    "content",
    "text",
    "screenshot",
    "media",
    "artifact_path",
    "stdout",
    "stderr",
    "url",
    "uri",
];

/// Unique sentinels: if any of these bytes reach an audit sink, a formatter or a
/// serializer, redaction failed.
const TOKEN_SENTINEL: &str = "SENTINEL-TOKEN-c1f0a7d24b6e";
const SOURCE_SENTINEL: &str = "func _ready():\n\tprint(\"SENTINEL-SOURCE-9ab3\")\n";
const SECRET_SENTINEL: &str = "SENTINEL-SECRET-4d81e77c0a2f";
const MEDIA_SENTINEL: &[u8] = b"\x89PNG\r\n\x1a\nSENTINEL-MEDIA-77b0e2c9";

/// A decision identifier shaped exactly like the Todo 26 digest. It is a `const`
/// rather than a `format!` so the draft template can borrow it for its lifetime.
const TEMPLATE_DECISION_ID: &str =
    "decision_0000000000000000000000000000000000000000000000000000000000000000";

// ---------------------------------------------------------------------------
// Shared fixtures
// ---------------------------------------------------------------------------

fn temp_root(label: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("godot-mcp-task31-{label}-"))
        .tempdir()
        .expect("temp root")
}

fn fixture_project() -> ProjectIdentity {
    ProjectIdentity::from_lease_key(
        LeaseKey {
            root: StableObjectId::Unix { dev: 31, ino: 310 },
            project_file: StableObjectId::Unix { dev: 31, ino: 311 },
        },
        7,
    )
}

fn fixture_principal() -> PrincipalId {
    PrincipalId::parse("principal_0123456789abcdef").expect("principal")
}

fn fixture_session() -> SessionId {
    SessionId::parse("session_0123456789abcdef").expect("session")
}

fn fixture_access() -> AccessContext {
    AccessContext::new(
        fixture_project(),
        fixture_principal(),
        Some(fixture_session()),
        2_000,
    )
}

fn fixture_artifact(byte_length: u64) -> ArtifactContract {
    let artifact_id =
        ArtifactId::parse(format!("artifact_{}", "4".repeat(64))).expect("artifact id");
    ArtifactContract::new(
        artifact_id.clone(),
        ArtifactResourceUri::for_artifact(&artifact_id),
        fixture_project(),
        Some(ArtifactOwner {
            principal: Some(fixture_principal()),
            session: Some(fixture_session()),
        }),
        ArtifactMetadata {
            kind: ArtifactKind::Report,
            media_type: "application/octet-stream".to_owned(),
            byte_length,
            sha256: "4".repeat(64),
            created_at_unix_ms: 1_000,
        },
        10_000,
        RangeSemantics::new(byte_length, byte_length, true).expect("range semantics"),
    )
    .expect("artifact contract")
}

/// The first `project_write` tool the `full` profile advertises. Derived from the
/// frozen matrix rather than hard-coded, so a manifest edit cannot silently make
/// this suite exercise a read instead of a mutation.
fn mutating_tool() -> String {
    let manifest = canonical_manifest(CapabilityProfile::Full);
    godot_mcp_server::test_support::policy_matrix()
        .into_iter()
        .filter(|row| row.capability == "project_write" && manifest.tools.contains(&row.tool))
        .map(|row| row.tool)
        .next()
        .expect("the full profile advertises at least one project_write tool")
}

/// The first read-only tool the `full` profile advertises.
fn reading_tool() -> String {
    let manifest = canonical_manifest(CapabilityProfile::Full);
    godot_mcp_server::test_support::policy_matrix()
        .into_iter()
        .filter(|row| row.capability == "read" && manifest.tools.contains(&row.tool))
        .map(|row| row.tool)
        .next()
        .expect("the full profile advertises at least one read tool")
}

/// Walk every key of a JSON document, so a forbidden key cannot hide in a nested
/// object or array element.
fn collect_keys(value: &Value, keys: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                keys.insert(key.clone());
                collect_keys(child, keys);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_keys(item, keys);
            }
        }
        _ => {}
    }
}

fn is_hex_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// A downstream handler that counts every dispatch it receives.
struct CountingHandler(Arc<AtomicUsize>);

#[async_trait]
impl KernelToolHandler for CountingHandler {
    async fn call(&self, _call: KernelToolCall) -> Result<Value, OutcomeError> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok(Value::Object(Map::new()))
    }
}

struct EnvelopeEchoHandler;

#[async_trait]
impl KernelToolHandler for EnvelopeEchoHandler {
    async fn call(&self, call: KernelToolCall) -> Result<Value, OutcomeError> {
        Ok(json!({
            "idempotency_id": call.idempotency_id,
            "metadata_removed": !call.arguments.contains_key("idempotencyId"),
            "cancelled": call.cancellation.is_cancelled(),
        }))
    }
}

#[tokio::test]
async fn production_kernel_enforces_limits_and_persists_audit() {
    let root = temp_root("production-kernel");
    let audit = Arc::new(
        AuditLog::open(AuditSinkConfig {
            path: root.path().join("audit").join("audit.jsonl"),
            max_bytes: 64 * 1024,
            max_files: 2,
            failure_policy: AuditFailurePolicy::FailClosed,
        })
        .expect("production audit sink opens"),
    );
    let limits = Arc::new(
        LimitEnforcer::new(LimitPolicy {
            max_body_bytes: 32,
            ..LimitPolicy::default()
        })
        .expect("production limits build"),
    );
    let dispatches = Arc::new(AtomicUsize::new(0));
    let tool = "health.get".to_owned();
    let mut kernel = ServiceKernel::new(CapabilityProfile::Full)
        .with_operational_controls(Arc::clone(&limits), Arc::clone(&audit));
    kernel
        .register_tool_handler(
            godot_mcp_domain::catalog::descriptor(&tool)
                .and_then(|tool| tool.handler)
                .expect("a catalog tool"),
            Arc::new(CountingHandler(Arc::clone(&dispatches))),
        )
        .expect("the read handler registers");

    let oversized = Map::from_iter([("payload".to_owned(), Value::String("x".repeat(128)))]);
    let refusal = kernel
        .call_tool(
            &tool,
            oversized,
            fixture_access(),
            CancellationToken::new(),
            ProgressReporter::noop(),
        )
        .await
        .expect_err("the production kernel refuses an oversized request");
    assert_eq!(refusal.kind(), ErrorKind::InvalidInput);
    assert_eq!(refusal.code(), "invalid_input");
    assert_eq!(
        dispatches.load(Ordering::Relaxed),
        0,
        "the limit refusal happens before dispatch"
    );

    kernel
        .call_tool(
            &tool,
            Map::new(),
            fixture_access(),
            CancellationToken::new(),
            ProgressReporter::noop(),
        )
        .await
        .expect("an in-bound request is dispatched");
    assert_eq!(dispatches.load(Ordering::Relaxed), 1);

    let bytes = audit.current_bytes().expect("audit bytes are durable");
    let rows = String::from_utf8(bytes).expect("audit is UTF-8 JSONL");
    let events: Vec<Value> = rows
        .lines()
        .map(|line| serde_json::from_str(line).expect("audit row is JSON"))
        .collect();
    assert_eq!(events.len(), 2, "refusal and success are both audited");
    assert_eq!(events[0]["outcome"]["status"], "refused");
    assert_eq!(events[1]["outcome"]["status"], "ok");
    assert_eq!(events[1]["transport"], "stdio");
    for event in &events {
        let effect = event["effect"]
            .as_object()
            .expect("the effect dimension is an object");
        assert!(
            !effect.contains_key("boundary")
                && !effect.contains_key("phase")
                && !effect.contains_key("content_digest"),
            "an effect dimension must omit facts the request did not produce"
        );
    }
}

#[tokio::test]
async fn fail_closed_post_effect_audit_failure_is_reported_to_caller() {
    let mutating = mutating_tool();
    let reading = reading_tool();
    let closed_root = temp_root("post-effect-fail-closed");
    let closed = AuditLimitsHarness::open(
        closed_root.path(),
        AuditLimitsConfig {
            fail_closed: true,
            record_failure_after_probe: true,
            ..AuditLimitsConfig::default()
        },
    )
    .expect("fail-closed harness opens");
    closed
        .audit()
        .probe_writable()
        .expect("the pre-effect audit probe succeeds");

    let retry_safe = closed
        .kernel_tool_call(&mutating, Some("post-effect-audit-1"))
        .await;
    assert!(
        retry_safe.dispatched,
        "the mutation completed before audit failed"
    );
    assert_eq!(retry_safe.code, ErrorKind::Unavailable.code());
    assert!(
        retry_safe.retryable,
        "the same idempotency identity is replay-safe"
    );
    assert!(
        retry_safe
            .message
            .as_deref()
            .is_some_and(|message| message.contains("mutation committed")),
        "the caller must learn that the effect committed"
    );

    let retry_unsafe = closed.kernel_tool_call(&mutating, None).await;
    assert!(
        retry_unsafe.dispatched,
        "the mutation completed before audit failed"
    );
    assert_eq!(retry_unsafe.code, ErrorKind::Unavailable.code());
    assert!(
        !retry_unsafe.retryable,
        "a mutation without an idempotency identity must not invite a retry"
    );

    let read = closed.kernel_tool_call(&reading, None).await;
    assert_eq!(read.code, "ok", "audit failure must not fail a read");

    let open_root = temp_root("post-effect-fail-open");
    let opened = AuditLimitsHarness::open(
        open_root.path(),
        AuditLimitsConfig {
            fail_closed: false,
            record_failure_after_probe: true,
            ..AuditLimitsConfig::default()
        },
    )
    .expect("fail-open harness opens");
    let fail_open = opened
        .kernel_tool_call(&mutating, Some("post-effect-audit-2"))
        .await;
    assert!(fail_open.dispatched);
    assert_eq!(
        fail_open.code, "ok",
        "an explicitly fail-open mutation keeps its successful outcome"
    );
}

#[test]
fn production_kernel_audit_consumes_policy_and_effect_facts() {
    let kernel = include_str!("../src/kernel.rs");
    let production = kernel.split("#[cfg(test)]").next().unwrap_or(kernel);
    let record_audit = production
        .split_once("fn record_audit")
        .expect("the production audit seam exists")
        .1
        .split_once("/// Read one resource")
        .expect("the production audit seam has a bounded source region")
        .0;

    assert!(
        !record_audit.contains("PolicyVerdict::Allow")
            && !record_audit.contains("PolicyReason::Granted"),
        "record_audit must not invent an allow/granted policy result"
    );
    assert_eq!(
        record_audit.matches("audit.policy_decision()").count(),
        1,
        "record_audit must consume exactly one decision carried from policy evaluation"
    );
    assert_eq!(
        record_audit.matches("audit.effect_facts()").count(),
        1,
        "record_audit must consume exactly one set of observed effect facts"
    );

    let router = include_str!("../src/router/mod.rs");
    assert!(
        router.contains("record_policy_decision"),
        "the production router must carry the decision returned by PolicyAuthority"
    );
    assert!(
        router.contains("record_effect_facts"),
        "the production router must carry boundary, phase, and digest facts after execution"
    );
}

#[test]
fn operational_controls_cover_resource_and_artifact_entry_points() {
    let root = temp_root("entry-point-controls");
    let audit = Arc::new(
        AuditLog::open(AuditSinkConfig {
            path: root.path().join("audit").join("audit.jsonl"),
            max_bytes: 64 * 1024,
            max_files: 2,
            failure_policy: AuditFailurePolicy::FailClosed,
        })
        .expect("production audit sink opens"),
    );
    let limits = Arc::new(
        LimitEnforcer::new(LimitPolicy {
            max_response_bytes: 1,
            max_range_bytes: 1,
            max_artifact_total_bytes: 3,
            ..LimitPolicy::default()
        })
        .expect("production limits build"),
    );
    let mut kernel = ServiceKernel::new(CapabilityProfile::Full)
        .with_operational_controls(Arc::clone(&limits), audit);

    let response_refusal = kernel
        .read_resource(
            "godot-mcp://resources/capabilities",
            None,
            &fixture_access(),
        )
        .expect_err("resource responses must obey the response-byte limit");
    assert_eq!(response_refusal.kind(), ErrorKind::InvalidInput);

    let registration_refusal = kernel
        .register_artifact(fixture_artifact(4), vec![0_u8; 4], &fixture_access())
        .expect_err("artifact registration must reserve the total-byte budget");
    assert_eq!(registration_refusal.kind(), ErrorKind::Unavailable);

    let contract = fixture_artifact(3);
    let uri = contract.resource_uri().as_str().to_owned();
    kernel
        .register_artifact(contract, vec![0_u8; 3], &fixture_access())
        .expect("an artifact within the total-byte budget registers");
    let range_refusal = kernel
        .read_resource(
            &uri,
            Some(ByteRange::new(0, 1).expect("two-byte range")),
            &fixture_access(),
        )
        .expect_err("artifact reads must obey the range-byte limit");
    assert_eq!(range_refusal.kind(), ErrorKind::InvalidInput);
}

#[test]
fn artifact_contract_has_exactly_its_documented_production_producers() {
    // Captures are the artifacts this build produces: the runtime boundary is
    // the production caller of the store's registration, and the operator
    // guide names exactly those producers.
    let guide = include_str!("../../../docs/operator/artifacts.md");
    for producer in ["runtime.screenshot.capture", "runtime.recording.capture"] {
        assert!(
            guide.contains(producer),
            "the operator guide must name the artifact producer {producer}"
        );
    }
    let boundary = include_str!("../src/engine_plane/session/boundary.rs");
    let boundary = boundary.split("#[cfg(test)]").next().unwrap_or(boundary);
    assert!(
        boundary.contains("self.artifacts.register("),
        "the runtime boundary must register captures through the production store"
    );
}

#[test]
fn binding_revocation_operations_have_production_callers() {
    let registry = include_str!("../src/binding/registry.rs");
    let http = include_str!("../src/http.rs");
    let registry = registry.split("#[cfg(test)]").next().unwrap_or(registry);
    let http = http.split("#[cfg(test)]").next().unwrap_or(http);
    let production = format!("{registry}\n{http}");
    for operation in [
        ".revoke_all()",
        ".revoke_other_generations(",
        ".revoke_expired(",
    ] {
        assert!(
            production.contains(operation),
            "production HTTP lifecycle must call `{operation}`"
        );
    }
}

#[test]
fn disposable_config_is_consumed_by_production_runtime_assembly() {
    let cli = include_str!("../../godot-mcp-cli/src/bin/godot-mcp.rs");
    assert!(
        cli.contains("config.disposable"),
        "the accepted disposable setting must be read by production startup"
    );
}

#[tokio::test]
async fn client_idempotency_and_cancellation_reach_the_router() {
    let kernel = include_str!("../src/kernel.rs");
    let handler = include_str!("../src/engine_plane/handler.rs");
    let process = include_str!("../src/engine_plane/process.rs");
    let canonical = include_str!("../src/engine_plane/canonical.rs");
    assert!(
        kernel.contains("idempotency_id: Option<String>"),
        "the kernel call envelope must carry the client idempotency identity"
    );
    assert!(
        kernel.contains("arguments.remove(IDEMPOTENCY_ARGUMENT)"),
        "the protocol-only idempotency field must be removed before strict tool deserialization"
    );
    assert!(
        handler.contains("call.idempotency_id") && handler.contains("call.cancellation"),
        "the engine handler must consume both live request fields"
    );
    for source in [process, canonical] {
        assert!(
            source.contains("idempotency_id: Option<&str>"),
            "each engine route must accept the kernel idempotency identity"
        );
        assert!(
            source.contains("cancellation: CancellationToken"),
            "each engine route must accept the live cancellation token"
        );
    }
    assert!(
        !process.contains("CancellationToken::new()")
            && !canonical.contains("CancellationToken::new()"),
        "engine routes must not replace the live cancellation token"
    );

    let mut kernel = ServiceKernel::new(CapabilityProfile::Full);
    kernel
        .register_tool_handler(
            godot_mcp_domain::catalog::descriptor("health.get")
                .and_then(|tool| tool.handler)
                .expect("a catalog tool"),
            Arc::new(EnvelopeEchoHandler),
        )
        .expect("echo handler registers");
    let health_schema = kernel
        .tools()
        .into_iter()
        .find(|tool| tool.name == "health.get")
        .expect("health.get schema");
    assert_eq!(
        health_schema.input_schema["properties"]["idempotencyId"]["type"],
        json!("string")
    );
    assert_eq!(
        health_schema.input_schema["properties"]["idempotencyId"]["minLength"],
        json!(1)
    );
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let result = kernel
        .call_tool(
            "health.get",
            Map::from_iter([(
                "idempotencyId".to_owned(),
                Value::String("client-request-7".to_owned()),
            )]),
            fixture_access(),
            cancellation,
            ProgressReporter::noop(),
        )
        .await
        .expect("the protocol-neutral envelope reaches the handler");
    assert_eq!(result["idempotency_id"], "client-request-7");
    assert_eq!(result["metadata_removed"], true);
    assert_eq!(result["cancelled"], true);

    let empty_error = kernel
        .call_tool(
            "health.get",
            Map::from_iter([("idempotencyId".to_owned(), Value::String(String::new()))]),
            fixture_access(),
            CancellationToken::new(),
            ProgressReporter::noop(),
        )
        .await
        .expect_err("an empty idempotency identifier must be refused");
    assert_eq!(empty_error.kind(), ErrorKind::InvalidInput);
    assert_eq!(
        empty_error.message(),
        "an idempotency identifier is required"
    );
}

#[test]
fn transport_identity_updates_canonical_principal_and_session() {
    let kernel = include_str!("../src/kernel.rs");
    assert!(
        kernel.contains("self.principal = canonical_principal"),
        "transport binding must replace the bootstrap principal"
    );
    assert!(
        kernel.contains("self.session = canonical_session"),
        "transport binding must replace the bootstrap session"
    );

    let original = fixture_access();
    let rebound = original.clone().with_transport_identity(
        TransportAuthMode::ProxiedOidc,
        ModePrincipal::Oidc {
            issuer: "https://issuer.example".to_owned(),
            subject: "operator-7".to_owned(),
        },
        "http-session-7",
    );
    assert_ne!(rebound.principal(), original.principal());
    assert_ne!(rebound.session(), original.session());
    assert_eq!(rebound.transport_mode(), TransportAuthMode::ProxiedOidc);
}

// ---------------------------------------------------------------------------
// Happy path
// ---------------------------------------------------------------------------

/// An allowed mutation produces exactly one redacted policy/effect/outcome audit
/// event, the sink is append-only and deterministically rotatable, and the limit
/// enforcer admits and recovers.
#[test]
fn audit_limits_happy_path() {
    let tool = mutating_tool();

    // --- 1. one allowed mutation, one dispatch, one recorded event ----------
    let root = temp_root("happy");
    let harness = AuditLimitsHarness::open(root.path(), AuditLimitsConfig::default())
        .expect("audit/limits harness opens");
    assert_eq!(harness.touches(), 0, "no dispatch has happened yet");

    let report = harness
        .attempt(AttemptSpec {
            tool: &tool,
            now_ms: 10_000,
            request_bytes: 512,
            response_bytes: 4_096,
        })
        .expect("an allowed mutation is admitted");
    assert!(report.dispatched, "the admitted mutation reached dispatch");
    assert!(
        report.audit_recorded,
        "the audit event was durably recorded"
    );
    assert_eq!(harness.touches(), 1, "exactly one downstream dispatch");

    let events = harness.audit_events().expect("audit events are readable");
    assert_eq!(events.len(), 1, "one attempt produces exactly one event");
    let event = &events[0];

    // --- 2. every required audit dimension is present ----------------------
    for field in [
        "schema_version",
        "sequence",
        "recorded_at_unix_ms",
        "transport",
        "profile",
        "tool",
        "actor",
        "project",
        "policy",
        "queue",
        "effect",
        "outcome",
        "correlation",
    ] {
        assert!(
            event.get(field).is_some(),
            "the audit event must carry the {field} dimension"
        );
    }

    assert_eq!(event["schema_version"], Value::from(1));
    assert_eq!(event["sequence"], Value::from(1));
    assert_eq!(event["transport"], Value::from("stdio"));
    assert_eq!(event["profile"], Value::from("full"));
    assert_eq!(event["tool"], Value::from(tool.as_str()));

    // --- 3. the policy dimension names the real verdict/reason/decision ----
    let policy = &event["policy"];
    assert_eq!(policy["capability"], Value::from("project_write"));
    assert_eq!(policy["verdict"], Value::from("allow"));
    assert_eq!(policy["reason"], Value::from("granted"));
    let decision_id = policy["decision_id"]
        .as_str()
        .expect("the decision ID is a string");
    assert!(
        decision_id.starts_with("decision_") && is_hex_digest(&decision_id["decision_".len()..]),
        "the decision ID is the non-sensitive Todo 26 digest shape"
    );

    // --- 4. the queue dimension reports the real lane and depth ------------
    let queue = &event["queue"];
    assert_eq!(queue["lane"], Value::from("mutation"));
    assert_eq!(queue["priority"], Value::from("interactive"));
    assert_eq!(queue["admitted"], Value::from(true));
    assert!(
        queue["queued_depth"].is_u64(),
        "the queued depth is a bounded integer"
    );

    // --- 5. the effect dimension reports boundary and terminal phase -------
    let effect = &event["effect"];
    assert_eq!(effect["boundary"], Value::from("file"));
    assert_eq!(effect["phase"], Value::from("committed"));

    // --- 6. the outcome dimension reports success without an error class ---
    let outcome = &event["outcome"];
    assert_eq!(outcome["status"], Value::from("ok"));
    assert!(
        outcome.get("error").is_none() || outcome["error"].is_null(),
        "a successful outcome carries no error class"
    );

    // --- 7. correlation joins the event to its request ---------------------
    let correlation = &event["correlation"];
    assert!(
        correlation["correlation_id"].as_str().is_some(),
        "the event carries a correlation ID"
    );
    assert!(
        correlation["request_id"].as_str().is_some(),
        "the event carries a request ID"
    );

    // --- 8. the actor and project are digests, never plaintext -------------
    let actor_digest = event["actor"]["principal_digest"]
        .as_str()
        .expect("the principal digest is a string");
    assert!(
        is_hex_digest(actor_digest),
        "the principal is a 64-hex digest"
    );
    assert_ne!(
        actor_digest,
        fixture_principal().as_str(),
        "the raw principal never appears"
    );
    let session_digest = event["actor"]["session_digest"]
        .as_str()
        .expect("the session digest is a string");
    assert!(
        is_hex_digest(session_digest),
        "the session is a 64-hex digest"
    );
    assert_ne!(
        session_digest,
        fixture_session().as_str(),
        "the raw session never appears"
    );
    let project_digest = event["project"]["object_digest"]
        .as_str()
        .expect("the project digest is a string");
    assert!(
        is_hex_digest(project_digest),
        "the project is a 64-hex digest"
    );
    assert_ne!(
        project_digest,
        fixture_project().object_id().as_str(),
        "the raw object identity never appears"
    );
    assert_eq!(
        event["project"]["generation"],
        Value::from(fixture_project().generation())
    );

    // --- 9. no forbidden key exists anywhere in the record -----------------
    let mut keys = BTreeSet::new();
    collect_keys(event, &mut keys);
    for forbidden in FORBIDDEN_AUDIT_KEYS {
        assert!(
            !keys.contains(*forbidden),
            "the audit record must not carry the `{forbidden}` key"
        );
    }
    for key in &keys {
        if key.ends_with("_digest") {
            // Every digest-shaped field really is a one-way digest.
            let pointer_value = find_by_key(event, key).expect("digest key resolves");
            assert!(
                pointer_value.is_null() || pointer_value.as_str().is_some_and(is_hex_digest),
                "the `{key}` field must be a 64-hex digest or absent"
            );
        }
    }

    // --- 10. the record is bounded ----------------------------------------
    let serialized = serde_json::to_vec(event).expect("audit events serialize");
    assert!(
        serialized.len() <= MAX_AUDIT_EVENT_BYTES,
        "the audit record must stay within its {MAX_AUDIT_EVENT_BYTES}-byte bound"
    );

    // --- 11. the sink is append-only --------------------------------------
    let after_first = harness
        .audit()
        .current_bytes()
        .expect("audit bytes readable");
    harness
        .attempt(AttemptSpec {
            tool: &tool,
            now_ms: 10_001,
            request_bytes: 512,
            response_bytes: 4_096,
        })
        .expect("a second allowed mutation is admitted");
    let after_second = harness
        .audit()
        .current_bytes()
        .expect("audit bytes readable");
    assert!(
        after_second.starts_with(&after_first),
        "the sink appends; it never rewrites a recorded event"
    );
    assert!(
        after_second.len() > after_first.len(),
        "the second event was appended"
    );
    let events = harness.audit_events().expect("audit events are readable");
    assert_eq!(events.len(), 2, "two attempts produce two events");
    assert_eq!(events[1]["sequence"], Value::from(2), "sequence advances");

    // --- 12. rotation is deterministic and bounded ------------------------
    let rotate_root = temp_root("rotate");
    let rotate = AuditLimitsHarness::open(
        rotate_root.path(),
        AuditLimitsConfig {
            max_audit_bytes: 900,
            max_audit_files: 2,
            ..AuditLimitsConfig::default()
        },
    )
    .expect("rotating harness opens");
    for index in 0..12_u64 {
        rotate
            .attempt(AttemptSpec {
                tool: &tool,
                now_ms: 20_000 + index,
                request_bytes: 64,
                response_bytes: 64,
            })
            .expect("each attempt within the rate window is admitted");
    }
    let rotations = rotate.audit().rotations();
    assert!(rotations > 0, "a small byte bound forces rotation");
    let retained_files = rotate
        .audit()
        .retained_files()
        .expect("retained files are countable");
    assert!(
        retained_files <= 3,
        "at most the live file plus `max_audit_files` rotations are retained, got {retained_files}"
    );
    let retained_bytes = rotate
        .audit()
        .retained_bytes()
        .expect("retained bytes are countable");
    assert!(
        retained_bytes <= 900 * 3,
        "retention is byte-bounded, got {retained_bytes}"
    );
    // Determinism: the same attempt count against the same bounds rotates the
    // same number of times.
    let replay_root = temp_root("rotate-replay");
    let replay = AuditLimitsHarness::open(
        replay_root.path(),
        AuditLimitsConfig {
            max_audit_bytes: 900,
            max_audit_files: 2,
            ..AuditLimitsConfig::default()
        },
    )
    .expect("replay harness opens");
    for index in 0..12_u64 {
        replay
            .attempt(AttemptSpec {
                tool: &tool,
                now_ms: 20_000 + index,
                request_bytes: 64,
                response_bytes: 64,
            })
            .expect("each replayed attempt is admitted");
    }
    assert_eq!(
        replay.audit().rotations(),
        rotations,
        "rotation depends only on byte bounds, so it is deterministic"
    );

    // --- 13. limits admit, exhaust, and recover ---------------------------
    let policy = LimitPolicy::default();
    let enforcer = Arc::new(
        godot_mcp_server::limits::LimitEnforcer::new(policy.clone()).expect("enforcer builds"),
    );
    let subject = SafeField::new("subject_happy").expect("safe subject");
    let mut held = Vec::new();
    for _ in 0..policy.max_subject_concurrency {
        held.push(
            enforcer
                .clone()
                .admit(&subject, 1_000)
                .expect("each slot below the bound is admitted"),
        );
    }
    let rejection = enforcer
        .clone()
        .admit(&subject, 1_000)
        .expect_err("the slot above the bound is refused");
    assert_eq!(rejection.error_kind(), ErrorKind::Unavailable);
    assert!(rejection.is_retryable());
    assert_eq!(rejection.http_status(), 429);
    drop(held);
    enforcer
        .clone()
        .admit(&subject, 1_000)
        .expect("dropping the guards recovers the concurrency budget");

    // --- 14. the reserved identifier is a name, never a capability --------
    for capability in [
        Capability::Read,
        Capability::ProjectWrite,
        Capability::EditorMutate,
        Capability::RuntimeExecute,
        Capability::ExportDestructive,
    ] {
        // Exhaustiveness is enforced by the compiler: adding an `unsafe_script`
        // variant to the frozen enum would fail to compile against this array.
        let name = godot_mcp_server::audit::capability_audit_name(capability);
        assert_ne!(
            name, RESERVED_UNSAFE_SCRIPT_CAPABILITY,
            "no capability variant may be the reserved identifier"
        );
    }
}

/// Resolve the first value stored under `key`, at any depth.
fn find_by_key(value: &Value, key: &str) -> Option<Value> {
    match value {
        Value::Object(map) => {
            if let Some(found) = map.get(key) {
                return Some(found.clone());
            }
            map.values().find_map(|child| find_by_key(child, key))
        }
        Value::Array(items) => items.iter().find_map(|item| find_by_key(item, key)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Failure paths
// ---------------------------------------------------------------------------

/// Raw token/source/media bytes never appear; rate and concurrency limits block
/// *before* dispatch; and a full registry/schema/config enumeration plus a real
/// negative MCP call prove no arbitrary-execution surface exists.
#[tokio::test]
async fn audit_limits_failure_paths() {
    let mutating = mutating_tool();
    let reading = reading_tool();

    // =====================================================================
    // A. Raw token/source/media bytes never appear
    // =====================================================================
    let token = TokenValue::new(TOKEN_SENTINEL);
    let source = SourceText::new(SOURCE_SENTINEL);
    let secret = SecretValue::new(SECRET_SENTINEL);
    let media = ScreenshotBytes::new(MEDIA_SENTINEL.to_vec());

    // A.1 formatting is redacted on both paths for all four wrappers.
    for rendered in [
        format!("{token:?}"),
        format!("{token}"),
        format!("{source:?}"),
        format!("{source}"),
        format!("{secret:?}"),
        format!("{secret}"),
        format!("{media:?}"),
        format!("{media}"),
    ] {
        assert_eq!(
            rendered, "[REDACTED]",
            "sensitive wrappers must format as [REDACTED]"
        );
    }

    // A.2 a raw payload cannot become an audit field at all: the bounded
    //     single-line field type rejects it structurally.
    assert!(
        SafeField::new(SOURCE_SENTINEL).is_err(),
        "raw source text can never be a safe audit field"
    );
    assert!(
        SafeField::new(String::from_utf8_lossy(MEDIA_SENTINEL).into_owned()).is_err(),
        "raw media bytes can never be a safe audit field"
    );

    // A.3 the only route into an event is a one-way digest.
    let token_digest = AuditDigest::of_token(&token);
    let source_digest = AuditDigest::of_source(&source);
    let secret_digest = AuditDigest::of_secret(&secret);
    let media_digest = AuditDigest::of_screenshot(&media);
    for digest in [&token_digest, &source_digest, &secret_digest, &media_digest] {
        assert!(
            is_hex_digest(digest.as_str()),
            "a digest is exactly 64 hex characters"
        );
        assert!(
            !digest.as_str().contains("SENTINEL"),
            "a digest never contains its input"
        );
    }
    assert_ne!(
        token_digest.as_str(),
        source_digest.as_str(),
        "digests are domain separated"
    );

    // A.4 record events carrying those digests and prove no sentinel survives
    //     into the sink, the Debug rendering, or the serialization.
    let root = temp_root("redaction");
    let sink = AuditSinkConfig {
        path: root.path().join("audit").join("audit.jsonl"),
        max_bytes: 64 * 1024,
        max_files: 4,
        failure_policy: AuditFailurePolicy::FailClosed,
    };
    let log = AuditLog::open(sink).expect("audit log opens");
    let credential_event = redaction_event(&log, &mutating, Some(&token_digest), None);
    let content_event = redaction_event(&log, &mutating, None, Some(&source_digest));
    let media_event = redaction_event(&log, &mutating, None, Some(&media_digest));
    for event in [&credential_event, &content_event, &media_event] {
        log.record(event).expect("the event records");
    }
    let sink_bytes = log.current_bytes().expect("audit bytes readable");
    let sink_text = String::from_utf8_lossy(&sink_bytes).into_owned();
    let mut renderings = vec![sink_text];
    for event in [&credential_event, &content_event, &media_event] {
        renderings.push(format!("{event:?}"));
        renderings.push(serde_json::to_string(event).expect("events serialize"));
    }
    for rendering in &renderings {
        for sentinel in [TOKEN_SENTINEL, SECRET_SENTINEL] {
            assert!(
                !rendering.contains(sentinel),
                "no audit output may contain a raw token or secret"
            );
        }
        assert!(
            !rendering.contains("SENTINEL-SOURCE"),
            "no audit output may contain raw source text"
        );
        assert!(
            !rendering.contains("SENTINEL-MEDIA"),
            "no audit output may contain raw media bytes"
        );
    }

    // A.5 an over-long field is refused rather than truncated into the sink.
    assert!(
        AuditEvent::seal(AuditEventDraft {
            tool: &"a".repeat(512),
            ..draft_template(&mutating, 1)
        })
        .is_err(),
        "an unbounded tool name cannot be sealed into an audit event"
    );

    // =====================================================================
    // B. Rate and concurrency limits block *before* dispatch
    // =====================================================================
    let rate_root = temp_root("rate");
    let rate = AuditLimitsHarness::open(
        rate_root.path(),
        AuditLimitsConfig {
            requests_per_window: 3,
            rate_window_ms: 1_000,
            ..AuditLimitsConfig::default()
        },
    )
    .expect("rate-limited harness opens");
    for index in 0..3_u64 {
        rate.attempt(AttemptSpec {
            tool: &mutating,
            now_ms: 5_000 + index,
            request_bytes: 32,
            response_bytes: 32,
        })
        .expect("attempts inside the window are admitted");
    }
    assert_eq!(rate.touches(), 3, "three admitted attempts dispatched");
    let refusal = rate
        .attempt(AttemptSpec {
            tool: &mutating,
            now_ms: 5_003,
            request_bytes: 32,
            response_bytes: 32,
        })
        .expect_err("the fourth attempt inside the window is rate limited");
    assert_eq!(refusal.code, ErrorKind::Unavailable.code());
    assert!(refusal.retryable, "a rate refusal is retryable");
    assert_eq!(refusal.http_status, 429);
    assert_eq!(
        rate.touches(),
        3,
        "a rate-limited attempt must never reach dispatch"
    );
    assert_eq!(
        rate.audit_events().expect("audit events readable").len(),
        3,
        "a pre-dispatch refusal is not recorded as an allowed mutation"
    );
    // Recovery once the window advances.
    rate.attempt(AttemptSpec {
        tool: &mutating,
        now_ms: 6_100,
        request_bytes: 32,
        response_bytes: 32,
    })
    .expect("the next window admits again");
    assert_eq!(rate.touches(), 4, "recovery reaches dispatch");

    // B.2 concurrency exhaustion also blocks before dispatch.
    let conc_root = temp_root("concurrency");
    let conc = AuditLimitsHarness::open(
        conc_root.path(),
        AuditLimitsConfig {
            subject_concurrency: 2,
            ..AuditLimitsConfig::default()
        },
    )
    .expect("concurrency harness opens");
    let subject = conc.subject();
    let enforcer = conc.gate().enforcer();
    let held = vec![
        enforcer
            .clone()
            .admit(&subject, 1_000)
            .expect("first slot admits"),
        enforcer
            .clone()
            .admit(&subject, 1_000)
            .expect("second slot admits"),
    ];
    let refusal = conc
        .attempt(AttemptSpec {
            tool: &mutating,
            now_ms: 1_000,
            request_bytes: 32,
            response_bytes: 32,
        })
        .expect_err("an attempt beyond the concurrency bound is refused");
    assert_eq!(refusal.code, ErrorKind::Unavailable.code());
    assert_eq!(refusal.http_status, 429);
    assert_eq!(conc.touches(), 0, "concurrency refusal precedes dispatch");
    drop(held);
    conc.attempt(AttemptSpec {
        tool: &mutating,
        now_ms: 1_001,
        request_bytes: 32,
        response_bytes: 32,
    })
    .expect("releasing the slots recovers");
    assert_eq!(conc.touches(), 1, "recovery reaches dispatch");

    // B.3 size and range limits are resource errors, also before dispatch.
    let size_root = temp_root("size");
    let size = AuditLimitsHarness::open(size_root.path(), AuditLimitsConfig::default())
        .expect("size harness opens");
    let policy = size.gate().enforcer().policy().clone();
    let refusal = size
        .attempt(AttemptSpec {
            tool: &mutating,
            now_ms: 1_000,
            request_bytes: policy.max_body_bytes + 1,
            response_bytes: 32,
        })
        .expect_err("an oversized request body is refused");
    assert_eq!(refusal.code, ErrorKind::InvalidInput.code());
    assert!(!refusal.retryable, "an oversized body is not retryable");
    assert_eq!(refusal.http_status, 413);
    assert_eq!(size.touches(), 0, "the oversized request never dispatched");
    let refusal = size
        .attempt(AttemptSpec {
            tool: &mutating,
            now_ms: 1_001,
            request_bytes: 32,
            response_bytes: policy.max_response_bytes + 1,
        })
        .expect_err("an oversized response is refused");
    assert_eq!(refusal.code, ErrorKind::InvalidInput.code());
    assert_eq!(refusal.http_status, 413);
    assert_eq!(size.touches(), 0, "the oversized response never dispatched");

    let enforcer = size.gate().enforcer();
    let rejection = enforcer
        .check_range_bytes(policy.max_range_bytes + 1)
        .expect_err("an oversized range is refused");
    assert_eq!(rejection.error_kind(), ErrorKind::InvalidInput);
    assert_eq!(rejection.http_status(), 416);
    enforcer
        .check_range_bytes(policy.max_range_bytes)
        .expect("a range at the bound is accepted");

    // B.4 artifact retention and total-size limits.
    let rejection = enforcer
        .check_retention(1_000, 1_000 + policy.artifact_retention_ms + 1)
        .expect_err("an artifact beyond retention is gone");
    assert_eq!(rejection.error_kind(), ErrorKind::NotFound);
    assert_eq!(rejection.http_status(), 404);
    enforcer
        .check_retention(1_000, 1_000 + policy.artifact_retention_ms)
        .expect("an artifact inside retention is readable");

    let store = Arc::new(
        godot_mcp_server::limits::LimitEnforcer::new(LimitPolicy {
            max_artifact_total_bytes: 1_024,
            ..LimitPolicy::default()
        })
        .expect("store enforcer builds"),
    );
    let reservation = store
        .clone()
        .admit_artifact(1_024)
        .expect("an artifact at the store bound is accepted");
    let rejection = store
        .clone()
        .admit_artifact(1)
        .expect_err("an artifact beyond the store bound is refused");
    assert_eq!(rejection.error_kind(), ErrorKind::Unavailable);
    assert!(rejection.is_retryable());
    assert_eq!(rejection.http_status(), 507);
    drop(reservation);
    store
        .clone()
        .admit_artifact(1)
        .expect("releasing the reservation recovers the store budget");

    // B.5 a zero limit is a configuration error, never an unbounded default.
    assert!(
        godot_mcp_server::limits::LimitEnforcer::new(LimitPolicy {
            max_body_bytes: 0,
            ..LimitPolicy::default()
        })
        .is_err(),
        "a zero limit must be refused rather than treated as unlimited"
    );

    // =====================================================================
    // C. Audit failure never silently permits a high-risk mutation
    // =====================================================================
    let closed_root = temp_root("fail-closed");
    let closed = AuditLimitsHarness::open(
        closed_root.path(),
        AuditLimitsConfig {
            fail_closed: true,
            ..AuditLimitsConfig::default()
        },
    )
    .expect("fail-closed harness opens");
    closed.audit().set_write_failure(true);
    let refusal = closed
        .attempt(AttemptSpec {
            tool: &mutating,
            now_ms: 1_000,
            request_bytes: 32,
            response_bytes: 32,
        })
        .expect_err("a fail-closed audit failure refuses the mutation");
    assert_eq!(refusal.code, ErrorKind::Unavailable.code());
    assert!(refusal.retryable, "the operator can retry after repair");
    assert!(
        !refusal.audit_recorded,
        "the refusal reports that nothing was recorded"
    );
    assert_eq!(
        closed.touches(),
        0,
        "a high-risk mutation must never dispatch when its audit failed"
    );
    // A read is not a high-risk mutation, so it proceeds while reporting the gap.
    let report = closed
        .attempt(AttemptSpec {
            tool: &reading,
            now_ms: 1_001,
            request_bytes: 32,
            response_bytes: 32,
        })
        .expect("an ordinary read still proceeds under a fail-closed policy");
    assert!(report.dispatched);
    assert!(
        !report.audit_recorded,
        "the read reports the unrecorded audit rather than claiming success"
    );
    closed.audit().set_write_failure(false);
    let report = closed
        .attempt(AttemptSpec {
            tool: &mutating,
            now_ms: 1_002,
            request_bytes: 32,
            response_bytes: 32,
        })
        .expect("repairing the sink restores high-risk mutation");
    assert!(report.audit_recorded);

    // An explicitly configured fail-open policy permits, but never silently: the
    // report still says the event was not recorded.
    let open_root = temp_root("fail-open");
    let opened = AuditLimitsHarness::open(
        open_root.path(),
        AuditLimitsConfig {
            fail_closed: false,
            ..AuditLimitsConfig::default()
        },
    )
    .expect("fail-open harness opens");
    opened.audit().set_write_failure(true);
    let report = opened
        .attempt(AttemptSpec {
            tool: &mutating,
            now_ms: 1_000,
            request_bytes: 32,
            response_bytes: 32,
        })
        .expect("a configured fail-open policy permits the mutation");
    assert!(report.dispatched);
    assert!(
        !report.audit_recorded,
        "fail-open still reports the missing record"
    );

    // =====================================================================
    // D. Structural absence proof: complete registry/schema enumeration
    // =====================================================================
    let surface = audit_execution_surface();
    assert_eq!(
        surface.profiles_audited, 4,
        "all four canonical profiles are enumerated"
    );
    assert!(
        !surface.reserved_is_callable,
        "the reserved identifier is not callable"
    );
    assert!(
        surface.execution_entries.is_empty(),
        "no manifest entry denotes arbitrary execution: {:?}",
        surface.execution_entries
    );

    let mut union: BTreeSet<String> = BTreeSet::new();
    let mut per_profile: BTreeMap<CapabilityProfile, BTreeSet<String>> = BTreeMap::new();
    let mut enumerated_tools = 0_usize;
    let mut enumerated_resources = 0_usize;
    for profile in PROFILES {
        let manifest = canonical_manifest(profile);
        for tool in &manifest.tools {
            enumerated_tools += 1;
            assert_ne!(
                tool.as_str(),
                RESERVED_UNSAFE_SCRIPT_CAPABILITY,
                "no profile may advertise the reserved identifier"
            );
            for shape in EXECUTION_SHAPES {
                assert_ne!(
                    tool.as_str(),
                    *shape,
                    "no profile may advertise an arbitrary-execution tool"
                );
            }
        }
        for resource in &manifest.resources {
            enumerated_resources += 1;
            for shape in EXECUTION_SHAPES {
                assert!(
                    !resource.contains(shape),
                    "no profile may advertise an arbitrary-execution resource"
                );
            }
        }
        if profile != CapabilityProfile::Full {
            union.extend(manifest.tools.iter().cloned());
        }
        per_profile.insert(profile, manifest.tools.clone());
    }
    assert_eq!(
        surface.tools_audited, enumerated_tools,
        "the production audit walks exactly the tools this test walks"
    );
    assert_eq!(
        surface.resources_audited, enumerated_resources,
        "the production audit walks exactly the resources this test walks"
    );
    for (profile, expected) in PROFILE_TOOL_COUNTS {
        assert_eq!(
            per_profile[&profile].len(),
            expected,
            "profile {profile:?} must advertise exactly {expected} tools"
        );
    }
    assert_eq!(
        per_profile[&CapabilityProfile::Full],
        union,
        "full is exactly the union of core, editor and runtime"
    );

    // D.2 the frozen capability enum has exactly five non-callable members and
    //     none of them is the reserved identifier.
    let capability_names: BTreeSet<&'static str> = [
        Capability::Read,
        Capability::ProjectWrite,
        Capability::EditorMutate,
        Capability::RuntimeExecute,
        Capability::ExportDestructive,
    ]
    .into_iter()
    .map(godot_mcp_server::audit::capability_audit_name)
    .collect();
    assert_eq!(
        capability_names.len(),
        5,
        "the capability enum is frozen at five"
    );
    assert!(
        !capability_names.contains(RESERVED_UNSAFE_SCRIPT_CAPABILITY),
        "the reserved identifier is not a capability variant"
    );

    // =====================================================================
    // E. Structural absence proof: registry refuses to bind a handler
    // =====================================================================
    let reg_root = temp_root("registry");
    let registry = AuditLimitsHarness::open(reg_root.path(), AuditLimitsConfig::default())
        .expect("registry harness opens");
    for name in std::iter::once(RESERVED_UNSAFE_SCRIPT_CAPABILITY).chain(
        EXECUTION_SHAPES
            .iter()
            .copied()
            .filter(|shape| *shape != RESERVED_UNSAFE_SCRIPT_CAPABILITY),
    ) {
        let code = registry
            .register_reserved_handler(name)
            .expect("registering an execution handler must be refused");
        assert_eq!(
            code,
            ErrorKind::NotFound.code(),
            "the kernel registry refuses `{name}` because no manifest contains it"
        );
    }
    assert_eq!(
        registry.touches(),
        0,
        "a refused registration never dispatches"
    );

    // =====================================================================
    // F. Structural absence proof: a real negative MCP call
    // =====================================================================
    for name in std::iter::once(RESERVED_UNSAFE_SCRIPT_CAPABILITY).chain(
        EXECUTION_SHAPES
            .iter()
            .copied()
            .filter(|shape| *shape != RESERVED_UNSAFE_SCRIPT_CAPABILITY),
    ) {
        let report = registry.negative_tool_call(name).await;
        assert_eq!(
            report.code,
            ErrorKind::NotFound.code(),
            "calling `{name}` through the kernel returns a stable not_found"
        );
        assert!(!report.dispatched, "calling `{name}` dispatched nothing");
    }
    assert_eq!(
        registry.touches(),
        0,
        "no negative call reached a downstream handler"
    );

    // F.2 the same negative call over a live MCP client/server pair.
    let dispatches = Arc::new(AtomicUsize::new(0));
    let mut kernel = ServiceKernel::new(CapabilityProfile::Full);
    kernel
        .register_tool_handler(
            godot_mcp_domain::catalog::descriptor(&mutating)
                .and_then(|tool| tool.handler)
                .expect("a catalog tool"),
            Arc::new(CountingHandler(Arc::clone(&dispatches))),
        )
        .expect("the mutating tool registers");
    let (server_transport, client_transport) = tokio::io::duplex(16 * 1024);
    let adapter = RmcpAdapter::new(Arc::new(kernel), fixture_access());
    let server_task = tokio::spawn(async move {
        let running = adapter.serve(server_transport).await?;
        running.waiting().await?;
        Ok::<(), anyhow::Error>(())
    });
    let client = ().serve(client_transport).await.expect("in-memory MCP client connects");
    let advertised: BTreeSet<String> = client
        .list_all_tools()
        .await
        .expect("tools/list succeeds")
        .into_iter()
        .map(|tool| tool.name.to_string())
        .collect();
    assert!(
        !advertised.contains(RESERVED_UNSAFE_SCRIPT_CAPABILITY),
        "the advertised MCP tool list omits the reserved identifier"
    );
    for shape in EXECUTION_SHAPES {
        assert!(
            !advertised.contains(*shape),
            "the advertised MCP tool list omits `{shape}`"
        );
    }
    let error = client
        .call_tool(CallToolRequestParams::new(
            RESERVED_UNSAFE_SCRIPT_CAPABILITY,
        ))
        .await
        .expect_err("calling the reserved identifier over MCP fails");
    let rendered = error.to_string();
    assert!(
        !rendered.contains("panic") && !rendered.is_empty(),
        "the MCP refusal is a stable protocol error"
    );
    assert_eq!(
        dispatches.load(Ordering::Relaxed),
        0,
        "the negative MCP call dispatched nothing"
    );
    client.cancel().await.expect("client shuts down");
    server_task
        .await
        .expect("server task joins")
        .expect("server exits cleanly");

    // =====================================================================
    // G. Structural absence proof: complete config-surface enumeration
    // =====================================================================
    let probe = config_surface_probe();
    assert!(
        probe.accepted.is_empty(),
        "no configuration key may enable arbitrary execution: {:?}",
        probe.accepted
    );
    assert!(
        probe.rejected >= 8,
        "the probe must cover every candidate switch, covered {}",
        probe.rejected
    );
    assert!(
        probe.sections_probed >= 4,
        "the probe must cover the top level and every nested section"
    );

    // =====================================================================
    // H. Supplementary binary/string scan, proven non-vacuous first
    // =====================================================================
    let planted = format!("prefix{RESERVED_UNSAFE_SCRIPT_CAPABILITY}suffix").into_bytes();
    assert_eq!(
        scan_bytes_for_reserved_identifier(&planted),
        1,
        "the scanner must find a planted occurrence, or its zero results mean nothing"
    );
    assert_eq!(
        scan_bytes_for_reserved_identifier(b"no execution surface here"),
        0,
        "the scanner reports zero for clean bytes"
    );
    for artifact in release_artifacts() {
        let bytes = std::fs::read(&artifact).expect("release artifact is readable");
        assert_eq!(
            scan_bytes_for_reserved_identifier(&bytes),
            0,
            "the release artifact contains no reserved identifier string"
        );
    }
}

/// Build one audit event carrying only digests, for the redaction assertions.
fn redaction_event(
    log: &AuditLog,
    tool: &str,
    credential: Option<&AuditDigest>,
    content: Option<&AuditDigest>,
) -> AuditEvent {
    let mut draft = draft_template(tool, log.next_sequence());
    draft.credential_digest = credential.cloned();
    draft.content_digest = content.cloned();
    AuditEvent::seal(draft).expect("the digest-only event seals")
}

/// A complete draft with every dimension populated by typed, content-free values.
fn draft_template(tool: &str, sequence: u64) -> AuditEventDraft<'_> {
    AuditEventDraft {
        sequence,
        recorded_at_unix_ms: 1_700_000_000_000,
        transport: AuditTransport::Stdio,
        profile: CapabilityProfile::Full,
        tool,
        principal_digest: AuditDigest::of_str("principal", "principal_0123456789abcdef"),
        session_digest: Some(AuditDigest::of_str("session", "session_0123456789abcdef")),
        credential_digest: None,
        project_digest: AuditDigest::of_str("project", "object"),
        project_generation: 7,
        capability: Capability::ProjectWrite,
        verdict: PolicyVerdict::Deny,
        reason: PolicyReason::MissingCapability,
        decision_id: TEMPLATE_DECISION_ID,
        queue: AuditQueueFacts {
            lane: AuditLane::Mutation,
            priority: AuditPriority::Interactive,
            queued_depth: 0,
            admitted: false,
        },
        boundary: Some(AuditBoundary::File),
        phase: Some(AuditPhase::Quarantined),
        content_digest: None,
        status: AuditStatus::Refused,
        error_kind: Some(ErrorKind::PermissionDenied),
        error_retryable: false,
        correlation: CorrelationId::new("corr_task31").expect("correlation id"),
        request: RequestId::new("req_task31").expect("request id"),
    }
}

/// Release-shaped artifacts to scan, when a locked release build has produced
/// them. The scan is supplementary, so its absence is not a failure.
fn release_artifacts() -> Vec<PathBuf> {
    let release = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(|root| root.join("target").join("release"));
    let Some(release) = release else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&release) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("godot-mcp") && !name.ends_with(".d"))
        })
        .collect()
}

/// Silence the unused-import warning for the cancellation token, which the
/// harness constructs on the test's behalf.
#[allow(dead_code)]
fn _unused_cancellation() -> CancellationToken {
    CancellationToken::new()
}

/// Silence the unused-import warning for the progress reporter.
#[allow(dead_code)]
fn _unused_progress() -> ProgressReporter {
    ProgressReporter::noop()
}

/// Silence the unused-import warning for the rejection alias.
#[allow(dead_code)]
fn _unused_rejection(rejection: &LimitRejection) -> u16 {
    rejection.http_status()
}
