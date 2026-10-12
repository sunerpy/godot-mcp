//! The one tool catalog every public surface is derived from.
//!
//! Each public method is exactly one [`ToolDescriptor`]: its name, title,
//! description, required capability, outcome plane, effect boundary and family,
//! MCP annotations, the profiles that may authorize it, the contract version
//! that introduced it and the [`HandlerKey`] of the operation that serves it,
//! which its input schema and its handler both come from. The profile manifests
//! ([`crate::manifests`]), the policy matrix, `tools/list`, argument
//! validation, the handler-completeness checks and the generated tool inventory
//! are all derived from [`TOOL_CATALOG`].
//!
//! The catalog lists **implemented** operations only. A method enters it in the
//! same change that ships its schema and production handler; the server refuses
//! to assemble a kernel in which a descriptor has no handler or a handler has no
//! descriptor. A planned method therefore cannot be advertised, and a profile
//! cannot manufacture a handler by naming a method.

use serde::Serialize;

use crate::CapabilityProfile;
use crate::contracts::{Capability, ContractVersion, OperationPlane};

mod handler;

pub use handler::{
    AuthoringOperation, BootstrapOperation, CoreOperation, EditorOperation, EngineOperation,
    HandlerKey, KernelOperation, SessionOperation,
};

/// The effect boundary a tool reaches, when it reaches one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolBoundary {
    /// Descriptor-relative project file writes.
    File,
    /// Supervised child processes.
    Process,
    /// Language and debug adapter channels.
    LspDap,
    /// The live editor bridge.
    Editor,
    /// A running game instance.
    Runtime,
}

/// The durable effect family of a mutating tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolFamily {
    /// A project file or project-state mutation.
    ProjectMutation,
    /// A live-editor mutation.
    EditorMutation,
    /// A supervised project or test run.
    RuntimeExecution,
    /// An export that writes release artifacts.
    DestructiveExport,
    /// A supervised diagnostic probe of the engine.
    DiagnosticProbe,
}

/// The idempotency family a tool's mutation identity is bound to.
///
/// This is part of the durable eight-element mutation identity, so its wire
/// spellings are frozen once journaled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityFamily {
    StaticDiscovery,
    StaticAnalysis,
    StaticSceneGraph,
    ProjectMutation,
    ProjectImport,
    EditorSceneMutation,
    EditorInspection,
    RuntimeExecution,
    RuntimeCommand,
    RuntimeInspection,
    RuntimeDebug,
    DiagnosticProbe,
    DestructiveExport,
}

/// What must be running for a tool to answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolNeeds {
    /// Only the held project files; no Godot process.
    ProjectFiles,
    /// A supervised headless Godot process started for the call.
    HeadlessEngine,
    /// A connected live editor.
    LiveEditor,
    /// A managed game session.
    GameSession,
    /// A managed editor session hosting the language server and debug
    /// adapter.
    EditorSession,
}

impl ToolNeeds {
    /// Stable wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProjectFiles => "project_files",
            Self::HeadlessEngine => "headless_engine",
            Self::LiveEditor => "live_editor",
            Self::GameSession => "game_session",
            Self::EditorSession => "editor_session",
        }
    }
}

/// Which server state lists a tool.
///
/// A server launched with a workspace authority starts in [`Self::Bootstrap`]
/// and moves to [`Self::ProjectBound`] exactly once, when `project.bind`
/// succeeds; a server launched with a project starts bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerState {
    /// Holding a workspace authority, with no project bound yet.
    Bootstrap,
    /// Bound to exactly one project for the rest of the process lifetime.
    ProjectBound,
}

const BOUND: &[ServerState] = &[ServerState::ProjectBound];
const BOOTSTRAP: &[ServerState] = &[ServerState::Bootstrap];
const EVERY_STATE: &[ServerState] = &[ServerState::Bootstrap, ServerState::ProjectBound];

/// MCP tool annotations: hints a client may use to decide how to present and
/// confirm a call. They are derived from the descriptor's effect facts, never
/// stated independently of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct ToolAnnotations {
    /// The tool changes nothing outside the server's own private state.
    pub read_only: bool,
    /// The tool may overwrite or remove existing project content.
    pub destructive: bool,
    /// Repeating the identical call has no further effect.
    pub idempotent: bool,
    /// The tool may interact with entities outside the held project, such as
    /// arbitrary code the project itself runs.
    pub open_world: bool,
}

const READ: ToolAnnotations = ToolAnnotations {
    read_only: true,
    destructive: false,
    idempotent: true,
    open_world: false,
};

const REWRITE: ToolAnnotations = ToolAnnotations {
    read_only: false,
    destructive: true,
    idempotent: true,
    open_world: false,
};

const IMPORT: ToolAnnotations = ToolAnnotations {
    read_only: false,
    destructive: false,
    idempotent: true,
    open_world: false,
};

const PROBE: ToolAnnotations = ToolAnnotations {
    read_only: false,
    destructive: false,
    idempotent: true,
    open_world: false,
};

const EXECUTE: ToolAnnotations = ToolAnnotations {
    read_only: false,
    destructive: false,
    idempotent: false,
    open_world: true,
};

const CREATE: ToolAnnotations = ToolAnnotations {
    read_only: false,
    destructive: false,
    idempotent: true,
    open_world: false,
};

const ALL_PROFILES: &[CapabilityProfile] = &[
    CapabilityProfile::Core,
    CapabilityProfile::Editor,
    CapabilityProfile::Runtime,
    CapabilityProfile::Full,
];
const EDITOR_PROFILES: &[CapabilityProfile] = &[CapabilityProfile::Editor, CapabilityProfile::Full];
/// The contract 0.2 authoring reads accompany the profiles that author or run
/// a project; `core` remains exactly the contract 0.1 static-analysis surface.
const AUTHORING_READ_PROFILES: &[CapabilityProfile] = &[
    CapabilityProfile::Editor,
    CapabilityProfile::Runtime,
    CapabilityProfile::Full,
];
const RUNTIME_PROFILES: &[CapabilityProfile] =
    &[CapabilityProfile::Runtime, CapabilityProfile::Full];

/// One public method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ToolDescriptor {
    /// Canonical dotted tool name.
    pub name: &'static str,
    /// Short human title.
    pub title: &'static str,
    /// What the tool does, its preconditions, how it relates to the workspace
    /// revision, and what must be running for it to answer.
    pub description: &'static str,
    /// Capability the caller's profile must grant.
    pub capability: Capability,
    /// Public outcome plane.
    pub plane: OperationPlane,
    /// Primary effect boundary, when the tool reaches one.
    pub boundary: Option<ToolBoundary>,
    /// Every plane the outcome needs, when it spans more than its boundary.
    /// Empty means "exactly the boundary, if any".
    pub composed: &'static [ToolBoundary],
    /// Whether a live editor must be consulted before any byte is written.
    pub consult_editor: bool,
    /// Effect family of a mutating tool; `None` for a read.
    pub family: Option<ToolFamily>,
    /// Idempotency family bound into the mutation identity.
    pub identity_family: IdentityFamily,
    /// What must be running for the tool to answer.
    pub needs: ToolNeeds,
    /// MCP annotations.
    pub annotations: ToolAnnotations,
    /// Profiles that may authorize the tool.
    pub profiles: &'static [CapabilityProfile],
    /// Contract version that introduced the tool.
    pub introduced_in: ContractVersion,
    /// Server states in which the tool is listed.
    pub states: &'static [ServerState],
    /// The operation that serves the tool; its input schema and its handler
    /// both come from it. `None` only in a frozen version 0.1 shape, which
    /// describes a method and is never served.
    pub handler: Option<HandlerKey>,
}

impl ToolDescriptor {
    /// Whether the tool produces a durable side effect and needs a permit.
    #[must_use]
    pub const fn is_mutation(&self) -> bool {
        self.family.is_some()
    }

    /// Whether `profile` may authorize this tool.
    #[must_use]
    pub fn in_profile(&self, profile: CapabilityProfile) -> bool {
        self.profiles.contains(&profile)
    }

    /// Whether the tool is listed in server state `state`.
    #[must_use]
    pub fn in_state(&self, state: ServerState) -> bool {
        self.states.contains(&state)
    }
}

const fn static_read(
    handler: HandlerKey,
    name: &'static str,
    title: &'static str,
    description: &'static str,
    identity_family: IdentityFamily,
) -> ToolDescriptor {
    ToolDescriptor {
        name,
        title,
        description,
        capability: Capability::Read,
        plane: OperationPlane::Static,
        boundary: None,
        composed: &[],
        consult_editor: false,
        family: None,
        identity_family,
        needs: ToolNeeds::ProjectFiles,
        annotations: READ,
        profiles: ALL_PROFILES,
        introduced_in: ContractVersion::V0_1_0,
        states: BOUND,
        handler: Some(handler),
    }
}

