//! Production diagnostics assembly over the live server owners.
//!
//! Only facts that cannot change after [`ProjectRuntime`](crate::stdio::ProjectRuntime)
//! opens are stored here. Index, plane, scheduler, journal and clock facts are
//! sampled for every report, so a health response cannot accidentally become a
//! startup snapshot presented as current state.

use std::path::Path;
use std::sync::{Arc, Mutex};

use godot_mcp_analysis::index::IndexSnapshot;
use godot_mcp_domain::CapabilityProfile;
use godot_mcp_domain::contracts::{OutcomeError, ProjectIdentity};
use godot_mcp_domain::manifests::canonical_manifest;
use godot_mcp_engine::discovery::{EnvironmentSnapshot, HostPlatform, discover};
use godot_mcp_engine::supervision::protocol::bundled_supervisor;
use serde_json::{Value, json};

use super::{
    AutoloadRecovery, BundleAudience, DigestSample, EngineCompatibility, EngineFacts, Freshness,
    HealthInputs, HttpReadiness, IndexFacts, JournalFacts, LeaseOwnership, MetricDimension,
    MetricName, MetricsRegistry, PluginProtocol, ProjectFacts, QueueFacts, RuntimeFacts,
    ServerHealth, SettingsReconciliation, TransportSurface, VersionFacts,
};
#[cfg(feature = "http")]
use super::{JwksState, LocalSecretState};
use crate::audit::AuditDigest;
use crate::core::{CoreSession, outcome_error};
use crate::idempotency::family_of;
use crate::observability::SafeField;
use crate::router::{EnginePresence, OutcomeRouter, PlaneHealth, RuntimePresence};
use crate::scheduler::{EffectPlane, Scheduler, SchedulerLimits, plane_of};

#[cfg(feature = "http")]
#[derive(Clone)]
pub(crate) enum HttpDiagnosticsSource {
    TrustedLocal {
        secret: Arc<crate::http::LaunchSecret>,
    },
    #[cfg(feature = "auth")]
    ProxiedOidc {
        boundary: Option<Arc<crate::auth::ResourceServer>>,
    },
    #[cfg(not(feature = "auth"))]
    ProxiedOidc,
}

#[derive(Clone)]
enum DiagnosticsTransport {
    Stdio,
    #[cfg(feature = "http")]
    Http(HttpDiagnosticsSource),
}

/// Live production owners plus facts fixed when one project runtime opens.
pub(crate) struct DiagnosticsContext {
    profile: CapabilityProfile,
    transport: Mutex<DiagnosticsTransport>,
    project_identity: ProjectIdentity,
    project: ProjectFacts,
    session: Arc<CoreSession>,
    scheduler: Arc<Scheduler>,
    router: Arc<OutcomeRouter>,
    scheduler_limits: SchedulerLimits,
    metrics: Arc<Mutex<MetricsRegistry>>,
    /// The boundary every launch is contained in, when the runtime has one.
    process: Option<Arc<crate::engine_plane::providers::LaunchProviders>>,
}

impl DiagnosticsContext {
    /// Bind diagnostics to the same live owners the production runtime uses.
    #[must_use]
    pub(crate) fn new(
        profile: CapabilityProfile,
        project_identity: ProjectIdentity,
        session: Arc<CoreSession>,
        scheduler: Arc<Scheduler>,
        router: Arc<OutcomeRouter>,
        scheduler_limits: SchedulerLimits,
    ) -> Self {
        let identity = AuditDigest::of_str("project", project_identity.object_id().as_str());
        let owner = AuditDigest::of_str("lease-owner", project_identity.object_id().as_str());
        let project = ProjectFacts {
            identity,
            lease: LeaseOwnership::HeldHere { owner },
            safe_root_generation: project_identity.generation(),
        };
        Self {
            profile,
            transport: Mutex::new(DiagnosticsTransport::Stdio),
            project_identity,
            project,
            session,
            scheduler,
            router,
            scheduler_limits,
            metrics: Arc::new(Mutex::new(MetricsRegistry::new())),
            process: None,
        }
    }

