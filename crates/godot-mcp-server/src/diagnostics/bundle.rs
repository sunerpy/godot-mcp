//! The redacted diagnostic bundle.
//!
//! ## Redaction is a property of the type, not of a policy
//!
//! Nothing in this module can hold raw material. Every textual field is a
//! `&'static str` chosen from a closed enum, every identity is an
//! [`AuditDigest`], and every detail is a `u64`. There is deliberately no
//! owned-string, path or byte-buffer field anywhere in the bundle, so an absolute
//! private path, a source line or a secret cannot appear in a rendered bundle
//! even if a caller supplied one as a fact. A module-level self-audit in the
//! parent module enforces that shape, so a future field of the wrong type fails
//! the build's own test rather than leaking quietly.
//!
//! ## Audience only removes, never adds
//!
//! [`BundleAudience::RemoteUnauthorized`] drops subject digests and numeric
//! measures. Those are not secrets — a digest is one-way and a measure is a
//! count — but an unauthenticated caller has no business learning how many
//! transactions a project has quarantined, so the narrower view is the default
//! for anyone who has not authenticated.

use godot_mcp_domain::contracts::{ComponentName, HealthStatus};

use super::ServerHealth;
use super::code::DiagnosticCode;
use super::finding::Finding;
use super::metrics::{MetricSample, MetricsRegistry};
use crate::audit::AuditDigest;

/// Who the bundle is being rendered for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BundleAudience {
    /// The local operator running the process.
    LocalOperator,
    /// An authenticated remote caller.
    RemoteAuthorized,
    /// A caller that presented no acceptable credential.
    RemoteUnauthorized,
}

impl BundleAudience {
    /// Stable snake_case name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LocalOperator => "local_operator",
            Self::RemoteAuthorized => "remote_authorized",
            Self::RemoteUnauthorized => "remote_unauthorized",
        }
    }

    /// Whether this audience receives subject digests and numeric measures.
    #[must_use]
    pub const fn receives_detail(self) -> bool {
        matches!(self, Self::LocalOperator | Self::RemoteAuthorized)
    }
}

/// Stable snake_case label for a public health component.
#[must_use]
pub const fn component_label(component: ComponentName) -> &'static str {
    match component {
        ComponentName::Project => "project",
        ComponentName::Index => "index",
        ComponentName::Godot => "godot",
        ComponentName::Editor => "editor",
        ComponentName::Runtime => "runtime",
        ComponentName::Scheduler => "scheduler",
        ComponentName::Authorization => "authorization",
    }
}

/// One finding as it appears in a bundle.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct BundleFinding {
    code: &'static str,
    severity: &'static str,
    component: &'static str,
    subject: Option<AuditDigest>,
    measure: Option<u64>,
    remediation: &'static str,
}

impl BundleFinding {
    /// The stable machine-readable code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        self.code
    }

    /// The severity label.
    #[must_use]
    pub const fn severity(&self) -> &'static str {
        self.severity
    }

    /// The component label.
    #[must_use]
    pub const fn component(&self) -> &'static str {
        self.component
    }

    /// The subject digest, when this audience receives one.
    #[must_use]
    pub fn subject(&self) -> Option<&AuditDigest> {
        self.subject.as_ref()
    }

    /// The numeric measure, when this audience receives one.
    #[must_use]
    pub const fn measure(&self) -> Option<u64> {
        self.measure
    }

    /// The remediation hint.
    #[must_use]
    pub const fn remediation(&self) -> &'static str {
        self.remediation
    }
}

/// One readiness surface as it appears in a bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct BundleReadiness {
    surface: &'static str,
    ready: bool,
}

impl BundleReadiness {
    /// The surface label.
    #[must_use]
    pub const fn surface(&self) -> &'static str {
        self.surface
    }

    /// Whether the surface is ready.
    #[must_use]
    pub const fn ready(&self) -> bool {
        self.ready
    }
}

/// A self-contained, redacted snapshot an operator can attach to a bug report.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DiagnosticBundle {
    schema_version: &'static str,
    generated_at_unix_ms: u64,
    server_version: &'static str,
    profile: &'static str,
    transport: &'static str,
    audience: &'static str,
    status: HealthStatus,
    readiness: Vec<BundleReadiness>,
    findings: Vec<BundleFinding>,
    metrics: Vec<MetricSample>,
}

