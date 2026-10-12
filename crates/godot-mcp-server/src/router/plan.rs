//! The plane-selection decision table.
//!
//! One canonical tool, one environment observation, one plan. The plan says which
//! planes the outcome *requires*, which strategy executes it, which planes will
//! actually produce an effect, and whether the answer is degraded. It is a pure
//! function: it opens nothing, spawns nothing and writes nothing.
//!
//! ## The required planes come from the canonical matrix, never from a copy
//!
//! Every row already exists in Todo 26's [capability
//! matrix](crate::policy::matrix), and the matrix is the only place that says what
//! a tool needs. This table therefore *reads* the row's boundary, capability
//! plane and operation family and adds exactly one thing the matrix does not
//! model: whether an outcome spans more than one plane. A second copy of the
//! matrix would be a second source of truth that could disagree with the one the
//! policy layer enforces.
//!
//! ## There is no "fell back to a weaker plane" state
//!
//! [`Degradation`] can only express a *declared reduction in fidelity on the same
//! plane*. It is structurally incapable of saying "the editor plane was required,
//! so a raw text write was used instead", because that sentence is exactly the
//! silent fallback this layer must never perform. An outcome whose required plane
//! cannot execute is refused with `unavailable`; it is never rewritten into a
//! weaker outcome that happens to succeed.

use std::collections::BTreeMap;

use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::contracts::{OperationPlane, OutcomeError};

use crate::journal::EffectBoundaryKind;
use crate::policy::matrix::row_of;
use crate::scheduler::{EffectPlane, PlaneAvailability};
use godot_mcp_domain::catalog::ToolDescriptor;

/// Whether a Godot engine is reachable, and in which shape.
///
/// This is an observation of the environment, never a wish. `HeadlessOnly` means
/// a binary exists and can be supervised as a child process; `EditorConnected`
/// additionally means a live editor is attached, which is what the language,
/// debug and editor-plugin channels need.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EnginePresence {
    /// No Godot binary is reachable at all.
    Absent,
    /// A binary can be supervised, but no editor is attached.
    HeadlessOnly,
    /// A live editor is attached.
    EditorConnected,
}

impl EnginePresence {
    /// Stable snake_case name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::HeadlessOnly => "headless_only",
            Self::EditorConnected => "editor_connected",
        }
    }
}

/// Whether a game instance is running and reachable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RuntimePresence {
    /// No instance is running.
    NoInstance,
    /// An instance is running and answering.
    GameRunning,
}

impl RuntimePresence {
    /// Stable snake_case name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoInstance => "no_instance",
            Self::GameRunning => "game_running",
        }
    }
}

/// Whether a receipt-bound channel negotiated its full feature set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ProtocolFidelity {
    /// Every negotiated capability the outcome needs is present.
    Full,
    /// The peer negotiated a reduced subset. The answer is still real, but it is
    /// narrower, and saying so is mandatory.
    Degraded,
}

impl ProtocolFidelity {
    /// Stable snake_case name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Degraded => "degraded",
        }
    }
}

/// How the workspace's three views of the project relate right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WorkspaceFreshness {
    /// The static index, the bytes on disk and any editor buffer agree.
    Fresh,
    /// The static index is behind the bytes on disk.
    StaleIndex,
    /// A live editor holds a buffer that diverges from the bytes on disk.
    UnsavedEditorBuffer,
}

impl WorkspaceFreshness {
    /// Stable snake_case name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fresh => "fresh",
            Self::StaleIndex => "stale_index",
            Self::UnsavedEditorBuffer => "unsaved_editor_buffer",
        }
    }
}

/// One complete, capability-aware observation of what can execute right now.
///
/// It pairs two independent facts, and both must hold for a plane to be usable:
///
/// 1. the boundary is compiled into this build ([`PlaneAvailability`], derived
///    from the effect gateway's own report); and
/// 2. the environment actually provides it (an engine, an editor, a running
///    instance).
///
/// Keeping them separate is what lets the router answer "why can this not run?"
/// with something actionable instead of a bare refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaneObservation {
    boundaries: PlaneAvailability,
    engine: EnginePresence,
    runtime: RuntimePresence,
    protocol: ProtocolFidelity,
    freshness: WorkspaceFreshness,
}