    /// Report which boundary contains this runtime's launches, or why none
    /// can.
    #[must_use]
    pub(crate) fn with_process_boundary(
        mut self,
        providers: Arc<crate::engine_plane::providers::LaunchProviders>,
    ) -> Self {
        self.process = Some(providers);
        self
    }

    #[cfg(feature = "http")]
    pub(crate) fn declare_http(&self, source: HttpDiagnosticsSource) {
        *self
            .transport
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = DiagnosticsTransport::Http(source);
    }

    /// Render the legacy health fields and the complete Todo 33 diagnostic facts.
    pub(crate) fn health_value(&self) -> Result<Value, OutcomeError> {
        let observed_at_unix_ms = current_unix_ms();
        let (transport, http) = self.transport_facts();
        let snapshot = self.session.snapshot();
        let engine = EngineObservation::observe(
            self.profile,
            self.session.engine_binary(),
            self.process.as_deref(),
        );
        let planes = self.router.health(observed_at_unix_ms);
        let mut report = legacy_health_value(
            self.profile,
            &self.project_identity,
            &snapshot,
            &engine.reach,
            Some(&planes),
        );

        let index = index_facts(&snapshot);
        let queue = QueueFacts {
            mode: self.scheduler.mode(),
            running: self.scheduler.running(),
            queued: self.scheduler.queued(),
            concurrency_limit: self.scheduler_limits.read_concurrency,
            queue_limit: self.scheduler_limits.queue_limit,
            journal: self.scheduler.journal_binding(),
        };
        let journal = journal_facts(self.scheduler.as_ref());
        // A running managed game is one whose probe is connected over its
        // authenticated session bridge, which the bridge's own heartbeats keep
        // honest; the probe is injected by launch, never as a project autoload.
        let game_running = planes
            .plane(EffectPlane::Runtime)
            .is_some_and(PlaneHealth::usable);
        let runtime = RuntimeFacts {
            presence: if game_running {
                RuntimePresence::GameRunning
            } else {
                RuntimePresence::NoInstance
            },
            heartbeat: if game_running {
                Freshness::Fresh { age_ms: 0 }
            } else {
                Freshness::Absent
            },
            autoload: AutoloadRecovery::NotRequired,
            rendezvous: if game_running {
                Freshness::Fresh { age_ms: 0 }
            } else {
                Freshness::Absent
            },
        };
        // The bridge admits only a plugin whose protocol major matches, so a
        // connected plugin is a matched one.
        let plugin = match planes.plane(EffectPlane::Editor) {
            Some(editor) if editor.usable() => PluginProtocol::Matched {
                version: godot_mcp_bridge::protocol::compat::PROTOCOL_MAJOR,
            },
            Some(editor) if editor.boundary_compiled_in() => PluginProtocol::NotConnected,
            _ => PluginProtocol::NotShipped,
        };
        let versions = VersionFacts {
            contract_matched: true,
            engine: EngineCompatibility::Unknown,
            plugin,
        };
        let health = ServerHealth::assemble(HealthInputs {
            profile: self.profile,
            transport,
            project: self.project.clone(),
            index: index.clone(),
            engine: engine.facts.clone(),
            planes: &planes,
            queue,
            journal: journal.clone(),
            runtime,
            http,
            versions,
            observed_at_unix_ms,
        });

        let bundle = {
            let mut metrics = self
                .metrics
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            metrics.observe(MetricName::QueueDepth, None, u64::from(queue.queued));
            for finding in health.findings() {
                metrics.observe(
                    MetricName::FindingsObserved,
                    Some(MetricDimension::Code(finding.code())),
                    1,
                );
                if finding.blocks_mutation() {
                    metrics.observe(
                        MetricName::MutationBlocks,
                        Some(MetricDimension::Code(finding.code())),
                        1,
                    );
                }
            }
            metrics.observe(MetricName::BundlesRendered, None, 1);
            health.bundle(BundleAudience::LocalOperator, &metrics)
        };
        let mut bundle_fields = serde_json::to_value(bundle)
            .map_err(|_| {
                outcome_error(
                    godot_mcp_domain::ErrorKind::Internal,
                    "diagnostic bundle serialization failed",
                )
            })?
            .as_object()
            .cloned()
            .ok_or_else(|| {
                outcome_error(
                    godot_mcp_domain::ErrorKind::Internal,
                    "diagnostic bundle was not an object",
                )
            })?;
        let diagnostic_status = bundle_fields.remove("status").ok_or_else(|| {
            outcome_error(
                godot_mcp_domain::ErrorKind::Internal,
                "diagnostic bundle omitted status",
            )
        })?;
        let object = report.as_object_mut().ok_or_else(|| {
            outcome_error(
                godot_mcp_domain::ErrorKind::Internal,
                "health report was not an object",
            )
        })?;
        object.append(&mut bundle_fields);
        object.insert("diagnostic_status".to_owned(), diagnostic_status);
        object.insert("codes".to_owned(), json!(health.codes()));
        object.insert("observed_at_unix_ms".to_owned(), json!(observed_at_unix_ms));
        object.insert("index".to_owned(), index_value(&index));
        object.insert("engine".to_owned(), engine_value(&engine.facts));
        object.insert("planes".to_owned(), planes_value(&planes));
        object.insert("queue".to_owned(), queue_value(queue));
        object.insert("journal".to_owned(), journal_value(&journal));
        object.insert("runtime".to_owned(), runtime_value(runtime));
        object.insert("http".to_owned(), json!({ "mode": http.mode() }));
        object.insert("versions".to_owned(), versions_value(versions));
        object.insert("lease".to_owned(), lease_value(&self.project.lease));
        object.insert(
            "safe_root_generation".to_owned(),
            json!(self.project.safe_root_generation),
        );
        Ok(report)
    }

