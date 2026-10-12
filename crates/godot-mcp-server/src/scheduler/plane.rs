//! Which plane an operation reaches, and whether that plane can execute today.
//!
//! The scheduler must integrate every process, LSP/DAP, resource (file), plugin
//! and runtime operation. Two of those five planes are backed by shipped
//! boundaries — `File` and `Process`. The LSP/DAP adapter implementation exists,
//! but the production runtime does not yet connect it to server-owned editor
//! launch receipts, so that boundary remains fail-closed. Neither the editor
//! plugin bridge nor the runtime plane is shipped in this release because Todos
//! 20-25 were cancelled.
//!
//! ## Unavailable planes stay in the code path
//!
//! A plane whose bridge is absent is *not* removed from this enum, from the
//! identity tuple's family list, or from the canonical capability matrix. The
//! frozen manifests still advertise those tools, so deleting them here would make
//! the scheduler lie about what a profile means. Instead the admission path keeps
//! the branch and refuses it honestly with [`ErrorKind::Unavailable`], matching
//! the policy layer's `BoundaryUnavailable` refusal, before any queueing and
//! before any effect.

use std::collections::BTreeMap;

use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::contracts::OutcomeError;

use crate::idempotency::OperationToolFamily;

/// The five effect planes the scheduler admits work to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EffectPlane {
    /// Descriptor-relative project file mutations.
    File,
    /// Supervised child processes.
    Process,
    /// Language/debug adapter channels.
    LspDap,
    /// Live editor plugin bridge. Not shipped in this release.
    Editor,
    /// Running game instance.
    Runtime,
}

impl EffectPlane {
    /// Every plane, in a stable order.
    pub const ALL: [Self; 5] = [
        Self::File,
        Self::Process,
        Self::LspDap,
        Self::Editor,
        Self::Runtime,
    ];

    /// Stable snake_case name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Process => "process",
            Self::LspDap => "lsp_dap",
            Self::Editor => "editor",
            Self::Runtime => "runtime",
        }
    }
}

/// Which planes have a real boundary compiled in and reachable.
///
/// This is an observation, never a wish: it is derived from the effect gateway's
/// own boundary availability so the scheduler cannot advertise a plane the
/// gateway would refuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaneAvailability {
    file: bool,
    process: bool,
    lsp_dap: bool,
    editor: bool,
    runtime: bool,
}

impl PlaneAvailability {
    /// Record one observation per plane.
    #[must_use]
    pub const fn new(
        file: bool,
        process: bool,
        lsp_dap: bool,
        editor: bool,
        runtime: bool,
    ) -> Self {
        Self {
            file,
            process,
            lsp_dap,
            editor,
            runtime,
        }
    }

    /// Whether one plane can execute.
    #[must_use]
    pub const fn of(&self, plane: EffectPlane) -> bool {
        match plane {
            EffectPlane::File => self.file,
            EffectPlane::Process => self.process,
            EffectPlane::LspDap => self.lsp_dap,
            EffectPlane::Editor => self.editor,
            EffectPlane::Runtime => self.runtime,
        }
    }

    /// The honest refusal for an unavailable plane: `unavailable`, naming the
    /// plane and nothing else. It carries no project, principal or session.
    #[must_use]
    pub fn unavailable_error(plane: EffectPlane) -> OutcomeError {
        let mut details = BTreeMap::new();
        details.insert(
            PLANE_DETAIL_KEY.to_owned(),
            godot_mcp_domain::contracts::GodotValue::String(plane.as_str().to_owned()),
        );
        OutcomeError::new(
            ErrorKind::Unavailable,
            format!("the {} boundary is not available", plane.as_str()),
            false,
            details,
        )
    }

    /// The `unavailable` refusal for `plane`, saying why it cannot serve.
    #[must_use]
    pub fn unavailable_error_because(plane: EffectPlane, reason: &str) -> OutcomeError {
        let refusal = Self::unavailable_error(plane);
        OutcomeError::new(
            refusal.kind(),
            format!("{}: {reason}", refusal.message()),
            refusal.retryable(),
            refusal.details().clone(),
        )
    }
}

/// Detail key naming the plane an `unavailable` refusal is about.
pub const PLANE_DETAIL_KEY: &str = "plane";

/// The plane an operation family reaches, or `None` for a family that touches no
/// boundary at all (a purely static read).
///
/// This is the single mapping; nothing else may decide a family's plane.
#[must_use]
pub const fn plane_of(family: OperationToolFamily) -> Option<EffectPlane> {
    match family {
        OperationToolFamily::StaticDiscovery
        | OperationToolFamily::StaticAnalysis
        | OperationToolFamily::StaticSceneGraph => None,
        OperationToolFamily::ProjectMutation | OperationToolFamily::DestructiveExport => {
            Some(EffectPlane::File)
        }
        OperationToolFamily::ProjectImport
        | OperationToolFamily::RuntimeExecution
        | OperationToolFamily::DiagnosticProbe => Some(EffectPlane::Process),
        OperationToolFamily::LanguageService | OperationToolFamily::RuntimeDebug => {
            Some(EffectPlane::LspDap)
        }
        OperationToolFamily::EditorSceneMutation | OperationToolFamily::EditorInspection => {
            Some(EffectPlane::Editor)
        }
        OperationToolFamily::RuntimeCommand | OperationToolFamily::RuntimeInspection => {
            Some(EffectPlane::Runtime)
        }
    }
}
