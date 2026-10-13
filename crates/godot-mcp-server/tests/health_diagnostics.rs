//! Todo 33 acceptance: health, readiness, bounded metrics and self-diagnostics.
//!
//! ## What these two cases own
//!
//! The happy case proves the property the plan states first: a **core** server is
//! `ready` while the editor plane is absent, and the absence is *actionable* — it
//! carries a remediation hint — without ever being counted as a server failure.
//! It also pins the readiness state machine across the core-only / editor /
//! runtime / HTTP profiles, the bounded-metric ceiling, and the fact that the
//! diagnostic bundle is structurally incapable of carrying a secret, a source
//! line or an absolute private path.
//!
//! The failure case drives the five named blocked/degraded conditions —
//! unresolved launch survivor, quarantined transaction, pending autoload
//! restoration, wrong Godot / plugin protocol, JWKS outage — and asserts that
//! each is reported with a precise machine-readable code, that mutation
//! readiness stays `false` until survivor *and* temporary-settings
//! reconciliation complete, and that nothing sensitive reaches an unauthorized
//! HTTP audience.
//!
//! ## Declared substitution: no live editor or game is attached here
//!
//! These cases drive the *same* diagnostic inputs the real collector receives
//! through deterministic injection, and assert the invariants that do not
//! depend on a live peer: presence/absence classification, freshness staleness
//! thresholds, honest remediation text, and the "optional absence is never a
//! server failure" rule. The production case runs the full assembly with its
//! editor bridge and managed sessions attached but idle. Behaviour under a real
//! connected editor add-on or a real running game is exercised by the
//! real-client smokes (`scripts/real_client/m4_smoke.py`, `m5_smoke.py`).

#[cfg(unix)]
use std::path::Path;

use godot_mcp_domain::CapabilityProfile;
use godot_mcp_domain::contracts::{ComponentName, HealthStatus, ProjectIdentity, SchedulerMode};
use godot_mcp_domain::objid::{LeaseKey, StableObjectId};
use godot_mcp_server::audit::AuditDigest;
use godot_mcp_server::diagnostics::{
    AutoloadRecovery, BundleAudience, DiagnosticBundle, DiagnosticCode, DiagnosticSeverity,
    DigestSample, EngineCompatibility, EngineFacts, EngineVersion, Freshness, HealthInputs,
    HttpReadiness, IndexFacts, JournalFacts, JwksState, LeaseOwnership, LocalSecretState,
    MetricDimension, MetricName, MetricsRegistry, PluginProtocol, ProjectFacts, QueueFacts,
    ReadinessSurface, RuntimeFacts, ServerHealth, SettingsReconciliation, TransportSurface,
    VersionFacts,
};
#[cfg(unix)]
use godot_mcp_server::idempotency::OperationToolFamily;
use godot_mcp_server::observability::SafeField;
use godot_mcp_server::router::{
    EnginePresence, PlaneObservation, ProtocolFidelity, RouterHealth, RuntimePresence,
    WorkspaceFreshness,
};
#[cfg(unix)]
use godot_mcp_server::scheduler::{AdmissionRequest, Lane, priority_for};
use godot_mcp_server::scheduler::{EffectPlane, JournalBinding, PlaneAvailability};
#[cfg(unix)]
use godot_mcp_server::test_support::OutcomeRouterHarness;
#[cfg(unix)]
use serde_json::Value;
#[cfg(unix)]
use tokio_util::sync::CancellationToken;

/// The secret-shaped material that must never reach any rendered diagnostic.
const SECRET: &str = "todo33-local-secret-must-not-leak";
/// The source text that must never reach any rendered diagnostic.
const SOURCE: &str = "extends Node\nvar api_key = 'todo33-source-must-not-leak'";
/// The absolute private path that must never reach any rendered diagnostic.
const ABSOLUTE_PATH: &str = "/home/todo33-operator/private/projects/game/project.godot";

// -- fixtures ---------------------------------------------------------------

fn project_identity() -> ProjectIdentity {
    ProjectIdentity::from_lease_key(
        LeaseKey {
            root: StableObjectId::Unix { dev: 33, ino: 330 },
            project_file: StableObjectId::Unix { dev: 33, ino: 331 },
        },
        7,
    )
}

/// A plane observation with only the file boundary compiled in, which is exactly
/// what a core stdio server has.
fn core_observation() -> PlaneObservation {
    PlaneObservation::new(
        PlaneAvailability::new(true, false, false, false, false),
        EnginePresence::Absent,
        RuntimePresence::NoInstance,
        ProtocolFidelity::Full,
        WorkspaceFreshness::Fresh,
    )
}

/// Everything a runtime-capable deployment observes when it is fully healthy.
fn runtime_observation() -> PlaneObservation {
    PlaneObservation::new(
        PlaneAvailability::new(true, true, true, false, true),
        EnginePresence::EditorConnected,
        RuntimePresence::GameRunning,
        ProtocolFidelity::Full,
        WorkspaceFreshness::Fresh,
    )
}

