use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::contracts::OutcomeError;
use godot_mcp_domain::safe_root::SafeRoot;
use godot_mcp_domain::sysfs::open_private_state_dir;
use godot_mcp_engine::discovery::{EnvironmentSnapshot, HostPlatform, discover};
use godot_mcp_engine::probe::{ProbeCache, ProbeError, ProbeRequest};
use godot_mcp_engine::releases::ReleaseLock;
use godot_mcp_engine::supervision::journal::TransportMode as EngineTransportMode;
use godot_mcp_engine::supervision::protocol::{
    EnvironmentAllowlist, OutputCaps, bundled_supervisor,
};
use godot_mcp_engine::validation::runner::EngineBinding;

use super::journal::EngineJournal;
use super::outcome_error;
use super::providers::{LaunchProviders, SharedProvider};
use crate::idempotency::TransportAuthMode;
use crate::journal::{HeldProject, TransactionStore, TransportMode};
use crate::kernel::AccessContext;
use crate::router::{
    EnginePresence, OutcomeRouter, PlaneObservation, ProtocolFidelity, RouterIdentity,
    RuntimePresence, WorkspaceFreshness,
};
use crate::scheduler::OperationDeadline;
use crate::workspace::WorkspaceCoordinator;

pub(crate) struct EngineRuntimeConfig {
    pub(crate) store: Arc<TransactionStore>,
    pub(crate) root: SafeRoot,
    pub(crate) project_root: PathBuf,
    pub(crate) state_dir: PathBuf,
    pub(crate) godot_binary: Option<PathBuf>,
    pub(crate) held: HeldProject,
    pub(crate) router: Arc<OutcomeRouter>,
    pub(crate) operation_deadline: OperationDeadline,
    pub(crate) workspace: Arc<WorkspaceCoordinator>,
    /// The live-editor bridge, when the profile serves the editor plane.
    pub(crate) editor: Option<Arc<crate::editor::bridge::EditorBridge>>,
    /// The boundary every launch is contained in, and the project's trust.
    pub(crate) providers: Arc<LaunchProviders>,
}

pub(crate) struct EngineRuntime {
    pub(super) journal: Arc<EngineJournal>,
    pub(super) project_root: PathBuf,
    pub(super) state_dir: PathBuf,
    pub(super) godot_binary: Option<PathBuf>,
    pub(super) held: HeldProject,
    pub(super) router: Arc<OutcomeRouter>,
    pub(super) operation_deadline: OperationDeadline,
    pub(super) workspace: Arc<WorkspaceCoordinator>,
    pub(super) editor: Option<Arc<crate::editor::bridge::EditorBridge>>,
    pub(super) providers: Arc<LaunchProviders>,
}

pub(super) struct ResolvedEngine {
    pub(super) binding: EngineBinding,
    pub(super) provider: SharedProvider,
    /// The build's raw `--version` line, which names its template directory.
    pub(super) version_line: String,
}

pub(super) enum EngineResolution {
    Ready(Box<ResolvedEngine>),
    MissingCapability { capability: String, message: String },
}

impl EngineRuntime {
    pub(crate) fn new(config: EngineRuntimeConfig) -> Self {
        Self {
            journal: Arc::new(EngineJournal::with_root(config.store, config.root)),
            project_root: config.project_root,
            state_dir: config.state_dir,
            godot_binary: config.godot_binary,
            held: config.held,
            router: config.router,
            operation_deadline: config.operation_deadline,
            workspace: config.workspace,
            editor: config.editor,
            providers: config.providers,
        }
    }

    pub(super) fn root(&self) -> Result<&SafeRoot, OutcomeError> {
        self.journal
            .root()
            .map_err(|error| outcome_error(ErrorKind::Internal, error.to_string()))
    }

    pub(super) fn route_identity(access: &AccessContext) -> RouterIdentity {
        RouterIdentity {
            transport_mode: access.transport_mode(),
            principal: access.mode_principal().to_journal_string(),
            session: access.connection_session().to_owned(),
        }
    }