impl DiagnosticBundle {
    /// Render one bundle for one audience.
    #[must_use]
    pub(super) fn render(
        health: &ServerHealth,
        metrics: &MetricsRegistry,
        audience: BundleAudience,
    ) -> Self {
        let detail = audience.receives_detail();
        let findings: &[Finding] = health.findings();
        Self {
            schema_version: godot_mcp_domain::CONTRACT_VERSION,
            generated_at_unix_ms: health.observed_at_unix_ms(),
            server_version: crate::SERVER_VERSION,
            profile: health.profile().manifest_name(),
            transport: health.transport().as_str(),
            audience: audience.as_str(),
            status: health.status(),
            readiness: health
                .readiness()
                .surfaces()
                .into_iter()
                .map(|(surface, ready)| BundleReadiness {
                    surface: surface.as_str(),
                    ready,
                })
                .collect(),
            findings: findings
                .iter()
                .map(|finding| BundleFinding {
                    code: finding.code().as_str(),
                    severity: finding.severity().as_str(),
                    component: component_label(finding.component()),
                    subject: if detail {
                        finding.subject().cloned()
                    } else {
                        None
                    },
                    measure: if detail { finding.measure() } else { None },
                    remediation: finding.remediation(),
                })
                .collect(),
            metrics: metrics.samples(),
        }
    }

    /// The contract version this bundle's shape belongs to.
    #[must_use]
    pub const fn schema_version(&self) -> &'static str {
        self.schema_version
    }

    /// When the bundle was rendered.
    #[must_use]
    pub const fn generated_at_unix_ms(&self) -> u64 {
        self.generated_at_unix_ms
    }

    /// The server build this bundle came from.
    #[must_use]
    pub const fn server_version(&self) -> &'static str {
        self.server_version
    }

    /// The advertised profile.
    #[must_use]
    pub const fn profile(&self) -> &'static str {
        self.profile
    }

    /// The transport that answered.
    #[must_use]
    pub const fn transport(&self) -> &'static str {
        self.transport
    }

    /// Who this rendering was for.
    #[must_use]
    pub const fn audience(&self) -> &'static str {
        self.audience
    }

    /// The overall status.
    #[must_use]
    pub const fn status(&self) -> HealthStatus {
        self.status
    }

    /// Every readiness surface.
    #[must_use]
    pub fn readiness(&self) -> &[BundleReadiness] {
        &self.readiness
    }

    /// Every finding.
    #[must_use]
    pub fn findings(&self) -> &[BundleFinding] {
        &self.findings
    }

    /// Every metric series.
    #[must_use]
    pub fn metrics(&self) -> &[MetricSample] {
        &self.metrics
    }

    /// Whether one code is present.
    #[must_use]
    pub fn has(&self, code: DiagnosticCode) -> bool {
        self.findings
            .iter()
            .any(|finding| finding.code == code.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use godot_mcp_domain::contracts::ComponentName;

    /// Every component has a unique stable label, so two components can never be
    /// confused in a machine-read bundle.
    #[test]
    fn every_component_label_is_unique() {
        let components = [
            ComponentName::Project,
            ComponentName::Index,
            ComponentName::Godot,
            ComponentName::Editor,
            ComponentName::Runtime,
            ComponentName::Scheduler,
            ComponentName::Authorization,
        ];
        let mut labels: Vec<&'static str> = components.into_iter().map(component_label).collect();
        let total = labels.len();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), total);
    }

    /// Only an authenticated audience receives detail.
    #[test]
    fn an_unauthenticated_audience_receives_no_detail() {
        assert!(BundleAudience::LocalOperator.receives_detail());
        assert!(BundleAudience::RemoteAuthorized.receives_detail());
        assert!(!BundleAudience::RemoteUnauthorized.receives_detail());
        assert_eq!(
            BundleAudience::RemoteUnauthorized.as_str(),
            "remote_unauthorized"
        );
    }
}