impl PlaneObservation {
    /// Record one observation.
    #[must_use]
    pub const fn new(
        boundaries: PlaneAvailability,
        engine: EnginePresence,
        runtime: RuntimePresence,
        protocol: ProtocolFidelity,
        freshness: WorkspaceFreshness,
    ) -> Self {
        Self {
            boundaries,
            engine,
            runtime,
            protocol,
            freshness,
        }
    }

    /// Which boundaries are compiled into this build.
    #[must_use]
    pub const fn boundaries(&self) -> PlaneAvailability {
        self.boundaries
    }

    /// Replace the compiled-in boundary set.
    ///
    /// The router calls this with the effect gateway's own report before it
    /// executes anything, so a caller-supplied observation can widen a *plan* but
    /// can never widen what actually runs.
    #[must_use]
    pub const fn with_boundaries(mut self, boundaries: PlaneAvailability) -> Self {
        self.boundaries = boundaries;
        self
    }

    /// Observed engine presence.
    #[must_use]
    pub const fn engine(&self) -> EnginePresence {
        self.engine
    }

    /// Observed running-instance presence.
    #[must_use]
    pub const fn runtime(&self) -> RuntimePresence {
        self.runtime
    }

    /// Observed protocol fidelity.
    #[must_use]
    pub const fn protocol(&self) -> ProtocolFidelity {
        self.protocol
    }

    /// Observed workspace freshness.
    #[must_use]
    pub const fn freshness(&self) -> WorkspaceFreshness {
        self.freshness
    }

    /// Whether the environment alone admits one plane, ignoring whether the
    /// boundary is compiled in.
    #[must_use]
    pub const fn environment_admits(&self, plane: EffectPlane) -> bool {
        match plane {
            // Descriptor-relative project writes are the server's own plane; they
            // never needed a Godot process.
            EffectPlane::File => true,
            EffectPlane::Process => !matches!(self.engine, EnginePresence::Absent),
            // Godot's language and debug adapters are hosted by the editor, so a
            // headless binary does not provide them.
            EffectPlane::LspDap | EffectPlane::Editor => {
                matches!(self.engine, EnginePresence::EditorConnected)
            }
            EffectPlane::Runtime => matches!(self.runtime, RuntimePresence::GameRunning),
        }
    }

    /// Whether one plane can actually execute: compiled in *and* provided.
    #[must_use]
    pub const fn effective(&self, plane: EffectPlane) -> bool {
        self.boundaries.of(plane) && self.environment_admits(plane)
    }
}

/// Which executor answers one outcome.
///
/// These are the five classes the architecture names. `ReceiptBoundLspDap` covers
/// every channel whose answer is bound to a receipt identity tuple — Todo 18's
/// language/debug adapters and the runtime bridge alike — because they share the
/// same receipt discipline. The plane itself is always reported separately, so
/// nothing about *where* the work ran is hidden by the coarser class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ExecutionStrategy {
    /// The server's own static plane: project bytes are read, and
    /// descriptor-relative project files are staged and published through the
    /// `file` boundary. No Godot process and no editor is involved.
    StaticAnalysis,
    /// A supervised Godot child process.
    Subprocess,
    /// A receipt-bound channel: the language/debug adapters or the runtime bridge.
    ReceiptBoundLspDap,
    /// The live editor plugin bridge.
    Editor,
    /// More than one plane in one outcome, in a fixed order.
    Composed,
}

impl ExecutionStrategy {
    /// Stable snake_case name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StaticAnalysis => "static_analysis",
            Self::Subprocess => "subprocess",
            Self::ReceiptBoundLspDap => "receipt_bound_lsp_dap",
            Self::Editor => "editor",
            Self::Composed => "composed",
        }
    }
}

/// Why an answer is narrower than the outcome's full fidelity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DegradationReason {
    /// The static index is behind the bytes on disk, so it is refreshed first.
    StaleStaticIndex,
    /// A live editor buffer diverges from the bytes on disk, and this answer is
    /// the bytes on disk. Saying so is mandatory: a disk answer must never be
    /// presented as if it were the live editor state.
    UnsavedEditorBufferDivergesFromDisk,
    /// The peer negotiated a reduced protocol feature set.
    NegotiatedProtocolSubset,
}