    /// Count one tool request without admitting attacker-controlled labels.
    pub(crate) fn note_tool(&self, name: MetricName, tool: &str) {
        self.metrics
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .note_tool(name, tool);
    }

    /// Count one no-handler refusal when the tool maps to an effect plane.
    pub(crate) fn note_plane_refusal(&self, tool: &str) {
        let Ok(family) = family_of(tool) else {
            return;
        };
        let Some(plane) = plane_of(family) else {
            return;
        };
        self.note_plane_refusal_on(plane);
    }

    /// Count one refusal caused by a specific unavailable plane.
    pub(crate) fn note_plane_refusal_on(&self, plane: crate::scheduler::EffectPlane) {
        self.metrics
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .observe(
                MetricName::PlaneRefusals,
                Some(MetricDimension::Plane(plane)),
                1,
            );
    }

    fn transport_facts(&self) -> (TransportSurface, HttpReadiness) {
        let source = self
            .transport
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        match source {
            DiagnosticsTransport::Stdio => (TransportSurface::Stdio, HttpReadiness::Disabled),
            #[cfg(feature = "http")]
            DiagnosticsTransport::Http(HttpDiagnosticsSource::TrustedLocal { secret }) => (
                TransportSurface::Http,
                HttpReadiness::TrustedLocal {
                    secret: local_secret_state(&secret),
                },
            ),
            #[cfg(all(feature = "http", feature = "auth"))]
            DiagnosticsTransport::Http(HttpDiagnosticsSource::ProxiedOidc { boundary }) => {
                let jwks = boundary.map_or(JwksState::Unreachable, |boundary| {
                    let keys = boundary.keys().len();
                    if keys == 0 {
                        JwksState::Empty
                    } else {
                        JwksState::Ready {
                            keys: u32::try_from(keys).unwrap_or(u32::MAX),
                        }
                    }
                });
                (TransportSurface::Http, HttpReadiness::ProxiedOidc { jwks })
            }
            #[cfg(all(feature = "http", not(feature = "auth")))]
            DiagnosticsTransport::Http(HttpDiagnosticsSource::ProxiedOidc) => (
                TransportSurface::Http,
                HttpReadiness::ProxiedOidc {
                    jwks: JwksState::Unreachable,
                },
            ),
        }
    }
}

