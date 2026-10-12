//! Bounded local metrics.
//!
//! ## Why the label type is a closed enum
//!
//! Cardinality explosion is the classic metrics failure: one `&str` label fed
//! from a request field, and the registry grows without limit until it is the
//! memory problem it was meant to observe. This registry has **no** string-label
//! API. A dimension can only be a [`MetricDimension`], every variant of which is
//! a closed enum, so the number of series a process can ever create is
//! `MetricName::ALL.len() * (1 + DIMENSION_CARDINALITY)` — a compile-time
//! constant returned by [`MetricsRegistry::bound`].
//!
//! High-cardinality input still needs somewhere to go, so
//! [`MetricsRegistry::note_tool`] funnels an arbitrary tool name through the
//! canonical capability matrix: a known tool lands on its capability, and every
//! unknown tool collapses into one shared bucket. Ten thousand distinct tool
//! names therefore produce at most six series, not ten thousand.

use std::collections::BTreeMap;

use godot_mcp_domain::contracts::Capability;

use super::code::{DiagnosticCode, DiagnosticSeverity};
use crate::scheduler::EffectPlane;

/// One counted quantity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MetricName {
    /// Requests admitted by the scheduler.
    RequestsAdmitted,
    /// Requests refused before execution.
    RequestsRefused,
    /// Observed admission-queue depth.
    QueueDepth,
    /// Refusals attributed to an unavailable plane.
    PlaneRefusals,
    /// Diagnostic findings observed.
    FindingsObserved,
    /// Mutations refused by a durable write block.
    MutationBlocks,
    /// Diagnostic bundles rendered.
    BundlesRendered,
}

impl MetricName {
    /// Every metric, in a stable order.
    pub const ALL: [Self; 7] = [
        Self::RequestsAdmitted,
        Self::RequestsRefused,
        Self::QueueDepth,
        Self::PlaneRefusals,
        Self::FindingsObserved,
        Self::MutationBlocks,
        Self::BundlesRendered,
    ];

    /// Stable snake_case name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RequestsAdmitted => "requests_admitted",
            Self::RequestsRefused => "requests_refused",
            Self::QueueDepth => "queue_depth",
            Self::PlaneRefusals => "plane_refusals",
            Self::FindingsObserved => "findings_observed",
            Self::MutationBlocks => "mutation_blocks",
            Self::BundlesRendered => "bundles_rendered",
        }
    }
}

/// The only label a series may carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MetricDimension {
    /// One effect plane.
    Plane(EffectPlane),
    /// One capability class.
    Capability(Capability),
    /// One diagnostic severity.
    Severity(DiagnosticSeverity),
    /// One diagnostic code.
    Code(DiagnosticCode),
    /// The shared bucket every non-canonical tool name collapses into.
    UnknownTool,
}

impl MetricDimension {
    /// How many distinct labels exist across every variant.
    pub const CARDINALITY: usize = EffectPlane::ALL.len()
        + CAPABILITIES.len()
        + DiagnosticSeverity::ALL.len()
        + DiagnosticCode::ALL.len()
        + 1;

    /// Stable snake_case label, unique across every variant.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Plane(plane) => match plane {
                EffectPlane::File => "plane_file",
                EffectPlane::Process => "plane_process",
                EffectPlane::LspDap => "plane_lsp_dap",
                EffectPlane::Editor => "plane_editor",
                EffectPlane::Runtime => "plane_runtime",
            },
            Self::Capability(capability) => match capability {
                Capability::Read => "capability_read",
                Capability::ProjectWrite => "capability_project_write",
                Capability::EditorMutate => "capability_editor_mutate",
                Capability::RuntimeExecute => "capability_runtime_execute",
                Capability::ExportDestructive => "capability_export_destructive",
            },
            Self::Severity(severity) => match severity {
                DiagnosticSeverity::Info => "severity_info",
                DiagnosticSeverity::Degraded => "severity_degraded",
                DiagnosticSeverity::Blocked => "severity_blocked",
            },
            Self::Code(code) => code.as_str(),
            Self::UnknownTool => "tool_unknown",
        }
    }
}

/// Every capability class, used only to size the dimension bound.
const CAPABILITIES: [Capability; 5] = [
    Capability::Read,
    Capability::ProjectWrite,
    Capability::EditorMutate,
    Capability::RuntimeExecute,
    Capability::ExportDestructive,
];

/// The hard ceiling on how many series one process can hold.
pub const MAX_METRIC_SERIES: usize = MetricName::ALL.len() * (1 + MetricDimension::CARDINALITY);

/// One series' identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
pub struct MetricLabel {
    /// The metric this series counts.
    pub name: MetricName,
    /// The bounded label, or `None` for the undimensioned series.
    pub dimension: Option<&'static str>,
}

impl serde::Serialize for MetricName {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

/// One rendered series.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct MetricSample {
    /// The metric name.
    pub name: &'static str,
    /// The bounded label, or `None`.
    pub dimension: Option<&'static str>,
    /// The counted value.
    pub value: u64,
}

