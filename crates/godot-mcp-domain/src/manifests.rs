//! Canonical capability-profile manifests.
//!
//! A profile is an authorization ceiling: it grants capabilities, and the tools
//! it lists are exactly the [`TOOL_CATALOG`](crate::catalog::TOOL_CATALOG)
//! descriptors that name it. Because the catalog contains implemented methods
//! only, a manifest can never advertise a method without a production handler.
//!
//! The version 0.1 manifests are retained verbatim by
//! [`historical_manifest_v0_1`]. They are the frozen first-release contract, are
//! not rewritten, and are no longer what discovery serves.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::CapabilityProfile;
use crate::catalog::{ServerState, TOOL_CATALOG};
use crate::contracts::{Capability, ContractVersion};

/// Stable capability, tool, and resource names for one public profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileManifest {
    pub schema_version: ContractVersion,
    pub profile: CapabilityProfile,
    pub capabilities: BTreeSet<Capability>,
    /// Tools listed while the server is bound to a project.
    pub tools: BTreeSet<String>,
    /// Tools listed while a server launched with a workspace authority has not
    /// bound a project yet. Absent from the version 0.1 manifests, which had no
    /// bootstrap state.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub bootstrap_tools: BTreeSet<String>,
    pub resources: BTreeSet<String>,
}

const CORE_RESOURCES: &[&str] = &[
    "godot-mcp://resources/capabilities",
    "godot-mcp://resources/health",
    "godot-mcp://resources/project",
    "godot-mcp://resources/scheduler",
];

const RUNTIME_RESOURCES: &[&str] = &["godot-mcp://artifacts/{artifact_id}"];

fn strings(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

/// The capabilities one profile grants.
#[must_use]
pub fn profile_capabilities(profile: CapabilityProfile) -> BTreeSet<Capability> {
    let granted: &[Capability] = match profile {
        CapabilityProfile::Core => &[Capability::Read],
        CapabilityProfile::Editor => &[
            Capability::Read,
            Capability::ProjectWrite,
            Capability::EditorMutate,
        ],
        CapabilityProfile::Runtime => &[
            Capability::Read,
            Capability::RuntimeExecute,
            Capability::ExportDestructive,
        ],
        CapabilityProfile::Full => &[
            Capability::Read,
            Capability::ProjectWrite,
            Capability::EditorMutate,
            Capability::RuntimeExecute,
            Capability::ExportDestructive,
        ],
    };
    granted.iter().copied().collect()
}

fn profile_resources(profile: CapabilityProfile) -> BTreeSet<String> {
    let mut resources = strings(CORE_RESOURCES);
    if matches!(
        profile,
        CapabilityProfile::Runtime | CapabilityProfile::Full
    ) {
        resources.extend(strings(RUNTIME_RESOURCES));
    }
    resources
}

fn tools_in(profile: CapabilityProfile, state: ServerState) -> BTreeSet<String> {
    TOOL_CATALOG
        .iter()
        .filter(|descriptor| descriptor.in_profile(profile) && descriptor.in_state(state))
        .map(|descriptor| descriptor.name.to_owned())
        .collect()
}

/// Return the deterministic canonical manifest for a profile, derived from the
/// tool catalog.
#[must_use]
pub fn canonical_manifest(profile: CapabilityProfile) -> ProfileManifest {
    ProfileManifest {
        schema_version: ContractVersion::current(),
        profile,
        capabilities: profile_capabilities(profile),
        tools: tools_in(profile, ServerState::ProjectBound),
        bootstrap_tools: tools_in(profile, ServerState::Bootstrap),
        resources: profile_resources(profile),
    }
}

const V0_1_CORE_TOOLS: &[&str] = &[
    "capabilities.get",
    "gdscript.diagnostics.get",
    "gdscript.impact.get",
    "gdscript.references.get",
    "gdscript.symbols.get",
    "health.get",
    "project.dependencies.get",
    "project.discover",
    "project.info.get",
    "project.validate",
    "resource.dependencies.get",
    "resource.tree.get",
    "scene.dependencies.get",
    "scene.tree.get",
    "script.validate",
];

const V0_1_EDITOR_TOOLS: &[&str] = &[
    "editor.selection.get",
    "editor.state.get",
    "node.create",
    "node.delete",
    "node.property.set",
    "node.reparent",
    "project.import.run",
    "project.uid.update",
    "resource.update",
    "scene.save",
];

const V0_1_RUNTIME_TOOLS: &[&str] = &[
    "debug.session.get",
    "input.action.send",
    "input.key.send",
    "input.mouse.send",
    "input.sequence.send",
    "project.run",
    "project.test",
    "runtime.body.get",
    "runtime.camera.get",
    "runtime.collision.get",
    "runtime.environment.get",
    "runtime.frame.get",
    "runtime.layer.get",
    "runtime.lighting.get",
    "runtime.mesh.get",
    "runtime.particles.get",
    "runtime.properties.get",
    "runtime.raycast",
    "runtime.recording.capture",
    "runtime.screenshot.capture",
    "runtime.signals.get",
    "runtime.tree.get",
    "runtime.ui.get",
    "script.check",
];

/// The frozen version 0.1 manifest for a profile, exactly as first released.
///
/// Version 0.1 profiles listed planned editor and runtime methods that answered
/// `unavailable`. That historical contract is kept byte-for-byte so its
/// committed artifacts remain verifiable; it is not what discovery serves.
#[must_use]
pub fn historical_manifest_v0_1(profile: CapabilityProfile) -> ProfileManifest {
    let mut tools = strings(V0_1_CORE_TOOLS);
    if matches!(profile, CapabilityProfile::Editor | CapabilityProfile::Full) {
        tools.extend(strings(V0_1_EDITOR_TOOLS));
    }
    if matches!(
        profile,
        CapabilityProfile::Runtime | CapabilityProfile::Full
    ) {
        tools.extend(strings(V0_1_RUNTIME_TOOLS));
    }
    ProfileManifest {
        schema_version: ContractVersion::V0_1_0,
        profile,
        capabilities: profile_capabilities(profile),
        tools,
        bootstrap_tools: BTreeSet::new(),
        resources: profile_resources(profile),
    }
}
