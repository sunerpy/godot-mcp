//! Health, readiness, bounded metrics and self-diagnostics.
//!
//! This is the surface an operator and an automated caller both read to answer
//! one question: *what can this server do right now, and if something is missing,
//! what do I change?*
//!
//! ## It composes the router's health rather than deriving a second one
//!
//! Plane facts come from [`RouterHealth`], which the router itself publishes. A
//! second derivation could disagree with the one the router enforces, and a
//! health surface that contradicts the thing it describes is worse than none.
//! [`HealthInputs::planes`] is therefore a borrow, not a copy of the logic.
//!
//! ## An absent optional plane is reported, never counted as a failure
//!
//! The editor plugin bridge and the running-instance bridge are not part of this
//! release, and a read-only deployment legitimately has no Godot binary. Those
//! components keep their entry in [`ServerHealth::components`] and report
//! `unavailable` with an actionable, *honest* remediation — one that names the
//! build fact instead of implying a plugin exists and merely needs starting.
//! Overall status is folded from finding **severities**, not from component
//! statuses, so an unavailable optional component cannot drag the server into
//! `unavailable`.
//!
//! ## Mutation readiness is closed until reconciliation is proven complete
//!
//! [`ReadinessSurface::Mutate`] stays false while any finding reports
//! `blocks_mutation`, and both an unresolved launch survivor and an unreconciled
//! temporary-settings state do. The decision reads the finding set, so clearing
//! one blocker leaves the other in place by construction.

mod bundle;
mod code;
mod context;
mod facts;
mod finding;
mod metrics;
mod readiness;

pub use bundle::{
    BundleAudience, BundleFinding, BundleReadiness, DiagnosticBundle, component_label,
};
pub use code::{DiagnosticCode, DiagnosticSeverity};
pub use facts::{
    AutoloadRecovery, DigestSample, EngineCompatibility, EngineFacts, EngineVersion, Freshness,
    HealthInputs, HttpReadiness, IndexFacts, JournalFacts, JwksState, LeaseOwnership,
    LocalSecretState, MAX_DIGEST_SAMPLE, PluginProtocol, ProjectFacts, QueueFacts, RuntimeFacts,
    SettingsReconciliation, TransportSurface, VersionFacts,
};
pub use finding::Finding;
pub use metrics::{
    MAX_METRIC_SERIES, MetricDimension, MetricLabel, MetricName, MetricSample, MetricsRegistry,
};
pub use readiness::{Readiness, ReadinessSurface};

#[cfg(feature = "http")]
pub(crate) use context::HttpDiagnosticsSource;
pub(crate) use context::{DiagnosticsContext, legacy_health};

use godot_mcp_domain::CapabilityProfile;
use godot_mcp_domain::contracts::{
    ComponentHealth, ComponentName, HealthReport, HealthStatus, ProjectIdentity, SchedulerMode,
};

use crate::idempotency::family_of;
use crate::router::{EnginePresence, PlaneHealth, RuntimePresence};
use crate::scheduler::{EffectPlane, plane_of};

/// Every public health component, in a stable order.
pub const COMPONENTS: [ComponentName; 7] = [
    ComponentName::Project,
    ComponentName::Index,
    ComponentName::Godot,
    ComponentName::Editor,
    ComponentName::Runtime,
    ComponentName::Scheduler,
    ComponentName::Authorization,
];

/// One component's health, with the remediation for whatever is missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentReport {
    component: ComponentName,
    status: HealthStatus,
    optional: bool,
    code: Option<DiagnosticCode>,
}

impl ComponentReport {
    /// Which component this describes.
    #[must_use]
    pub const fn component(&self) -> ComponentName {
        self.component
    }

    /// This component's status.
    #[must_use]
    pub const fn status(&self) -> HealthStatus {
        self.status
    }

    /// Whether this component may be unavailable without the server failing.
    #[must_use]
    pub const fn is_optional(&self) -> bool {
        self.optional
    }

    /// The code that explains a non-ready status.
    #[must_use]
    pub const fn code(&self) -> Option<DiagnosticCode> {
        self.code
    }

