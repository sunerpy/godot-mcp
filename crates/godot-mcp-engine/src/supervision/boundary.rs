//! Containment boundaries: the one place a supervised process may exist.
//!
//! A boundary is a **stable, named object**: for the strong paths (Windows Job
//! Object, a configured container/VM adapter) and the delegated Linux cgroup it
//! can exist before any child does, and for the trusted-Unix weak path it is an inert
//! private *locator* (a pre-bound control endpoint plus a fresh 256-bit nonce)
//! that likewise exists before any process of any kind. Either way the launch
//! protocol can durably record the boundary's identity **before** it creates
//! anything, which is what closes the pre-`staged_fsynced` crash window: a
//! missing durability mark is never read as "no process exists".
//!
//! There are exactly two platform paths and never a third:
//!
//! * [`BoundaryStrength::Strong`] — required for untrusted launches. Membership
//!   is enumerable from the identity alone, by a process that did not create the
//!   boundary, after a restart. No member may break away, and closing the
//!   boundary kills what is still inside it.
//! * [`BoundaryStrength::Weak`] — permitted for *trusted* projects only: the
//!   minimal bundled supervisor/anchor protocol, which explicitly does not
//!   contain a deliberate detach and does not survive supervisor `SIGKILL` after
//!   release (see [`WEAK_RESIDUAL_RISKS`]), and a delegated cgroup v2 sub-tree,
//!   which limits and terminates its members but cannot keep one that runs as
//!   the delegating user from moving itself into another cgroup of the
//!   sub-tree.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use godot_mcp_domain::ErrorKind;
use serde_json::{Value, json};

/// The residual risks the weak anchor protocol explicitly does **not** cover.
/// Named so no caller can claim weak containment is stronger than it is.
pub const WEAK_RESIDUAL_RISKS: [&str; 2] = ["deliberate_detach", "post_release_supervisor_sigkill"];

/// The argv token every gated child carries **before** it execs user code.
///
/// The bundled barrier program is created with this token in its argument
/// vector and replaces its own image with the supervised program only after its
/// gate opens, so the token's presence in a live process's `argv` is a real OS
/// observation that user code has not started yet.
pub const BARRIER_ARGV_MARKER: &str = "--godot-mcp-barrier";

/// Whether the requested code is trusted. Untrusted code never uses the weak
/// anchor protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustLevel {
    /// A project the operator has declared trusted.
    Trusted,
    /// Anything else. Requires a strong boundary or the launch fails before spawn.
    Untrusted,
}

/// How strongly a boundary contains its members.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundaryStrength {
    /// Kernel/hypervisor-enforced: no breakaway, kill-on-close, resource limits.
    Strong,
    /// The trusted-Unix supervisor/anchor protocol, with documented residual risk.
    Weak,
}

impl BoundaryStrength {
    /// Canonical wire spelling recorded in the launch payload.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Strong => "strong",
            Self::Weak => "weak",
        }
    }

    /// Parse the canonical wire spelling.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "strong" => Some(Self::Strong),
            "weak" => Some(Self::Weak),
            _ => None,
        }
    }
}

/// Which concrete containment mechanism a boundary is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundaryKind {
    /// A configured container/VM adapter.
    ContainerAdapter,
    /// A Linux cgroup v2 sub-tree.
    CgroupV2,
    /// A Windows Job Object with no-breakaway + kill-on-close.
    WindowsJobObject,
    /// The trusted-Unix private locator (control endpoint + nonce).
    UnixLocator,
}

impl BoundaryKind {
    /// Canonical wire spelling recorded in the launch payload.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ContainerAdapter => "container_adapter",
            Self::CgroupV2 => "cgroup_v2",
            Self::WindowsJobObject => "windows_job_object",
            Self::UnixLocator => "unix_locator",
        }
    }

    /// Parse the canonical wire spelling.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "container_adapter" => Some(Self::ContainerAdapter),
            "cgroup_v2" => Some(Self::CgroupV2),
            "windows_job_object" => Some(Self::WindowsJobObject),
            "unix_locator" => Some(Self::UnixLocator),
            _ => None,
        }
    }

    /// The strength this mechanism provides. This is a property of the
    /// mechanism, never of the caller's wishes.
    #[must_use]
    pub const fn strength(self) -> BoundaryStrength {
        match self {
            Self::ContainerAdapter | Self::WindowsJobObject => BoundaryStrength::Strong,
            Self::CgroupV2 | Self::UnixLocator => BoundaryStrength::Weak,
        }
    }
}

