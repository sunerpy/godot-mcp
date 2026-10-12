//! Precedence between the editor buffer, the bytes on disk and the static index —
//! and the actionable conflict that must be returned when they disagree.
//!
//! Three views of the same project exist at once, and they can disagree:
//!
//! 1. a **live editor buffer**, which may hold an unsaved edit;
//! 2. the **bytes on disk**, which is what every `file`-plane write compares and
//!    replaces; and
//! 3. the **static index**, which is derived from the bytes on disk and can lag
//!    them.
//!
//! ## The precedence rule
//!
//! A live plane always answers from live state, and a live editor buffer outranks
//! disk. The index never outranks anything: it is derived, so when it lags it is
//! refreshed rather than trusted.
//!
//! ## Why a diverging buffer is a conflict and not a fallback
//!
//! When a `file`-plane mutation is asked to write a path whose authoritative
//! content lives in an unsaved editor buffer, there are exactly three things the
//! router could do, and two of them are data loss:
//!
//! * overwrite the bytes on disk — the unsaved edit is destroyed the next time the
//!   editor saves, or the write is destroyed the moment the editor saves;
//! * ask the engine to re-serialize the resource — same outcome, with the loss
//!   moved one layer down; or
//! * refuse, and say exactly what the caller can do about it.
//!
//! Only the third is honest, so it is the only one implemented. The refusal is a
//! `conflict`, it is not retryable (retrying changes nothing), and it carries
//! remediation naming both the editor-safe route and the way to clear the
//! divergence.

use std::collections::BTreeMap;

use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::contracts::{GodotValue, OutcomeError};

use super::plan::{RoutePlan, WorkspaceFreshness};
use crate::scheduler::EffectPlane;

/// Which view of a resource is authoritative right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SnapshotAuthority {
    /// A live plane holds the authoritative state.
    EditorBuffer,
    /// The bytes on disk are authoritative.
    Disk,
}

impl SnapshotAuthority {
    /// Stable snake_case name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EditorBuffer => "editor_buffer",
            Self::Disk => "disk",
        }
    }
}

/// The precedence rules, as a pure decision.
pub struct SnapshotPrecedence;

impl SnapshotPrecedence {
    /// Which view answers for one plane under one freshness observation.
    ///
    /// A live plane is always answered live. For the server's own static/`file`
    /// plane, disk wins — except when a live editor buffer diverges from it, in
    /// which case the buffer is authoritative and a *write* to that path is a
    /// conflict rather than an overwrite.
    #[must_use]
    pub const fn authority_for(
        plane: EffectPlane,
        freshness: WorkspaceFreshness,
    ) -> SnapshotAuthority {
        match plane {
            // A live plane answers from live state, always. A derived or stale view
            // must never be substituted for it.
            EffectPlane::Editor | EffectPlane::Runtime | EffectPlane::LspDap => {
                SnapshotAuthority::EditorBuffer
            }
            EffectPlane::File | EffectPlane::Process => match freshness {
                // A lagging index is refreshed from disk, so disk still wins.
                WorkspaceFreshness::Fresh | WorkspaceFreshness::StaleIndex => {
                    SnapshotAuthority::Disk
                }
                WorkspaceFreshness::UnsavedEditorBuffer => SnapshotAuthority::EditorBuffer,
            },
        }
    }
}

/// Concrete, ordered steps a caller can take to resolve a conflict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remediation {
    steps: Vec<String>,
}

impl Remediation {
    /// Build one remediation from its ordered steps.
    #[must_use]
    pub fn new(steps: Vec<String>) -> Self {
        Self { steps }
    }

    /// The ordered steps.
    #[must_use]
    pub fn steps(&self) -> &[String] {
        &self.steps
    }

    /// Whether any step was recorded. An empty remediation is a bug: a refusal
    /// nobody can act on is indistinguishable from a dead end.
    #[must_use]
    pub fn is_actionable(&self) -> bool {
        !self.steps.is_empty()
    }
}

/// A precedence conflict: the plane that was asked to act is not the plane that
/// holds the authoritative state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaneConflict {
    path: String,
    required: EffectPlane,
    holder: SnapshotAuthority,
    remediation: Remediation,
}

