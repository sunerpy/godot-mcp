//! Router health, and the remediation that makes an unusable plane actionable.
//!
//! Health reports two independent facts per plane — is the boundary compiled into
//! this build, and does the environment provide it — because the fix is different
//! in each case. A missing boundary is a build/release fact an operator cannot
//! change at run time; a missing environment is something they can. Collapsing the
//! two into a single "unavailable" bit would leave the caller with nothing to act
//! on.
//!
//! Nothing here guesses. The write block is read from the scheduler's own durable
//! query, so health can never claim mutations are permitted while the journal says
//! otherwise.

use super::conflict::Remediation;
use super::plan::PlaneObservation;
use crate::scheduler::EffectPlane;

/// One plane's health, with the remediation for whichever fact is missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaneHealth {
    plane: EffectPlane,
    boundary_compiled_in: bool,
    environment_present: bool,
    remediation: Option<Remediation>,
}

impl PlaneHealth {
    /// Which plane this entry describes.
    #[must_use]
    pub const fn plane(&self) -> EffectPlane {
        self.plane
    }

    /// Whether a real boundary implementation is compiled into this build.
    #[must_use]
    pub const fn boundary_compiled_in(&self) -> bool {
        self.boundary_compiled_in
    }

    /// Whether the environment provides the plane right now.
    #[must_use]
    pub const fn environment_present(&self) -> bool {
        self.environment_present
    }

    /// Whether the plane can actually execute. Both facts must hold.
    #[must_use]
    pub const fn usable(&self) -> bool {
        self.boundary_compiled_in && self.environment_present
    }

    /// What an operator can do about an unusable plane.
    #[must_use]
    pub const fn remediation(&self) -> Option<&Remediation> {
        self.remediation.as_ref()
    }
}

/// The router's health, covering every plane and the live write block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouterHealth {
    planes: Vec<PlaneHealth>,
    mutations_blocked: Option<String>,
    observed_at_unix_ms: u64,
}

impl RouterHealth {
    /// Build one plane-health report from an environment observation.
    ///
    /// This is the *only* public entry point, and it delegates to the same
    /// [`health_report`] the router itself calls. The self-diagnostics layer
    /// (Todo 33) composes this report rather than deriving plane facts a second
    /// time: a second derivation could disagree with the one the router already
    /// publishes, and a health surface that contradicts the router is worse than
    /// no health surface at all.
    #[must_use]
    pub fn observe(
        observation: &PlaneObservation,
        mutations_blocked: Option<String>,
        observed_at_unix_ms: u64,
    ) -> Self {
        health_report(observation, mutations_blocked, observed_at_unix_ms)
    }

    /// Every plane's entry, in [`EffectPlane::ALL`] order.
    #[must_use]
    pub fn planes(&self) -> Vec<PlaneHealth> {
        self.planes.clone()
    }

    /// One plane's entry.
    #[must_use]
    pub fn plane(&self, plane: EffectPlane) -> Option<&PlaneHealth> {
        self.planes.iter().find(|entry| entry.plane == plane)
    }

    /// The planes that can execute right now.
    #[must_use]
    pub fn usable_planes(&self) -> Vec<EffectPlane> {
        self.planes
            .iter()
            .filter(|entry| entry.usable())
            .map(|entry| entry.plane)
            .collect()
    }

    /// Why every mutation is currently refused, if it is.
    #[must_use]
    pub fn mutations_blocked(&self) -> Option<&str> {
        self.mutations_blocked.as_deref()
    }

    /// The trusted observation timestamp this report was taken at.
    #[must_use]
    pub const fn observed_at_unix_ms(&self) -> u64 {
        self.observed_at_unix_ms
    }

    /// This report with `plane`'s environment fact replaced by `present`,
    /// and its remediation derived again.
    #[must_use]
    pub(crate) fn with_environment(mut self, plane: EffectPlane, present: bool) -> Self {
        for entry in &mut self.planes {
            if entry.plane == plane {
                entry.environment_present = present;
                entry.remediation = remediation_for(plane, entry.boundary_compiled_in, present);
            }
        }
        self
    }

    /// Say why `plane`'s boundary cannot serve on this host, when it is
    /// attached but has nothing it may contain a launch in.
    #[must_use]
    pub(crate) fn with_unavailable_reason(
        mut self,
        plane: EffectPlane,
        reason: Option<String>,
    ) -> Self {
        let Some(reason) = reason else {
            return self;
        };
        for entry in &mut self.planes {
            if entry.plane == plane && !entry.boundary_compiled_in {
                let mut steps = vec![format!(
                    "this server attaches the {} boundary but cannot use it here: {reason}",
                    plane.as_str()
                )];
                if !entry.environment_present {
                    steps.push(missing_environment_step(plane).to_owned());
                }
                entry.remediation = Some(Remediation::new(steps));
            }
        }
        self
    }

    /// Every remediation this report carries.
    #[must_use]
    pub fn remediation(&self) -> Vec<Remediation> {
        self.planes
            .iter()
            .filter_map(|entry| entry.remediation.clone())
            .collect()
    }
}

/// Build the health report for one observation and one write-block state.
pub(crate) fn health_report(
    observation: &PlaneObservation,
    mutations_blocked: Option<String>,
    observed_at_unix_ms: u64,
) -> RouterHealth {
    let planes = EffectPlane::ALL
        .into_iter()
        .map(|plane| {
            let boundary_compiled_in = observation.boundaries().of(plane);
            let environment_present = observation.environment_admits(plane);
            PlaneHealth {
                plane,
                boundary_compiled_in,
                environment_present,
                remediation: remediation_for(plane, boundary_compiled_in, environment_present),
            }
        })
        .collect();
    RouterHealth {
        planes,
        mutations_blocked,
        observed_at_unix_ms,
    }
}