    /// What an operator can do about it.
    #[must_use]
    pub fn remediation(&self) -> Option<&'static str> {
        self.code.map(DiagnosticCode::remediation)
    }
}

/// One complete health assembly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerHealth {
    profile: CapabilityProfile,
    transport: TransportSurface,
    status: HealthStatus,
    readiness: Readiness,
    findings: Vec<Finding>,
    components: Vec<ComponentReport>,
    usable_planes: Vec<EffectPlane>,
    lease: LeaseOwnership,
    safe_root_generation: u64,
    settings: SettingsReconciliation,
    rendezvous: Freshness,
    heartbeat: Freshness,
    autoload: AutoloadRecovery,
    http: HttpReadiness,
    observed_at_unix_ms: u64,
}

impl ServerHealth {
    /// Collect every fact into one report.
    #[must_use]
    pub fn assemble(inputs: HealthInputs<'_>) -> Self {
        let findings = collect(&inputs);
        let readiness = Readiness::decide(&inputs, &findings);
        let status = findings
            .iter()
            .fold(DiagnosticSeverity::Info, |worst, finding| {
                worst.max_of(finding.severity())
            });
        let components = COMPONENTS
            .into_iter()
            .map(|component| component_report(component, &inputs, &findings))
            .collect();
        Self {
            profile: inputs.profile,
            transport: inputs.transport,
            status: status_of(status),
            readiness,
            findings,
            components,
            usable_planes: inputs.planes.usable_planes(),
            lease: inputs.project.lease.clone(),
            safe_root_generation: inputs.project.safe_root_generation,
            settings: inputs.journal.settings,
            rendezvous: inputs.runtime.rendezvous,
            heartbeat: inputs.runtime.heartbeat,
            autoload: inputs.runtime.autoload,
            http: inputs.http,
            observed_at_unix_ms: inputs.observed_at_unix_ms,
        }
    }

    /// The advertised capability profile.
    #[must_use]
    pub const fn profile(&self) -> CapabilityProfile {
        self.profile
    }

    /// The transport that answered.
    #[must_use]
    pub const fn transport(&self) -> TransportSurface {
        self.transport
    }

    /// The overall status, folded from finding severities.
    #[must_use]
    pub const fn status(&self) -> HealthStatus {
        self.status
    }

    /// Readiness per surface.
    #[must_use]
    pub const fn readiness(&self) -> &Readiness {
        &self.readiness
    }

    /// Every finding, in collection order.
    #[must_use]
    pub fn findings(&self) -> &[Finding] {
        &self.findings
    }

    /// One finding by code.
    #[must_use]
    pub fn finding(&self, code: DiagnosticCode) -> Option<&Finding> {
        self.findings.iter().find(|finding| finding.code() == code)
    }

    /// Whether one code is present.
    #[must_use]
    pub fn has(&self, code: DiagnosticCode) -> bool {
        self.finding(code).is_some()
    }

