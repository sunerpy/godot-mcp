//! Which containment boundary a supervised Godot launch is created in.
//!
//! One registry per project runtime, built when the runtime opens from the
//! host and the `[process]` configuration. It names the one provider every new
//! launch uses, or why this host offers none, and every provider a durable
//! launch record may name: startup reconciliation and a stop reopen a boundary
//! whichever provider created it, so a configuration change never orphans a
//! process.
//!
//! | host | unset `process.boundary` | configurable |
//! | --- | --- | --- |
//! | Linux | the weak locator, for a trusted project | `cgroup`, `adapter`, `weak` |
//! | macOS | the weak locator, for a trusted project | `adapter`, `weak` |
//! | Windows | a Job Object | `job_object` |
//! | others | nothing: this build cannot observe processes there | — |
//!
//! An untrusted project is never given the weak locator. Without a strong
//! boundary it starts nothing, and the process plane says why.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use godot_mcp_domain::config::{ProcessAdapter, ProcessBoundary, ProcessConfig, ProcessTrust};
use godot_mcp_domain::contracts::{ErrorKind, OutcomeError};
use godot_mcp_engine::supervision::adapter::ContainerAdapterProvider;
use godot_mcp_engine::supervision::boundary::{
    BoundaryError, BoundaryIdentity, BoundaryKind, BoundaryProvider, ContainmentBoundary,
    TrustLevel,
};
use godot_mcp_engine::supervision::cgroup::CgroupV2Provider;
use godot_mcp_engine::supervision::job_object::JobObjectProvider;
use godot_mcp_engine::supervision::weak_unix::UnixLocatorProvider;

/// A provider shared by every launch site of one runtime.
pub(crate) type SharedProvider = Arc<dyn BoundaryProvider + Send + Sync>;

/// The cgroup this server moves itself into inside its delegated cgroup.
#[cfg(target_os = "linux")]
const SERVER_LEAF: &str = "godot-mcp-server";
/// The cgroup every launch cgroup is created under.
#[cfg(target_os = "linux")]
const LAUNCH_ROOT: &str = "godot-mcp-launches";
/// The controllers a launch cgroup's limits are enforced by.
#[cfg(target_os = "linux")]
const CONTROLLERS: [&str; 3] = ["cpu", "memory", "pids"];
/// The prefix of every launch Job Object's name.
const JOB_PREFIX: &str = "godot-mcp";

/// The operating system a registry is built for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Host {
    Linux,
    MacOs,
    Windows,
    Other,
}

impl Host {
    /// The host this build runs on.
    pub(crate) const fn current() -> Self {
        if cfg!(target_os = "linux") {
            Self::Linux
        } else if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(windows) {
            Self::Windows
        } else {
            Self::Other
        }
    }
}

/// The boundary a configuration selects on a host, before it is built.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Choice {
    Weak,
    Cgroup,
    Adapter(ProcessAdapter),
    JobObject,
    Nothing(String),
}

/// Why the weak locator is never used for untrusted code.
const UNTRUSTED_WEAK: &str = "an untrusted project is never contained by the weak locator; set \
                              process.boundary to a strong boundary (adapter on Linux and macOS)";
/// Why a delegated cgroup is never used for untrusted code.
const UNTRUSTED_CGROUP: &str = "a delegated cgroup limits and terminates its members but does not \
                                keep a member from moving itself out of it, so it never contains \
                                an untrusted project; set process.boundary to adapter";
/// What the locator and the adapter rely on.
const OBSERVATIONS: &str = "needs the live process observations this build implements on Linux \
                            and macOS only";