#[cfg(feature = "http")]
fn local_secret_state(secret: &crate::http::LaunchSecret) -> LocalSecretState {
    let path = secret.path();
    match path.try_exists() {
        Ok(false) => return LocalSecretState::Missing,
        Err(_) => return LocalSecretState::Unreadable,
        Ok(true) => {}
    }
    match crate::http::LaunchSecret::mode_of(path) {
        Ok(mode) if mode != crate::http::LAUNCH_SECRET_FILE_MODE => {
            LocalSecretState::InsecurePermissions { mode }
        }
        Ok(_) => match crate::http::LaunchSecret::read_token(path) {
            Ok(candidate) if secret.matches_bearer(&candidate) => LocalSecretState::Ready,
            Ok(_) => LocalSecretState::Unreadable,
            Err(crate::http::LaunchSecretError::InsecurePermissions { mode }) => {
                LocalSecretState::InsecurePermissions { mode }
            }
            Err(_) if matches!(path.try_exists(), Ok(false)) => LocalSecretState::Missing,
            Err(_) => LocalSecretState::Unreadable,
        },
        Err(_) if matches!(path.try_exists(), Ok(false)) => LocalSecretState::Missing,
        Err(_) => LocalSecretState::Unreadable,
    }
}

/// Preserve the original five-component `health.get` shape when no production
/// diagnostics context is attached (for example, a focused handler unit test).
pub(crate) fn legacy_health(
    profile: CapabilityProfile,
    session: &CoreSession,
) -> Result<Value, OutcomeError> {
    let snapshot = session.snapshot();
    let engine = EngineObservation::observe(profile, session.engine_binary(), None);
    Ok(legacy_health_value(
        profile,
        session.project_identity(),
        &snapshot,
        &engine.reach,
        None,
    ))
}

/// `planes` is the router's plane health; without it (a bare kernel with no
/// production assembly) no plane-backed boundary is attached at all.
fn legacy_health_value(
    profile: CapabilityProfile,
    project: &ProjectIdentity,
    snapshot: &Result<Arc<IndexSnapshot>, OutcomeError>,
    engine: &EngineReach,
    planes: Option<&crate::router::RouterHealth>,
) -> Value {
    let (index_status, index_message, status) = match snapshot {
        Ok(snapshot) if snapshot.is_current() => ("ready", None, "ready"),
        Ok(snapshot) => (
            "degraded",
            Some(format!(
                "the analysis index is behind the filesystem ({})",
                snapshot.coherence().code()
            )),
            "degraded",
        ),
        Err(error) => ("unavailable", Some(error.message().to_owned()), "degraded"),
    };
    let profile_name = profile.manifest_name();
    let editor = PlaneComponent::Editor.observe(profile, planes);
    let runtime = PlaneComponent::Runtime.observe(profile, planes);
    json!({
        "status": status,
        "project": project,
        "components": [
            { "component": "project", "status": "ready", "message": Value::Null },
            { "component": "index", "status": index_status, "message": index_message },
            {
                "component": "godot",
                "status": engine.status(),
                "message": engine.message(profile_name),
            },
            {
                "component": "editor",
                "status": editor.status(),
                "message": editor.message(profile_name),
            },
            {
                "component": "runtime",
                "status": runtime.status(),
                "message": runtime.message(profile_name),
            },
        ],
        "coherence": snapshot
            .as_ref()
            .map(|snapshot| coherence_value(snapshot.coherence()))
            .unwrap_or(Value::Null),
    })
}

fn index_facts(snapshot: &Result<Arc<IndexSnapshot>, OutcomeError>) -> IndexFacts {
    match snapshot {
        Ok(snapshot) if snapshot.is_current() => IndexFacts::Fresh {
            generation: snapshot.project().generation(),
        },
        Ok(snapshot) => IndexFacts::Stale {
            generation: snapshot.project().generation(),
            coherence: SafeField::new(snapshot.coherence().code())
                .expect("index coherence codes satisfy SafeField bounds"),
        },
        Err(_) => IndexFacts::Unreadable,
    }
}