impl DegradationReason {
    /// Stable snake_case name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StaleStaticIndex => "stale_static_index",
            Self::UnsavedEditorBufferDivergesFromDisk => "unsaved_editor_buffer_diverges_from_disk",
            Self::NegotiatedProtocolSubset => "negotiated_protocol_subset",
        }
    }
}

/// How an answer's fidelity compares with the outcome's full fidelity.
///
/// There are exactly two states, and that is the point: an answer is either full
/// fidelity, or it is a *declared* reduction **on the plane the outcome
/// required**. There is deliberately no variant meaning "a different, weaker
/// plane was substituted", because a silent substitution is the failure mode this
/// whole layer exists to prevent. An outcome whose required plane cannot run is
/// refused, not rewritten.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Degradation {
    /// Full fidelity.
    None,
    /// The required plane answered, with a declared reduction.
    ReducedFidelity {
        /// The plane that answered. It is always the required one.
        plane: EffectPlane,
        /// Why the answer is narrower.
        reason: DegradationReason,
    },
}

impl Degradation {
    /// Whether the answer is degraded.
    #[must_use]
    pub const fn is_degraded(self) -> bool {
        matches!(self, Self::ReducedFidelity { .. })
    }

    /// Stable snake_case name of the reason, when there is one.
    #[must_use]
    pub const fn reason_name(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::ReducedFidelity { reason, .. } => Some(reason.as_str()),
        }
    }
}

/// One outcome's complete routing decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutePlan {
    tool: String,
    descriptor: &'static ToolDescriptor,
    required: Vec<EffectPlane>,
    effects: Vec<EffectPlane>,
    strategy: ExecutionStrategy,
    degradation: Degradation,
    outcome_plane: OperationPlane,
    requires_editor_consult: bool,
    requires_snapshot_refresh: bool,
}

impl RoutePlan {
    /// Canonical tool this plan is for.
    #[must_use]
    pub fn tool(&self) -> &str {
        &self.tool
    }

    /// The catalog descriptor this plan was derived from.
    #[must_use]
    pub const fn descriptor(&self) -> &'static ToolDescriptor {
        self.descriptor
    }

    /// Planes the outcome requires, in execution order.
    ///
    /// Empty means the outcome is answered from project bytes with no boundary at
    /// all.
    #[must_use]
    pub fn required_planes(&self) -> Vec<EffectPlane> {
        self.required.clone()
    }

    /// Planes that will produce an effect. Empty for every read.
    #[must_use]
    pub fn effects(&self) -> Vec<EffectPlane> {
        self.effects.clone()
    }

    /// Which executor answers this outcome.
    #[must_use]
    pub const fn strategy(&self) -> ExecutionStrategy {
        self.strategy
    }

    /// Whether this answer is full fidelity or a declared reduction.
    #[must_use]
    pub const fn degradation(&self) -> Degradation {
        self.degradation
    }

    /// The public outcome plane the contract reports.
    #[must_use]
    pub const fn outcome_plane(&self) -> OperationPlane {
        self.outcome_plane
    }

    /// Whether the outcome mutates anything.
    #[must_use]
    pub fn is_mutation(&self) -> bool {
        !self.effects.is_empty()
    }

    /// Whether a live editor buffer must be consulted before bytes are written.
    #[must_use]
    pub const fn requires_editor_consult(&self) -> bool {
        self.requires_editor_consult
    }

    /// Whether an affected snapshot must be refreshed before or after the answer.
    #[must_use]
    pub const fn requires_snapshot_refresh(&self) -> bool {
        self.requires_snapshot_refresh
    }

    /// The plane whose boundary produces this outcome's effect, if any.
    ///
    /// For a composed mutation this is the *first* effect plane, which is the one
    /// admission must serialize on.
    #[must_use]
    pub fn primary_effect_plane(&self) -> Option<EffectPlane> {
        self.effects.first().copied()
    }
}