fn healthy_project() -> ProjectFacts {
    ProjectFacts {
        identity: AuditDigest::of_str("project", ABSOLUTE_PATH),
        lease: LeaseOwnership::HeldHere {
            owner: AuditDigest::of_str("lease-owner", ABSOLUTE_PATH),
        },
        safe_root_generation: 7,
    }
}

fn healthy_queue() -> QueueFacts {
    QueueFacts {
        mode: SchedulerMode::Accepting,
        running: 0,
        queued: 0,
        concurrency_limit: 4,
        queue_limit: 64,
        journal: JournalBinding {
            idempotency: true,
            store: true,
            survivors: true,
        },
    }
}

fn healthy_journal() -> JournalFacts {
    JournalFacts {
        quarantined: DigestSample::empty(),
        unresolved_survivors: DigestSample::empty(),
        settings: SettingsReconciliation::Reconciled,
        block_readable: true,
    }
}

fn absent_runtime() -> RuntimeFacts {
    RuntimeFacts {
        presence: RuntimePresence::NoInstance,
        heartbeat: Freshness::Absent,
        autoload: AutoloadRecovery::NotRequired,
        rendezvous: Freshness::Absent,
    }
}

fn live_runtime() -> RuntimeFacts {
    RuntimeFacts {
        presence: RuntimePresence::GameRunning,
        heartbeat: Freshness::Fresh { age_ms: 120 },
        autoload: AutoloadRecovery::Restored,
        rendezvous: Freshness::Fresh { age_ms: 40 },
    }
}

fn matched_versions() -> VersionFacts {
    VersionFacts {
        contract_matched: true,
        engine: EngineCompatibility::Supported,
        plugin: PluginProtocol::NotShipped,
    }
}

fn healthy_engine() -> EngineFacts {
    EngineFacts {
        presence: EnginePresence::EditorConnected,
        binary: Some(AuditDigest::of_str("godot-binary", ABSOLUTE_PATH)),
        version: Some(EngineVersion::new(4, 3, 0)),
        capabilities_probed: true,
    }
}

// -- happy path -------------------------------------------------------------