fn journal_facts(scheduler: &Scheduler) -> JournalFacts {
    let mut facts = JournalFacts {
        quarantined: DigestSample::empty(),
        unresolved_survivors: DigestSample::empty(),
        // Plan line 420: Todo 25 is cancelled, so no shipped path installs a temporary autoload
        // or setting; the outstanding restoration set is empty and therefore vacuously settled.
        settings: SettingsReconciliation::Reconciled,
        block_readable: true,
    };
    match scheduler.quarantined_transactions() {
        Ok(transactions) if !transactions.is_empty() => {
            let total = u32::try_from(transactions.len()).unwrap_or(u32::MAX);
            let sample = transactions
                .iter()
                .map(|transaction| AuditDigest::of_str("transaction", transaction))
                .collect();
            facts.quarantined = DigestSample::of(total, sample);
        }
        Ok(_) => {}
        Err(_) => facts.block_readable = false,
    }
    match scheduler.unresolved_survivor() {
        Ok(Some(reason)) => {
            facts.unresolved_survivors =
                DigestSample::of(1, vec![AuditDigest::of_str("survivor", &reason)]);
        }
        Ok(None) => {}
        Err(_) => facts.block_readable = false,
    }
    facts
}

fn coherence_value(coherence: &godot_mcp_analysis::index::Coherence) -> Value {
    json!({
        "verdict": coherence.code(),
        "reason": coherence
            .stale_reason()
            .map(|reason| reason.code().to_owned())
            .or_else(|| coherence
                .rescan_reason()
                .map(|reason| reason.code().to_owned())),
        "subjects": coherence.subjects(),
    })
}

fn index_value(index: &IndexFacts) -> Value {
    match index {
        IndexFacts::Fresh { generation } => json!({
            "state": "fresh",
            "generation": generation,
            "coherence": Value::Null,
        }),
        IndexFacts::Stale {
            generation,
            coherence,
        } => json!({
            "state": "stale",
            "generation": generation,
            "coherence": coherence.as_str(),
        }),
        IndexFacts::Unreadable => json!({
            "state": "unreadable",
            "generation": Value::Null,
            "coherence": Value::Null,
        }),
    }
}

fn engine_value(engine: &EngineFacts) -> Value {
    json!({
        "presence": engine.presence.as_str(),
        "binary": engine.binary,
        "version": engine.version.map(|version| json!({
            "major": version.major(),
            "minor": version.minor(),
            "patch": version.patch(),
        })),
        "capabilities_probed": engine.capabilities_probed,
    })
}

fn planes_value(planes: &crate::router::RouterHealth) -> Value {
    Value::Array(
        planes
            .planes()
            .into_iter()
            .map(|plane| {
                json!({
                    "plane": plane.plane().as_str(),
                    "boundary_compiled_in": plane.boundary_compiled_in(),
                    "environment_present": plane.environment_present(),
                    "usable": plane.usable(),
                })
            })
            .collect(),
    )
}

fn queue_value(queue: QueueFacts) -> Value {
    json!({
        "mode": queue.mode,
        "running": queue.running,
        "queued": queue.queued,
        "concurrency_limit": queue.concurrency_limit,
        "queue_limit": queue.queue_limit,
        "journal": {
            "idempotency": queue.journal.idempotency,
            "store": queue.journal.store,
            "survivors": queue.journal.survivors,
        },
    })
}

fn journal_value(journal: &JournalFacts) -> Value {
    json!({
        "quarantined": {
            "total": journal.quarantined.total(),
            "sample": journal.quarantined.sample(),
        },
        "unresolved_survivors": {
            "total": journal.unresolved_survivors.total(),
            "sample": journal.unresolved_survivors.sample(),
        },
        "settings_reconciliation": settings_label(journal.settings),
        "block_readable": journal.block_readable,
    })
}

fn runtime_value(runtime: RuntimeFacts) -> Value {
    json!({
        "presence": runtime.presence.as_str(),
        "heartbeat": runtime.heartbeat.as_str(),
        "autoload": runtime.autoload.as_str(),
        "rendezvous": runtime.rendezvous.as_str(),
    })
}

fn versions_value(versions: VersionFacts) -> Value {
    json!({
        "contract_matched": versions.contract_matched,
        "engine": versions.engine.as_str(),
        "plugin": plugin_value(versions.plugin),
    })
}