/// Which planes one canonical tool needs beyond its matrix row's boundary.
///
/// The spans are catalog facts about the operation, not preferences: for example
/// `project.import.run` supervises a Godot child whose purpose is to rewrite
/// import metadata (process, then file), and `scene.save` must consult a live
/// editor that may hold the authoritative buffer before any byte is written.
fn composition_of(descriptor: &ToolDescriptor) -> Option<(Vec<EffectPlane>, bool)> {
    if descriptor.composed.is_empty() {
        return None;
    }
    Some((
        descriptor
            .composed
            .iter()
            .map(|boundary| plane_of_boundary(crate::policy::matrix::boundary_kind(*boundary)))
            .collect(),
        descriptor.consult_editor,
    ))
}

/// The plane one effect boundary belongs to.
pub(crate) const fn plane_of_boundary(boundary: EffectBoundaryKind) -> EffectPlane {
    match boundary {
        EffectBoundaryKind::File => EffectPlane::File,
        EffectBoundaryKind::Process => EffectPlane::Process,
        EffectBoundaryKind::LspDap => EffectPlane::LspDap,
        EffectBoundaryKind::Editor => EffectPlane::Editor,
        EffectBoundaryKind::Runtime => EffectPlane::Runtime,
    }
}

/// Build the plan for one canonical tool under one observation.
///
/// # Errors
/// Returns `not_found` for a tool with no canonical matrix row. There is no
/// permissive default: a tool the matrix does not name is not routable.
pub(crate) fn plan_for(
    tool: &str,
    observation: &PlaneObservation,
) -> Result<RoutePlan, OutcomeError> {
    let descriptor =
        godot_mcp_domain::catalog::descriptor(tool).ok_or_else(|| unknown_operation(tool))?;
    Ok(plan_for_descriptor(descriptor, observation))
}

/// Build the plan for one already-resolved catalog descriptor.
pub(crate) fn plan_for_descriptor(
    descriptor: &'static ToolDescriptor,
    observation: &PlaneObservation,
) -> RoutePlan {
    let row = row_of(descriptor);
    let row_plane = row.boundary.map(plane_of_boundary);
    let (required, requires_editor_consult) = match composition_of(descriptor) {
        Some((planes, consult)) => (planes, consult),
        None => (row_plane.into_iter().collect::<Vec<_>>(), false),
    };
    // Only a row with an operation family produces a side effect. A read that is
    // answered *through* a live plane still names that plane in `required`, but it
    // must never be reported as an effect.
    let effects = if row.requires_permit() {
        required.clone()
    } else {
        Vec::new()
    };
    let strategy = strategy_for(&required);
    let degradation = degradation_for(&required, observation);
    RoutePlan {
        tool: descriptor.name.to_owned(),
        descriptor,
        required,
        effects,
        strategy,
        degradation,
        outcome_plane: row.plane,
        requires_editor_consult,
        requires_snapshot_refresh: matches!(
            observation.freshness(),
            WorkspaceFreshness::StaleIndex
        ),
    }
}

/// The one mapping from a required plane set to an executor class.
fn strategy_for(required: &[EffectPlane]) -> ExecutionStrategy {
    match required {
        [] | [EffectPlane::File] => ExecutionStrategy::StaticAnalysis,
        [EffectPlane::Process] => ExecutionStrategy::Subprocess,
        [EffectPlane::LspDap | EffectPlane::Runtime] => ExecutionStrategy::ReceiptBoundLspDap,
        [EffectPlane::Editor] => ExecutionStrategy::Editor,
        _ => ExecutionStrategy::Composed,
    }
}