fn choose(host: Host, process: &ProcessConfig) -> Choice {
    let untrusted = process.trust == ProcessTrust::Untrusted;
    let observable = matches!(host, Host::Linux | Host::MacOs);
    match (process.boundary, host) {
        (Some(ProcessBoundary::Weak), _) if untrusted => Choice::Nothing(UNTRUSTED_WEAK.to_owned()),
        (Some(ProcessBoundary::Weak), _) if observable => Choice::Weak,
        (Some(ProcessBoundary::Weak), _) => {
            Choice::Nothing(format!("the weak locator {OBSERVATIONS}"))
        }
        (Some(ProcessBoundary::Cgroup), Host::Linux) if untrusted => {
            Choice::Nothing(UNTRUSTED_CGROUP.to_owned())
        }
        (Some(ProcessBoundary::Cgroup), Host::Linux) => Choice::Cgroup,
        (Some(ProcessBoundary::Cgroup), _) => {
            Choice::Nothing("cgroup v2 containment exists only on Linux".to_owned())
        }
        (Some(ProcessBoundary::Adapter), _) if observable => match &process.adapter {
            Some(adapter) => Choice::Adapter(adapter.clone()),
            None => {
                Choice::Nothing("process.boundary adapter names no [process.adapter]".to_owned())
            }
        },
        (Some(ProcessBoundary::Adapter), _) => {
            Choice::Nothing(format!("the container adapter {OBSERVATIONS}"))
        }
        (Some(ProcessBoundary::JobObject), Host::Windows) | (None, Host::Windows) => {
            Choice::JobObject
        }
        (Some(ProcessBoundary::JobObject), _) => {
            Choice::Nothing("a Job Object exists only on Windows".to_owned())
        }
        (None, Host::Linux) if untrusted => Choice::Nothing(
            "an untrusted project needs a strong boundary: set process.boundary to adapter"
                .to_owned(),
        ),
        (None, Host::MacOs) if untrusted => Choice::Nothing(
            "an untrusted project needs a strong boundary: set process.boundary to adapter"
                .to_owned(),
        ),
        (None, Host::Linux | Host::MacOs) => Choice::Weak,
        (None, Host::Other) => Choice::Nothing(
            "this build starts no Godot process on this operating system".to_owned(),
        ),
    }
}

/// The launch providers of one runtime.
pub(crate) struct LaunchProviders {
    selected: Result<SharedProvider, String>,
    known: Vec<SharedProvider>,
    trust: TrustLevel,
}

impl std::fmt::Debug for LaunchProviders {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LaunchProviders")
            .field(
                "selected",
                &self.selected.as_ref().map(|provider| provider.kind()),
            )
            .field("trust", &self.trust)
            .finish_non_exhaustive()
    }
}

impl LaunchProviders {
    /// The registry for this host and `process`; `supervisor` is the bundled
    /// supervisor the weak locator spawns.
    pub(crate) fn for_host(process: &ProcessConfig, state_dir: &Path, supervisor: PathBuf) -> Self {
        Self::assemble(
            Host::current(),
            process,
            state_dir,
            supervisor,
            delegated_launch_root,
        )
    }

    fn assemble(
        host: Host,
        process: &ProcessConfig,
        state_dir: &Path,
        supervisor: PathBuf,
        delegated: impl FnOnce() -> Result<PathBuf, String>,
    ) -> Self {
        let weak: SharedProvider = Arc::new(UnixLocatorProvider::new(
            state_dir.join("launches"),
            supervisor,
        ));
        let selected: Result<SharedProvider, String> = match choose(host, process) {
            Choice::Weak => Ok(Arc::clone(&weak)),
            Choice::Cgroup => {
                delegated().map(|root| Arc::new(CgroupV2Provider::new(root)) as SharedProvider)
            }
            Choice::Adapter(adapter) => Ok(Arc::new(ContainerAdapterProvider::new(
                PathBuf::from(adapter.program),
                PathBuf::from(adapter.root),
            ))),
            Choice::JobObject => Ok(Arc::new(JobObjectProvider::new(JOB_PREFIX.to_owned()))),
            Choice::Nothing(reason) => Err(reason),
        };
        // A record another configuration made stays reopenable: the weak
        // locator, and the host's strong mechanism, reopen from the recorded
        // identity alone.
        let mut known: Vec<SharedProvider> = Vec::new();
        if let Ok(provider) = &selected {
            known.push(Arc::clone(provider));
        }
        let mut fallback = vec![weak];
        match host {
            Host::Linux => fallback.push(Arc::new(CgroupV2Provider::new(PathBuf::from(
                "/sys/fs/cgroup",
            )))),
            Host::Windows => fallback.push(Arc::new(JobObjectProvider::new(JOB_PREFIX.to_owned()))),
            Host::MacOs | Host::Other => {}
        }
        for provider in fallback {
            if !known.iter().any(|known| known.kind() == provider.kind()) {
                known.push(provider);
            }
        }
        Self {
            selected,
            known,
            trust: match process.trust {
                ProcessTrust::Trusted => TrustLevel::Trusted,
                ProcessTrust::Untrusted => TrustLevel::Untrusted,
            },
        }
    }