/// A boundary's durable identity.
///
/// This is what `intent_fsynced` records **before** any process is created, and
/// what a restarted process uses to enumerate the boundary's members directly.
/// A bare process-group ID is deliberately not representable here: the weak
/// path's identity is the control endpoint plus the nonce, and the recorded
/// supervisor PID/start-time/PGID tuple lives in the staged payload where all
/// three must match exactly before any live process is trusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundaryIdentity {
    /// Which mechanism this identity belongs to.
    pub kind: BoundaryKind,
    /// The mechanism's own concrete identity (Job Object name, cgroup path,
    /// container identifier, or locator directory).
    pub identity: String,
    /// Weak path only: the pre-bound control endpoint's path.
    pub control_endpoint: Option<String>,
    /// Weak path only: the fresh 256-bit nonce, lowercase hex.
    pub control_nonce: Option<String>,
    /// Weak path only: the inode of the pre-bound control socket the eventual
    /// supervisor inherits. Recorded before any process exists, so a restarted
    /// process can ask the OS which live process holds *that exact* endpoint
    /// rather than scanning process groups.
    pub control_socket_inode: Option<u64>,
}

impl BoundaryIdentity {
    /// The launch-payload facts that durably bind this identity.
    #[must_use]
    pub fn to_facts(&self) -> BTreeMap<String, Value> {
        let mut facts = BTreeMap::new();
        facts.insert("boundary_kind".to_owned(), json!(self.kind.as_str()));
        facts.insert("boundary_identity".to_owned(), json!(self.identity));
        facts.insert(
            "boundary_strength".to_owned(),
            json!(self.kind.strength().as_str()),
        );
        facts.insert(
            "control_endpoint".to_owned(),
            self.control_endpoint
                .as_ref()
                .map_or(Value::Null, |endpoint| json!(endpoint)),
        );
        facts.insert(
            "control_nonce".to_owned(),
            self.control_nonce
                .as_ref()
                .map_or(Value::Null, |nonce| json!(nonce)),
        );
        facts.insert(
            "control_socket_inode".to_owned(),
            self.control_socket_inode
                .map_or(Value::Null, |inode| json!(inode)),
        );
        facts
    }

    /// Rebuild an identity from durable launch-payload facts.
    #[must_use]
    pub fn from_facts(facts: &BTreeMap<String, Value>) -> Option<Self> {
        let kind = BoundaryKind::parse(facts.get("boundary_kind")?.as_str()?)?;
        let identity = facts.get("boundary_identity")?.as_str()?.to_owned();
        Some(Self {
            kind,
            identity,
            control_endpoint: facts
                .get("control_endpoint")
                .and_then(Value::as_str)
                .map(str::to_owned),
            control_nonce: facts
                .get("control_nonce")
                .and_then(Value::as_str)
                .map(str::to_owned),
            control_socket_inode: facts.get("control_socket_inode").and_then(Value::as_u64),
        })
    }

    /// The strength implied by the mechanism.
    #[must_use]
    pub const fn strength(&self) -> BoundaryStrength {
        self.kind.strength()
    }

    /// The cgroup directory a barrier must join before it blocks, for the cgroup
    /// v2 mechanism only. Every other mechanism places the process inside its
    /// boundary at creation time, so there is nothing for the barrier to join.
    #[must_use]
    pub fn cgroup_dir(&self) -> Option<PathBuf> {
        (self.kind == BoundaryKind::CgroupV2).then(|| PathBuf::from(&self.identity))
    }
}

/// The limits a boundary actually enforces. A boundary that claims strength but
/// does not enforce no-breakaway and kill-on-close is refused before spawn.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BoundaryLimits {
    /// Verbatim limit description, retained for provenance.
    pub raw: String,
    /// Whether a member can never leave the boundary.
    pub no_breakaway: bool,
    /// Whether closing the boundary kills whatever is still inside it.
    pub kill_on_close: bool,
    /// Maximum number of member processes.
    pub max_processes: Option<u64>,
    /// Maximum committed memory, in bytes.
    pub max_memory_bytes: Option<u64>,
    /// CPU rate cap, as a percentage.
    pub cpu_percent: Option<u32>,
}