/// A static authoring read introduced by contract 0.2.0.
const fn bound_read(
    handler: HandlerKey,
    name: &'static str,
    title: &'static str,
    description: &'static str,
    identity_family: IdentityFamily,
) -> ToolDescriptor {
    ToolDescriptor {
        introduced_in: ContractVersion::V0_2_0,
        profiles: AUTHORING_READ_PROFILES,
        ..static_read(handler, name, title, description, identity_family)
    }
}

/// A static read listed in both server states.
const fn any_state_read(
    handler: HandlerKey,
    name: &'static str,
    title: &'static str,
    description: &'static str,
    identity_family: IdentityFamily,
) -> ToolDescriptor {
    ToolDescriptor {
        states: EVERY_STATE,
        ..static_read(handler, name, title, description, identity_family)
    }
}

/// A journaled engine-backed text scene/resource mutation introduced by
/// contract 0.2.0.
const fn engine_write(
    handler: HandlerKey,
    name: &'static str,
    title: &'static str,
    description: &'static str,
    annotations: ToolAnnotations,
) -> ToolDescriptor {
    ToolDescriptor {
        needs: ToolNeeds::HeadlessEngine,
        ..file_write(handler, name, title, description, annotations)
    }
}

/// A journaled project file mutation introduced by contract 0.2.0.
const fn file_write(
    handler: HandlerKey,
    name: &'static str,
    title: &'static str,
    description: &'static str,
    annotations: ToolAnnotations,
) -> ToolDescriptor {
    ToolDescriptor {
        name,
        title,
        description,
        capability: Capability::ProjectWrite,
        plane: OperationPlane::Project,
        boundary: Some(ToolBoundary::File),
        composed: &[],
        consult_editor: false,
        family: Some(ToolFamily::ProjectMutation),
        identity_family: IdentityFamily::ProjectMutation,
        needs: ToolNeeds::ProjectFiles,
        annotations,
        profiles: EDITOR_PROFILES,
        introduced_in: ContractVersion::V0_2_0,
        states: BOUND,
        handler: Some(handler),
    }
}

/// A read answered by the connected live editor, introduced by contract 0.2.0.
const fn editor_read(
    handler: HandlerKey,
    name: &'static str,
    title: &'static str,
    description: &'static str,
) -> ToolDescriptor {
    ToolDescriptor {
        name,
        title,
        description,
        capability: Capability::Read,
        plane: OperationPlane::Editor,
        boundary: Some(ToolBoundary::Editor),
        composed: &[],
        consult_editor: false,
        family: None,
        identity_family: IdentityFamily::EditorInspection,
        needs: ToolNeeds::LiveEditor,
        annotations: READ,
        profiles: EDITOR_PROFILES,
        introduced_in: ContractVersion::V0_2_0,
        states: BOUND,
        handler: Some(handler),
    }
}

/// A journaled change made through the connected live editor, introduced by
/// contract 0.2.0.
const fn editor_mutation(
    handler: HandlerKey,
    name: &'static str,
    title: &'static str,
    description: &'static str,
    annotations: ToolAnnotations,
) -> ToolDescriptor {
    ToolDescriptor {
        capability: Capability::EditorMutate,
        family: Some(ToolFamily::EditorMutation),
        identity_family: IdentityFamily::EditorSceneMutation,
        annotations,
        ..editor_read(handler, name, title, description)
    }
}

const EDITOR_STATE: ToolAnnotations = ToolAnnotations {
    read_only: false,
    destructive: false,
    idempotent: true,
    open_world: false,
};

const HISTORY: ToolAnnotations = ToolAnnotations {
    read_only: false,
    destructive: true,
    idempotent: false,
    open_world: false,
};

/// A read of a managed game session, answered by its runtime probe,
/// introduced by contract 0.2.0.
const fn runtime_read(
    handler: HandlerKey,
    name: &'static str,
    title: &'static str,
    description: &'static str,
) -> ToolDescriptor {
    ToolDescriptor {
        name,
        title,
        description,
        capability: Capability::Read,
        plane: OperationPlane::Runtime,
        boundary: Some(ToolBoundary::Runtime),
        composed: &[],
        consult_editor: false,
        family: None,
        identity_family: IdentityFamily::RuntimeInspection,
        needs: ToolNeeds::GameSession,
        annotations: READ,
        profiles: RUNTIME_PROFILES,
        introduced_in: ContractVersion::V0_2_0,
        states: BOUND,
        handler: Some(handler),
    }
}

/// A journaled command to a managed game session, introduced by contract
/// 0.2.0.
const fn runtime_command(
    handler: HandlerKey,
    name: &'static str,
    title: &'static str,
    description: &'static str,
    annotations: ToolAnnotations,
) -> ToolDescriptor {
    ToolDescriptor {
        capability: Capability::RuntimeExecute,
        family: Some(ToolFamily::RuntimeExecution),
        identity_family: IdentityFamily::RuntimeCommand,
        annotations,
        ..runtime_read(handler, name, title, description)
    }
}

/// A read of the managed sessions this server owns, answered from their
/// records and captured output.
const fn session_read(
    handler: HandlerKey,
    name: &'static str,
    title: &'static str,
    description: &'static str,
) -> ToolDescriptor {
    ToolDescriptor {
        boundary: None,
        identity_family: IdentityFamily::RuntimeExecution,
        needs: ToolNeeds::HeadlessEngine,
        ..runtime_read(handler, name, title, description)
    }
}

/// A read of a managed editor session's language server and debug adapter,
/// introduced by contract 0.2.0.
const fn debug_read(
    handler: HandlerKey,
    name: &'static str,
    title: &'static str,
    description: &'static str,
) -> ToolDescriptor {
    ToolDescriptor {
        boundary: Some(ToolBoundary::LspDap),
        identity_family: IdentityFamily::RuntimeDebug,
        needs: ToolNeeds::EditorSession,
        ..runtime_read(handler, name, title, description)
    }
}

/// A journaled command to a managed editor session's language server or
/// debug adapter, or the stop of that session, introduced by contract 0.2.0.
const fn debug_command(
    handler: HandlerKey,
    name: &'static str,
    title: &'static str,
    description: &'static str,
    annotations: ToolAnnotations,
) -> ToolDescriptor {
    ToolDescriptor {
        capability: Capability::RuntimeExecute,
        family: Some(ToolFamily::RuntimeExecution),
        annotations,
        ..debug_read(handler, name, title, description)
    }
}

/// Synthetic input or a capture: it changes the running game (or adds an
/// artifact) and is never repeated by a replay.
const INPUT: ToolAnnotations = ToolAnnotations {
    read_only: false,
    destructive: false,
    idempotent: false,
    open_world: false,
};

/// Stopping a session: repeating it changes nothing further.
const STOP: ToolAnnotations = ToolAnnotations {
    read_only: false,
    destructive: true,
    idempotent: true,
    open_world: false,
};

/// `scene.batch.apply`. Its headless helper writes through the file boundary;
/// when the scene is open in a connected editor the same request is applied by
/// the editor instead, which [`SCENE_BATCH_APPLY_IN_EDITOR`] describes.
const SCENE_BATCH_APPLY: ToolDescriptor = ToolDescriptor {
    consult_editor: true,
    ..engine_write(
        EngineOperation::SceneBatch.key(),
        "scene.batch.apply",
        "Edit a scene",
        "Apply one atomic batch of typed `operations` to the text scene at `path` whose SHA-256 \
         is `expectedSha256`: add, remove, rename, move or duplicate nodes, set properties, \
         attach scripts, (re)instance scenes, connect signals, edit groups and sub-resources. \
         Nodes are named by path or by a scene tree `handle`; a handle of older bytes refuses. \
         The engine applies and saves it; any refused operation publishes nothing. A scene open \
         in the connected editor is changed there instead, as one undoable action, and saved; \
         one holding unsaved editor changes refuses.",
        REWRITE,
    )
};

/// `scene.batch.apply` as executed by the connected live editor: the same
/// public request and result, authorized and journaled at the editor boundary.
/// Never listed on its own.
pub const SCENE_BATCH_APPLY_IN_EDITOR: ToolDescriptor = ToolDescriptor {
    boundary: Some(ToolBoundary::Editor),
    needs: ToolNeeds::LiveEditor,
    ..SCENE_BATCH_APPLY
};

/// The descriptor a tool executes under when the live editor carries it out,
/// for a tool whose request the editor can serve unchanged.
#[must_use]
pub fn in_editor_descriptor(name: &str) -> Option<&'static ToolDescriptor> {
    (name == SCENE_BATCH_APPLY_IN_EDITOR.name).then_some(&SCENE_BATCH_APPLY_IN_EDITOR)
}