/// The actionable step for whichever fact is missing.
///
/// The build-side fact is named first because it dominates: attaching an editor
/// to a build that ships no editor bridge changes nothing, so telling an operator
/// to attach one would be misleading.
fn remediation_for(
    plane: EffectPlane,
    boundary_compiled_in: bool,
    environment_present: bool,
) -> Option<Remediation> {
    if boundary_compiled_in && environment_present {
        return None;
    }
    let mut steps = Vec::new();
    if !boundary_compiled_in {
        steps.push(format!(
            "this server attaches no {} effect boundary, so every {} outcome is refused with \
             `unavailable`; only a server started with a profile that serves the plane attaches \
             it",
            plane.as_str(),
            plane.as_str()
        ));
        steps.push(missing_boundary_note(plane).to_owned());
    }
    if !environment_present {
        steps.push(missing_environment_step(plane).to_owned());
    }
    Some(Remediation::new(steps))
}

/// Why a boundary is absent from this build, per plane.
const fn missing_boundary_note(plane: EffectPlane) -> &'static str {
    match plane {
        EffectPlane::File => {
            "the file boundary is the server's own plane; its absence means the project lease was \
             not held"
        }
        EffectPlane::Process => {
            "supervised child processes are not attached to this transport's effect gateway"
        }
        EffectPlane::LspDap => {
            "managed editor sessions host the language server and debug adapter under the \
             runtime and full profiles; this server's assembly attaches none"
        }
        EffectPlane::Editor => {
            "the live editor bridge serves the godot_mcp editor add-on under the editor and full \
             profiles; this server's assembly attaches none"
        }
        EffectPlane::Runtime => {
            "managed game sessions serve runtime reads, input and capture under the runtime and \
             full profiles; this server's assembly attaches none"
        }
    }
}

/// What an operator can change to provide the plane.
const fn missing_environment_step(plane: EffectPlane) -> &'static str {
    match plane {
        EffectPlane::File => {
            "hold the project lease so descriptor-relative writes have a safe root"
        }
        EffectPlane::Process => "install a Godot binary this server can supervise",
        EffectPlane::LspDap => {
            "start a managed editor session with editor.session.start so an editor this server \
             owns hosts the language server and debug adapter"
        }
        EffectPlane::Editor => {
            "install the godot_mcp add-on with editor.addon.install and open the project in the \
             Godot editor so the add-on connects"
        }
        EffectPlane::Runtime => {
            "start a managed game session with project.run.start so a running game can answer"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::plan::{
        EnginePresence, ProtocolFidelity, RuntimePresence, WorkspaceFreshness,
    };
    use crate::scheduler::PlaneAvailability;

    fn observation(boundaries: PlaneAvailability, engine: EnginePresence) -> PlaneObservation {
        PlaneObservation::new(
            boundaries,
            engine,
            RuntimePresence::NoInstance,
            ProtocolFidelity::Full,
            WorkspaceFreshness::Fresh,
        )
    }

    /// Health covers every plane, and every unusable plane is actionable.
    #[test]
    fn every_unusable_plane_carries_actionable_remediation() {
        let report = health_report(
            &observation(
                PlaneAvailability::new(true, false, false, false, false),
                EnginePresence::HeadlessOnly,
            ),
            None,
            7,
        );
        assert_eq!(report.planes().len(), EffectPlane::ALL.len());
        assert_eq!(report.usable_planes(), vec![EffectPlane::File]);
        assert_eq!(report.observed_at_unix_ms(), 7);
        assert!(report.mutations_blocked().is_none());
        for plane in EffectPlane::ALL {
            let entry = report.plane(plane).expect("every plane has an entry");
            match entry.usable() {
                true => assert!(entry.remediation().is_none()),
                false => assert!(
                    entry
                        .remediation()
                        .is_some_and(super::Remediation::is_actionable),
                    "{} must be actionable",
                    plane.as_str()
                ),
            }
        }
    }

    /// A compiled-out boundary is never reported as merely "environment missing":
    /// telling an operator to connect the editor add-on to a server that attaches
    /// no editor bridge would be a false remedy.
    #[test]
    fn a_compiled_out_boundary_dominates_the_remedy() {
        let report = health_report(
            &observation(
                PlaneAvailability::new(true, true, true, false, true),
                EnginePresence::EditorConnected,
            ),
            None,
            0,
        );
        let editor = report.plane(EffectPlane::Editor).expect("an entry");
        assert!(!editor.boundary_compiled_in());
        assert!(editor.environment_present());
        assert!(!editor.usable());
        let steps = editor.remediation().expect("actionable").steps().join(" ");
        assert!(steps.contains("this server attaches no editor effect boundary"));
        assert!(!steps.contains("editor.addon.install"));
    }

    /// A write block is surfaced verbatim rather than summarised away.
    #[test]
    fn a_write_block_is_surfaced_rather_than_summarised() {
        let report = health_report(
            &observation(
                PlaneAvailability::new(true, false, false, false, false),
                EnginePresence::HeadlessOnly,
            ),
            Some("project has quarantined transactions".to_owned()),
            0,
        );
        assert_eq!(
            report.mutations_blocked(),
            Some("project has quarantined transactions")
        );
    }
}