/// A core server reports `ready` with the editor absent and a remediation hint,
/// the readiness state machine agrees with each profile, metrics stay bounded,
/// and the bundle cannot carry sensitive material.
#[test]
fn health_diagnostics_happy_path() {
    let planes = RouterHealth::observe(&core_observation(), None, 1_700_000_000_000);
    let health = ServerHealth::assemble(HealthInputs {
        profile: CapabilityProfile::Core,
        transport: TransportSurface::Stdio,
        project: healthy_project(),
        index: IndexFacts::Fresh { generation: 12 },
        engine: EngineFacts {
            presence: EnginePresence::Absent,
            binary: None,
            version: None,
            capabilities_probed: false,
        },
        planes: &planes,
        queue: healthy_queue(),
        journal: healthy_journal(),
        runtime: absent_runtime(),
        http: HttpReadiness::Disabled,
        versions: matched_versions(),
        observed_at_unix_ms: 1_700_000_000_000,
    });

    // -- a core server is ready, and the optional planes do not change that ---
    assert_eq!(health.status(), HealthStatus::Ready);
    assert!(health.readiness().of(ReadinessSurface::Serve));
    assert!(health.readiness().of(ReadinessSurface::Read));
    assert!(
        health.readiness().of(ReadinessSurface::Mutate),
        "a reconciled core server may mutate project files"
    );
    assert!(!health.readiness().of(ReadinessSurface::Editor));
    assert!(!health.readiness().of(ReadinessSurface::Runtime));
    assert!(!health.readiness().of(ReadinessSurface::Http));

    // -- the editor is absent, reported, and actionable ----------------------
    let editor = health
        .component(ComponentName::Editor)
        .expect("the editor component is reported, never omitted");
    assert_eq!(editor.status(), HealthStatus::Unavailable);
    assert!(
        editor.is_optional(),
        "an absent editor is optional, never a server failure"
    );
    let hint = editor
        .remediation()
        .expect("an actionable remediation hint");
    assert!(
        !hint.is_empty(),
        "the absent editor plane must carry a remediation hint"
    );
    // The hint must be honest: this release ships no editor plugin, so it must
    // not imply one exists and merely needs starting.
    for dishonest in [
        "install the plugin",
        "enable the plugin",
        "start the plugin",
    ] {
        assert!(
            !hint.to_ascii_lowercase().contains(dishonest),
            "the hint must not imply a plugin that this release does not ship: {hint}"
        );
    }
    assert!(
        health.has(DiagnosticCode::EditorPlaneNotShipped),
        "the absent editor plane has a stable machine-readable code"
    );
    assert_eq!(
        health
            .finding(DiagnosticCode::EditorPlaneNotShipped)
            .map(|finding| finding.severity()),
        Some(DiagnosticSeverity::Info),
        "an optional absence can never exceed informational severity"
    );
    assert!(health.has(DiagnosticCode::RuntimePlaneNotShipped));
    assert!(health.has(DiagnosticCode::LspDapPlaneNotShipped));
    assert!(health.has(DiagnosticCode::GodotBinaryAbsent));
    for code in health.findings().iter().map(|finding| finding.code()) {
        assert_eq!(
            health
                .finding(code)
                .map(|finding| finding.severity())
                .expect("each reported code is retrievable"),
            DiagnosticSeverity::Info,
            "a healthy core server reports only informational findings: {}",
            code.as_str()
        );
    }

    // -- the plane facts come from the router's own health, not a second copy -
    assert_eq!(health.usable_planes(), vec![EffectPlane::File]);
    assert_eq!(health.safe_root_generation(), 7);
    assert!(matches!(
        health.lease_owner(),
        LeaseOwnership::HeldHere { .. }
    ));
    assert_eq!(health.settings(), SettingsReconciliation::Reconciled);
    assert_eq!(health.rendezvous(), Freshness::Absent);
    assert!(matches!(health.http(), HttpReadiness::Disabled));

    // -- the public contract projection stays a valid domain report ----------
    let report = health.to_report(project_identity());
    let rendered = serde_json::to_string(&report).expect("the domain report serializes");
    assert!(rendered.contains("\"editor\""));
    assert!(rendered.contains("\"unavailable\""));

    // -- readiness state machine across the four profiles -------------------
    let live = RouterHealth::observe(&runtime_observation(), None, 1_700_000_000_000);
    let editor_profile = ServerHealth::assemble(HealthInputs {
        profile: CapabilityProfile::Editor,
        transport: TransportSurface::Stdio,
        project: healthy_project(),
        index: IndexFacts::Fresh { generation: 12 },
        engine: healthy_engine(),
        planes: &live,
        queue: healthy_queue(),
        journal: healthy_journal(),
        runtime: absent_runtime(),
        http: HttpReadiness::Disabled,
        versions: matched_versions(),
        observed_at_unix_ms: 1_700_000_000_000,
    });
    // The editor profile advertises editor tools, but this release ships no
    // editor boundary, so editor readiness is false and the server is degraded
    // rather than unavailable — reads and file mutations still work.
    assert!(!editor_profile.readiness().of(ReadinessSurface::Editor));
    assert!(editor_profile.readiness().of(ReadinessSurface::Read));
    assert!(editor_profile.readiness().of(ReadinessSurface::Mutate));
    assert_eq!(editor_profile.status(), HealthStatus::Degraded);
    assert!(editor_profile.has(DiagnosticCode::EditorPlaneNotShipped));

    let runtime_profile = ServerHealth::assemble(HealthInputs {
        profile: CapabilityProfile::Runtime,
        transport: TransportSurface::Stdio,
        project: healthy_project(),
        index: IndexFacts::Fresh { generation: 12 },
        engine: healthy_engine(),
        planes: &live,
        queue: healthy_queue(),
        journal: healthy_journal(),
        runtime: live_runtime(),
        http: HttpReadiness::Disabled,
        versions: matched_versions(),
        observed_at_unix_ms: 1_700_000_000_000,
    });
    assert!(
        runtime_profile.readiness().of(ReadinessSurface::Runtime),
        "a compiled-in runtime boundary with a live instance is runtime-ready"
    );
    assert!(!runtime_profile.readiness().of(ReadinessSurface::Http));
    assert_eq!(
        runtime_profile.rendezvous(),
        Freshness::Fresh { age_ms: 40 }
    );

    let http_profile = ServerHealth::assemble(HealthInputs {
        profile: CapabilityProfile::Full,
        transport: TransportSurface::Http,
        project: healthy_project(),
        index: IndexFacts::Fresh { generation: 12 },
        engine: healthy_engine(),
        planes: &live,
        queue: healthy_queue(),
        journal: healthy_journal(),
        runtime: live_runtime(),
        http: HttpReadiness::TrustedLocal {
            secret: LocalSecretState::Ready,
        },
        versions: matched_versions(),
        observed_at_unix_ms: 1_700_000_000_000,
    });
    assert!(
        http_profile.readiness().of(ReadinessSurface::Http),
        "a readable per-launch local secret makes the HTTP surface ready"
    );
    assert!(http_profile.readiness().of(ReadinessSurface::Serve));

    // -- bounded metrics ----------------------------------------------------
    let mut metrics = MetricsRegistry::new();
    for index in 0..5_000_u32 {
        // Every distinct high-cardinality tool name funnels into the bounded
        // capability dimension; an unknown tool cannot mint a new series.
        metrics.note_tool(MetricName::RequestsAdmitted, &format!("tool.{index}"));
        metrics.note_tool(MetricName::RequestsRefused, "graph.query");
    }
    for plane in EffectPlane::ALL {
        metrics.observe(
            MetricName::PlaneRefusals,
            Some(MetricDimension::Plane(plane)),
            1,
        );
    }
    for finding in health.findings() {
        metrics.observe(
            MetricName::FindingsObserved,
            Some(MetricDimension::Code(finding.code())),
            1,
        );
    }
    assert!(
        metrics.series() <= MetricsRegistry::bound(),
        "metric series ({}) must never exceed the compiled bound ({})",
        metrics.series(),
        MetricsRegistry::bound()
    );
    assert!(
        MetricsRegistry::bound() < 4_096,
        "the metric bound must stay small enough to be a real ceiling"
    );
    assert_eq!(
        metrics.value(
            MetricName::PlaneRefusals,
            Some(MetricDimension::Plane(EffectPlane::Editor))
        ),
        1
    );
    let labels = metrics.labels();
    assert_eq!(
        labels.len(),
        metrics.series(),
        "every series has exactly one bounded label"
    );

    // -- the bundle cannot carry a secret, a source line or an absolute path -
    for audience in [
        BundleAudience::LocalOperator,
        BundleAudience::RemoteAuthorized,
        BundleAudience::RemoteUnauthorized,
    ] {
        let bundle = health.bundle(audience, &metrics);
        assert_scrubbed(&bundle);
        let rendered = serde_json::to_string(&bundle).expect("the bundle serializes");
        assert!(rendered.contains("editor_plane_not_shipped"));
        assert!(rendered.contains("\"schema_version\""));
    }
}