/// Every implemented public method, in canonical name order.
pub const TOOL_CATALOG: &[ToolDescriptor] = &[
    any_state_read(
        KernelOperation::Capabilities.key(),
        "capabilities.get",
        "Capability profiles",
        "Report the capability profiles, their granted capabilities and the exact tool and \
         resource sets each one authorizes in this build. Optional `profile` narrows the report \
         to one profile. Read-only; needs no Godot process.",
        IdentityFamily::StaticDiscovery,
    ),
    static_read(
        CoreOperation::ScriptDiagnostics.key(),
        "gdscript.diagnostics.get",
        "GDScript diagnostics",
        "List static parse and analysis diagnostics for the project or for one `script`, paged \
         with `offset`/`limit`, from the coherent workspace snapshot. This is a static check, not \
         an engine compile; use script.check for that. Read-only; needs no Godot process.",
        IdentityFamily::StaticAnalysis,
    ),
    static_read(
        CoreOperation::ScriptImpact.key(),
        "gdscript.impact.get",
        "GDScript change impact",
        "List the scenes, resources and scripts that depend on one `script` or one of its declared \
         symbols, transitively, from the coherent workspace snapshot. Read-only; needs no Godot \
         process.",
        IdentityFamily::StaticAnalysis,
    ),
    static_read(
        CoreOperation::ScriptReferences.key(),
        "gdscript.references.get",
        "GDScript references",
        "List references to and callers of one `script`, or of one function, signal or class name \
         it declares (`symbol`), from the coherent workspace snapshot, with the confidence of each \
         resolved edge. Read-only; needs no Godot process.",
        IdentityFamily::StaticAnalysis,
    ),
    static_read(
        CoreOperation::ScriptSymbols.key(),
        "gdscript.symbols.get",
        "GDScript symbols",
        "List the classes, functions, signals, variables and constants one `script` declares, \
         parsed from its current bytes and paged with `offset`/`limit`. Read-only; needs no Godot \
         process.",
        IdentityFamily::StaticAnalysis,
    ),
    any_state_read(
        CoreOperation::Health.key(),
        "health.get",
        "Server health",
        "Report the health of the held project, the analysis index, the durable journal, the \
         scheduler and each effect plane, with bounded metrics and the current write-block \
         reason, if any. Read-only; needs no Godot process.",
        IdentityFamily::StaticDiscovery,
    ),
    static_read(
        CoreOperation::ProjectDependencies.key(),
        "project.dependencies.get",
        "Project dependency graph",
        "List every resolved and dangling dependency edge between project scripts, scenes and \
         resources, paged with `offset`/`limit`, from the coherent workspace snapshot. Read-only; \
         needs no Godot process.",
        IdentityFamily::StaticSceneGraph,
    ),
    static_read(
        CoreOperation::ProjectDiscover.key(),
        "project.discover",
        "Project inventory",
        "List the project's scripts, scenes, resources and other files with their kinds, paged \
         with `offset`/`limit`, from the coherent workspace snapshot. Read-only; needs no Godot \
         process.",
        IdentityFamily::StaticDiscovery,
    ),
    static_read(
        CoreOperation::ProjectInfo.key(),
        "project.info.get",
        "Project information",
        "Report the held project's path-free identity, its parsed project settings summary and \
         the workspace revision the answer was taken from. Read-only; needs no Godot process.",
        IdentityFamily::StaticDiscovery,
    ),
    static_read(
        CoreOperation::ProjectValidate.key(),
        "project.validate",
        "Static project validation",
        "Statically validate the whole project — parse errors, dangling references and invalid \
         settings — from the coherent workspace snapshot, paged with `offset`/`limit`. It does not \
         run the engine and says so. Read-only; needs no Godot process.",
        IdentityFamily::StaticAnalysis,
    ),
    static_read(
        CoreOperation::ResourceDependencies.key(),
        "resource.dependencies.get",
        "Resource dependencies",
        "List the external and sub-resource dependencies of one text `resource` (.tres) and their \
         resolution, from the coherent workspace snapshot. Read-only; needs no Godot process.",
        IdentityFamily::StaticSceneGraph,
    ),
    static_read(
        CoreOperation::ResourceTree.key(),
        "resource.tree.get",
        "Resource structure",
        "Return the parsed structure of one text `resource` (.tres): its type, properties, \
         sub-resources and references, paged with `offset`/`limit`. Read-only; needs no Godot \
         process.",
        IdentityFamily::StaticSceneGraph,
    ),
    static_read(
        CoreOperation::SceneDependencies.key(),
        "scene.dependencies.get",
        "Scene dependencies",
        "List the scripts, sub-scenes and resources one text `scene` (.tscn) depends on and their \
         resolution, from the coherent workspace snapshot. Read-only; needs no Godot process.",
        IdentityFamily::StaticSceneGraph,
    ),
    static_read(
        CoreOperation::SceneTree.key(),
        "scene.tree.get",
        "Scene tree",
        "Return the node tree of one text `scene` (.tscn) with node types, attached scripts, \
         groups and signal connections, paged with `offset`/`limit`, plus the file's SHA-256 and \
         a `handle` per node that scene.batch.apply accepts until the file changes. Read-only; \
         needs no Godot process.",
        IdentityFamily::StaticSceneGraph,
    ),
    static_read(
        CoreOperation::ScriptValidate.key(),
        "script.validate",
        "Static script validation",
        "Statically validate one `script` with the GDScript parser and report its diagnostics. It \
         does not run the engine; script.check performs the engine check. Read-only; needs no \
         Godot process.",
        IdentityFamily::StaticAnalysis,
    ),
    ToolDescriptor {
        name: "project.import.run",
        title: "Import project resources",
        description: "Run the Godot importer headlessly over the held project, rewriting its \
             import metadata, and report the classified verdict, parsed engine diagnostics, \
             bounded output, which `expected_artifacts` were produced and, for each of `assets`, \
             its import state and imported artifact hashes. Needs a supervised headless Godot \
             editor process; the workspace revision advances after it commits.",
        capability: Capability::ProjectWrite,
        plane: OperationPlane::Project,
        boundary: Some(ToolBoundary::Process),
        composed: &[ToolBoundary::Process, ToolBoundary::File],
        consult_editor: false,
        family: Some(ToolFamily::ProjectMutation),
        identity_family: IdentityFamily::ProjectImport,
        needs: ToolNeeds::HeadlessEngine,
        annotations: IMPORT,
        profiles: EDITOR_PROFILES,
        introduced_in: ContractVersion::V0_1_0,
        states: BOUND,
        handler: Some(EngineOperation::ImportProject.key()),
    },
    ToolDescriptor {
        name: "project.uid.update",
        title: "Update a resource UID",
        description: "Register or repair the UID of one text `target` resource through a \
             supervised headless Godot canonical re-save, published with compare-and-swap. \
             `dry_run` validates without writing. Needs a headless Godot editor process; the \
             workspace revision advances after it commits.",
        capability: Capability::ProjectWrite,
        plane: OperationPlane::Project,
        boundary: Some(ToolBoundary::File),
        composed: &[],
        consult_editor: false,
        family: Some(ToolFamily::ProjectMutation),
        identity_family: IdentityFamily::ProjectMutation,
        needs: ToolNeeds::HeadlessEngine,
        annotations: REWRITE,
        profiles: EDITOR_PROFILES,
        introduced_in: ContractVersion::V0_1_0,
        states: BOUND,
        handler: Some(EngineOperation::UpdateUid.key()),
    },
    ToolDescriptor {
        name: "resource.update",
        title: "Canonically re-save a resource",
        description: "Canonicalize one existing text `target` resource through a supervised \
             headless Godot re-save and publish the result with compare-and-swap. `dry_run` \
             validates without writing. Needs a headless Godot editor process; the workspace \
             revision advances after it commits.",
        capability: Capability::ProjectWrite,
        plane: OperationPlane::Project,
        boundary: Some(ToolBoundary::File),
        composed: &[],
        consult_editor: false,
        family: Some(ToolFamily::ProjectMutation),
        identity_family: IdentityFamily::ProjectMutation,
        needs: ToolNeeds::HeadlessEngine,
        annotations: REWRITE,
        profiles: EDITOR_PROFILES,
        introduced_in: ContractVersion::V0_1_0,
        states: BOUND,
        handler: Some(EngineOperation::UpdateResource.key()),
    },
    ToolDescriptor {
        name: "scene.save",
        title: "Canonically re-save a scene",
        description: "Canonicalize one existing text scene `target` through a supervised \
             headless Godot re-save. A live editor may hold the authoritative buffer, so the \
             editor plane must be consulted first; without a connected editor the call is \
             refused as unavailable rather than risking an unsaved edit.",
        capability: Capability::ProjectWrite,
        plane: OperationPlane::Project,
        boundary: Some(ToolBoundary::File),
        composed: &[ToolBoundary::Editor, ToolBoundary::File],
        consult_editor: true,
        family: Some(ToolFamily::ProjectMutation),
        identity_family: IdentityFamily::ProjectMutation,
        needs: ToolNeeds::LiveEditor,
        annotations: REWRITE,
        profiles: EDITOR_PROFILES,
        introduced_in: ContractVersion::V0_1_0,
        states: BOUND,
        handler: Some(EngineOperation::SaveScene.key()),
    },
    ToolDescriptor {
        name: "project.run",
        title: "Run the project once",
        description: "Run the project's main scene, or `main_scene`, in a supervised Godot \
             process until it exits or `deadline_ms` elapses, and report the classified \
             verdict, exit facts, parsed engine diagnostics and bounded output. A game that \
             keeps running ends as a timeout. Executes project code; needs a Godot build.",
        capability: Capability::RuntimeExecute,
        plane: OperationPlane::Runtime,
        boundary: Some(ToolBoundary::Process),
        composed: &[],
        consult_editor: false,
        family: Some(ToolFamily::RuntimeExecution),
        identity_family: IdentityFamily::RuntimeExecution,
        needs: ToolNeeds::HeadlessEngine,
        annotations: EXECUTE,
        profiles: RUNTIME_PROFILES,
        introduced_in: ContractVersion::V0_1_0,
        states: BOUND,
        handler: Some(EngineOperation::RunProject.key()),
    },
    ToolDescriptor {
        name: "project.run.start",
        title: "Start a game session",
        description: "Start the project's main scene, or `scene`, as a managed game session \
             this server owns, and answer once its runtime probe is connected rather than when \
             the game exits. `display` window renders into the server's display (captures need \
             it); headless renders nothing. The session ends at `lifetimeSeconds` at the latest. \
             Executes project code; needs a Godot build.",
        capability: Capability::RuntimeExecute,
        plane: OperationPlane::Runtime,
        boundary: Some(ToolBoundary::Process),
        composed: &[],
        consult_editor: false,
        family: Some(ToolFamily::RuntimeExecution),
        identity_family: IdentityFamily::RuntimeExecution,
        needs: ToolNeeds::HeadlessEngine,
        annotations: EXECUTE,
        profiles: RUNTIME_PROFILES,
        introduced_in: ContractVersion::V0_2_0,
        states: BOUND,
        handler: Some(SessionOperation::RunStart.key()),
    },
    session_read(
        SessionOperation::RunStatus.key(),
        "project.run.status",
        "Session status",
        "Report one managed session — its state, scene, display, timestamps, containment \
         strength, exit facts, whether its probe is connected and what input it holds — or, \
         without `session`, every session this server owns. Changes nothing.",
    ),
    session_read(
        SessionOperation::RunLogs.key(),
        "project.run.logs",
        "Session output",
        "Read one bounded page of a managed session's captured `stream` (stdout or stderr) from \
         byte `offset`, while it runs or after it ended, with the next offset, the total length \
         and whether the end was reached. Changes nothing.",
    ),
    runtime_command(
        SessionOperation::RunStop.key(),
        "project.run.stop",
        "Stop a game session",
        "Stop a managed game session: its probe releases every held input and quits the game; a \
         game that does not end by itself is terminated inside its boundary. Stopping a session \
         that already ended answers its record. Answers the exit facts.",
        STOP,
    ),
    ToolDescriptor {
        name: "project.test",
        title: "Run committed tests",
        description: "Run the project's already-committed GUT or gdUnit4 test suite, optionally \
             narrowed by `selection`, in a supervised headless Godot process and report the \
             parsed summary and verdict. A project without a framework is unsupported; nothing \
             is installed. Executes project code; needs a Godot build.",
        capability: Capability::RuntimeExecute,
        plane: OperationPlane::Runtime,
        boundary: Some(ToolBoundary::Process),
        composed: &[],
        consult_editor: false,
        family: Some(ToolFamily::RuntimeExecution),
        identity_family: IdentityFamily::RuntimeExecution,
        needs: ToolNeeds::HeadlessEngine,
        annotations: EXECUTE,
        profiles: RUNTIME_PROFILES,
        introduced_in: ContractVersion::V0_1_0,
        states: BOUND,
        handler: Some(EngineOperation::RunTests.key()),
    },
    ToolDescriptor {
        name: "script.check",
        title: "Engine-check a script",
        description: "Check one `script` with the engine's own `--check-only` compiler in a \
             supervised headless Godot process, after a calibration canary proves the flag is \
             honoured, and report the engine diagnostics. Needs a Godot editor build.",
        capability: Capability::RuntimeExecute,
        plane: OperationPlane::Project,
        boundary: Some(ToolBoundary::Process),
        composed: &[],
        consult_editor: false,
        family: Some(ToolFamily::DiagnosticProbe),
        identity_family: IdentityFamily::DiagnosticProbe,
        needs: ToolNeeds::HeadlessEngine,
        annotations: PROBE,
        profiles: RUNTIME_PROFILES,
        introduced_in: ContractVersion::V0_1_0,
        states: BOUND,
        handler: Some(EngineOperation::CheckScript.key()),
    },
    // --- contract 0.2.0: bootstrap -------------------------------------------
    ToolDescriptor {
        name: "workspace.projects.list",
        title: "Workspace projects",
        description: "List the Godot projects (directories holding a project.godot) under the \
             server's authorized workspace, as workspace-relative paths with their configured \
             names, so one can be bound. Available only before a project is bound; read-only; \
             needs no Godot process.",
        states: BOOTSTRAP,
        introduced_in: ContractVersion::V0_2_0,
        ..static_read(
            BootstrapOperation::ProjectsList.key(),
            "",
            "",
            "",
            IdentityFamily::StaticDiscovery,
        )
    },
    ToolDescriptor {
        name: "project.create",
        title: "Create a project",
        description: "Create a new Godot project in `path`, a workspace-relative directory that \
             is absent or empty, with application name `name` and an optional `renderer`. It \
             refuses to escape the workspace, follow a symlink or replace an existing project. \
             Available only before a project is bound; call project.bind next. Needs no Godot \
             process; requires `idempotencyId`.",
        capability: Capability::ProjectWrite,
        plane: OperationPlane::Project,
        boundary: Some(ToolBoundary::File),
        composed: &[],
        consult_editor: false,
        family: Some(ToolFamily::ProjectMutation),
        identity_family: IdentityFamily::ProjectMutation,
        needs: ToolNeeds::ProjectFiles,
        annotations: CREATE,
        profiles: EDITOR_PROFILES,
        introduced_in: ContractVersion::V0_2_0,
        states: BOOTSTRAP,
        handler: Some(BootstrapOperation::ProjectCreate.key()),
    },
    ToolDescriptor {
        name: "project.bind",
        title: "Bind the project",
        description: "Bind this server, once and for the rest of its lifetime, to the Godot \
             project at workspace-relative `path`: acquire its single-writer lock, reconcile its \
             journal and publish workspace revision 1. The tool list then changes to the project \
             tools. Changes only the server's private state; needs no Godot process.",
        states: BOOTSTRAP,
        introduced_in: ContractVersion::V0_2_0,
        ..static_read(
            BootstrapOperation::ProjectBind.key(),
            "",
            "",
            "",
            IdentityFamily::StaticDiscovery,
        )
    },
    // --- contract 0.2.0: coherent files --------------------------------------
    bound_read(
        AuthoringOperation::FilesList.key(),
        "project.files.list",
        "Project files",
        "List the project's files with their kind, byte length and SHA-256, optionally under one \
         `prefix`, paged with `offset`/`limit`, from the coherent workspace snapshot. The hashes \
         are the preconditions mutations require. Read-only; needs no Godot process.",
        IdentityFamily::StaticDiscovery,
    ),
    bound_read(
        AuthoringOperation::FileGet.key(),
        "file.get",
        "Read a file",
        "Return one project file's SHA-256, byte length and contents — UTF-8 text, or base64 \
         for binary data — bounded by `maxBytes` from byte `offset`, with the workspace \
         revision it was read at. Engine cache and server state are not readable. Read-only; \
         needs no Godot process.",
        IdentityFamily::StaticDiscovery,
    ),
    file_write(
        AuthoringOperation::FileCreate.key(),
        "file.create",
        "Create a text file",
        "Create one new UTF-8 text file at `path` with `content`; the path must be absent, and \
         missing directories are created. `dryRun` returns the bounded diff without writing; \
         `expectedRevision` refuses if the workspace moved on. Journaled and published with \
         compare-and-swap; requires `idempotencyId`; returns the new revision.",
        CREATE,
    ),
    file_write(
        AuthoringOperation::FilePatch.key(),
        "file.patch",
        "Patch a text file",
        "Change one UTF-8 text file whose current SHA-256 is `expectedSha256` by one `change`: \
         a whole replacement (`content`), bounded line `edits` or a strict `unifiedDiff`, and \
         return the bounded diff. `dryRun` writes nothing. Journaled compare-and-swap; requires \
         `idempotencyId`; returns the new revision.",
        REWRITE,
    ),
    file_write(
        AuthoringOperation::FileMove.key(),
        "file.move",
        "Move a file",
        "Move one file whose SHA-256 is `expectedSha256` from `from` to an absent `to`, together \
         with its engine `.import`/`.uid` sidecars, in one journaled transaction. References \
         to the old path are not rewritten. `dryRun` writes nothing. Requires `idempotencyId`; \
         returns the new revision.",
        REWRITE,
    ),
    file_write(
        AuthoringOperation::FileDelete.key(),
        "file.delete",
        "Delete a file",
        "Delete one file whose SHA-256 is `expectedSha256`, together with its engine \
         `.import`/`.uid` sidecars, in one journaled transaction; the removal is proven against \
         the recorded bytes. `dryRun` writes nothing. Requires `idempotencyId`; returns the new \
         revision.",
        REWRITE,
    ),
    // --- contract 0.2.0: GDScript authoring ----------------------------------
    file_write(
        AuthoringOperation::ScriptCreate.key(),
        "script.create",
        "Create a GDScript file",
        "Create one new `.gd` script at an absent `path` after a static parse of `content`. A \
         script with parse errors is refused unless `allowParseErrors` is true, and its \
         diagnostics are returned either way; nothing is executed. Journaled compare-and-swap; \
         requires `idempotencyId`.",
        CREATE,
    ),
    file_write(
        AuthoringOperation::ScriptPatch.key(),
        "script.patch",
        "Patch a GDScript file",
        "Change one `.gd` script whose SHA-256 is `expectedSha256` by one `change` (`content`, \
         line `edits` or a strict `unifiedDiff`), after a static parse of the result; parse \
         errors refuse unless `allowParseErrors` is true. Nothing is executed; use script.check \
         for the engine verdict. Requires `idempotencyId`.",
        REWRITE,
    ),
    // --- contract 0.2.0: project settings ------------------------------------
    bound_read(
        AuthoringOperation::SettingsGet.key(),
        "project.settings.get",
        "Project settings",
        "Return the typed project settings — application name, main scene, autoloads, input \
         actions and their events, enabled plugins and the allowlisted values — with the \
         SHA-256 of project.godot that project.settings.patch requires. Read-only; needs no \
         Godot process.",
        IdentityFamily::StaticDiscovery,
    ),
    file_write(
        AuthoringOperation::SettingsPatch.key(),
        "project.settings.patch",
        "Change project settings",
        "Apply typed `operations` — set or clear the main scene, set or remove an autoload or an \
         input action, set an allowlisted value, enable or disable an installed plugin — to \
         project.godot whose SHA-256 is `expectedSha256`, preserving unrelated keys and \
         comments. Journaled; requires `idempotencyId`.",
        REWRITE,
    ),
    // --- contract 0.2.0: semantic scenes and resources ----------------------
    engine_write(
        EngineOperation::SceneCreate.key(),
        "scene.create",
        "Create a scene",
        "Create one new text scene at an absent `path` with a `rootType` root node and an optional \
         batch of typed `operations` — nodes, instances, typed properties, scripts, signal \
         connections, groups and sub-resources — applied and saved by a supervised headless \
         Godot process, then published with compare-and-swap. Requires `idempotencyId`.",
        CREATE,
    ),
    SCENE_BATCH_APPLY,
    engine_write(
        EngineOperation::ResourceCreate.key(),
        "resource.create",
        "Create a resource",
        "Create one new text resource of engine `class` at an absent `path`, with typed \
         `operations` setting its initial properties and sub-resources, saved by a supervised \
         headless Godot process and published with compare-and-swap. Requires `idempotencyId`; \
         returns the diff and the parsed result.",
        CREATE,
    ),
    engine_write(
        EngineOperation::ResourceBatch.key(),
        "resource.batch.apply",
        "Edit a resource",
        "Apply one atomic batch of typed `operations` — set or clear properties, create \
         sub-resources — to the text resource at `path` whose SHA-256 is `expectedSha256`, \
         through a supervised headless Godot process. Any refused operation publishes nothing; \
         the resource keeps its UID.",
        REWRITE,
    ),
    // --- contract 0.2.0: assets ----------------------------------------------
    file_write(
        AuthoringOperation::AssetPut.key(),
        "asset.put",
        "Upload an asset",
        "Write one binary asset at `path` from base64 `contentBase64` whose complete SHA-256 is \
         `sha256`: inline, or in ordered chunks of `totalBytes` bytes, each after the first \
         naming its `upload` id and offset, published once complete. A replacement needs \
         `expectedSha256`. Journaled; requires `idempotencyId`.",
        REWRITE,
    ),
    file_write(
        AuthoringOperation::AssetDelete.key(),
        "asset.delete",
        "Delete an asset",
        "Delete one asset whose SHA-256 is `expectedSha256`, together with its `.import` \
         sidecar, in one journaled transaction. The engine's imported cache is left to the next \
         import. `dryRun` writes nothing. Requires `idempotencyId`; returns the new revision.",
        REWRITE,
    ),
    bound_read(
        AuthoringOperation::AssetStatus.key(),
        "asset.status",
        "Asset import status",
        "Report one asset's SHA-256 and byte length and its import state — importer, resource \
         type, UID and the byte length and SHA-256 of every imported artifact — without \
         exposing any engine cache path. Read-only; needs no Godot process.",
        IdentityFamily::StaticDiscovery,
    ),
    // --- contract 0.2.0: the godot_mcp editor add-on ------------------------
    ToolDescriptor {
        profiles: EDITOR_PROFILES,
        ..bound_read(
            AuthoringOperation::AddonStatus.key(),
            "editor.addon.status",
            "Editor add-on status",
            "Compare the bundled godot_mcp editor add-on with addons/godot_mcp/ in the project: \
             which files are installed and unchanged, edited, missing or foreign, whether \
             project.godot enables the plugin, and what an install, update or remove would do. \
             Read-only; needs no Godot process.",
            IdentityFamily::StaticDiscovery,
        )
    },
    file_write(
        AuthoringOperation::AddonInstall.key(),
        "editor.addon.install",
        "Install the editor add-on",
        "Write the bundled godot_mcp editor add-on into addons/godot_mcp/, refusing any \
         occupied path, and record its content ownership; `enable` also enables the plugin in \
         project.godot. The editor loads it when it next starts or rescans plugins. One \
         journaled transaction; requires `idempotencyId`.",
        CREATE,
    ),
    file_write(
        AuthoringOperation::AddonUpdate.key(),
        "editor.addon.update",
        "Update the editor add-on",
        "Bring addons/godot_mcp/ to the bundled add-on: unchanged files are replaced, missing \
         ones written, and a file you edited is kept as `<name>.godot-mcp-backup` beside the \
         new one. Nothing changes when it is current. One journaled transaction; requires \
         `idempotencyId`.",
        REWRITE,
    ),
    file_write(
        AuthoringOperation::AddonRemove.key(),
        "editor.addon.remove",
        "Remove the editor add-on",
        "Remove the add-on files whose content still matches what was installed, keep any you \
         edited, and disable the plugin in project.godot. One journaled transaction; requires \
         `idempotencyId`.",
        REWRITE,
    ),
    // --- contract 0.2.0: the live editor --------------------------------------
    ToolDescriptor {
        boundary: None,
        ..editor_read(
            EditorOperation::StatusGet.key(),
            "editor.status.get",
            "Live editor status",
            "Report whether a Godot editor running the godot_mcp plugin is connected, with its \
             Godot and plugin versions, and, when it is, the open scenes and which one is being \
             edited, the selection, unsaved state and the edited scene's undo history. Changes \
             nothing in the editor.",
        )
    },
    editor_read(
        EditorOperation::SceneTreeGet.key(),
        "editor.scene.tree.get",
        "Live scene tree",
        "Return the scene the connected editor is editing (or the open `scene`, when current) as \
         the editor holds it: nodes with class, script, instanced scene, groups and changed \
         stored properties as tagged values, persistent signal connections, unsaved state and a \
         semantic digest. Reading adds no undo history and marks nothing unsaved.",
    ),
    editor_read(
        EditorOperation::TypesGet.key(),
        "editor.types.get",
        "Engine class metadata",
        "Describe one `class` as the connected editor sees it: an engine class of its exact \
         Godot build, or a project script class by its `class_name` — inheritance, its own \
         properties with types, hints and defaults, signals, methods and enums. Inherited \
         members are listed under the parent class.",
    ),
    editor_mutation(
        EditorOperation::SceneOpen.key(),
        "editor.scene.open",
        "Open a scene in the editor",
        "Open the text scene at `path` in the connected editor and make it the scene being \
         edited; `reload` re-reads an open scene from its file, refused while the editor holds \
         changes the file does not. Changes editor state only; no project file is written.",
        EDITOR_STATE,
    ),
    editor_mutation(
        EditorOperation::SelectionSet.key(),
        "editor.selection.set",
        "Select nodes in the editor",
        "Select `nodes` — paths relative to the root of the scene being edited — in the \
         connected editor, replacing the selection. Changes editor state only.",
        EDITOR_STATE,
    ),
    editor_mutation(
        EditorOperation::Undo.key(),
        "editor.undo",
        "Undo in the editor",
        "Undo the most recent action in the undo history of the scene being edited, in the \
         connected editor, as the editor's own Undo does; refused with nothing changed while \
         the editor's global or remote history also has an action to undo. `save` also saves \
         the scene. Returns the scene's semantic digest after the undo. A lost answer is \
         reported as unresolved and never repeated.",
        HISTORY,
    ),
    editor_mutation(
        EditorOperation::Redo.key(),
        "editor.redo",
        "Redo in the editor",
        "Redo the most recently undone action in the undo history of the scene being edited, \
         in the connected editor, as the editor's own Redo does; refused with nothing changed \
         while the editor's global or remote history also has an action to redo. `save` also \
         saves the scene. Returns the scene's semantic digest after the redo. A lost answer is \
         reported as unresolved and never repeated.",
        HISTORY,
    ),
    editor_mutation(
        EditorOperation::AddonReload.key(),
        "editor.addon.reload",
        "Reload the editor add-on",
        "Disable and re-enable the godot_mcp plugin in the connected editor, so it reloads its \
         scripts; the editor reconnects with a new session.",
        EDITOR_STATE,
    ),
    runtime_read(
        SessionOperation::TreeGet.key(),
        "runtime.tree.get",
        "Live scene tree",
        "Read the running game's scene tree from `root` (the whole tree by default) to `depth`: \
         each node's path, class, script, groups, child count and visibility, with the probe's \
         `revision` (it advances whenever a node enters, leaves or is renamed) and the engine \
         `frame` observed. Changes nothing in the game.",
    ),
    runtime_read(
        SessionOperation::PropertiesGet.key(),
        "runtime.properties.get",
        "Live node properties",
        "Read the current values of one running node's `properties` (its stored and \
         editor-visible ones by default) as tagged values, with the probe's revision and frame. \
         A value the contract cannot carry is named, not serialized. Changes nothing in the game.",
    ),
    runtime_read(
        SessionOperation::SignalsGet.key(),
        "runtime.signals.get",
        "Live node signals",
        "Read one running node's signals with their arguments and current connections; with \
         `watch` the probe also counts every later emission of those signals, which later reads \
         report. Answers the probe's revision and frame. Changes nothing in the game.",
    ),
    runtime_read(
        SessionOperation::UiGet.key(),
        "runtime.ui.get",
        "Live UI",
        "Read the running game's visible Controls: path, class, global rectangle, text, \
         disabled and pressed state, and which one has focus, with the probe's revision and \
         frame. Changes nothing in the game.",
    ),
    runtime_command(
        SessionOperation::ActionSend.key(),
        "input.action.send",
        "Send an input action",
        "Press, release or tap an input `action` in a managed game session. A press is held \
         until released, `holdMs` passes, the session stops or the probe loses the server, \
         whichever is first. Answers what the session holds afterwards.",
        INPUT,
    ),
    runtime_command(
        SessionOperation::KeySend.key(),
        "input.key.send",
        "Send a key",
        "Press, release or tap one `key` (a Godot key name) with optional `modifiers` in a \
         managed game session, held under the same expiry rules as an action. Answers what the \
         session holds afterwards.",
        INPUT,
    ),
    runtime_command(
        SessionOperation::MouseSend.key(),
        "input.mouse.send",
        "Send mouse input",
        "Send one mouse `event` to a managed game session: a `move` to a position, or a \
         `button` press, release or tap there, held under the same expiry rules as an action. \
         Answers what the session holds afterwards.",
        INPUT,
    ),
    runtime_command(
        SessionOperation::SequenceSend.key(),
        "input.sequence.send",
        "Send an input sequence",
        "Apply up to 64 input `steps` in order in a managed game session, with `wait` steps \
         between them (at most 10 s in total). Everything the sequence pressed is released when \
         it ends, fails or is cancelled. Answers how many steps ran.",
        INPUT,
    ),
    runtime_command(
        SessionOperation::ScreenshotCapture.key(),
        "runtime.screenshot.capture",
        "Capture a screenshot",
        "Capture the managed game session's root viewport after the next drawn frame as a PNG \
         artifact, read back from the probe and checked against its digest, and answer its \
         opaque artifact URI, size and SHA-256. Needs a windowed session.",
        INPUT,
    ),
    runtime_command(
        SessionOperation::RecordingCapture.key(),
        "runtime.recording.capture",
        "Record the game",
        "Record `durationMs` (at most 10 s) of the managed game session's root viewport at `fps` \
         (at most 30), optionally scaled to `maxWidth`, as one animated PNG artifact, and answer \
         its opaque artifact URI, frame count, size and SHA-256. Needs a windowed session.",
        INPUT,
    ),
    ToolDescriptor {
        name: "editor.session.start",
        title: "Start an editor session",
        description: "Start a headless Godot editor on the project as a managed session this \
             server owns, hosting the GDScript language server and debug adapter on loopback \
             ports the server allocates, and answer once both listen and are held by that \
             editor. debug.command reaches them only through this session's launch receipt. A \
             game the debugger runs renders into the server's display when it has one. The \
             session ends at `lifetimeSeconds` at the latest. Runs editor plugins and tool \
             scripts; needs a Godot editor build.",
        capability: Capability::RuntimeExecute,
        plane: OperationPlane::Runtime,
        boundary: Some(ToolBoundary::Process),
        composed: &[],
        consult_editor: false,
        family: Some(ToolFamily::RuntimeExecution),
        identity_family: IdentityFamily::RuntimeExecution,
        needs: ToolNeeds::HeadlessEngine,
        annotations: EXECUTE,
        profiles: RUNTIME_PROFILES,
        introduced_in: ContractVersion::V0_2_0,
        states: BOUND,
        handler: Some(SessionOperation::EditorStart.key()),
    },
    debug_command(
        SessionOperation::EditorStop.key(),
        "editor.session.stop",
        "Stop an editor session",
        "Stop a managed editor session: its language-server and debug-adapter connections are \
         closed and the editor, with any game it runs under the debugger, is terminated inside \
         its boundary. Stopping a session that already ended answers its record. Answers the \
         exit facts.",
        STOP,
    ),
    debug_read(
        SessionOperation::DebugGet.key(),
        "debug.session.get",
        "Debug session state",
        "Report a managed editor session's language server and debug adapter — whether each \
         endpoint is still owned by the editor's launch receipt, whether it is connected, the \
         debug launch state, the last stop, the breakpoints set and recent debug events — or, \
         without `session`, every editor session's. Changes nothing.",
    ),
    debug_command(
        SessionOperation::DebugCommand.key(),
        "debug.command",
        "Language server and debugger",
        "Send one `command`, keyed by its name and carrying its own fields, to a managed \
         editor session through its launch receipt. Language server: diagnostics (the engine's \
         own for a script) or symbols. Debug adapter: breakpoints, launch (run the main scene \
         under the debugger), stopped (wait for a breakpoint or pause), stack, scopes, \
         variables, continue, pause, stop. Lines are one-based; there is no expression \
         evaluation.",
        INPUT,
    ),
    ToolDescriptor {
        name: "export.presets.list",
        title: "Export presets",
        description: "List the project's export presets: name, platform, whether it is the \
             runnable one, the export template it needs and whether exactly that template of \
             the resolved Godot build is installed, and the names (never the values) of any \
             secret written into a preset. Changes nothing; template facts need a Godot build.",
        capability: Capability::Read,
        plane: OperationPlane::Project,
        boundary: None,
        composed: &[],
        consult_editor: false,
        family: None,
        identity_family: IdentityFamily::StaticDiscovery,
        needs: ToolNeeds::HeadlessEngine,
        annotations: READ,
        profiles: RUNTIME_PROFILES,
        introduced_in: ContractVersion::V0_2_0,
        states: BOUND,
        handler: Some(SessionOperation::ExportPresetsList.key()),
    },
    ToolDescriptor {
        name: "project.export",
        title: "Export the project",
        description: "Start exporting `preset` (a release build, or `mode` debug) through \
             exactly the installed export template of the resolved Godot build, as a managed \
             export this server owns, and answer at once with its export ID. The output goes to \
             the server's private storage, never into the project; every file a successful \
             export produces becomes an artifact, and `smoke` runs the build headless once where \
             this host can run it: a Linux build on Linux, a Windows build on Windows, a macOS \
             app bundle, exported as one zip, on a Mac. A preset with a secret written into it, \
             a missing template and an unsupported platform are refused before anything runs. \
             Executes project code; needs a Godot editor build.",
        capability: Capability::ExportDestructive,
        plane: OperationPlane::Project,
        boundary: Some(ToolBoundary::Process),
        composed: &[],
        consult_editor: false,
        family: Some(ToolFamily::DestructiveExport),
        identity_family: IdentityFamily::DestructiveExport,
        needs: ToolNeeds::HeadlessEngine,
        annotations: EXECUTE,
        profiles: RUNTIME_PROFILES,
        introduced_in: ContractVersion::V0_2_0,
        states: BOUND,
        handler: Some(SessionOperation::ExportStart.key()),
    },
    ToolDescriptor {
        name: "project.export.status",
        title: "Export status",
        description: "Report one managed export — its state, preset, platform and template, \
             progress while it runs, and when it ended the exit facts, every output file with \
             its size, SHA-256 and artifact URI (until the artifact expires) and the smoke run's \
             result — or, without `export`, every export this server owns. Changes nothing.",
        capability: Capability::Read,
        plane: OperationPlane::Project,
        boundary: None,
        composed: &[],
        consult_editor: false,
        family: None,
        identity_family: IdentityFamily::DestructiveExport,
        needs: ToolNeeds::HeadlessEngine,
        annotations: READ,
        profiles: RUNTIME_PROFILES,
        introduced_in: ContractVersion::V0_2_0,
        states: BOUND,
        handler: Some(SessionOperation::ExportStatus.key()),
    },
    ToolDescriptor {
        name: "project.export.cancel",
        title: "Cancel an export",
        description: "Cancel a running managed export: its process is terminated inside its \
             boundary and its partial output is removed, so nothing of it becomes an artifact. \
             Cancelling an export that already ended answers its record.",
        capability: Capability::ExportDestructive,
        plane: OperationPlane::Project,
        boundary: Some(ToolBoundary::Process),
        composed: &[],
        consult_editor: false,
        family: Some(ToolFamily::DestructiveExport),
        identity_family: IdentityFamily::DestructiveExport,
        needs: ToolNeeds::HeadlessEngine,
        annotations: STOP,
        profiles: RUNTIME_PROFILES,
        introduced_in: ContractVersion::V0_2_0,
        states: BOUND,
        handler: Some(SessionOperation::ExportCancel.key()),
    },
];