    /// The provider every new launch uses.
    ///
    /// # Errors
    /// Returns `unavailable` naming why this host offers no boundary.
    pub(crate) fn selected(&self) -> Result<SharedProvider, OutcomeError> {
        self.selected.clone().map_err(|reason| {
            OutcomeError::new(
                ErrorKind::Unavailable,
                format!("no process boundary can contain a Godot launch here: {reason}"),
                false,
                std::collections::BTreeMap::new(),
            )
        })
    }

    /// The selected boundary's kind and strength, or why a launch cannot
    /// start on this host now.
    pub(crate) fn status(&self) -> Result<(BoundaryKind, &'static str), String> {
        let provider = self.selected.as_ref().map_err(Clone::clone)?;
        provider.available().map_err(|error| {
            format!(
                "the {} boundary is unavailable: {error}",
                provider.kind().as_str()
            )
        })?;
        Ok((provider.kind(), provider.strength().as_str()))
    }

    /// Whether a launch can start now.
    pub(crate) fn usable(&self) -> bool {
        self.status().is_ok()
    }

    /// Every provider a durable record may name, selected first.
    pub(crate) fn known(&self) -> Vec<&dyn BoundaryProvider> {
        self.known
            .iter()
            .map(|provider| provider.as_ref() as &dyn BoundaryProvider)
            .collect()
    }

    /// Reopen the boundary `identity` names, whichever provider made it.
    /// `None` means that provider proved the boundary is gone.
    ///
    /// # Errors
    /// Returns the matching provider's error, and `unavailable` when no
    /// provider of this server can reopen that kind of boundary: nothing may
    /// conclude such a boundary is empty.
    pub(crate) fn reopen(
        &self,
        identity: &BoundaryIdentity,
    ) -> Result<Option<Box<dyn ContainmentBoundary>>, BoundaryError> {
        self.known
            .iter()
            .find(|provider| provider.kind() == identity.kind)
            .ok_or_else(|| {
                BoundaryError::Unavailable(format!(
                    "this server has no {} provider to reopen the boundary with",
                    identity.kind.as_str()
                ))
            })?
            .reopen(identity)
    }

    /// The trust every launch request carries.
    pub(crate) const fn trust(&self) -> TrustLevel {
        self.trust
    }
}

/// The launch root inside the cgroup v2 sub-tree delegated to the cgroup this
/// server runs in, prepared once per process.
fn delegated_launch_root() -> Result<PathBuf, String> {
    static PREPARED: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    PREPARED.get_or_init(prepare_delegated_cgroup).clone()
}