impl BoundaryLimits {
    /// Parse the `key=value` limit line every adapter/boundary reports.
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        let mut limits = Self {
            raw: raw.trim().to_owned(),
            ..Self::default()
        };
        for field in raw.split_whitespace() {
            let Some((key, value)) = field.split_once('=') else {
                continue;
            };
            match key {
                "no-breakaway" => limits.no_breakaway = value == "true",
                "kill-on-close" => limits.kill_on_close = value == "true",
                "pids" => limits.max_processes = value.parse().ok(),
                "memory" => limits.max_memory_bytes = value.parse().ok(),
                "cpu" => limits.cpu_percent = value.parse().ok(),
                _ => {}
            }
        }
        limits
    }

    /// Whether these limits are sufficient for an untrusted launch.
    #[must_use]
    pub const fn contains_untrusted_code(&self) -> bool {
        self.no_breakaway && self.kill_on_close
    }
}

/// One live member of a boundary, as observed from the OS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberObservation {
    /// The member's PID.
    pub pid: u32,
    /// The member's process start time, when observable. A PID without a
    /// matching start time is never treated as the same process.
    pub start_time: Option<String>,
    /// Whether the member is still the gated barrier, i.e. provably has not
    /// started user code.
    pub gated: bool,
}

/// What can be *proven* about a boundary's contents right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainmentState {
    /// Proven to contain no member process.
    Empty,
    /// Proven to contain exactly these members.
    Contained(Vec<MemberObservation>),
    /// Neither can be proven. The caller must quarantine rather than guess.
    Unprovable(String),
}

/// The plan for one gated child creation. Never a shell command string: the
/// program and its arguments stay separate all the way to `execve`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnPlan {
    /// The executable to run, already revalidated by hash.
    pub program: PathBuf,
    /// The argument array handed to the program, without `argv[0]`.
    pub arguments: Vec<String>,
    /// The complete environment the child will see. Nothing else is inherited.
    pub environment: Vec<(String, String)>,
    /// Working directory for the child.
    pub work_dir: PathBuf,
    /// Where the child's stdout is captured. The protocol's own stdout is never
    /// inherited.
    pub stdout_path: PathBuf,
    /// Where the child's stderr is captured.
    pub stderr_path: PathBuf,
    /// The one-shot gate the child blocks on before it execs user code.
    pub gate_path: PathBuf,
    /// The bundled barrier/supervisor binary.
    pub barrier_binary: PathBuf,
    /// Linux cgroup v2 only: the `cgroup.procs` directory the barrier must join
    /// (writing its own PID) *before* it blocks on its gate, so a member is inside
    /// the boundary before it can ever run user code.
    pub join_cgroup: Option<PathBuf>,
}

/// The gated child's own identity, bound into the receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatedChild {
    /// The child's PID.
    pub pid: u32,
    /// The child's process start time.
    pub start_time: Option<String>,
}

/// How a supervised process ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitOutcome {
    /// Exit status, when the process exited normally.
    pub exit_code: Option<i32>,
    /// Signal name, when the process was terminated by a signal.
    pub signal: Option<String>,
    /// Whether the supervisor's deadline, not the child, ended the run.
    pub timed_out: bool,
}

/// The weak path's live control channel, kept behind a trait so the strong
/// paths carry no Unix-socket machinery.
pub trait AnchorLink: Send {
    /// Ask the blocked supervisor to create the gated child. Returns the
    /// child's own identity.
    ///
    /// # Errors
    /// Returns [`BoundaryError`] when the control channel fails or the
    /// supervisor reports an unexpected message.
    fn create_gated_child(&mut self) -> Result<GatedChild, BoundaryError>;

    /// Send the authenticated release down the control channel.
    ///
    /// # Errors
    /// Returns [`BoundaryError`] when the control channel fails.
    fn release(&mut self) -> Result<(), BoundaryError>;

    /// Wait for the supervisor's exit report, bounded by `deadline`.
    ///
    /// # Errors
    /// Returns [`BoundaryError::SupervisorLost`] when the supervisor died
    /// without reporting, which is the documented weak-mode residual risk.
    fn wait(&mut self, deadline: Duration) -> Result<WaitOutcome, BoundaryError>;

    /// Terminate the supervisor's whole process group.
    fn terminate(&mut self);