/// The version 0.1 planned editor and runtime operations, exactly as the
/// first-release policy matrix shaped them.
///
/// None of these is implemented and none is ever discoverable. Test builds use
/// them only to exercise routing and policy logic — plane refusals, composition,
/// degradation — for operation shapes later milestones implement, without
/// putting a handlerless descriptor into [`TOOL_CATALOG`].
#[cfg(any(test, feature = "test-hooks"))]
pub const HISTORICAL_V0_1_PLANNED: &[ToolDescriptor] = &[
    historical_read(
        "editor.selection.get",
        OperationPlane::Editor,
        ToolBoundary::Editor,
        IdentityFamily::EditorInspection,
        EDITOR_PROFILES,
    ),
    historical_read(
        "editor.state.get",
        OperationPlane::Editor,
        ToolBoundary::Editor,
        IdentityFamily::EditorInspection,
        EDITOR_PROFILES,
    ),
    historical_effect(
        "node.create",
        Capability::EditorMutate,
        OperationPlane::Editor,
        ToolBoundary::Editor,
        ToolFamily::EditorMutation,
        IdentityFamily::EditorSceneMutation,
        EDITOR_PROFILES,
    ),
    historical_effect(
        "node.delete",
        Capability::EditorMutate,
        OperationPlane::Editor,
        ToolBoundary::Editor,
        ToolFamily::EditorMutation,
        IdentityFamily::EditorSceneMutation,
        EDITOR_PROFILES,
    ),
    historical_effect(
        "node.property.set",
        Capability::EditorMutate,
        OperationPlane::Editor,
        ToolBoundary::Editor,
        ToolFamily::EditorMutation,
        IdentityFamily::EditorSceneMutation,
        EDITOR_PROFILES,
    ),
    historical_effect(
        "node.reparent",
        Capability::EditorMutate,
        OperationPlane::Editor,
        ToolBoundary::Editor,
        ToolFamily::EditorMutation,
        IdentityFamily::EditorSceneMutation,
        EDITOR_PROFILES,
    ),
    historical_read(
        "runtime.body.get",
        OperationPlane::Runtime,
        ToolBoundary::Runtime,
        IdentityFamily::RuntimeInspection,
        RUNTIME_PROFILES,
    ),
    historical_read(
        "runtime.camera.get",
        OperationPlane::Runtime,
        ToolBoundary::Runtime,
        IdentityFamily::RuntimeInspection,
        RUNTIME_PROFILES,
    ),
    historical_read(
        "runtime.collision.get",
        OperationPlane::Runtime,
        ToolBoundary::Runtime,
        IdentityFamily::RuntimeInspection,
        RUNTIME_PROFILES,
    ),
    historical_read(
        "runtime.environment.get",
        OperationPlane::Runtime,
        ToolBoundary::Runtime,
        IdentityFamily::RuntimeInspection,
        RUNTIME_PROFILES,
    ),
    historical_read(
        "runtime.frame.get",
        OperationPlane::Runtime,
        ToolBoundary::Runtime,
        IdentityFamily::RuntimeInspection,
        RUNTIME_PROFILES,
    ),
    historical_read(
        "runtime.layer.get",
        OperationPlane::Runtime,
        ToolBoundary::Runtime,
        IdentityFamily::RuntimeInspection,
        RUNTIME_PROFILES,
    ),
    historical_read(
        "runtime.lighting.get",
        OperationPlane::Runtime,
        ToolBoundary::Runtime,
        IdentityFamily::RuntimeInspection,
        RUNTIME_PROFILES,
    ),
    historical_read(
        "runtime.mesh.get",
        OperationPlane::Runtime,
        ToolBoundary::Runtime,
        IdentityFamily::RuntimeInspection,
        RUNTIME_PROFILES,
    ),
    historical_read(
        "runtime.particles.get",
        OperationPlane::Runtime,
        ToolBoundary::Runtime,
        IdentityFamily::RuntimeInspection,
        RUNTIME_PROFILES,
    ),
    historical_read(
        "runtime.raycast",
        OperationPlane::Runtime,
        ToolBoundary::Runtime,
        IdentityFamily::RuntimeInspection,
        RUNTIME_PROFILES,
    ),
];