fn plugin_value(plugin: PluginProtocol) -> Value {
    match plugin {
        PluginProtocol::NotShipped => json!({ "state": "not_shipped" }),
        PluginProtocol::NotConnected => json!({ "state": "not_connected" }),
        PluginProtocol::Matched { version } => {
            json!({ "state": "matched", "version": version })
        }
        PluginProtocol::Mismatch { expected, observed } => json!({
            "state": "mismatch",
            "expected": expected,
            "observed": observed,
        }),
    }
}

fn settings_label(settings: SettingsReconciliation) -> &'static str {
    match settings {
        SettingsReconciliation::Reconciled => "reconciled",
        SettingsReconciliation::Pending { .. } => "pending",
        SettingsReconciliation::Unknown => "unknown",
    }
}

fn lease_value(lease: &LeaseOwnership) -> Value {
    match lease {
        LeaseOwnership::NotHeld => json!({ "ownership": "not_held" }),
        LeaseOwnership::HeldHere { owner } => {
            json!({ "ownership": "held_here", "owner": owner })
        }
        LeaseOwnership::HeldElsewhere { owner } => {
            json!({ "ownership": "held_elsewhere", "owner": owner })
        }
    }
}

fn current_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

struct EngineObservation {
    reach: EngineReach,
    facts: EngineFacts,
}

impl EngineObservation {
    fn observe(
        profile: CapabilityProfile,
        configured: Option<&Path>,
        process: Option<&crate::engine_plane::providers::LaunchProviders>,
    ) -> Self {
        let resolved = discover(
            configured,
            HostPlatform::current(),
            &EnvironmentSnapshot::from_process_env(),
        );
        let facts = match &resolved {
            Ok(binary) => EngineFacts {
                presence: EnginePresence::HeadlessOnly,
                binary: Some(AuditDigest::of_bytes(
                    "godot-binary",
                    binary.path.as_os_str().as_encoded_bytes(),
                )),
                version: None,
                capabilities_probed: false,
            },
            Err(_) => EngineFacts {
                presence: EnginePresence::Absent,
                binary: None,
                version: None,
                capabilities_probed: false,
            },
        };
        let reach = if !advertises_plane(profile, EffectPlane::Process) {
            EngineReach::NoProcessTool
        } else if resolved.is_err() {
            if configured.is_some() {
                EngineReach::ConfiguredUnresolved
            } else {
                EngineReach::Undiscovered
            }
        } else if !bundled_supervisor().is_file() {
            EngineReach::SupervisorMissing
        } else {
            match process.map(crate::engine_plane::providers::LaunchProviders::status) {
                Some(Err(reason)) => EngineReach::NoBoundary { reason },
                Some(Ok((kind, strength))) => EngineReach::Reachable {
                    boundary: Some(format!("{} ({strength})", kind.as_str())),
                },
                None => EngineReach::Reachable { boundary: None },
            }
        };
        Self { reach, facts }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum EngineReach {
    NoProcessTool,
    /// A launch can start, inside the named boundary when it is known.
    Reachable {
        boundary: Option<String>,
    },
    SupervisorMissing,
    /// A Godot executable and the supervisor are present, but no boundary
    /// can contain a launch on this host or with this configuration.
    NoBoundary {
        reason: String,
    },
    ConfiguredUnresolved,
    Undiscovered,
}

impl EngineReach {
    const fn status(&self) -> &'static str {
        match self {
            Self::Reachable { .. } => "ready",
            Self::SupervisorMissing => "degraded",
            Self::NoProcessTool
            | Self::NoBoundary { .. }
            | Self::ConfiguredUnresolved
            | Self::Undiscovered => "unavailable",
        }
    }

    fn message(&self, profile: &str) -> String {
        match self {
            Self::NoProcessTool => format!(
                "the {profile} profile advertises no tool whose effect reaches the process plane, \
                 so nothing it exposes starts a Godot process"
            ),
            Self::Reachable { boundary } => format!(
                "a Godot executable resolved and the bundled supervisor is present, so the \
                 {profile} profile's process-plane tools can create a Godot process inside {}; \
                 each launch still probes the build and refuses one whose advertised \
                 command-line capabilities it does not recognise",
                boundary.as_ref().map_or_else(
                    || "a containment boundary".to_owned(),
                    |boundary| format!("a {boundary} containment boundary")
                )
            ),
            Self::NoBoundary { reason } => format!(
                "a Godot executable resolved and the bundled supervisor is present, but no \
                 process boundary can contain a launch here: {reason}; the {profile} profile's \
                 process-plane tools answer `unavailable` until one can"
            ),
            Self::SupervisorMissing => format!(
                "a Godot executable resolved but the bundled `godot-mcp-supervisor` executable is \
                 not beside this server, so the {profile} profile's process-plane tools cannot \
                 create a contained process until the release archive's `bin/` pair is restored \
                 together"
            ),
            Self::ConfiguredUnresolved => format!(
                "the explicitly configured Godot executable could not be resolved to a regular \
                 file, so the \
                 {profile} profile's process-plane tools answer `unavailable` until `godot_binary` \
                 names a resolvable regular file"
            ),
            Self::Undiscovered => format!(
                "no Godot executable was found in this host's conventional install locations or on \
                 `PATH`, and no explicit path was resolved here, so the {profile} profile's \
                 process-plane tools answer `unavailable` until `godot_binary` names one \
                 (`GODOT_MCP_GODOT_BINARY` is only read when a configuration file is supplied)"
            ),
        }
    }
}

fn advertises_plane(profile: CapabilityProfile, plane: EffectPlane) -> bool {
    canonical_manifest(profile).tools.iter().any(|tool| {
        family_of(tool).is_ok_and(|family| plane_of(family).is_some_and(|found| found == plane))
    })
}

/// One of the two plane-backed health components.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlaneComponent {
    Editor,
    Runtime,
}

