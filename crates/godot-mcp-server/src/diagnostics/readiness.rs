//! The readiness state machine.
//!
//! Readiness is answered per *surface*, not as one boolean, because the surfaces
//! fail independently: a server whose editor bridge is absent still serves reads
//! and still mutates project files, and saying "not ready" would be a lie that
//! costs the caller every working capability.
//!
//! ## Mutation readiness is the strict one
//!
//! [`ReadinessSurface::Mutate`] is false while *any* collected finding reports
//! `blocks_mutation`, and both an unresolved launch survivor and an unreconciled
//! temporary-settings state do. Neither one alone is enough to reopen writes:
//! the decision reads the finding set, so removing one blocker leaves the other
//! in place by construction rather than by a remembered rule.

use godot_mcp_domain::contracts::ComponentName;

use super::code::{DiagnosticCode, DiagnosticSeverity};
use super::facts::{HealthInputs, IndexFacts, LeaseOwnership, TransportSurface};
use super::finding::Finding;
use crate::router::{EnginePresence, PlaneHealth, RuntimePresence};
use crate::scheduler::EffectPlane;

/// One independently answerable readiness surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ReadinessSurface {
    /// The transport can answer at all.
    Serve,
    /// Read outcomes can be answered.
    Read,
    /// Mutating outcomes can be admitted.
    Mutate,
    /// Editor-plane outcomes can be answered.
    Editor,
    /// Runtime-plane outcomes can be answered.
    Runtime,
    /// The HTTP surface can accept a remote caller.
    Http,
}

impl ReadinessSurface {
    /// Every surface, in a stable order.
    pub const ALL: [Self; 6] = [
        Self::Serve,
        Self::Read,
        Self::Mutate,
        Self::Editor,
        Self::Runtime,
        Self::Http,
    ];

    /// Stable snake_case name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Serve => "serve",
            Self::Read => "read",
            Self::Mutate => "mutate",
            Self::Editor => "editor",
            Self::Runtime => "runtime",
            Self::Http => "http",
        }
    }
}

/// Readiness for every surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Readiness {
    serve: bool,
    read: bool,
    mutate: bool,
    editor: bool,
    runtime: bool,
    http: bool,
}

impl Readiness {
    /// Decide every surface from the observed facts and the collected findings.
    #[must_use]
    pub(super) fn decide(inputs: &HealthInputs<'_>, findings: &[Finding]) -> Self {
        let read = !matches!(inputs.index, IndexFacts::Unreadable)
            && !matches!(inputs.project.lease, LeaseOwnership::NotHeld)
            && !has(findings, DiagnosticCode::SchedulerStopped);
        let mutate = read
            && !findings.iter().any(Finding::blocks_mutation)
            && inputs.journal.settings.is_complete();
        let http = inputs.http.is_enabled()
            && !findings.iter().any(|finding| {
                finding.component() == ComponentName::Authorization
                    && finding.severity() == DiagnosticSeverity::Blocked
            });
        let editor = inputs
            .planes
            .plane(EffectPlane::Editor)
            .is_some_and(PlaneHealth::usable)
            && inputs.engine.presence == EnginePresence::EditorConnected
            && !has(findings, DiagnosticCode::PluginProtocolMismatch);
        let runtime = inputs
            .planes
            .plane(EffectPlane::Runtime)
            .is_some_and(PlaneHealth::usable)
            && inputs.runtime.presence == RuntimePresence::GameRunning
            && inputs.runtime.heartbeat.is_fresh()
            && inputs.runtime.autoload.is_settled()
            && inputs.runtime.rendezvous.is_fresh();
        let serve = read && (inputs.transport == TransportSurface::Stdio || http);
        Self {
            serve,
            read,
            mutate,
            editor,
            runtime,
            http,
        }
    }

    /// Whether one surface is ready.
    #[must_use]
    pub const fn of(&self, surface: ReadinessSurface) -> bool {
        match surface {
            ReadinessSurface::Serve => self.serve,
            ReadinessSurface::Read => self.read,
            ReadinessSurface::Mutate => self.mutate,
            ReadinessSurface::Editor => self.editor,
            ReadinessSurface::Runtime => self.runtime,
            ReadinessSurface::Http => self.http,
        }
    }

    /// Every surface with its verdict, in [`ReadinessSurface::ALL`] order.
    #[must_use]
    pub fn surfaces(&self) -> Vec<(ReadinessSurface, bool)> {
        ReadinessSurface::ALL
            .into_iter()
            .map(|surface| (surface, self.of(surface)))
            .collect()
    }
}

fn has(findings: &[Finding], code: DiagnosticCode) -> bool {
    findings.iter().any(|finding| finding.code() == code)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every surface is addressable and named, so a caller can branch on a stable
    /// string rather than positional order.
    #[test]
    fn every_surface_is_named_and_addressable() {
        let readiness = Readiness {
            serve: true,
            read: true,
            mutate: false,
            editor: false,
            runtime: false,
            http: true,
        };
        assert_eq!(readiness.surfaces().len(), ReadinessSurface::ALL.len());
        assert!(readiness.of(ReadinessSurface::Serve));
        assert!(!readiness.of(ReadinessSurface::Mutate));
        let mut names: Vec<&'static str> = ReadinessSurface::ALL
            .into_iter()
            .map(ReadinessSurface::as_str)
            .collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total);
    }
}