/// The frozen version 0.1 shapes of planned operations a later milestone
/// implemented (test builds only). Each implemented descriptor keeps its frozen
/// shape's capability, plane, boundary, effect family, identity family and
/// profiles, which `implemented_planned_operations_keep_their_frozen_shape`
/// pins, so tests written against the frozen shape stay meaningful.
#[cfg(any(test, feature = "test-hooks"))]
pub const IMPLEMENTED_V0_1_PLANNED: &[ToolDescriptor] = &[
    historical_read(
        "debug.session.get",
        OperationPlane::Runtime,
        ToolBoundary::LspDap,
        IdentityFamily::RuntimeDebug,
        RUNTIME_PROFILES,
    ),
    historical_effect(
        "input.action.send",
        Capability::RuntimeExecute,
        OperationPlane::Runtime,
        ToolBoundary::Runtime,
        ToolFamily::RuntimeExecution,
        IdentityFamily::RuntimeCommand,
        RUNTIME_PROFILES,
    ),
    historical_effect(
        "input.key.send",
        Capability::RuntimeExecute,
        OperationPlane::Runtime,
        ToolBoundary::Runtime,
        ToolFamily::RuntimeExecution,
        IdentityFamily::RuntimeCommand,
        RUNTIME_PROFILES,
    ),
    historical_effect(
        "input.mouse.send",
        Capability::RuntimeExecute,
        OperationPlane::Runtime,
        ToolBoundary::Runtime,
        ToolFamily::RuntimeExecution,
        IdentityFamily::RuntimeCommand,
        RUNTIME_PROFILES,
    ),
    historical_effect(
        "input.sequence.send",
        Capability::RuntimeExecute,
        OperationPlane::Runtime,
        ToolBoundary::Runtime,
        ToolFamily::RuntimeExecution,
        IdentityFamily::RuntimeCommand,
        RUNTIME_PROFILES,
    ),
    historical_read(
        "runtime.properties.get",
        OperationPlane::Runtime,
        ToolBoundary::Runtime,
        IdentityFamily::RuntimeInspection,
        RUNTIME_PROFILES,
    ),
    historical_effect(
        "runtime.recording.capture",
        Capability::RuntimeExecute,
        OperationPlane::Runtime,
        ToolBoundary::Runtime,
        ToolFamily::RuntimeExecution,
        IdentityFamily::RuntimeCommand,
        RUNTIME_PROFILES,
    ),
    historical_effect(
        "runtime.screenshot.capture",
        Capability::RuntimeExecute,
        OperationPlane::Runtime,
        ToolBoundary::Runtime,
        ToolFamily::RuntimeExecution,
        IdentityFamily::RuntimeCommand,
        RUNTIME_PROFILES,
    ),
    historical_read(
        "runtime.signals.get",
        OperationPlane::Runtime,
        ToolBoundary::Runtime,
        IdentityFamily::RuntimeInspection,
        RUNTIME_PROFILES,
    ),
    historical_read(
        "runtime.tree.get",
        OperationPlane::Runtime,
        ToolBoundary::Runtime,
        IdentityFamily::RuntimeInspection,
        RUNTIME_PROFILES,
    ),
    historical_read(
        "runtime.ui.get",
        OperationPlane::Runtime,
        ToolBoundary::Runtime,
        IdentityFamily::RuntimeInspection,
        RUNTIME_PROFILES,
    ),
];

