//! Round 9 B4 regressions for shared admission, artifact ownership, and endpoint honesty.

use std::collections::BTreeSet;
use std::sync::Arc;

use godot_mcp_domain::CapabilityProfile;
use godot_mcp_domain::config::Limits;
use godot_mcp_domain::contracts::{
    ArtifactContract, ArtifactId, ArtifactKind, ArtifactMetadata, ArtifactOwner,
    ArtifactResourceUri, PrincipalId, ProjectIdentity, RangeSemantics,
};
use godot_mcp_domain::objid::{LeaseKey, StableObjectId};
use godot_mcp_server::audit::{AuditFailurePolicy, AuditLog, AuditSinkConfig};
use godot_mcp_server::binding::{BindingRegistry, SessionBindingPolicy, binding_refusal};
use godot_mcp_server::idempotency::{ModePrincipal, TransportAuthMode};
use godot_mcp_server::kernel::{AccessContext, ProgressReporter, ServiceKernel};
use godot_mcp_server::limits::{LimitEnforcer, LimitPolicy};
use serde_json::Map;
use tokio_util::sync::CancellationToken;

const NOW: u64 = 4_000;
const EXPIRY: u64 = 9_000;

fn project(generation: u64) -> ProjectIdentity {
    ProjectIdentity::from_lease_key(
        LeaseKey {
            root: StableObjectId::Unix { dev: 39, ino: 1 },
            project_file: StableObjectId::Unix { dev: 39, ino: 2 },
        },
        generation,
    )
}

fn bootstrap_access() -> AccessContext {
    AccessContext::new(
        project(4),
        PrincipalId::parse("principal_0123456789abcdef").expect("principal"),
        None,
        NOW,
    )
}

fn oidc(subject: &str) -> ModePrincipal {
    ModePrincipal::Oidc {
        issuer: "https://issuer.example".to_owned(),
        subject: subject.to_owned(),
    }
}

fn policy() -> SessionBindingPolicy {
    SessionBindingPolicy::new(
        CapabilityProfile::Full,
        "https://mcp.example.com/mcp",
        project(4),
        &Limits::default(),
    )
    .expect("policy")
}

fn artifact(letter: char, owner: ArtifactOwner) -> ArtifactContract {
    let id = ArtifactId::parse(format!("artifact_{}", letter.to_string().repeat(64)))
        .expect("artifact id");
    ArtifactContract::new(
        id.clone(),
        ArtifactResourceUri::for_artifact(&id),
        project(4),
        Some(owner),
        ArtifactMetadata {
            kind: ArtifactKind::Report,
            media_type: "application/octet-stream".to_owned(),
            byte_length: 1,
            sha256: "0".repeat(64),
            created_at_unix_ms: 1_000,
        },
        EXPIRY,
        RangeSemantics::new(1, 1, true).expect("range semantics"),
    )
    .expect("artifact contract")
}

#[test]
fn transport_mode_is_part_of_the_canonical_artifact_principal() {
    // Given the same opaque spelling in two different authentication modes.
    let stdio = bootstrap_access().with_transport_identity(
        TransportAuthMode::Stdio,
        ModePrincipal::LocalOs {
            principal: "same-principal".to_owned(),
        },
        "same-session",
    );
    let trusted_local = bootstrap_access().with_transport_identity(
        TransportAuthMode::TrustedLocal,
        ModePrincipal::LaunchSecret {
            principal: "same-principal".to_owned(),
        },
        "same-session",
    );

    // When canonical owner identities are derived, then the mode keeps them distinct.
    assert_ne!(stdio.principal(), trusted_local.principal());
    assert_ne!(stdio.session(), trusted_local.session());
}