    /// Every reported code, in collection order.
    #[must_use]
    pub fn codes(&self) -> Vec<&'static str> {
        self.findings
            .iter()
            .map(|finding| finding.code().as_str())
            .collect()
    }

    /// Every code currently holding mutations closed, in collection order.
    #[must_use]
    pub fn blocked_mutation_codes(&self) -> Vec<&'static str> {
        self.findings
            .iter()
            .filter(|finding| finding.blocks_mutation())
            .map(|finding| finding.code().as_str())
            .collect()
    }

    /// Every component report.
    #[must_use]
    pub fn components(&self) -> &[ComponentReport] {
        &self.components
    }

    /// One component report.
    #[must_use]
    pub fn component(&self, component: ComponentName) -> Option<&ComponentReport> {
        self.components
            .iter()
            .find(|report| report.component == component)
    }

    /// The planes that can execute right now, from the router's own health.
    #[must_use]
    pub fn usable_planes(&self) -> Vec<EffectPlane> {
        self.usable_planes.clone()
    }

    /// Who holds the project's object-identity lease.
    #[must_use]
    pub fn lease_owner(&self) -> LeaseOwnership {
        self.lease.clone()
    }

    /// The `SafeRoot` generation the lease published.
    #[must_use]
    pub const fn safe_root_generation(&self) -> u64 {
        self.safe_root_generation
    }

    /// Whether temporary project settings were reconciled.
    #[must_use]
    pub const fn settings(&self) -> SettingsReconciliation {
        self.settings
    }

    /// The runtime rendezvous record's freshness.
    #[must_use]
    pub const fn rendezvous(&self) -> Freshness {
        self.rendezvous
    }

    /// The running instance's heartbeat freshness.
    #[must_use]
    pub const fn heartbeat(&self) -> Freshness {
        self.heartbeat
    }

    /// Whether displaced autoloads were restored.
    #[must_use]
    pub const fn autoload(&self) -> AutoloadRecovery {
        self.autoload
    }

    /// The HTTP surface's readiness and active authentication mode.
    #[must_use]
    pub const fn http(&self) -> HttpReadiness {
        self.http
    }

    /// The trusted observation timestamp.
    #[must_use]
    pub const fn observed_at_unix_ms(&self) -> u64 {
        self.observed_at_unix_ms
    }

    /// Project this assembly onto the frozen public health contract.
    #[must_use]
    pub fn to_report(&self, project: ProjectIdentity) -> HealthReport {
        let components = self
            .components
            .iter()
            .map(|report| {
                ComponentHealth::new(
                    report.component,
                    report.status,
                    report.remediation().map(str::to_owned),
                )
            })
            .collect();
        HealthReport::new(project, self.status, components, self.observed_at_unix_ms)
    }

    /// Render a redacted bundle for one audience.
    #[must_use]
    pub fn bundle(&self, audience: BundleAudience, metrics: &MetricsRegistry) -> DiagnosticBundle {
        DiagnosticBundle::render(self, metrics, audience)
    }
}

/// Map a severity onto the frozen public status.
const fn status_of(severity: DiagnosticSeverity) -> HealthStatus {
    match severity {
        DiagnosticSeverity::Info => HealthStatus::Ready,
        DiagnosticSeverity::Degraded => HealthStatus::Degraded,
        DiagnosticSeverity::Blocked => HealthStatus::Unavailable,
    }
}

/// Whether the profile advertises any tool whose effect reaches `plane`.
///
/// Derived by joining the frozen canonical manifest to the two single mappings
/// that own the question — [`family_of`] for tool to family and [`plane_of`] for
/// family to plane. Enumerating profile variants by hand is what made this layer
/// claim the `editor` profile needs no supervised process, even though
/// `project.import.run` is an `editor` tool that launches one.
fn advertises_plane(profile: CapabilityProfile, plane: EffectPlane) -> bool {
    godot_mcp_domain::catalog::TOOL_CATALOG
        .iter()
        .filter(|descriptor| descriptor.in_profile(profile))
        .any(|descriptor| {
            let composed = descriptor.composed.iter().map(|boundary| {
                crate::router::plane_of_boundary(crate::policy::matrix::boundary_kind(*boundary))
            });
            let primary = descriptor.boundary.map(|boundary| {
                crate::router::plane_of_boundary(crate::policy::matrix::boundary_kind(boundary))
            });
            composed.chain(primary).any(|found| found == plane)
                || family_of(descriptor.name)
                    .is_ok_and(|family| plane_of(family).is_some_and(|found| found == plane))
        })
}

/// Whether the profile advertises the editor plane's tools.
fn advertises_editor(profile: CapabilityProfile) -> bool {
    advertises_plane(profile, EffectPlane::Editor)
}

/// Whether the profile advertises the runtime plane's tools.
fn advertises_runtime(profile: CapabilityProfile) -> bool {
    advertises_plane(profile, EffectPlane::Runtime)
}

/// Whether the profile advertises tools that need a supervised process.
fn advertises_process(profile: CapabilityProfile) -> bool {
    advertises_plane(profile, EffectPlane::Process)
}

/// Whether the profile advertises tools that need the editor-hosted adapters.
fn advertises_lsp_dap(profile: CapabilityProfile) -> bool {
    advertises_plane(profile, EffectPlane::LspDap)
}