impl PlaneConflict {
    /// The descriptor-relative resource in conflict.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The plane the refused outcome required.
    #[must_use]
    pub const fn required_plane(&self) -> EffectPlane {
        self.required
    }

    /// Which view holds the authoritative state.
    #[must_use]
    pub const fn holder(&self) -> SnapshotAuthority {
        self.holder
    }

    /// What the caller can do about it.
    #[must_use]
    pub const fn remediation(&self) -> &Remediation {
        &self.remediation
    }

    /// The caller-facing refusal: a non-retryable `conflict`.
    ///
    /// It is deliberately not retryable. Retrying an identical write against an
    /// identical divergence produces an identical conflict; only a caller action
    /// changes the answer, which is why the remediation travels with it.
    #[must_use]
    pub fn to_outcome(&self) -> OutcomeError {
        let mut details = BTreeMap::new();
        details.insert("path".to_owned(), GodotValue::String(self.path.clone()));
        details.insert(
            "authoritative_view".to_owned(),
            GodotValue::String(self.holder.as_str().to_owned()),
        );
        details.insert(
            "required_plane".to_owned(),
            GodotValue::String(self.required.as_str().to_owned()),
        );
        for (index, step) in self.remediation.steps().iter().enumerate() {
            details.insert(
                format!("remediation_{index}"),
                GodotValue::String(step.clone()),
            );
        }
        OutcomeError::new(
            ErrorKind::Conflict,
            format!(
                "the authoritative state for {} lives in the {} view, so a {} write is refused \
                 rather than overwriting it",
                self.path,
                self.holder.as_str(),
                self.required.as_str()
            ),
            false,
            details,
        )
    }
}