/// Whether the answer is degraded, and why.
///
/// The three cases are the only reductions this layer can express, and each one
/// keeps the answer on the plane the outcome required:
///
/// * a static/`file` answer taken while the index lags disk;
/// * a static/`file` answer taken while an editor buffer diverges from disk —
///   declared, never presented as live editor state; and
/// * a receipt-bound answer over a peer that negotiated a subset.
///
/// A read that *requires* a live plane is never degraded: it is answered live or
/// refused. That is why the editor and runtime planes never appear here.
fn degradation_for(required: &[EffectPlane], observation: &PlaneObservation) -> Degradation {
    if required.contains(&EffectPlane::LspDap)
        && observation.protocol() == ProtocolFidelity::Degraded
    {
        return Degradation::ReducedFidelity {
            plane: EffectPlane::LspDap,
            reason: DegradationReason::NegotiatedProtocolSubset,
        };
    }
    let answers_from_disk = required.is_empty() || required == [EffectPlane::File];
    if !answers_from_disk {
        return Degradation::None;
    }
    match observation.freshness() {
        WorkspaceFreshness::Fresh => Degradation::None,
        WorkspaceFreshness::StaleIndex => Degradation::ReducedFidelity {
            plane: EffectPlane::File,
            reason: DegradationReason::StaleStaticIndex,
        },
        WorkspaceFreshness::UnsavedEditorBuffer => Degradation::ReducedFidelity {
            plane: EffectPlane::File,
            reason: DegradationReason::UnsavedEditorBufferDivergesFromDisk,
        },
    }
}