// -- failure paths ----------------------------------------------------------

/// An unresolved survivor, a quarantined transaction, a pending autoload
/// restoration, a wrong Godot / plugin protocol and a JWKS outage each report the
/// precise blocked or degraded capability, with no sensitive value and no raw
/// artifact path.
#[test]
fn health_diagnostics_failure_paths() {
    let observation = PlaneObservation::new(
        PlaneAvailability::new(true, true, true, false, true),
        EnginePresence::EditorConnected,
        RuntimePresence::GameRunning,
        ProtocolFidelity::Degraded,
        WorkspaceFreshness::StaleIndex,
    );
    let planes = RouterHealth::observe(
        &observation,
        Some("project has quarantined transactions".to_owned()),
        1_700_000_000_500,
    );
    let secret_digest = AuditDigest::of_str("hop-secret", SECRET);
    let source_digest = AuditDigest::of_str("source", SOURCE);
    let health = ServerHealth::assemble(HealthInputs {
        profile: CapabilityProfile::Full,
        transport: TransportSurface::Http,
        project: ProjectFacts {
            identity: AuditDigest::of_str("project", ABSOLUTE_PATH),
            lease: LeaseOwnership::HeldElsewhere {
                owner: AuditDigest::of_str("lease-owner", ABSOLUTE_PATH),
            },
            safe_root_generation: 9,
        },
        index: IndexFacts::Stale {
            generation: 4,
            coherence: SafeField::new("index_behind_disk").expect("a bounded coherence code"),
        },
        engine: EngineFacts {
            presence: EnginePresence::EditorConnected,
            binary: Some(AuditDigest::of_str("godot-binary", ABSOLUTE_PATH)),
            version: Some(EngineVersion::new(3, 5, 3)),
            capabilities_probed: true,
        },
        planes: &planes,
        queue: QueueFacts {
            mode: SchedulerMode::Quarantined,
            running: 4,
            queued: 64,
            concurrency_limit: 4,
            queue_limit: 64,
            journal: JournalBinding {
                idempotency: true,
                store: true,
                survivors: false,
            },
        },
        journal: JournalFacts {
            quarantined: DigestSample::of(
                3,
                vec![
                    AuditDigest::of_str("transaction", "txn-0001"),
                    AuditDigest::of_str("transaction", "txn-0002"),
                ],
            ),
            unresolved_survivors: DigestSample::of(
                1,
                vec![AuditDigest::of_str("survivor", "pid-4242")],
            ),
            settings: SettingsReconciliation::Pending { count: 2 },
            block_readable: true,
        },
        runtime: RuntimeFacts {
            presence: RuntimePresence::GameRunning,
            heartbeat: Freshness::Stale { age_ms: 45_000 },
            autoload: AutoloadRecovery::Pending,
            rendezvous: Freshness::Stale { age_ms: 90_000 },
        },
        http: HttpReadiness::ProxiedOidc {
            jwks: JwksState::Unreachable,
        },
        versions: VersionFacts {
            contract_matched: true,
            engine: EngineCompatibility::Unsupported,
            plugin: PluginProtocol::Mismatch {
                expected: 3,
                observed: 1,
            },
        },
        observed_at_unix_ms: 1_700_000_000_500,
    });

    // -- 1. unresolved launch survivor --------------------------------------
    let survivor = health
        .finding(DiagnosticCode::UnresolvedLaunchSurvivor)
        .expect("an unresolved survivor is reported");
    assert_eq!(survivor.component(), ComponentName::Scheduler);
    assert!(DiagnosticCode::UnresolvedLaunchSurvivor.blocks_mutation());
    assert_eq!(survivor.measure(), Some(1));
    assert!(
        survivor.subject().is_some(),
        "the survivor is identified by digest, never by pid or path"
    );

    // -- 2. quarantined transaction -----------------------------------------
    let quarantine = health
        .finding(DiagnosticCode::TransactionQuarantined)
        .expect("a quarantined transaction is reported");
    assert_eq!(quarantine.measure(), Some(3));
    assert!(DiagnosticCode::TransactionQuarantined.blocks_mutation());
    assert_eq!(quarantine.severity(), DiagnosticSeverity::Degraded);

    // -- 3. pending autoload restoration ------------------------------------
    let autoload = health
        .finding(DiagnosticCode::AutoloadRestorationPending)
        .expect("a pending autoload restoration is reported");
    assert_eq!(autoload.component(), ComponentName::Runtime);
    assert!(health.has(DiagnosticCode::RuntimeHeartbeatStale));
    assert!(health.has(DiagnosticCode::RendezvousStale));
    assert_eq!(
        health
            .finding(DiagnosticCode::RendezvousStale)
            .and_then(godot_mcp_server::diagnostics::Finding::measure),
        Some(90_000)
    );

    // -- 4. wrong Godot / plugin protocol -----------------------------------
    assert!(health.has(DiagnosticCode::GodotVersionUnsupported));
    let protocol = health
        .finding(DiagnosticCode::PluginProtocolMismatch)
        .expect("a plugin protocol mismatch is reported");
    assert_eq!(protocol.measure(), Some(1), "the observed protocol version");
    assert_eq!(protocol.severity(), DiagnosticSeverity::Degraded);

    // -- 5. JWKS outage ------------------------------------------------------
    let jwks = health
        .finding(DiagnosticCode::JwksUnreachable)
        .expect("a JWKS outage is reported");
    assert_eq!(jwks.component(), ComponentName::Authorization);
    assert!(!health.readiness().of(ReadinessSurface::Http));
    assert_eq!(health.status(), HealthStatus::Unavailable);

    // -- mutation readiness stays false until reconciliation completes -------
    assert!(
        !health.readiness().of(ReadinessSurface::Mutate),
        "an unresolved survivor and pending temporary settings block mutations"
    );
    assert!(health.has(DiagnosticCode::TemporarySettingsUnreconciled));
    assert!(health.has(DiagnosticCode::ProjectLeaseForeignOwner));
    assert!(health.has(DiagnosticCode::IndexStale));
    assert!(health.has(DiagnosticCode::SchedulerJournalUnbound));
    assert!(health.has(DiagnosticCode::SchedulerQuarantined));
    assert!(health.has(DiagnosticCode::QueueSaturated));
    assert_eq!(
        health.blocked_mutation_codes(),
        vec![
            "project_lease_foreign_owner",
            "transaction_quarantined",
            "unresolved_launch_survivor",
            "temporary_settings_unreconciled",
            "scheduler_journal_unbound",
            "scheduler_quarantined",
        ],
        "every mutation block is named by a stable code, in a stable order"
    );

    // Resolving the survivor alone is not enough: temporary settings must be
    // reconciled too before mutation readiness may flip.
    let survivor_resolved = reconciled(&planes, false, true);
    assert!(
        !survivor_resolved.readiness().of(ReadinessSurface::Mutate),
        "settings reconciliation alone does not unblock mutations"
    );
    let settings_resolved = reconciled(&planes, true, false);
    assert!(
        !settings_resolved.readiness().of(ReadinessSurface::Mutate),
        "survivor resolution alone does not unblock mutations"
    );
    let both_resolved = reconciled(&planes, true, true);
    assert!(
        both_resolved.readiness().of(ReadinessSurface::Mutate),
        "with both reconciled, mutation readiness flips true"
    );

    // -- an unreadable write-block state is a refusal, never an assumed clear -
    let unreadable = ServerHealth::assemble(HealthInputs {
        profile: CapabilityProfile::Core,
        transport: TransportSurface::Stdio,
        project: healthy_project(),
        index: IndexFacts::Unreadable,
        engine: EngineFacts {
            presence: EnginePresence::Absent,
            binary: None,
            version: None,
            capabilities_probed: false,
        },
        planes: &RouterHealth::observe(&core_observation(), None, 1_700_000_000_600),
        queue: healthy_queue(),
        journal: JournalFacts {
            quarantined: DigestSample::empty(),
            unresolved_survivors: DigestSample::empty(),
            settings: SettingsReconciliation::Unknown,
            block_readable: false,
        },
        runtime: absent_runtime(),
        http: HttpReadiness::Disabled,
        versions: matched_versions(),
        observed_at_unix_ms: 1_700_000_000_600,
    });
    assert!(!unreadable.readiness().of(ReadinessSurface::Mutate));
    assert!(unreadable.has(DiagnosticCode::WriteBlockUnreadable));
    assert!(unreadable.has(DiagnosticCode::IndexUnreadable));
    assert!(unreadable.has(DiagnosticCode::TemporarySettingsUnreconciled));

    // -- an insecure local secret is reported without its value -------------
    let insecure = ServerHealth::assemble(HealthInputs {
        profile: CapabilityProfile::Core,
        transport: TransportSurface::Http,
        project: healthy_project(),
        index: IndexFacts::Fresh { generation: 3 },
        engine: EngineFacts {
            presence: EnginePresence::Absent,
            binary: None,
            version: None,
            capabilities_probed: false,
        },
        planes: &RouterHealth::observe(&core_observation(), None, 1_700_000_000_700),
        queue: healthy_queue(),
        journal: healthy_journal(),
        runtime: absent_runtime(),
        http: HttpReadiness::TrustedLocal {
            secret: LocalSecretState::InsecurePermissions { mode: 0o644 },
        },
        versions: matched_versions(),
        observed_at_unix_ms: 1_700_000_000_700,
    });
    assert!(insecure.has(DiagnosticCode::LocalSecretInsecurePermissions));
    assert!(!insecure.readiness().of(ReadinessSurface::Http));
    assert_eq!(
        insecure
            .finding(DiagnosticCode::LocalSecretInsecurePermissions)
            .and_then(godot_mcp_server::diagnostics::Finding::measure),
        Some(0o644)
    );

    // -- nothing sensitive reaches an unauthorized HTTP audience ------------
    let mut metrics = MetricsRegistry::new();
    metrics.observe(MetricName::MutationBlocks, None, 6);
    for finding in health.findings() {
        metrics.observe(
            MetricName::FindingsObserved,
            Some(MetricDimension::Severity(finding.severity())),
            1,
        );
    }
    let unauthorized = health.bundle(BundleAudience::RemoteUnauthorized, &metrics);
    assert_scrubbed(&unauthorized);
    let rendered = serde_json::to_string(&unauthorized).expect("the bundle serializes");
    assert!(rendered.contains("jwks_unreachable"));
    assert!(
        !rendered.contains(secret_digest.as_str()),
        "an unauthorized audience receives no subject digests at all"
    );
    assert!(!rendered.contains(source_digest.as_str()));
    for finding in unauthorized.findings() {
        assert!(
            finding.subject().is_none(),
            "an unauthorized audience never receives a subject digest"
        );
        assert!(
            finding.measure().is_none(),
            "an unauthorized audience never receives a numeric measure"
        );
    }

    let authorized = health.bundle(BundleAudience::RemoteAuthorized, &metrics);
    assert_scrubbed(&authorized);
    assert!(
        authorized
            .findings()
            .iter()
            .any(|finding| finding.subject().is_some()),
        "an authorized audience does receive digests, which are not secrets"
    );
}