/// Prepare the delegated cgroup: the server moves itself into its own leaf so
/// the delegated cgroup may enable the controllers that limit launches, then
/// creates the launch root under it with the same controllers.
#[cfg(target_os = "linux")]
fn prepare_delegated_cgroup() -> Result<PathBuf, String> {
    let membership = std::fs::read_to_string("/proc/self/cgroup")
        .map_err(|error| format!("this server's cgroup could not be read: {error}"))?;
    let relative = membership
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .ok_or_else(|| "this server is not in a cgroup v2 hierarchy".to_owned())?;
    let own = Path::new("/sys/fs/cgroup").join(relative.trim().trim_start_matches('/'));
    let delegated = if own.file_name().is_some_and(|name| name == SERVER_LEAF) {
        own.parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| own.clone())
    } else {
        own.clone()
    };
    // Every reason is public, and the cgroup path is the boundary's locator, so
    // none of them names it.
    let refused = |what: &str, error: &dyn std::fmt::Display| {
        format!(
            "the cgroup this server runs in is not delegated to it, {what}: {error}; start the \
             server inside a delegated cgroup, for example with `systemd-run --user --scope -p \
             Delegate=yes`"
        )
    };
    let server = delegated.join(SERVER_LEAF);
    let launches = delegated.join(LAUNCH_ROOT);
    create_cgroup(&server).map_err(|error| refused("creating this server's leaf", &error))?;
    if own != server {
        std::fs::write(server.join("cgroup.procs"), std::process::id().to_string())
            .map_err(|error| refused("moving this server into its own leaf", &error))?;
    }
    let offered = std::fs::read_to_string(delegated.join("cgroup.controllers"))
        .map_err(|error| refused("reading its controllers", &error))?;
    let missing: Vec<&str> = CONTROLLERS
        .iter()
        .copied()
        .filter(|controller| !offered.split_whitespace().any(|offer| offer == *controller))
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "the delegated cgroup does not offer the {} controller(s) launch limits need",
            missing.join(", ")
        ));
    }
    let enable = CONTROLLERS
        .iter()
        .map(|controller| format!("+{controller}"))
        .collect::<Vec<_>>()
        .join(" ");
    std::fs::write(delegated.join("cgroup.subtree_control"), &enable).map_err(|error| {
        refused(
            "enabling the cpu, memory and pids controllers (another process may still run in \
             it)",
            &error,
        )
    })?;
    create_cgroup(&launches).map_err(|error| refused("creating the launch root", &error))?;
    std::fs::write(launches.join("cgroup.subtree_control"), &enable)
        .map_err(|error| refused("enabling the launch root's controllers", &error))?;
    Ok(launches)
}

#[cfg(target_os = "linux")]
fn create_cgroup(path: &Path) -> std::io::Result<()> {
    match std::fs::create_dir(path) {
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        other => other,
    }
}