#[cfg(any(test, feature = "test-hooks"))]
const fn historical_read(
    name: &'static str,
    plane: OperationPlane,
    boundary: ToolBoundary,
    identity_family: IdentityFamily,
    profiles: &'static [CapabilityProfile],
) -> ToolDescriptor {
    ToolDescriptor {
        name,
        title: name,
        description: "Version 0.1 planned operation; not implemented and never discoverable.",
        capability: Capability::Read,
        plane,
        boundary: Some(boundary),
        composed: &[],
        consult_editor: false,
        family: None,
        identity_family,
        needs: ToolNeeds::LiveEditor,
        annotations: READ,
        profiles,
        introduced_in: ContractVersion::V0_1_0,
        states: BOUND,
        handler: None,
    }
}

#[cfg(any(test, feature = "test-hooks"))]
#[allow(clippy::too_many_arguments)]
const fn historical_effect(
    name: &'static str,
    capability: Capability,
    plane: OperationPlane,
    boundary: ToolBoundary,
    family: ToolFamily,
    identity_family: IdentityFamily,
    profiles: &'static [CapabilityProfile],
) -> ToolDescriptor {
    ToolDescriptor {
        name,
        title: name,
        description: "Version 0.1 planned operation; not implemented and never discoverable.",
        capability,
        plane,
        boundary: Some(boundary),
        composed: &[],
        consult_editor: false,
        family: Some(family),
        identity_family,
        needs: ToolNeeds::LiveEditor,
        annotations: EXECUTE,
        profiles,
        introduced_in: ContractVersion::V0_1_0,
        states: BOUND,
        handler: None,
    }
}