impl PlaneComponent {
    fn observe(
        self,
        profile: CapabilityProfile,
        planes: Option<&crate::router::RouterHealth>,
    ) -> PlaneComponentReach {
        let health = planes.and_then(|planes| planes.plane(self.effect_plane()));
        PlaneComponentReach {
            plane: self,
            advertised: advertises_plane(profile, self.effect_plane()),
            attached: health.is_some_and(PlaneHealth::boundary_compiled_in),
            present: health.is_some_and(PlaneHealth::usable),
        }
    }

    const fn effect_plane(self) -> EffectPlane {
        match self {
            Self::Editor => EffectPlane::Editor,
            Self::Runtime => EffectPlane::Runtime,
        }
    }

    const fn subject(self) -> &'static str {
        match self {
            Self::Editor => "editor observation and node mutation",
            Self::Runtime => "running-instance observation and synthetic input",
        }
    }

    /// What this server attaches for the plane.
    const fn boundary(self) -> &'static str {
        match self {
            Self::Editor => "live editor bridge",
            Self::Runtime => "managed game sessions",
        }
    }

    /// What is missing while the plane is attached but idle, and how to
    /// provide it.
    const fn absent(self) -> &'static str {
        match self {
            Self::Editor => {
                "no godot_mcp editor add-on is connected; editor.addon.install installs it and \
                 opening the project in the Godot editor connects it"
            }
            Self::Runtime => "no managed game session is running; project.run.start starts one",
        }
    }

    const fn present(self) -> &'static str {
        match self {
            Self::Editor => "a godot_mcp editor add-on is connected",
            Self::Runtime => "a managed game session is running with its probe connected",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PlaneComponentReach {
    plane: PlaneComponent,
    advertised: bool,
    attached: bool,
    present: bool,
}

impl PlaneComponentReach {
    const fn status(self) -> &'static str {
        if self.present { "ready" } else { "unavailable" }
    }

    fn message(self, profile: &str) -> String {
        let subject = self.plane.subject();
        let boundary = self.plane.boundary();
        if !self.advertised {
            format!("the {profile} profile advertises none of {subject}'s tools")
        } else if !self.attached {
            format!(
                "{subject} is advertised by the {profile} profile, but this server attaches no \
                 {boundary}, so its tools answer `unavailable`"
            )
        } else if !self.present {
            format!(
                "{subject} is served by the {profile} profile's {boundary}; {}",
                self.plane.absent()
            )
        } else {
            format!(
                "{subject} is served by the {profile} profile's {boundary}; {}",
                self.plane.present()
            )
        }
    }
}