/// The severity to record: a plane the profile advertises is at least degraded,
/// because part of the advertised surface cannot execute. `Finding::new` clamps an
/// optional absence, so this can never turn one into a server failure.
fn severity_for(code: DiagnosticCode, advertised: bool) -> DiagnosticSeverity {
    if advertised {
        code.baseline_severity()
            .max_of(DiagnosticSeverity::Degraded)
    } else {
        code.baseline_severity()
    }
}

/// Collect every finding, in the order the report publishes them.
///
/// The order is part of the contract: [`ServerHealth::blocked_mutation_codes`]
/// preserves it so an operator reads the blockers in the order they must be
/// cleared — project identity first, then durable journal state, then scheduler
/// admission.
fn collect(inputs: &HealthInputs<'_>) -> Vec<Finding> {
    let mut findings = Vec::new();
    collect_project(inputs, &mut findings);
    collect_index(inputs, &mut findings);
    collect_engine(inputs, &mut findings);
    collect_planes(inputs, &mut findings);
    collect_plugin(inputs, &mut findings);
    collect_journal(inputs, &mut findings);
    collect_scheduler(inputs, &mut findings);
    collect_runtime(inputs, &mut findings);
    collect_authorization(inputs, &mut findings);
    findings
}

fn collect_project(inputs: &HealthInputs<'_>, findings: &mut Vec<Finding>) {
    match &inputs.project.lease {
        LeaseOwnership::NotHeld => findings.push(Finding::new(
            DiagnosticCode::ProjectLeaseNotHeld,
            DiagnosticCode::ProjectLeaseNotHeld.baseline_severity(),
            Some(inputs.project.identity.clone()),
            None,
        )),
        LeaseOwnership::HeldElsewhere { owner } => findings.push(Finding::new(
            DiagnosticCode::ProjectLeaseForeignOwner,
            DiagnosticCode::ProjectLeaseForeignOwner.baseline_severity(),
            Some(owner.clone()),
            Some(inputs.project.safe_root_generation),
        )),
        LeaseOwnership::HeldHere { .. } => {}
    }
    if !inputs.versions.contract_matched {
        findings.push(Finding::new(
            DiagnosticCode::ContractVersionMismatch,
            DiagnosticCode::ContractVersionMismatch.baseline_severity(),
            None,
            None,
        ));
    }
}

fn collect_index(inputs: &HealthInputs<'_>, findings: &mut Vec<Finding>) {
    match &inputs.index {
        IndexFacts::Fresh { .. } => {}
        IndexFacts::Stale {
            generation,
            coherence,
        } => findings.push(Finding::new(
            DiagnosticCode::IndexStale,
            DiagnosticCode::IndexStale.baseline_severity(),
            Some(crate::audit::AuditDigest::of_str(
                "index-coherence",
                coherence.as_str(),
            )),
            Some(*generation),
        )),
        IndexFacts::Unreadable => findings.push(Finding::new(
            DiagnosticCode::IndexUnreadable,
            DiagnosticCode::IndexUnreadable.baseline_severity(),
            None,
            None,
        )),
    }
}

fn collect_engine(inputs: &HealthInputs<'_>, findings: &mut Vec<Finding>) {
    if inputs.engine.presence == EnginePresence::Absent {
        findings.push(Finding::new(
            DiagnosticCode::GodotBinaryAbsent,
            severity_for(
                DiagnosticCode::GodotBinaryAbsent,
                advertises_process(inputs.profile),
            ),
            None,
            None,
        ));
        return;
    }
    if inputs.versions.engine == EngineCompatibility::Unsupported {
        findings.push(Finding::new(
            DiagnosticCode::GodotVersionUnsupported,
            DiagnosticCode::GodotVersionUnsupported.baseline_severity(),
            inputs.engine.binary.clone(),
            inputs.engine.version.map(EngineVersion::as_measure),
        ));
    }
    if !inputs.engine.capabilities_probed {
        findings.push(Finding::new(
            DiagnosticCode::GodotCapabilitiesUnprobed,
            severity_for(
                DiagnosticCode::GodotCapabilitiesUnprobed,
                advertises_process(inputs.profile),
            ),
            inputs.engine.binary.clone(),
            None,
        ));
    }
}