    pub(super) fn route_observation(&self) -> PlaneObservation {
        PlaneObservation::new(
            self.router.plane_availability(),
            EnginePresence::HeadlessOnly,
            RuntimePresence::NoInstance,
            ProtocolFidelity::Full,
            // The workspace coordinator, not an assumption, says whether the
            // static view is current.
            if self.workspace.current().coherence().is_coherent() {
                WorkspaceFreshness::Fresh
            } else {
                WorkspaceFreshness::StaleIndex
            },
        )
    }

    pub(super) async fn resolve_engine(&self) -> Result<ResolvedEngine, OutcomeError> {
        match self.resolve_engine_for_operation().await? {
            EngineResolution::Ready(resolved) => Ok(*resolved),
            EngineResolution::MissingCapability { message, .. } => {
                Err(outcome_error(ErrorKind::Unsupported, message))
            }
        }
    }

    pub(super) async fn resolve_engine_for_operation(
        &self,
    ) -> Result<EngineResolution, OutcomeError> {
        // No launch can start without a boundary, so nothing is probed first.
        let provider = self.providers.selected()?;
        let resolved = discover(
            self.godot_binary.as_deref(),
            HostPlatform::current(),
            &EnvironmentSnapshot::from_process_env(),
        )
        .map_err(|error| outcome_error(error.error_kind(), error.to_string()))?;
        let lock = ReleaseLock::from_json_slice(include_bytes!(
            "../../../../tests/fixtures/godot-releases.lock.json"
        ))
        .map_err(|error| outcome_error(ErrorKind::Internal, error.to_string()))?;
        let snapshot = match ProbeCache::new(lock)
            .probe_uncached(&ProbeRequest {
                binary: resolved.path.clone(),
                deadline: PROBE_DEADLINE,
            })
            .await
        {
            Ok(snapshot) => snapshot,
            Err(error) => {
                if let ProbeError::MissingCapability { capability, .. } = &error {
                    return Ok(EngineResolution::MissingCapability {
                        capability: capability.clone(),
                        message: error.to_string(),
                    });
                }
                return Err(outcome_error(error.error_kind(), error.to_string()));
            }
        };
        let environment = private_engine_environment(&self.state_dir)?;
        Ok(EngineResolution::Ready(Box::new(ResolvedEngine {
            binding: EngineBinding {
                program: resolved.path,
                binary_sha256: snapshot.key.binary_sha256,
                supervisor_binary: bundled_supervisor(),
                project_root: self.project_root.clone(),
                work_dir: self.state_dir.join("engine-launches"),
                environment,
                caps: OutputCaps {
                    stdout_bytes: 1_048_576,
                    stderr_bytes: 1_048_576,
                },
                trust: self.providers.trust(),
                capabilities: snapshot.capabilities,
            },
            provider,
            version_line: snapshot.version_line,
        })))
    }
}

/// How long `--version` and `--help` may take. A freshly downloaded macOS app is
/// assessed by Gatekeeper before its first run: on a macOS CI host, Godot
/// 4.4.1's first `--help` took longer than 10 s, while every later one returned
/// at once.
const PROBE_DEADLINE: Duration = Duration::from_secs(60);

fn private_engine_environment(
    state_dir: &std::path::Path,
) -> Result<EnvironmentAllowlist, OutcomeError> {
    let home = state_dir.join("engine-home");
    open_private_state_dir(&home)
        .map_err(|error| outcome_error(ErrorKind::Internal, error.to_string()))?;
    let home = home.to_str().ok_or_else(|| {
        outcome_error(
            ErrorKind::Internal,
            "the private engine home path is not valid UTF-8",
        )
    })?;
    let environment = engine_environment(EngineHost::current(), home);
    // A Windows engine's profile directories are created where it expects them.
    for name in ["APPDATA", "LOCALAPPDATA", "TEMP"] {
        if let Some(directory) = environment.extra.get(name) {
            std::fs::create_dir_all(directory)
                .map_err(|error| outcome_error(ErrorKind::Internal, error.to_string()))?;
        }
    }
    Ok(environment)
}

/// The family of host an engine child runs on, for its environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EngineHost {
    Unix,
    Windows,
}

impl EngineHost {
    const fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else {
            Self::Unix
        }
    }
}