    /// `SIGKILL` the supervisor itself, leaving its group unguaranteed. Used
    /// only by the residual-risk fault hook.
    fn sigkill_supervisor(&mut self);
}

/// One created anchor: the first process the protocol creates inside (or
/// behind) the boundary.
///
/// On a strong path the anchor *is* the gated child. On the weak path the anchor
/// is the minimal bundled supervisor, blocked on its inherited control channel
/// with no child of any kind yet.
pub struct Anchor {
    /// The anchor process's PID.
    pub pid: u32,
    /// The anchor process's start time, when observable.
    pub start_time: Option<String>,
    /// The anchor's process group, weak path only. Never an identity by itself.
    pub process_group: Option<u32>,
    /// The one-shot gate the eventual child blocks on.
    pub gate_path: PathBuf,
    /// The directly waitable child handle, when the anchor is our own child.
    pub child: Option<std::process::Child>,
    /// The weak path's control channel.
    pub link: Option<Box<dyn AnchorLink>>,
    /// The gated child, once created.
    pub gated: Option<GatedChild>,
}

impl std::fmt::Debug for Anchor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Anchor")
            .field("pid", &self.pid)
            .field("start_time", &self.start_time)
            .field("process_group", &self.process_group)
            .field("gated", &self.gated)
            .finish_non_exhaustive()
    }
}

/// A containment boundary that already exists.
pub trait ContainmentBoundary {
    /// The boundary's durable identity.
    fn identity(&self) -> &BoundaryIdentity;

    /// How strongly this boundary contains its members.
    fn strength(&self) -> BoundaryStrength {
        self.identity().strength()
    }

    /// The limits this boundary enforces.
    ///
    /// # Errors
    /// Returns [`BoundaryError`] when the limits cannot be read.
    fn limits(&self) -> Result<BoundaryLimits, BoundaryError>;

    /// Every live member, enumerated from the boundary identity alone.
    ///
    /// # Errors
    /// Returns [`BoundaryError`] when enumeration itself fails.
    fn members(&self) -> Result<Vec<MemberObservation>, BoundaryError>;

    /// What can be proven about the boundary's contents right now.
    ///
    /// # Errors
    /// Returns [`BoundaryError`] when the boundary cannot be inspected at all.
    fn state(&self) -> Result<ContainmentState, BoundaryError>;

    /// Whether a live process is provably the exact member a durable record
    /// named.
    ///
    /// The default (strong-path) implementation enumerates the boundary and
    /// requires an exact PID + start-time match, because for a strong boundary
    /// membership itself is already kernel-enforced. The weak path overrides this
    /// to additionally require the authenticated endpoint/nonce credential, and to
    /// report a mismatch as an error rather than as absence — a live process
    /// holding the recorded endpoint with the wrong PID/start-time/PGID tuple is
    /// PID reuse, which must quarantine rather than be read as "nothing is there".
    ///
    /// # Errors
    /// Returns [`BoundaryError`] when the observation cannot be made, or when a
    /// live process contradicts the durable record.
    fn adopt_recorded_member(
        &self,
        pid: u32,
        start_time: Option<&str>,
        process_group: Option<u32>,
    ) -> Result<Option<MemberObservation>, BoundaryError> {
        let _ = process_group;
        let Some(member) = self.members()?.into_iter().find(|member| member.pid == pid) else {
            return Ok(None);
        };
        match (member.start_time.as_deref(), start_time) {
            (Some(observed), Some(recorded)) if observed == recorded => Ok(Some(member)),
            _ => Err(BoundaryError::NotProvenEmpty(format!(
                "boundary member {pid} does not match the recorded start time (observed {:?}, \
                 recorded {start_time:?})",
                member.start_time
            ))),
        }
    }

    /// Terminate every member.
    ///
    /// # Errors
    /// Returns [`BoundaryError`] when termination fails.
    fn terminate_all(&self) -> Result<(), BoundaryError>;

    /// Terminate one recorded process group and prove it empty. Only the weak path
    /// records a process group, so the default is to terminate the whole boundary.
    ///
    /// # Errors
    /// Returns [`BoundaryError`] when the group cannot be proven empty.
    fn terminate_recorded_group(&self, process_group: u32) -> Result<(), BoundaryError> {
        let _ = process_group;
        self.terminate_all()
    }