fn collect_planes(inputs: &HealthInputs<'_>, findings: &mut Vec<Finding>) {
    for plane in [
        EffectPlane::Process,
        EffectPlane::LspDap,
        EffectPlane::Editor,
        EffectPlane::Runtime,
    ] {
        let Some(entry) = inputs.planes.plane(plane) else {
            continue;
        };
        let compiled_in = entry.boundary_compiled_in();
        let (code, advertised) = match plane {
            EffectPlane::Process if !compiled_in => (
                DiagnosticCode::ProcessPlaneUnavailable,
                advertises_process(inputs.profile),
            ),
            EffectPlane::LspDap if !compiled_in => (
                DiagnosticCode::LspDapPlaneNotShipped,
                advertises_lsp_dap(inputs.profile),
            ),
            EffectPlane::Editor if !compiled_in => (
                DiagnosticCode::EditorPlaneNotShipped,
                advertises_editor(inputs.profile),
            ),
            EffectPlane::Editor if !entry.environment_present() => (
                DiagnosticCode::EditorNotAttached,
                advertises_editor(inputs.profile),
            ),
            EffectPlane::Runtime if !compiled_in => (
                DiagnosticCode::RuntimePlaneNotShipped,
                advertises_runtime(inputs.profile),
            ),
            EffectPlane::File
            | EffectPlane::Process
            | EffectPlane::LspDap
            | EffectPlane::Editor
            | EffectPlane::Runtime => continue,
        };
        findings.push(Finding::new(
            code,
            severity_for(code, advertised),
            None,
            None,
        ));
    }
}

fn collect_plugin(inputs: &HealthInputs<'_>, findings: &mut Vec<Finding>) {
    if let PluginProtocol::Mismatch { observed, .. } = inputs.versions.plugin {
        findings.push(Finding::new(
            DiagnosticCode::PluginProtocolMismatch,
            DiagnosticCode::PluginProtocolMismatch.baseline_severity(),
            None,
            Some(u64::from(observed)),
        ));
    }
}

fn collect_journal(inputs: &HealthInputs<'_>, findings: &mut Vec<Finding>) {
    if !inputs.journal.quarantined.is_empty() {
        findings.push(Finding::new(
            DiagnosticCode::TransactionQuarantined,
            DiagnosticCode::TransactionQuarantined.baseline_severity(),
            inputs.journal.quarantined.first().cloned(),
            Some(u64::from(inputs.journal.quarantined.total())),
        ));
    }
    if !inputs.journal.unresolved_survivors.is_empty() {
        findings.push(Finding::new(
            DiagnosticCode::UnresolvedLaunchSurvivor,
            DiagnosticCode::UnresolvedLaunchSurvivor.baseline_severity(),
            inputs.journal.unresolved_survivors.first().cloned(),
            Some(u64::from(inputs.journal.unresolved_survivors.total())),
        ));
    }
    if !inputs.journal.settings.is_complete() {
        findings.push(Finding::new(
            DiagnosticCode::TemporarySettingsUnreconciled,
            DiagnosticCode::TemporarySettingsUnreconciled.baseline_severity(),
            None,
            inputs.journal.settings.pending(),
        ));
    }
    if !inputs.journal.block_readable {
        findings.push(Finding::new(
            DiagnosticCode::WriteBlockUnreadable,
            DiagnosticCode::WriteBlockUnreadable.baseline_severity(),
            None,
            None,
        ));
    }
}