/// The environment a Godot child gets: its private home, and none of the
/// server's own environment except, on Windows, the few system variables a
/// program cannot run without. Godot keeps its settings and data under the
/// profile directories, which therefore point inside the private home too.
fn engine_environment(host: EngineHost, home: &str) -> EnvironmentAllowlist {
    let mut environment = EnvironmentAllowlist::default();
    environment.extra.insert("HOME".to_owned(), home.to_owned());
    if host == EngineHost::Windows {
        for (name, value) in [
            ("USERPROFILE", home.to_owned()),
            ("APPDATA", format!(r"{home}\AppData\Roaming")),
            ("LOCALAPPDATA", format!(r"{home}\AppData\Local")),
            ("TEMP", format!(r"{home}\Temp")),
            ("TMP", format!(r"{home}\Temp")),
        ] {
            environment.extra.insert(name.to_owned(), value);
        }
        // Winsock and the system DLL search fail without the system root.
        for name in [
            "SystemRoot",
            "SystemDrive",
            "windir",
            "NUMBER_OF_PROCESSORS",
            "PROCESSOR_ARCHITECTURE",
            "OS",
            "PATHEXT",
        ] {
            environment.allow.insert(name.to_owned());
        }
    }
    environment
}

pub(super) const fn journal_transport(mode: TransportAuthMode) -> TransportMode {
    match mode {
        TransportAuthMode::Stdio => TransportMode::Stdio,
        TransportAuthMode::TrustedLocal => TransportMode::TrustedLocal,
        TransportAuthMode::ProxiedOidc => TransportMode::ProxiedOidc,
    }
}

pub(super) const fn engine_transport(mode: TransportAuthMode) -> EngineTransportMode {
    match mode {
        TransportAuthMode::Stdio => EngineTransportMode::Stdio,
        TransportAuthMode::TrustedLocal => EngineTransportMode::TrustedLocal,
        TransportAuthMode::ProxiedOidc => EngineTransportMode::ProxiedOidc,
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::fs;

    use super::*;

    #[test]
    fn a_windows_engine_lives_in_its_private_home_with_only_what_windows_needs() {
        let home = r"C:\state\engine-home";
        let environment = engine_environment(EngineHost::Windows, home);
        for (name, value) in [
            ("HOME", home.to_owned()),
            ("USERPROFILE", home.to_owned()),
            ("APPDATA", format!(r"{home}\AppData\Roaming")),
            ("LOCALAPPDATA", format!(r"{home}\AppData\Local")),
            ("TEMP", format!(r"{home}\Temp")),
            ("TMP", format!(r"{home}\Temp")),
        ] {
            assert_eq!(environment.extra.get(name), Some(&value), "{name}");
        }
        // Winsock and the loader find the system through these, and nothing of
        // the server's own profile is copied.
        for name in ["SystemRoot", "SystemDrive", "windir", "PATHEXT"] {
            assert!(environment.allow.contains(name), "{name}");
        }
        for name in ["USERNAME", "PATH", "APPDATA", "USERPROFILE"] {
            assert!(!environment.allow.contains(name), "{name}");
        }
        let unix = engine_environment(EngineHost::Unix, "/state/engine-home");
        assert!(unix.allow.is_empty());
        assert_eq!(
            unix.extra,
            [("HOME".to_owned(), "/state/engine-home".to_owned())]
                .into_iter()
                .collect()
        );
    }

    #[cfg(unix)]
    #[test]
    fn engine_environment_uses_only_a_private_home() {
        let state_dir =
            std::env::temp_dir().join(format!("godot-mcp-engine-home-{}", std::process::id()));
        let _ = fs::remove_dir_all(&state_dir);
        fs::create_dir_all(&state_dir).expect("state directory");

        let environment = private_engine_environment(&state_dir).expect("private environment");
        let home = state_dir.join("engine-home");

        assert!(environment.allow.is_empty());
        assert_eq!(
            environment.extra,
            [("HOME".to_owned(), home.to_string_lossy().into_owned())]
                .into_iter()
                .collect()
        );
        assert!(home.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            assert_eq!(
                fs::metadata(&home)
                    .expect("home metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        let _ = fs::remove_dir_all(&state_dir);
    }
}