#[cfg(not(target_os = "linux"))]
fn prepare_delegated_cgroup() -> Result<PathBuf, String> {
    Err("cgroup v2 containment exists only on Linux".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(boundary: Option<ProcessBoundary>, trust: ProcessTrust) -> ProcessConfig {
        ProcessConfig {
            boundary,
            trust,
            adapter: None,
        }
    }

    fn adapter() -> ProcessAdapter {
        ProcessAdapter {
            program: "/opt/adapter".to_owned(),
            root: "/var/lib/boundaries".to_owned(),
        }
    }

    #[test]
    fn the_host_default_is_the_weak_locator_where_processes_are_observable_and_a_job_object_on_windows()
     {
        let trusted = config(None, ProcessTrust::Trusted);
        assert_eq!(choose(Host::Linux, &trusted), Choice::Weak);
        assert_eq!(choose(Host::MacOs, &trusted), Choice::Weak);
        assert_eq!(choose(Host::Windows, &trusted), Choice::JobObject);
        assert!(matches!(choose(Host::Other, &trusted), Choice::Nothing(_)));
    }

    #[test]
    fn an_untrusted_project_is_never_given_the_weak_locator() {
        for host in [Host::Linux, Host::MacOs] {
            for boundary in [None, Some(ProcessBoundary::Weak)] {
                let Choice::Nothing(reason) =
                    choose(host, &config(boundary, ProcessTrust::Untrusted))
                else {
                    panic!("{boundary:?} on {host:?} must not contain untrusted code");
                };
                assert!(reason.contains("strong boundary"), "{reason}");
            }
        }
        assert_eq!(
            choose(Host::Windows, &config(None, ProcessTrust::Untrusted)),
            Choice::JobObject
        );
    }

    /// A member of a delegated cgroup runs as the user the sub-tree is
    /// delegated to, so it can write its PID into another cgroup of that
    /// sub-tree: the cgroup limits and terminates its members, but cannot keep
    /// one in. An untrusted project needs the adapter.
    #[test]
    fn a_delegated_cgroup_never_contains_an_untrusted_project() {
        let Choice::Nothing(reason) = choose(
            Host::Linux,
            &config(Some(ProcessBoundary::Cgroup), ProcessTrust::Untrusted),
        ) else {
            panic!("a delegated cgroup must not contain untrusted code");
        };
        assert!(reason.contains("moving itself out"), "{reason}");
        assert!(reason.contains("adapter"), "{reason}");
        assert_eq!(
            choose(
                Host::Linux,
                &config(Some(ProcessBoundary::Cgroup), ProcessTrust::Trusted)
            ),
            Choice::Cgroup
        );
    }

    #[test]
    fn a_boundary_this_host_lacks_is_refused_with_its_reason() {
        for (host, boundary, expected) in [
            (Host::Windows, ProcessBoundary::Cgroup, "only on Linux"),
            (Host::MacOs, ProcessBoundary::Cgroup, "only on Linux"),
            (Host::Linux, ProcessBoundary::JobObject, "only on Windows"),
            (Host::MacOs, ProcessBoundary::JobObject, "only on Windows"),
            (Host::Other, ProcessBoundary::Weak, "Linux and macOS only"),
            (
                Host::Other,
                ProcessBoundary::Adapter,
                "Linux and macOS only",
            ),
        ] {
            let Choice::Nothing(reason) =
                choose(host, &config(Some(boundary), ProcessTrust::Trusted))
            else {
                panic!("{boundary:?} on {host:?} must be refused");
            };
            assert!(reason.contains(expected), "{reason}");
        }
        let configured = ProcessConfig {
            boundary: Some(ProcessBoundary::Adapter),
            trust: ProcessTrust::Untrusted,
            adapter: Some(adapter()),
        };
        for host in [Host::Linux, Host::MacOs] {
            assert_eq!(choose(host, &configured), Choice::Adapter(adapter()));
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_missing_supervisor_is_reported_without_where_this_server_is_installed() {
        let state = tempfile::tempdir().expect("state");
        let install = tempfile::tempdir().expect("install directory");
        let providers = LaunchProviders::assemble(
            Host::Linux,
            &config(None, ProcessTrust::Trusted),
            state.path(),
            install.path().join("godot-mcp-supervisor"),
            || unreachable!("the weak locator needs no cgroup"),
        );
        let reason = providers
            .status()
            .expect_err("no supervisor is beside this server");
        assert!(reason.contains("`godot-mcp-supervisor`"), "{reason}");
        for private in [install.path(), state.path()] {
            assert!(!reason.contains(&*private.to_string_lossy()), "{reason}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_launch_root_that_is_no_cgroup_is_refused_without_naming_it() {
        let state = tempfile::tempdir().expect("state");
        let root = tempfile::tempdir().expect("an ordinary directory");
        let path = root.path().to_path_buf();
        let providers = LaunchProviders::assemble(
            Host::Linux,
            &config(Some(ProcessBoundary::Cgroup), ProcessTrust::Trusted),
            state.path(),
            state.path().join("godot-mcp-supervisor"),
            move || Ok(path),
        );
        let reason = providers.status().expect_err("not a cgroup");
        assert!(reason.contains("cgroup v2"), "{reason}");
        assert!(
            !reason.contains(&*root.path().to_string_lossy()),
            "the launch root is a boundary locator and stays private: {reason}"
        );
    }

    #[test]
    fn a_missing_adapter_is_refused_by_its_setting_and_not_its_path() {
        let state = tempfile::tempdir().expect("state");
        let missing = state.path().join("no-such-adapter");
        let configured = ProcessConfig {
            boundary: Some(ProcessBoundary::Adapter),
            trust: ProcessTrust::Untrusted,
            adapter: Some(ProcessAdapter {
                program: missing.display().to_string(),
                root: state.path().join("boundaries").display().to_string(),
            }),
        };
        let providers = LaunchProviders::assemble(
            Host::Linux,
            &configured,
            state.path(),
            state.path().join("godot-mcp-supervisor"),
            || unreachable!("an adapter needs no cgroup"),
        );
        let reason = providers.status().expect_err("the adapter is missing");
        assert!(reason.contains("process.adapter.program"), "{reason}");
        assert!(
            !reason.contains(&*state.path().to_string_lossy()),
            "{reason}"
        );
    }

    #[test]
    fn a_macos_registry_launches_a_trusted_project_in_the_weak_locator() {
        let state = tempfile::tempdir().expect("state");
        let providers = LaunchProviders::assemble(
            Host::MacOs,
            &config(None, ProcessTrust::Trusted),
            state.path(),
            state.path().join("supervisor"),
            || panic!("macOS never prepares a cgroup"),
        );
        let selected = providers.selected().expect("the weak locator is selected");
        assert_eq!(selected.kind(), BoundaryKind::UnixLocator);
        assert_eq!(
            providers
                .known()
                .iter()
                .map(|provider| provider.kind())
                .collect::<Vec<_>>(),
            vec![BoundaryKind::UnixLocator]
        );
        let untrusted = LaunchProviders::assemble(
            Host::MacOs,
            &config(None, ProcessTrust::Untrusted),
            state.path(),
            state.path().join("supervisor"),
            || panic!("macOS never prepares a cgroup"),
        );
        let Err(refused) = untrusted.selected() else {
            panic!("an untrusted project on macOS gets no boundary without an adapter");
        };
        assert!(refused.message().contains("adapter"), "{refused:?}");
    }

    #[test]
    fn a_cgroup_that_is_not_delegated_leaves_launches_unavailable_with_its_reason() {
        let state = tempfile::tempdir().expect("state");
        let providers = LaunchProviders::assemble(
            Host::Linux,
            &config(Some(ProcessBoundary::Cgroup), ProcessTrust::Trusted),
            state.path(),
            state.path().join("supervisor"),
            || Err("the cgroup this server runs in is not delegated to it".to_owned()),
        );
        let Err(refused) = providers.selected() else {
            panic!("nothing is selected");
        };
        assert_eq!(refused.kind(), ErrorKind::Unavailable);
        assert!(refused.message().contains("not delegated"), "{refused:?}");
        assert!(!providers.usable());
        assert!(providers.status().unwrap_err().contains("not delegated"));
        // Records the locator or a cgroup made are still reopenable.
        let kinds: Vec<BoundaryKind> = providers
            .known()
            .iter()
            .map(|provider| provider.kind())
            .collect();
        assert_eq!(
            kinds,
            vec![BoundaryKind::UnixLocator, BoundaryKind::CgroupV2]
        );
    }

    #[test]
    fn the_registry_reopens_a_boundary_whichever_provider_made_it() {
        let state = tempfile::tempdir().expect("state");
        let launches = tempfile::tempdir().expect("launch root");
        let root = launches.path().to_path_buf();
        let providers = LaunchProviders::assemble(
            Host::Linux,
            &config(Some(ProcessBoundary::Cgroup), ProcessTrust::Trusted),
            state.path(),
            state.path().join("supervisor"),
            move || Ok(root),
        );
        assert_eq!(providers.trust(), TrustLevel::Trusted);
        let selected = providers.selected().expect("a cgroup is selected");
        assert_eq!(selected.kind(), BoundaryKind::CgroupV2);
        // An earlier server's cgroup, wherever it lived, reopens from its
        // recorded path.
        let earlier = tempfile::tempdir().expect("earlier cgroup");
        std::fs::write(earlier.path().join("cgroup.procs"), "").expect("procs");
        let identity = BoundaryIdentity {
            kind: BoundaryKind::CgroupV2,
            identity: earlier.path().display().to_string(),
            control_endpoint: None,
            control_nonce: None,
            control_socket_inode: None,
        };
        assert!(providers.reopen(&identity).expect("reopen").is_some());
        // A boundary no provider here can reopen is never taken for empty.
        let foreign = BoundaryIdentity {
            kind: BoundaryKind::WindowsJobObject,
            ..identity
        };
        assert!(providers.reopen(&foreign).is_err());
        assert_eq!(
            providers
                .known()
                .iter()
                .map(|provider| provider.kind())
                .collect::<Vec<_>>(),
            vec![BoundaryKind::CgroupV2, BoundaryKind::UnixLocator]
        );
    }
}