/// Contract 0.2: discovery lists only implemented tools, so the kernel's
/// no-handler path is unreachable in a production assembly. A listed tool whose
/// required plane has no compiled-in boundary is still refused as
/// `unavailable` before its handler runs, and that refusal records the plane.
#[cfg(unix)]
#[tokio::test]
async fn production_no_handler_refusal_records_the_tool_plane() {
    let temp = tempfile::Builder::new()
        .prefix("godot-mcp-task33-plane-refusal-")
        .tempdir()
        .expect("temp project root");
    let root = temp.path().join("project");
    copy_fixture_tree(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/core/project"),
        &root,
    );
    let harness = OutcomeRouterHarness::open(&root).expect("production runtime");

    let absent = harness
        .call_tool("editor.selection.get", serde_json::json!({}))
        .await
        .expect_err("a method without a production handler is not advertised");
    assert_eq!(
        absent.to_string(),
        "durable store failed: tool was not found",
        "a handlerless method must be absent rather than reach a placeholder"
    );

    // `scene.save` must consult the editor plane, which has no boundary in this
    // build, so the listed tool is refused before any Godot launch.
    let refusal = harness
        .call_tool(
            "scene.save",
            serde_json::json!({ "target": "scenes/main.tscn", "idempotencyId": "plane-refusal-1" }),
        )
        .await
        .expect_err("a tool whose required plane is absent must be refused");
    assert!(
        refusal
            .to_string()
            .contains("the editor boundary is not available"),
        "the refusal must name the missing plane: {refusal}"
    );

    let health = harness
        .call_tool("health.get", serde_json::json!({}))
        .await
        .expect("production health.get");
    let plane_refusals = health["metrics"]
        .as_array()
        .expect("bounded metrics")
        .iter()
        .find(|sample| sample["name"] == "plane_refusals" && sample["dimension"] == "plane_editor")
        .and_then(|sample| sample["value"].as_u64());
    assert_eq!(
        plane_refusals,
        Some(1),
        "one plane refusal must expose plane_refusals/plane_editor = 1"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn production_health_uses_live_diagnostics_bindings() {
    let temp = tempfile::Builder::new()
        .prefix("godot-mcp-task33-production-")
        .tempdir()
        .expect("temp project root");
    let root = temp.path().join("project");
    copy_fixture_tree(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/core/project"),
        &root,
    );
    let harness = OutcomeRouterHarness::open(&root).expect("production runtime");
    assert!(harness.assembled_through_production_path());

    let scheduler = harness.scheduler();
    let request = AdmissionRequest::new(
        Lane::Read,
        priority_for(OperationToolFamily::StaticDiscovery),
        CancellationToken::new(),
        current_unix_ms(),
    );
    let admission = scheduler
        .admit_read_classed(&request)
        .await
        .expect("hold one live read");
    assert_eq!(scheduler.running(), 1);

    let before = current_unix_ms();
    let tool = harness
        .call_tool("health.get", serde_json::json!({}))
        .await
        .expect("production health.get");
    let after = current_unix_ms();

    let components = tool["components"].as_array().expect("components array");
    assert_eq!(
        components.len(),
        5,
        "the legacy components length is frozen"
    );
    assert_eq!(
        components
            .iter()
            .map(|component| component["component"].as_str().expect("component name"))
            .collect::<Vec<_>>(),
        vec!["project", "index", "godot", "editor", "runtime"],
        "the legacy components order is frozen"
    );
    for component in components {
        let mut keys = component
            .as_object()
            .expect("component object")
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        keys.sort_unstable();
        assert_eq!(keys, vec!["component", "message", "status"]);
    }

    assert_eq!(
        tool["index"]["state"], "fresh",
        "index fact must track the production CoreSession snapshot"
    );
    assert_eq!(tool["index"]["generation"], 0);
    assert_eq!(
        tool["queue"]["running"],
        scheduler.running(),
        "queue.running fact must track the production Scheduler count"
    );
    assert_eq!(tool["queue"]["running"], 1);
    assert_eq!(
        tool["queue"]["queued"],
        scheduler.queued(),
        "queue.queued fact must track the production Scheduler count"
    );

    let observed_at = tool["observed_at_unix_ms"]
        .as_u64()
        .expect("live observation timestamp");
    assert!(
        (before..=after).contains(&observed_at),
        "observed_at_unix_ms must come from the call-time clock"
    );
    let expected_planes = harness
        .router()
        .health(observed_at)
        .planes()
        .into_iter()
        .map(|plane| {
            serde_json::json!({
                "plane": plane.plane().as_str(),
                "boundary_compiled_in": plane.boundary_compiled_in(),
                "environment_present": plane.environment_present(),
                "usable": plane.usable(),
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(
        tool["planes"],
        Value::Array(expected_planes),
        "plane facts must be the production OutcomeRouter health"
    );

    assert_eq!(
        tool["journal"]["settings_reconciliation"], "reconciled",
        "cancelled Todo 25 leaves an empty, vacuously reconciled restoration set"
    );
    assert_eq!(tool["versions"]["engine"], "unknown");
    assert_eq!(
        tool["server_version"],
        env!("CARGO_PKG_VERSION"),
        "the health document names the release's version"
    );
    // The full profile attaches the editor bridge, managed game sessions and
    // managed editor sessions; with no add-on connected and no session
    // running, each plane is reported idle with its own remedy, never as
    // absent from the build.
    assert_eq!(tool["versions"]["plugin"]["state"], "not_connected");
    let codes = tool["codes"].as_array().expect("diagnostic codes");
    for attached in [
        "editor_plane_not_shipped",
        "runtime_plane_not_shipped",
        "lsp_dap_plane_not_shipped",
    ] {
        assert!(
            !codes.contains(&serde_json::json!(attached)),
            "{attached} must not be reported for a plane this server attaches: {codes:?}"
        );
    }
    for idle in ["editor_not_attached", "game_not_running"] {
        assert!(
            codes.contains(&serde_json::json!(idle)),
            "{idle} names the idle plane: {codes:?}"
        );
    }
    for (component, remedy) in [
        ("editor", "editor.addon.install"),
        ("runtime", "project.run.start"),
    ] {
        let entry = components
            .iter()
            .find(|entry| entry["component"] == serde_json::json!(component))
            .expect("the component is reported");
        assert_eq!(entry["status"], "unavailable", "{entry}");
        assert!(
            entry["message"]
                .as_str()
                .is_some_and(|message| message.contains(remedy) && message.contains("full profile")),
            "an idle {component} plane names its remedy: {entry}"
        );
    }
    assert_eq!(tool["engine"]["version"], Value::Null);
    assert_eq!(tool["engine"]["capabilities_probed"], false);
    assert_eq!(tool["http"]["mode"], "disabled");
    assert_eq!(tool["runtime"]["presence"], "no_instance");
    assert_eq!(tool["runtime"]["heartbeat"], "absent");
    assert_eq!(tool["runtime"]["rendezvous"], "absent");
    assert_eq!(tool["runtime"]["autoload"], "not_required");
    assert_eq!(tool["readiness"][2]["surface"], "mutate");
    assert_eq!(
        tool["readiness"][2]["ready"], true,
        "mutation readiness must stay open when no real mutation blocker exists"
    );
    assert!(
        !tool["codes"]
            .as_array()
            .expect("diagnostic codes")
            .contains(&serde_json::json!("temporary_settings_unreconciled")),
        "a vacuously reconciled settings set must not emit a mutation blocker"
    );
    assert!(
        tool["metrics"]
            .as_array()
            .expect("bounded metrics")
            .iter()
            .any(|sample| {
                sample["name"] == "requests_admitted"
                    && sample["dimension"] == "capability_read"
                    && sample["value"] == 1
            }),
        "the shared MetricsRegistry must record the production tool admission"
    );

    let resource = harness
        .read_resource("godot-mcp://resources/health")
        .expect("production health resource");
    for field in [
        "status",
        "project",
        "components",
        "coherence",
        "diagnostic_status",
        "profile",
        "transport",
        "readiness",
        "findings",
        "codes",
        "index",
        "engine",
        "planes",
        "queue",
        "journal",
        "runtime",
        "http",
        "versions",
        "lease",
        "safe_root_generation",
    ] {
        assert_eq!(
            resource.get(field),
            tool.get(field),
            "health resource and health.get disagree on {field}"
        );
    }

    harness
        .quarantine("00000000-0000-4000-8000-000000003301", "scripts/player.gd")
        .expect("quarantine a production transaction");
    harness.block_survivor("supervised process 4242 is still contained");
    let blocked = harness
        .call_tool("health.get", serde_json::json!({}))
        .await
        .expect("health with simultaneous journal blocks");
    assert_eq!(
        blocked["journal"]["quarantined"]["total"], 1,
        "quarantine fact must come from its independent scheduler read"
    );
    assert_eq!(
        blocked["journal"]["unresolved_survivors"]["total"], 1,
        "survivor fact must still be read when quarantine takes precedence in write_block"
    );
    assert_eq!(
        blocked["journal"]["block_readable"], true,
        "both independent journal reads succeeded"
    );

    drop(admission);
}

// -- shared helpers ---------------------------------------------------------

/// Re-assemble the failing deployment with the survivor and/or the temporary
/// settings reconciled, holding every other fact fixed.
fn reconciled(
    planes: &RouterHealth,
    survivor_resolved: bool,
    settings_reconciled: bool,
) -> ServerHealth {
    ServerHealth::assemble(HealthInputs {
        profile: CapabilityProfile::Core,
        transport: TransportSurface::Stdio,
        project: healthy_project(),
        index: IndexFacts::Fresh { generation: 4 },
        engine: EngineFacts {
            presence: EnginePresence::Absent,
            binary: None,
            version: None,
            capabilities_probed: false,
        },
        planes,
        queue: healthy_queue(),
        journal: JournalFacts {
            quarantined: DigestSample::empty(),
            unresolved_survivors: if survivor_resolved {
                DigestSample::empty()
            } else {
                DigestSample::of(1, vec![AuditDigest::of_str("survivor", "pid-4242")])
            },
            settings: if settings_reconciled {
                SettingsReconciliation::Reconciled
            } else {
                SettingsReconciliation::Pending { count: 2 }
            },
            block_readable: true,
        },
        runtime: absent_runtime(),
        http: HttpReadiness::Disabled,
        versions: matched_versions(),
        observed_at_unix_ms: 1_700_000_000_800,
    })
}

/// Every rendering path of the bundle — `Debug`, `Display`-of-JSON and
/// serialization — must be free of secret material, source text and absolute
/// private paths.
fn assert_scrubbed(bundle: &DiagnosticBundle) {
    let rendered = serde_json::to_string_pretty(bundle).expect("the bundle serializes");
    let debug = format!("{bundle:?}");
    for forbidden in [
        SECRET,
        SOURCE,
        "todo33-source-must-not-leak",
        ABSOLUTE_PATH,
        "/home/todo33-operator",
        "project.godot",
    ] {
        assert!(
            !rendered.contains(forbidden),
            "the serialized bundle leaked {forbidden}"
        );
        assert!(
            !debug.contains(forbidden),
            "the bundle's Debug leaked {forbidden}"
        );
    }
    // A bundle can never contain a path separator run that would indicate an
    // absolute filesystem path made it through.
    assert!(!rendered.contains("/home/"));
    assert!(!rendered.contains("C:\\"));
    assert!(!debug.contains("/home/"));
}

#[cfg(unix)]
fn current_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(unix)]
fn copy_fixture_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("create fixture destination");
    for entry in std::fs::read_dir(from).expect("read fixture directory") {
        let entry = entry.expect("fixture entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("fixture entry type").is_dir() {
            copy_fixture_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).expect("copy fixture file");
        }
    }
}