fn collect_scheduler(inputs: &HealthInputs<'_>, findings: &mut Vec<Finding>) {
    if !inputs.queue.journal.is_complete() {
        findings.push(Finding::new(
            DiagnosticCode::SchedulerJournalUnbound,
            DiagnosticCode::SchedulerJournalUnbound.baseline_severity(),
            None,
            None,
        ));
    }
    let mode_code = match inputs.queue.mode {
        SchedulerMode::Accepting => None,
        SchedulerMode::Draining => Some(DiagnosticCode::SchedulerDraining),
        SchedulerMode::Quarantined => Some(DiagnosticCode::SchedulerQuarantined),
        SchedulerMode::Stopped => Some(DiagnosticCode::SchedulerStopped),
    };
    if let Some(code) = mode_code {
        findings.push(Finding::new(
            code,
            code.baseline_severity(),
            None,
            Some(u64::from(inputs.queue.running)),
        ));
    }
    if inputs.queue.is_saturated() {
        findings.push(Finding::new(
            DiagnosticCode::QueueSaturated,
            DiagnosticCode::QueueSaturated.baseline_severity(),
            None,
            Some(u64::from(inputs.queue.queued)),
        ));
    }
}

fn collect_runtime(inputs: &HealthInputs<'_>, findings: &mut Vec<Finding>) {
    let boundary = inputs
        .planes
        .plane(EffectPlane::Runtime)
        .is_some_and(PlaneHealth::boundary_compiled_in);
    if !boundary {
        return;
    }
    if inputs.runtime.presence == RuntimePresence::NoInstance {
        findings.push(Finding::new(
            DiagnosticCode::GameNotRunning,
            severity_for(
                DiagnosticCode::GameNotRunning,
                advertises_runtime(inputs.profile),
            ),
            None,
            None,
        ));
        return;
    }
    if !inputs.runtime.heartbeat.is_fresh() {
        findings.push(Finding::new(
            DiagnosticCode::RuntimeHeartbeatStale,
            DiagnosticCode::RuntimeHeartbeatStale.baseline_severity(),
            None,
            inputs.runtime.heartbeat.age_ms(),
        ));
    }
    if !inputs.runtime.autoload.is_settled() {
        findings.push(Finding::new(
            DiagnosticCode::AutoloadRestorationPending,
            DiagnosticCode::AutoloadRestorationPending.baseline_severity(),
            None,
            None,
        ));
    }
    if !inputs.runtime.rendezvous.is_fresh() {
        findings.push(Finding::new(
            DiagnosticCode::RendezvousStale,
            DiagnosticCode::RendezvousStale.baseline_severity(),
            None,
            inputs.runtime.rendezvous.age_ms(),
        ));
    }
}

fn collect_authorization(inputs: &HealthInputs<'_>, findings: &mut Vec<Finding>) {
    let (code, measure) = match inputs.http {
        HttpReadiness::Disabled => (Some(DiagnosticCode::HttpDisabled), None),
        HttpReadiness::TrustedLocal { secret } => match secret {
            LocalSecretState::Ready => (None, None),
            LocalSecretState::Missing => (Some(DiagnosticCode::LocalSecretMissing), None),
            LocalSecretState::Unreadable => (Some(DiagnosticCode::LocalSecretUnreadable), None),
            LocalSecretState::InsecurePermissions { mode } => (
                Some(DiagnosticCode::LocalSecretInsecurePermissions),
                Some(u64::from(mode)),
            ),
        },
        HttpReadiness::ProxiedOidc { jwks } => match jwks {
            JwksState::Ready { .. } => (None, None),
            JwksState::Empty => (Some(DiagnosticCode::JwksEmpty), None),
            JwksState::Unreachable => (Some(DiagnosticCode::JwksUnreachable), None),
            JwksState::Stale { age_ms } => (Some(DiagnosticCode::JwksStale), Some(age_ms)),
        },
    };
    if let Some(code) = code {
        findings.push(Finding::new(code, code.baseline_severity(), None, measure));
    }
}

/// The worst severity among one component's findings, and the code that carried
/// it. Ties keep the *first* finding, so the code an operator sees is the first
/// one in collection order at that severity.
fn worst_in(
    component: ComponentName,
    findings: &[Finding],
) -> Option<(DiagnosticSeverity, DiagnosticCode)> {
    findings
        .iter()
        .filter(|finding| finding.component() == component)
        .fold(None, |worst, finding| match worst {
            Some((severity, _)) if severity >= finding.severity() => worst,
            _ => Some((finding.severity(), finding.code())),
        })
}