    /// Close the boundary. Refused while a live member remains.
    ///
    /// # Errors
    /// Returns [`BoundaryError`] when the boundary cannot be proven empty or
    /// cannot be removed.
    fn close(&self) -> Result<(), BoundaryError>;

    /// Create the anchor process: the gated child on a strong path, the blocked
    /// minimal supervisor on the weak path.
    ///
    /// # Errors
    /// Returns [`BoundaryError`] when creation fails.
    fn create_anchor(&self, plan: &SpawnPlan) -> Result<Anchor, BoundaryError>;

    /// Create the gated child. On a strong path the anchor already *is* the
    /// gated child, so this only re-derives its identity; on the weak path the
    /// blocked supervisor creates it behind a second gate.
    ///
    /// # Errors
    /// Returns [`BoundaryError`] when creation fails.
    fn create_gated_child(&self, anchor: &mut Anchor) -> Result<GatedChild, BoundaryError>;

    /// Whether `pid` is observably a member of *this* boundary.
    ///
    /// # Errors
    /// Returns [`BoundaryError`] when membership cannot be inspected.
    fn confirm_membership(&self, pid: u32) -> Result<bool, BoundaryError>;

    /// Open the gate, letting the child exec user code.
    ///
    /// # Errors
    /// Returns [`BoundaryError`] when the gate cannot be opened.
    fn release(&self, anchor: &mut Anchor) -> Result<(), BoundaryError>;

    /// Wait for the supervised process, bounded by `deadline`.
    ///
    /// # Errors
    /// Returns [`BoundaryError`] when waiting fails.
    fn wait(&self, anchor: &mut Anchor, deadline: Duration) -> Result<WaitOutcome, BoundaryError>;
}

/// Creates and reopens boundaries of one concrete kind.
pub trait BoundaryProvider {
    /// Which mechanism this provider offers.
    fn kind(&self) -> BoundaryKind;

    /// The strength this provider's boundaries have.
    fn strength(&self) -> BoundaryStrength {
        self.kind().strength()
    }

    /// Whether this mechanism is usable on this host right now.
    ///
    /// # Errors
    /// Returns [`BoundaryError::Unavailable`] with a reason when it is not.
    fn available(&self) -> Result<(), BoundaryError>;

    /// Pre-create a boundary for `transaction_id`, before any process exists.
    ///
    /// # Errors
    /// Returns [`BoundaryError`] when the boundary cannot be created.
    fn create(&self, transaction_id: &str) -> Result<Box<dyn ContainmentBoundary>, BoundaryError>;

    /// Reopen an already-recorded boundary by identity alone, as a restarted
    /// process must. `Ok(None)` means the boundary no longer exists.
    ///
    /// # Errors
    /// Returns [`BoundaryError`] when the boundary cannot be inspected.
    fn reopen(
        &self,
        identity: &BoundaryIdentity,
    ) -> Result<Option<Box<dyn ContainmentBoundary>>, BoundaryError>;
}

/// Choose the provider for `trust`, failing closed when an untrusted launch has
/// no strong boundary available.
///
/// The first *available* provider whose strength satisfies `trust` wins, in the
/// caller's declared preference order. An untrusted launch never falls back to
/// the weak anchor protocol, even when it is the only thing available.
///
/// # Errors
/// Returns [`super::protocol::LaunchError::StrongBoundaryUnavailable`] when no
/// strong boundary is available for an untrusted launch, and
/// [`super::protocol::LaunchError::NoBoundary`] when nothing at all is available
/// for a trusted launch.
pub fn select_provider<'p>(
    trust: TrustLevel,
    providers: &[&'p dyn BoundaryProvider],
) -> Result<&'p dyn BoundaryProvider, super::protocol::LaunchError> {
    let mut refusals: Vec<String> = Vec::new();
    for provider in providers {
        if trust == TrustLevel::Untrusted && provider.strength() != BoundaryStrength::Strong {
            refusals.push(format!(
                "{}: weak containment is never used for untrusted code",
                provider.kind().as_str()
            ));
            continue;
        }
        match provider.available() {
            Ok(()) => return Ok(*provider),
            Err(error) => refusals.push(format!("{}: {error}", provider.kind().as_str())),
        }
    }
    Err(match trust {
        TrustLevel::Untrusted => super::protocol::LaunchError::StrongBoundaryUnavailable {
            refusals: refusals.join("; "),
        },
        TrustLevel::Trusted => super::protocol::LaunchError::NoBoundary {
            refusals: refusals.join("; "),
        },
    })
}

