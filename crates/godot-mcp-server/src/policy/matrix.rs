//! The capability matrix: one row per catalog tool.
//!
//! Rows are a projection of [`TOOL_CATALOG`]; nothing here restates a tool name.
//! The acceptance suite checks that the matrix covers every manifest tool exactly
//! once, names no tool the manifests do not contain, and that every row's
//! required capability is granted by every profile that lists the tool. Because
//! the catalog lists implemented methods only, every row is executable in this
//! build; a plane that is temporarily unreachable still resolves to
//! `unavailable` at evaluation time through
//! [`BoundaryAvailability`](super::authority::BoundaryAvailability).

use godot_mcp_domain::catalog::{
    ServerState, TOOL_CATALOG, ToolBoundary, ToolDescriptor, ToolFamily, descriptor,
};
use godot_mcp_domain::contracts::{Capability, OperationPlane};

use crate::journal::{EffectBoundaryKind, OperationFamily};

/// What one canonical tool requires before it may be queued or dispatched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OperationPolicy {
    /// Canonical tool name.
    pub(crate) tool: &'static str,
    /// Capability the caller's profile must advertise.
    pub(crate) capability: Capability,
    /// Public outcome plane the tool answers on.
    pub(crate) plane: OperationPlane,
    /// Effect boundary the tool must reach, when it reaches one at all.
    ///
    /// A read that is answered from a live editor or a running instance still
    /// names its boundary, because its availability gates the answer even though
    /// no permit is minted for it.
    pub(crate) boundary: Option<EffectBoundaryKind>,
    /// Operation family for a *mutating* tool. `Some` means the tool produces a
    /// side effect and therefore requires a production permit; `None` means the
    /// tool is read-only and no permit exists for it.
    pub(crate) family: Option<OperationFamily>,
}

impl OperationPolicy {
    /// Whether this tool produces a side effect and therefore needs a permit.
    pub(crate) const fn requires_permit(&self) -> bool {
        self.family.is_some()
    }
}

/// The server-side effect boundary a catalog boundary names.
pub(crate) const fn boundary_kind(boundary: ToolBoundary) -> EffectBoundaryKind {
    match boundary {
        ToolBoundary::File => EffectBoundaryKind::File,
        ToolBoundary::Process => EffectBoundaryKind::Process,
        ToolBoundary::LspDap => EffectBoundaryKind::LspDap,
        ToolBoundary::Editor => EffectBoundaryKind::Editor,
        ToolBoundary::Runtime => EffectBoundaryKind::Runtime,
    }
}

/// The journal operation family a catalog family names.
pub(crate) const fn operation_family(family: ToolFamily) -> OperationFamily {
    match family {
        ToolFamily::ProjectMutation => OperationFamily::ProjectMutation,
        ToolFamily::EditorMutation => OperationFamily::EditorMutation,
        ToolFamily::RuntimeExecution => OperationFamily::RuntimeExecution,
        ToolFamily::DestructiveExport => OperationFamily::DestructiveExport,
        ToolFamily::DiagnosticProbe => OperationFamily::DiagnosticProbe,
    }
}

/// The policy row of one catalog descriptor.
pub(crate) const fn row_of(descriptor: &ToolDescriptor) -> OperationPolicy {
    OperationPolicy {
        tool: descriptor.name,
        capability: descriptor.capability,
        plane: descriptor.plane,
        boundary: match descriptor.boundary {
            Some(boundary) => Some(boundary_kind(boundary)),
            None => None,
        },
        family: match descriptor.family {
            Some(family) => Some(operation_family(family)),
            None => None,
        },
    }
}

/// Every row in the matrix, in catalog order.
///
/// The matrix governs a bound project. Bootstrap-only tools are absent: they
/// run before any project exists and are authorized by profile membership in
/// the bootstrap kernel alone.
pub(crate) fn all_rows() -> Vec<OperationPolicy> {
    TOOL_CATALOG
        .iter()
        .filter(|descriptor| descriptor.in_state(ServerState::ProjectBound))
        .map(row_of)
        .collect()
}

/// The row for one canonical tool, or `None` when the tool is not a canonical
/// project operation at all. A tool with no row is denied by default; the
/// matrix never falls back to a permissive default.
pub(crate) fn row_for(tool: &str) -> Option<OperationPolicy> {
    descriptor(tool)
        .filter(|descriptor| descriptor.in_state(ServerState::ProjectBound))
        .map(row_of)
}