/// Whether a component may be unavailable without the server having failed.
const fn is_optional(component: ComponentName) -> bool {
    matches!(
        component,
        ComponentName::Godot
            | ComponentName::Editor
            | ComponentName::Runtime
            | ComponentName::Authorization
    )
}

/// One component's report.
///
/// The component status is *not* a rename of the folded severity: a plane whose
/// boundary is absent is `unavailable` for that component even though the
/// finding is informational, because the component genuinely cannot execute. The
/// separation is what lets the server stay `ready` while honestly reporting an
/// unavailable optional plane.
fn component_report(
    component: ComponentName,
    inputs: &HealthInputs<'_>,
    findings: &[Finding],
) -> ComponentReport {
    let worst = worst_in(component, findings);
    let folded = worst.map_or(HealthStatus::Ready, |(severity, _)| status_of(severity));
    let status = match component {
        ComponentName::Project => {
            if matches!(inputs.project.lease, LeaseOwnership::NotHeld) {
                HealthStatus::Unavailable
            } else {
                folded
            }
        }
        ComponentName::Index => match inputs.index {
            IndexFacts::Fresh { .. } => HealthStatus::Ready,
            IndexFacts::Stale { .. } => HealthStatus::Degraded,
            IndexFacts::Unreadable => HealthStatus::Unavailable,
        },
        ComponentName::Godot => {
            if inputs.engine.presence == EnginePresence::Absent {
                HealthStatus::Unavailable
            } else {
                folded
            }
        }
        ComponentName::Editor => plane_status(inputs, EffectPlane::Editor, folded),
        ComponentName::Runtime => plane_status(inputs, EffectPlane::Runtime, folded),
        ComponentName::Scheduler => {
            if inputs.queue.mode == SchedulerMode::Stopped {
                HealthStatus::Unavailable
            } else {
                folded
            }
        }
        ComponentName::Authorization => {
            if inputs.http.is_enabled() {
                folded
            } else {
                HealthStatus::Unavailable
            }
        }
    };
    ComponentReport {
        component,
        status,
        optional: is_optional(component),
        code: worst.map(|(_, code)| code),
    }
}