/// The refusal for a tool the canonical matrix does not name.
pub(crate) fn unknown_operation(tool: &str) -> OutcomeError {
    OutcomeError::new(
        ErrorKind::NotFound,
        format!("operation {tool} is not a canonical capability-matrix operation"),
        false,
        BTreeMap::new(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(
        engine: EnginePresence,
        runtime: RuntimePresence,
        protocol: ProtocolFidelity,
        freshness: WorkspaceFreshness,
    ) -> PlaneObservation {
        PlaneObservation::new(
            PlaneAvailability::new(true, true, true, true, true),
            engine,
            runtime,
            protocol,
            freshness,
        )
    }

    fn everything() -> PlaneObservation {
        observation(
            EnginePresence::EditorConnected,
            RuntimePresence::GameRunning,
            ProtocolFidelity::Full,
            WorkspaceFreshness::Fresh,
        )
    }

    /// Every canonical manifest tool has exactly one plan, editor and runtime
    /// tools included: a plane whose bridge is absent still routes, and is still
    /// refused honestly at execution time.
    #[test]
    fn every_canonical_tool_has_exactly_one_plan() {
        use godot_mcp_domain::CapabilityProfile;
        use godot_mcp_domain::manifests::canonical_manifest;

        let observation = everything();
        for profile in [
            CapabilityProfile::Core,
            CapabilityProfile::Editor,
            CapabilityProfile::Runtime,
            CapabilityProfile::Full,
        ] {
            for tool in &canonical_manifest(profile).tools {
                let plan = plan_for(tool, &observation)
                    .unwrap_or_else(|error| panic!("{tool} has no plan: {error}"));
                assert_eq!(plan.tool(), tool);
                assert!(
                    plan.effects().is_empty() || plan.effects() == plan.required_planes(),
                    "{tool} must produce effects only on the planes it requires"
                );
            }
        }
        assert_eq!(
            plan_for("not.a.tool", &observation)
                .expect_err("an unknown tool is not routable")
                .code(),
            "not_found"
        );
    }

    /// The plan of one frozen version 0.1 planned operation shape. These shapes
    /// are not catalog tools; they keep the live-plane rules exercised until the
    /// milestones that implement them land.
    fn historical(tool: &str, observation: &PlaneObservation) -> RoutePlan {
        plan_for_descriptor(
            godot_mcp_domain::catalog::historical_v0_1_descriptor(tool)
                .unwrap_or_else(|| panic!("{tool} is a version 0.1 planned operation")),
            observation,
        )
    }

    /// A read is never reported as an effect, even when it reads through a live
    /// plane whose boundary would otherwise mint a permit.
    #[test]
    fn a_read_through_a_live_plane_is_never_an_effect() {
        for tool in [
            "editor.state.get",
            "editor.selection.get",
            "runtime.tree.get",
            "debug.session.get",
        ] {
            let plan = historical(tool, &everything());
            assert!(
                !plan.required_planes().is_empty(),
                "{tool} reads through a plane"
            );
            assert!(plan.effects().is_empty(), "{tool} must report no effect");
            assert!(!plan.is_mutation());
        }
    }

    /// The environment gates each plane exactly once, and the file plane never
    /// depends on Godot.
    #[test]
    fn the_environment_gates_each_plane_exactly_once() {
        let absent = observation(
            EnginePresence::Absent,
            RuntimePresence::NoInstance,
            ProtocolFidelity::Full,
            WorkspaceFreshness::Fresh,
        );
        assert!(absent.environment_admits(EffectPlane::File));
        assert!(!absent.environment_admits(EffectPlane::Process));
        assert!(!absent.environment_admits(EffectPlane::LspDap));
        assert!(!absent.environment_admits(EffectPlane::Editor));
        assert!(!absent.environment_admits(EffectPlane::Runtime));

        let headless = observation(
            EnginePresence::HeadlessOnly,
            RuntimePresence::NoInstance,
            ProtocolFidelity::Full,
            WorkspaceFreshness::Fresh,
        );
        assert!(headless.environment_admits(EffectPlane::Process));
        assert!(!headless.environment_admits(EffectPlane::Editor));

        let attached = everything();
        for plane in EffectPlane::ALL {
            assert!(attached.environment_admits(plane));
        }
    }

    /// A compiled-out boundary cannot be claimed into existence by an observation.
    #[test]
    fn a_missing_boundary_cannot_be_claimed_into_existence() {
        let claimed = PlaneObservation::new(
            PlaneAvailability::new(true, false, false, false, false),
            EnginePresence::EditorConnected,
            RuntimePresence::GameRunning,
            ProtocolFidelity::Full,
            WorkspaceFreshness::Fresh,
        );
        assert!(claimed.effective(EffectPlane::File));
        for plane in [
            EffectPlane::Process,
            EffectPlane::LspDap,
            EffectPlane::Editor,
            EffectPlane::Runtime,
        ] {
            assert!(
                claimed.environment_admits(plane),
                "the environment provides {}",
                plane.as_str()
            );
            assert!(
                !claimed.effective(plane),
                "but {} has no boundary in this build",
                plane.as_str()
            );
        }
    }

    /// Degradation only ever reduces fidelity on the required plane.
    #[test]
    fn degradation_never_substitutes_a_weaker_plane() {
        for tool in ["node.create", "editor.state.get", "runtime.tree.get"] {
            for freshness in [
                WorkspaceFreshness::Fresh,
                WorkspaceFreshness::StaleIndex,
                WorkspaceFreshness::UnsavedEditorBuffer,
            ] {
                let plan = historical(
                    tool,
                    &observation(
                        EnginePresence::EditorConnected,
                        RuntimePresence::GameRunning,
                        ProtocolFidelity::Full,
                        freshness,
                    ),
                );
                assert_eq!(
                    plan.degradation(),
                    Degradation::None,
                    "{tool} answers live or not at all; it must never degrade to disk"
                );
            }
        }
        let plan = plan_for(
            "scene.tree.get",
            &observation(
                EnginePresence::EditorConnected,
                RuntimePresence::GameRunning,
                ProtocolFidelity::Full,
                WorkspaceFreshness::UnsavedEditorBuffer,
            ),
        )
        .expect("a canonical read");
        match plan.degradation() {
            Degradation::ReducedFidelity { plane, reason } => {
                assert_eq!(plane, EffectPlane::File);
                assert_eq!(
                    reason,
                    DegradationReason::UnsavedEditorBufferDivergesFromDisk
                );
            }
            Degradation::None => panic!("a diverging buffer must be declared"),
        }
    }

    /// Exactly two outcomes span more than one plane, and both are composed.
    #[test]
    fn only_the_two_genuinely_multi_plane_outcomes_are_composed() {
        use godot_mcp_domain::CapabilityProfile;
        use godot_mcp_domain::manifests::canonical_manifest;

        let observation = everything();
        let mut composed = Vec::new();
        for tool in &canonical_manifest(CapabilityProfile::Full).tools {
            let plan = plan_for(tool, &observation).expect("a canonical tool");
            if plan.strategy() == ExecutionStrategy::Composed {
                composed.push(tool.clone());
            }
        }
        composed.sort();
        assert_eq!(composed, vec!["project.import.run", "scene.save"]);
    }
}