/// A bounded counter registry.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MetricsRegistry {
    series: BTreeMap<(MetricName, Option<&'static str>), u64>,
}

impl MetricsRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            series: BTreeMap::new(),
        }
    }

    /// The compile-time ceiling on series count.
    #[must_use]
    pub const fn bound() -> usize {
        MAX_METRIC_SERIES
    }

    /// Add to one series.
    ///
    /// A new key is refused once the ceiling is reached. The type system already
    /// makes that unreachable; the check is here so the bound is *enforced*
    /// rather than merely argued.
    pub fn observe(&mut self, name: MetricName, dimension: Option<MetricDimension>, delta: u64) {
        let key = (name, dimension.map(MetricDimension::as_str));
        if let Some(value) = self.series.get_mut(&key) {
            *value = value.saturating_add(delta);
            return;
        }
        if self.series.len() < MAX_METRIC_SERIES {
            self.series.insert(key, delta);
        }
    }

    /// Count one call to an arbitrary tool name.
    ///
    /// A canonical tool lands on its capability dimension; anything else collapses
    /// into [`MetricDimension::UnknownTool`], so an attacker-chosen or generated
    /// tool name cannot mint a series.
    pub fn note_tool(&mut self, name: MetricName, tool: &str) {
        let dimension = crate::policy::row_for(tool).map_or(MetricDimension::UnknownTool, |row| {
            MetricDimension::Capability(row.capability)
        });
        self.observe(name, Some(dimension), 1);
    }

    /// One series' value, or zero.
    #[must_use]
    pub fn value(&self, name: MetricName, dimension: Option<MetricDimension>) -> u64 {
        self.series
            .get(&(name, dimension.map(MetricDimension::as_str)))
            .copied()
            .unwrap_or(0)
    }

    /// How many series exist.
    #[must_use]
    pub fn series(&self) -> usize {
        self.series.len()
    }

    /// Every series' identity, in a stable order.
    #[must_use]
    pub fn labels(&self) -> Vec<MetricLabel> {
        self.series
            .keys()
            .map(|&(name, dimension)| MetricLabel { name, dimension })
            .collect()
    }

    /// Every series, in a stable order.
    #[must_use]
    pub fn samples(&self) -> Vec<MetricSample> {
        self.series
            .iter()
            .map(|(&(name, dimension), &value)| MetricSample {
                name: name.as_str(),
                dimension,
                value,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// High-cardinality input cannot create series: every unknown tool name
    /// collapses into one bucket, so the registry stays a fixed size.
    #[test]
    fn high_cardinality_tool_names_cannot_grow_the_registry() {
        let mut registry = MetricsRegistry::new();
        for index in 0..20_000_u32 {
            registry.note_tool(MetricName::RequestsAdmitted, &format!("generated.{index}"));
        }
        assert_eq!(registry.series(), 1);
        assert_eq!(
            registry.value(
                MetricName::RequestsAdmitted,
                Some(MetricDimension::UnknownTool)
            ),
            20_000
        );
        assert!(registry.series() <= MetricsRegistry::bound());
    }

    /// Saturating every possible series still respects the compiled ceiling, and
    /// every label is unique so two dimensions can never share a series.
    #[test]
    fn every_possible_series_stays_inside_the_bound() {
        let mut registry = MetricsRegistry::new();
        let mut labels = Vec::new();
        for plane in EffectPlane::ALL {
            labels.push(MetricDimension::Plane(plane));
        }
        for capability in CAPABILITIES {
            labels.push(MetricDimension::Capability(capability));
        }
        for severity in DiagnosticSeverity::ALL {
            labels.push(MetricDimension::Severity(severity));
        }
        for code in DiagnosticCode::ALL {
            labels.push(MetricDimension::Code(code));
        }
        labels.push(MetricDimension::UnknownTool);
        assert_eq!(labels.len(), MetricDimension::CARDINALITY);

        let mut names: Vec<&'static str> = labels
            .iter()
            .copied()
            .map(MetricDimension::as_str)
            .collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "every dimension label must be unique");

        for name in MetricName::ALL {
            registry.observe(name, None, 1);
            for &dimension in &labels {
                registry.observe(name, Some(dimension), 1);
            }
        }
        assert_eq!(registry.series(), MetricsRegistry::bound());
        assert_eq!(registry.labels().len(), registry.series());
        assert_eq!(registry.samples().len(), registry.series());
    }

    /// A counter saturates rather than wrapping: a wrapped counter would report a
    /// tiny number after a huge amount of work.
    #[test]
    fn a_counter_saturates() {
        let mut registry = MetricsRegistry::new();
        registry.observe(MetricName::QueueDepth, None, u64::MAX);
        registry.observe(MetricName::QueueDepth, None, 5);
        assert_eq!(registry.value(MetricName::QueueDepth, None), u64::MAX);
    }
}