/// A plane-backed component is unavailable whenever its plane cannot execute.
fn plane_status(
    inputs: &HealthInputs<'_>,
    plane: EffectPlane,
    folded: HealthStatus,
) -> HealthStatus {
    if inputs.planes.plane(plane).is_some_and(PlaneHealth::usable) {
        folded
    } else {
        HealthStatus::Unavailable
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The code region of one source file: everything between the first `use`
    /// declaration and the test module. Module documentation legitimately *names*
    /// the types it forbids, so a scan that included the prose would fail on its
    /// own explanation.
    fn code_region(source: &str) -> &str {
        let start = source.find("\nuse ").map_or(0, |index| index + 1);
        let end = source.find("#[cfg(test)]").unwrap_or(source.len());
        &source[start..end]
    }

    /// The bundle and its findings cannot hold raw material *by shape*. An owned
    /// string, a path or a byte buffer is the only way a secret, a source line or
    /// an absolute private path could reach a rendered diagnostic, so none of
    /// those types may appear in the rendering layer at all.
    #[test]
    fn the_rendering_layer_carries_no_raw_material_type() {
        for (name, source) in [
            ("bundle.rs", include_str!("diagnostics/bundle.rs")),
            ("finding.rs", include_str!("diagnostics/finding.rs")),
            ("metrics.rs", include_str!("diagnostics/metrics.rs")),
        ] {
            let region = code_region(source);
            for forbidden in ["String", "PathBuf", "Vec<u8>", "serde_json", "OsStr"] {
                assert!(
                    !region.contains(forbidden),
                    "{name} must not carry `{forbidden}` in its code region"
                );
            }
        }
    }

    /// The diagnostics layer never unwraps a redaction wrapper. `expose` is the
    /// single deliberate escape hatch in Todo 4's wrappers, and a diagnostics
    /// surface has no legitimate reason to call it.
    #[test]
    fn the_diagnostics_layer_never_exposes_redacted_material() {
        for (name, source) in [
            ("mod.rs", include_str!("diagnostics.rs")),
            ("bundle.rs", include_str!("diagnostics/bundle.rs")),
            ("code.rs", include_str!("diagnostics/code.rs")),
            ("context.rs", include_str!("diagnostics/context.rs")),
            ("facts.rs", include_str!("diagnostics/facts.rs")),
            ("finding.rs", include_str!("diagnostics/finding.rs")),
            ("metrics.rs", include_str!("diagnostics/metrics.rs")),
            ("readiness.rs", include_str!("diagnostics/readiness.rs")),
        ] {
            let region = code_region(source);
            assert!(
                !region.contains(".expose("),
                "{name} must never unwrap a redaction wrapper"
            );
        }
    }

    #[test]
    fn the_context_has_no_explicit_launch_secret_rendering() {
        let source = code_region(include_str!("diagnostics/context.rs"));
        let compact: String = source
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect();
        for forbidden in [
            "bearer_token(",
            ":?}",
            ":#?}",
            "dbg!(",
            "?secret",
            "secret=?",
            "Debug::fmt(",
            "LaunchSecret::path(",
        ] {
            assert!(
                !compact.contains(forbidden),
                "context.rs must not render launch-secret material via `{forbidden}`"
            );
        }

        let path_uses: Vec<usize> = source
            .match_indices("secret.path()")
            .map(|(index, _)| index)
            .collect();
        assert_eq!(
            path_uses.len(),
            1,
            "context.rs may inspect the launch-secret path only in its state classifier"
        );
        let classifier_start = source
            .find("fn local_secret_state(")
            .expect("launch-secret state classifier exists");
        let classifier_end = source
            .find("/// Preserve the original five-component")
            .expect("classifier end marker exists");
        assert!(
            (classifier_start..classifier_end).contains(&path_uses[0]),
            "the launch-secret path must not reach response assembly"
        );
    }

    /// Every component is reported exactly once, so a caller can index the report
    /// by component without checking for duplicates or gaps.
    #[test]
    fn every_component_is_reported_exactly_once() {
        let mut names: Vec<&'static str> = COMPONENTS.into_iter().map(component_label).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total);
        assert_eq!(COMPONENTS.len(), 7);
        for component in COMPONENTS {
            let optional = is_optional(component);
            match component {
                ComponentName::Project | ComponentName::Index | ComponentName::Scheduler => {
                    assert!(!optional, "{} is load-bearing", component_label(component));
                }
                ComponentName::Godot
                | ComponentName::Editor
                | ComponentName::Runtime
                | ComponentName::Authorization => {
                    assert!(optional, "{} is optional", component_label(component));
                }
            }
        }
    }

    /// Advertising a plane can raise a finding to degraded but never to blocked,
    /// so a profile that names an unshipped bridge still leaves the server usable.
    #[test]
    fn advertising_a_plane_never_makes_its_absence_a_failure() {
        for profile in [
            CapabilityProfile::Core,
            CapabilityProfile::Editor,
            CapabilityProfile::Runtime,
            CapabilityProfile::Full,
        ] {
            for code in [
                DiagnosticCode::EditorPlaneNotShipped,
                DiagnosticCode::RuntimePlaneNotShipped,
                DiagnosticCode::LspDapPlaneNotShipped,
                DiagnosticCode::ProcessPlaneUnavailable,
                DiagnosticCode::GodotBinaryAbsent,
            ] {
                let advertised = advertises_editor(profile)
                    || advertises_runtime(profile)
                    || advertises_process(profile)
                    || advertises_lsp_dap(profile);
                let finding = Finding::new(code, severity_for(code, advertised), None, None);
                assert_ne!(
                    finding.severity(),
                    DiagnosticSeverity::Blocked,
                    "{} under {} must not be blocking",
                    code.as_str(),
                    profile.manifest_name()
                );
                assert_ne!(status_of(finding.severity()), HealthStatus::Unavailable);
            }
        }
    }
}
