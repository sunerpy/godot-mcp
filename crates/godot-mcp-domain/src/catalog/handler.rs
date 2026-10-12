//! The handler key of every catalog tool.
//!
//! A [`ToolDescriptor`] names the code that serves it with a [`HandlerKey`]:
//! one server component and that component's operation. The server takes a
//! tool's input schema and its handler from this key alone, through exhaustive
//! matches over these enums, so an operation without a schema or a handler
//! does not compile, and a tool's name is written nowhere but in its
//! descriptor.

use serde::Serialize;

use super::{TOOL_CATALOG, ToolDescriptor};

/// Declare one component's operations together with `ALL`, generated from the
/// same variant list so neither can leave out what the other names.
macro_rules! operations {
    (
        $(#[$meta:meta])*
        $name:ident => $component:ident { $($variant:ident),+ $(,)? }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
        pub enum $name {
            $($variant),+
        }

        impl $name {
            /// Every operation, in declaration order.
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            /// The handler key of this operation.
            #[must_use]
            pub const fn key(self) -> HandlerKey {
                HandlerKey::$component(self)
            }

            /// The descriptor of the catalog tool this operation serves.
            #[must_use]
            pub fn descriptor(self) -> &'static ToolDescriptor {
                self.key().descriptor()
            }

            /// The public name of the tool this operation serves.
            #[must_use]
            pub fn tool_name(self) -> &'static str {
                self.descriptor().name
            }
        }
    };
}

operations! {
    /// What the service kernel answers itself.
    KernelOperation => Kernel { Capabilities }
}

operations! {
    /// Static project reads and analysis over the coherent workspace snapshot.
    CoreOperation => Core {
        ProjectInfo,
        ProjectDiscover,
        ProjectDependencies,
        ProjectValidate,
        ScriptValidate,
        ScriptSymbols,
        ScriptReferences,
        ScriptImpact,
        ScriptDiagnostics,
        SceneTree,
        SceneDependencies,
        ResourceTree,
        ResourceDependencies,
        Health,
    }
}

operations! {
    /// Headless engine runs: import, validation, tests and semantic authoring.
    EngineOperation => Engine {
        ImportProject,
        SceneCreate,
        SceneBatch,
        ResourceCreate,
        ResourceBatch,
        UpdateUid,
        UpdateResource,
        SaveScene,
        RunProject,
        RunTests,
        CheckScript,
    }
}

operations! {
    /// File, script, settings and asset authoring and the add-on lifecycle.
    AuthoringOperation => Authoring {
        FilesList,
        FileGet,
        FileCreate,
        FilePatch,
        FileMove,
        FileDelete,
        ScriptCreate,
        ScriptPatch,
        SettingsGet,
        SettingsPatch,
        AssetPut,
        AssetDelete,
        AssetStatus,
        AddonStatus,
        AddonInstall,
        AddonUpdate,
        AddonRemove,
    }
}

operations! {
    /// Reads and changes made through the connected live editor.
    EditorOperation => Editor {
        StatusGet,
        SceneTreeGet,
        TypesGet,
        SceneOpen,
        SelectionSet,
        Undo,
        Redo,
        AddonReload,
    }
}

operations! {
    /// Managed game, editor and export sessions and what runs in them.
    SessionOperation => Session {
        RunStart,
        RunStatus,
        RunLogs,
        RunStop,
        TreeGet,
        PropertiesGet,
        SignalsGet,
        UiGet,
        ActionSend,
        KeySend,
        MouseSend,
        SequenceSend,
        ScreenshotCapture,
        RecordingCapture,
        EditorStart,
        EditorStop,
        DebugGet,
        DebugCommand,
        ExportPresetsList,
        ExportStart,
        ExportStatus,
        ExportCancel,
    }
}

operations! {
    /// Listing, creating and binding projects in a server that holds a
    /// workspace but no project yet.
    BootstrapOperation => Bootstrap { ProjectsList, ProjectCreate, ProjectBind }
}

/// The code that serves one catalog tool: a server component and its
/// operation there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum HandlerKey {
    /// Answered by the service kernel itself.
    Kernel(KernelOperation),
    /// Static project reads and analysis.
    Core(CoreOperation),
    /// Headless engine runs.
    Engine(EngineOperation),
    /// Project file authoring.
    Authoring(AuthoringOperation),
    /// The connected live editor.
    Editor(EditorOperation),
    /// Managed sessions.
    Session(SessionOperation),
    /// The bootstrap state of a workspace server.
    Bootstrap(BootstrapOperation),
}

impl HandlerKey {
    /// Every handler key, component by component.
    pub fn all() -> impl Iterator<Item = Self> {
        KernelOperation::ALL
            .iter()
            .map(|operation| operation.key())
            .chain(CoreOperation::ALL.iter().map(|operation| operation.key()))
            .chain(EngineOperation::ALL.iter().map(|operation| operation.key()))
            .chain(
                AuthoringOperation::ALL
                    .iter()
                    .map(|operation| operation.key()),
            )
            .chain(EditorOperation::ALL.iter().map(|operation| operation.key()))
            .chain(
                SessionOperation::ALL
                    .iter()
                    .map(|operation| operation.key()),
            )
            .chain(
                BootstrapOperation::ALL
                    .iter()
                    .map(|operation| operation.key()),
            )
    }

    /// The descriptor of the catalog tool this key serves.
    ///
    /// # Panics
    /// Never for a key of this build: every key serves exactly one catalog
    /// tool, which `every_operation_serves_exactly_one_catalog_tool` pins.
    #[must_use]
    pub fn descriptor(self) -> &'static ToolDescriptor {
        TOOL_CATALOG
            .iter()
            .find(|descriptor| descriptor.handler == Some(self))
            .expect("every handler key serves exactly one catalog tool")
    }
}