/// The conflict a plan raises against one resource, if any.
///
/// Only a *mutation* whose bytes land on the `file` plane can conflict this way: a
/// read of the bytes on disk is a legitimate answer (declared as reduced fidelity
/// by the plan), and a mutation on a live plane already goes through the view that
/// holds the buffer.
pub(crate) fn precedence_conflict(
    plan: &RoutePlan,
    freshness: WorkspaceFreshness,
    path: &str,
) -> Option<PlaneConflict> {
    if !plan.is_mutation() || !plan.effects().contains(&EffectPlane::File) {
        return None;
    }
    // A plan that consults the live editor buffer first cannot destroy it: the
    // buffer is the thing it changes. `scene.save` is exactly that plan, which is
    // why it is the remediation rather than the refusal.
    if plan.requires_editor_consult() {
        return None;
    }
    if SnapshotPrecedence::authority_for(EffectPlane::File, freshness)
        != SnapshotAuthority::EditorBuffer
    {
        return None;
    }
    Some(PlaneConflict {
        path: path.to_owned(),
        required: EffectPlane::File,
        holder: SnapshotAuthority::EditorBuffer,
        remediation: Remediation::new(vec![
            format!(
                "save or discard the unsaved editor buffer for {path} in the attached Godot \
                 editor, then re-issue this write"
            ),
            format!(
                "or re-issue the change through the editor plane so the live buffer is the thing \
                 that changes: scene.save for {path}, or node.create / node.delete / \
                 node.reparent / node.property.set for a node inside it"
            ),
            "this write was not attempted: neither the bytes on disk nor the editor buffer were \
             modified"
                .to_owned(),
        ]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::plan::{
        EnginePresence, PlaneObservation, ProtocolFidelity, RuntimePresence, plan_for,
    };
    use crate::scheduler::PlaneAvailability;

    fn plan(tool: &str, freshness: WorkspaceFreshness) -> RoutePlan {
        plan_for(
            tool,
            &PlaneObservation::new(
                PlaneAvailability::new(true, true, true, true, true),
                EnginePresence::EditorConnected,
                RuntimePresence::GameRunning,
                ProtocolFidelity::Full,
                freshness,
            ),
        )
        .expect("a canonical tool")
    }

    /// A live plane is answered live; the index never outranks disk.
    #[test]
    fn a_live_buffer_outranks_disk_and_the_index_outranks_nothing() {
        for plane in [
            EffectPlane::Editor,
            EffectPlane::Runtime,
            EffectPlane::LspDap,
        ] {
            for freshness in [
                WorkspaceFreshness::Fresh,
                WorkspaceFreshness::StaleIndex,
                WorkspaceFreshness::UnsavedEditorBuffer,
            ] {
                assert_eq!(
                    SnapshotPrecedence::authority_for(plane, freshness),
                    SnapshotAuthority::EditorBuffer
                );
            }
        }
        assert_eq!(
            SnapshotPrecedence::authority_for(EffectPlane::File, WorkspaceFreshness::StaleIndex),
            SnapshotAuthority::Disk
        );
        assert_eq!(
            SnapshotPrecedence::authority_for(
                EffectPlane::File,
                WorkspaceFreshness::UnsavedEditorBuffer
            ),
            SnapshotAuthority::EditorBuffer
        );
    }

    /// A diverging buffer conflicts with a file write and with nothing else.
    #[test]
    fn only_a_file_plane_write_conflicts_with_a_diverging_buffer() {
        let write = plan("resource.update", WorkspaceFreshness::UnsavedEditorBuffer);
        let conflict =
            precedence_conflict(&write, WorkspaceFreshness::UnsavedEditorBuffer, "a.tscn")
                .expect("a diverging buffer conflicts with a byte write");
        assert_eq!(conflict.holder(), SnapshotAuthority::EditorBuffer);
        assert_eq!(conflict.required_plane(), EffectPlane::File);
        assert!(conflict.remediation().is_actionable());
        let outcome = conflict.to_outcome();
        assert_eq!(outcome.code(), "conflict");
        assert!(!outcome.retryable());
        assert!(outcome.details().contains_key("remediation_0"));

        let live = PlaneObservation::new(
            PlaneAvailability::new(true, true, true, true, true),
            EnginePresence::EditorConnected,
            RuntimePresence::GameRunning,
            ProtocolFidelity::Full,
            WorkspaceFreshness::UnsavedEditorBuffer,
        );
        let mut plans = vec![plan(
            "scene.tree.get",
            WorkspaceFreshness::UnsavedEditorBuffer,
        )];
        // The editor and runtime mutations are frozen version 0.1 shapes: not
        // catalog tools yet, but the precedence rule for them must not change.
        for historical in ["node.create", "runtime.screenshot.capture"] {
            plans.push(crate::router::plan::plan_for_descriptor(
                godot_mcp_domain::catalog::historical_v0_1_descriptor(historical)
                    .expect("a version 0.1 planned operation"),
                &live,
            ));
        }
        for plan in plans {
            let tool = plan.tool().to_owned();
            assert!(
                precedence_conflict(&plan, WorkspaceFreshness::UnsavedEditorBuffer, "a.tscn")
                    .is_none(),
                "{tool} does not write bytes on the file plane"
            );
        }
        let fresh = plan("resource.update", WorkspaceFreshness::Fresh);
        assert!(precedence_conflict(&fresh, WorkspaceFreshness::Fresh, "a.tscn").is_none());
    }

    /// The editor-safe route is the remediation, so it must not be the refusal.
    #[test]
    fn the_editor_safe_route_is_never_refused_as_its_own_conflict() {
        let save = plan("scene.save", WorkspaceFreshness::UnsavedEditorBuffer);
        assert!(save.requires_editor_consult());
        assert!(
            precedence_conflict(&save, WorkspaceFreshness::UnsavedEditorBuffer, "a.tscn").is_none(),
            "a plan that changes the buffer itself cannot destroy the buffer"
        );
        let write = plan("resource.update", WorkspaceFreshness::UnsavedEditorBuffer);
        let conflict =
            precedence_conflict(&write, WorkspaceFreshness::UnsavedEditorBuffer, "a.tscn")
                .expect("a raw byte write does conflict");
        assert!(
            conflict
                .remediation()
                .steps()
                .iter()
                .any(|step| step.contains("scene.save")),
            "the refusal must point at the route that is safe"
        );
    }
}