#[test]
fn seeded_artifact_is_bound_only_to_its_mode_specific_principal() {
    // Given one principal-owned artifact and two authenticated subjects in the same project.
    let policy = policy();
    let owner = policy
        .admit(
            TransportAuthMode::ProxiedOidc,
            oidc("owner"),
            BTreeSet::new(),
        )
        .expect("owner subject");
    let stranger = policy
        .admit(
            TransportAuthMode::ProxiedOidc,
            oidc("stranger"),
            BTreeSet::new(),
        )
        .expect("stranger subject");
    let owner_access = bootstrap_access().with_transport_identity(
        owner.mode(),
        owner.principal().clone(),
        "owner-session",
    );
    let contract = artifact(
        'a',
        ArtifactOwner {
            principal: Some(owner_access.principal().clone()),
            session: None,
        },
    );
    let uri = contract.resource_uri().as_str().to_owned();
    let registry = BindingRegistry::new(policy).with_artifacts(vec![contract]);
    let owner_session = registry
        .bind_session("owner-internal", &owner, EXPIRY)
        .expect("owner session");
    let stranger_session = registry
        .bind_session("stranger-internal", &stranger, EXPIRY)
        .expect("stranger session");

    // When both sessions request the artifact, then only its owner resolves it.
    registry
        .authorize_artifact_read(
            owner_session.handle().as_str(),
            Some(&owner),
            NOW,
            &uri,
            None,
        )
        .expect("owner read");
    let stolen = registry
        .authorize_artifact_read(
            stranger_session.handle().as_str(),
            Some(&stranger),
            NOW,
            &uri,
            None,
        )
        .expect_err("stranger read");
    assert_eq!(stolen.as_bytes(), binding_refusal().as_bytes());
}

#[tokio::test]
async fn every_operational_entry_point_uses_the_shared_admission_and_audit() {
    // Given a kernel whose process budget allows exactly four operations.
    let root = tempfile::tempdir().expect("temp root");
    let audit = Arc::new(
        AuditLog::open(AuditSinkConfig {
            path: root.path().join("audit.jsonl"),
            max_bytes: 64 * 1024,
            max_files: 2,
            failure_policy: AuditFailurePolicy::FailClosed,
        })
        .expect("audit"),
    );
    let limits = Arc::new(
        LimitEnforcer::new(LimitPolicy {
            max_process_requests: 4,
            ..LimitPolicy::default()
        })
        .expect("limits"),
    );
    let access = bootstrap_access();
    let contract = artifact(
        'b',
        ArtifactOwner {
            principal: Some(access.principal().clone()),
            session: access.session().cloned(),
        },
    );
    let uri = contract.resource_uri().as_str().to_owned();
    let mut kernel = ServiceKernel::new(CapabilityProfile::Full)
        .with_operational_controls(limits, Arc::clone(&audit));

    // When registration, tool dispatch, resource read, and artifact read each enter once.
    kernel
        .register_artifact(contract, vec![0], &access)
        .expect("register artifact");
    kernel
        .call_tool(
            "capabilities.get",
            Map::new(),
            access.clone(),
            CancellationToken::new(),
            ProgressReporter::noop(),
        )
        .await
        .expect("call tool");
    kernel
        .read_resource("godot-mcp://resources/project", None, &access)
        .expect("read resource");
    kernel
        .read_resource(&uri, None, &access)
        .expect("read artifact");

    // Then a fifth operation is refused and all five outcomes are audited.
    kernel
        .read_resource("godot-mcp://resources/project", None, &access)
        .expect_err("process request bound");
    let rows = String::from_utf8(audit.current_bytes().expect("audit bytes")).expect("UTF-8");
    assert_eq!(rows.lines().count(), 5);
}

#[test]
fn operational_entry_points_have_one_admission_implementation() {
    // Given the production kernel source.
    let source = include_str!("../src/kernel.rs");
    let production = source.split("#[cfg(test)]").next().unwrap_or(source);

    // When its operational entry points are counted, then all delegate to one seam.
    assert_eq!(production.matches("begin_operational_request(").count(), 5);
    assert_eq!(production.matches("admit_request(").count(), 2);
}