/// The frozen version 0.1 shape of one planned operation (test builds only).
#[cfg(any(test, feature = "test-hooks"))]
#[must_use]
pub fn historical_v0_1_descriptor(name: &str) -> Option<&'static ToolDescriptor> {
    HISTORICAL_V0_1_PLANNED
        .iter()
        .chain(IMPLEMENTED_V0_1_PLANNED)
        .find(|descriptor| descriptor.name == name)
}

/// The descriptor of one implemented tool.
#[must_use]
pub fn descriptor(name: &str) -> Option<&'static ToolDescriptor> {
    TOOL_CATALOG
        .iter()
        .find(|descriptor| descriptor.name == name)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::manifests::canonical_manifest;

    /// Every operation of every server component serves exactly one catalog
    /// tool, and every catalog tool names the operation that serves it: the
    /// descriptor is the only place a tool's name meets its handler.
    #[test]
    fn every_operation_serves_exactly_one_catalog_tool() {
        let keys: Vec<HandlerKey> = HandlerKey::all().collect();
        for key in &keys {
            let serving: Vec<&str> = TOOL_CATALOG
                .iter()
                .filter(|tool| tool.handler == Some(*key))
                .map(|tool| tool.name)
                .collect();
            assert_eq!(serving.len(), 1, "{key:?} serves {serving:?}");
            assert_eq!(key.descriptor().name, serving[0], "{key:?}");
        }
        for tool in TOOL_CATALOG {
            let key = tool
                .handler
                .unwrap_or_else(|| panic!("{} names no handler", tool.name));
            assert!(
                keys.contains(&key),
                "{}: {key:?} is no component's operation",
                tool.name
            );
        }
        assert_eq!(keys.len(), TOOL_CATALOG.len());
    }

    /// The frozen version 0.1 shapes describe methods, never the code that
    /// serves one, so no test can mistake a shape for a served tool.
    #[test]
    fn frozen_planned_shapes_name_no_handler() {
        for frozen in HISTORICAL_V0_1_PLANNED
            .iter()
            .chain(IMPLEMENTED_V0_1_PLANNED)
        {
            assert_eq!(frozen.handler, None, "{}", frozen.name);
        }
    }

    #[test]
    fn names_are_unique() {
        let names: Vec<&str> = TOOL_CATALOG.iter().map(|tool| tool.name).collect();
        let unique: BTreeSet<&str> = names.iter().copied().collect();
        assert_eq!(names.len(), unique.len(), "duplicate catalog names");
    }

    #[test]
    fn every_profile_that_lists_a_tool_grants_its_capability() {
        for tool in TOOL_CATALOG {
            for profile in tool.profiles {
                assert!(
                    canonical_manifest(*profile)
                        .capabilities
                        .contains(&tool.capability),
                    "{} is listed by {} without its capability",
                    tool.name,
                    profile.manifest_name()
                );
            }
            assert!(
                tool.in_profile(CapabilityProfile::Full),
                "{} must be in the full profile",
                tool.name
            );
        }
    }

    #[test]
    fn historical_planned_shapes_are_exactly_the_unimplemented_v0_1_tools() {
        use crate::manifests::historical_manifest_v0_1;

        let implemented: BTreeSet<&str> = TOOL_CATALOG.iter().map(|tool| tool.name).collect();
        let historical: BTreeSet<&str> = HISTORICAL_V0_1_PLANNED
            .iter()
            .map(|tool| tool.name)
            .collect();
        assert!(implemented.is_disjoint(&historical));
        let v0_1: BTreeSet<String> = historical_manifest_v0_1(CapabilityProfile::Full).tools;
        let v0_1_unimplemented: BTreeSet<&str> = v0_1
            .iter()
            .map(String::as_str)
            .filter(|name| !implemented.contains(name))
            .collect();
        assert_eq!(v0_1_unimplemented, historical);
    }

    #[test]
    fn implemented_planned_operations_keep_their_frozen_shape() {
        use crate::manifests::historical_manifest_v0_1;

        let v0_1: BTreeSet<String> = historical_manifest_v0_1(CapabilityProfile::Full).tools;
        for frozen in IMPLEMENTED_V0_1_PLANNED {
            assert!(
                v0_1.contains(frozen.name),
                "{} was planned in 0.1",
                frozen.name
            );
            let implemented =
                descriptor(frozen.name).unwrap_or_else(|| panic!("{} is implemented", frozen.name));
            assert_eq!(implemented.capability, frozen.capability, "{}", frozen.name);
            assert_eq!(implemented.plane, frozen.plane, "{}", frozen.name);
            assert_eq!(implemented.boundary, frozen.boundary, "{}", frozen.name);
            assert_eq!(implemented.family, frozen.family, "{}", frozen.name);
            assert_eq!(
                implemented.identity_family, frozen.identity_family,
                "{}",
                frozen.name
            );
            assert_eq!(implemented.profiles, frozen.profiles, "{}", frozen.name);
        }
    }

    #[test]
    fn annotations_follow_the_effect_facts() {
        for tool in TOOL_CATALOG {
            assert_eq!(
                tool.annotations.read_only,
                !tool.is_mutation(),
                "{}: read_only must be exactly the absence of an effect family",
                tool.name
            );
            if tool.is_mutation() {
                assert!(tool.boundary.is_some(), "{} needs a boundary", tool.name);
            }
            if tool.annotations.read_only {
                assert!(!tool.annotations.destructive, "{}", tool.name);
            }
            assert!(
                tool.description.len() >= 40,
                "{} needs a real description",
                tool.name
            );
        }
    }
}