/// A containment-boundary failure.
#[derive(Debug, thiserror::Error)]
pub enum BoundaryError {
    /// The mechanism is not usable on this host.
    #[error("containment boundary is unavailable: {0}")]
    Unavailable(String),
    /// The boundary could not be created.
    #[error("containment boundary could not be created: {0}")]
    Create(String),
    /// The boundary's members could not be enumerated.
    #[error("containment boundary members could not be enumerated: {0}")]
    Enumerate(String),
    /// A member could not be terminated, or the boundary could not be proven
    /// empty afterwards.
    #[error("containment boundary could not be proven empty: {0}")]
    NotProvenEmpty(String),
    /// A child could not be created.
    #[error("gated child could not be created: {0}")]
    Spawn(String),
    /// The gate could not be opened.
    #[error("gate release failed: {0}")]
    Release(String),
    /// The weak path's supervisor died without reporting: the documented
    /// residual risk of the weak anchor protocol.
    #[error("supervisor was lost before it reported completion: {0}")]
    SupervisorLost(String),
    /// The control channel failed.
    #[error("control channel failed: {0}")]
    Control(String),
    /// Waiting for the supervised process failed.
    #[error("waiting for the supervised process failed: {0}")]
    Wait(String),
}

impl BoundaryError {
    /// Map to the stable domain error taxonomy.
    #[must_use]
    pub const fn error_kind(&self) -> ErrorKind {
        match self {
            Self::Unavailable(_) => ErrorKind::Unsupported,
            Self::Create(_) | Self::Spawn(_) | Self::Release(_) | Self::Control(_) => {
                ErrorKind::Internal
            }
            Self::Enumerate(_) | Self::NotProvenEmpty(_) | Self::SupervisorLost(_) => {
                ErrorKind::Uncertain
            }
            Self::Wait(_) => ErrorKind::Internal,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strength_is_a_property_of_the_mechanism() {
        assert_eq!(
            BoundaryKind::UnixLocator.strength(),
            BoundaryStrength::Weak,
            "the locator protocol can never advertise strong containment"
        );
        assert_eq!(
            BoundaryKind::CgroupV2.strength(),
            BoundaryStrength::Weak,
            "a delegated cgroup cannot keep a member in"
        );
        for strong in [
            BoundaryKind::ContainerAdapter,
            BoundaryKind::WindowsJobObject,
        ] {
            assert_eq!(strong.strength(), BoundaryStrength::Strong);
        }
    }

    #[test]
    fn an_identity_round_trips_through_durable_facts() {
        let identity = BoundaryIdentity {
            kind: BoundaryKind::UnixLocator,
            identity: "/tmp/state/tx".to_owned(),
            control_endpoint: Some("/tmp/state/tx/control.sock".to_owned()),
            control_nonce: Some("ab".repeat(32)),
            control_socket_inode: Some(987_654),
        };
        let facts = identity.to_facts();
        assert_eq!(facts["boundary_strength"], json!("weak"));
        assert_eq!(BoundaryIdentity::from_facts(&facts), Some(identity));
    }

    /// A delegated cgroup cannot keep a member that runs as the delegating
    /// user from moving itself out, so it is weak containment.
    #[test]
    fn a_delegated_cgroup_is_weak_containment() {
        assert_eq!(BoundaryKind::CgroupV2.strength(), BoundaryStrength::Weak);
        assert_eq!(
            BoundaryKind::ContainerAdapter.strength(),
            BoundaryStrength::Strong
        );
    }

    #[test]
    fn limits_without_no_breakaway_never_contain_untrusted_code() {
        let weakened = BoundaryLimits::parse("pids=64 no-breakaway=false kill-on-close=true");
        assert!(!weakened.contains_untrusted_code());
        let enforced = BoundaryLimits::parse(
            "pids=64 memory=536870912 cpu=50 no-breakaway=true kill-on-close=true",
        );
        assert!(enforced.contains_untrusted_code());
        assert_eq!(enforced.max_processes, Some(64));
        assert_eq!(enforced.max_memory_bytes, Some(536_870_912));
        assert_eq!(enforced.cpu_percent, Some(50));
    }
}
