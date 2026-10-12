//! The typed facts the diagnostics layer observes.
//!
//! Every fact is either a closed enum, a bounded [`SafeField`], a content digest
//! or a number. None of them is a free-form `String`, a `PathBuf` or a byte
//! buffer, which is what makes it impossible for a raw secret, a source line or
//! an absolute private path to reach a rendered diagnostic by accident. The
//! caller must digest such material *before* it becomes a fact, and
//! [`AuditDigest`] is the only way to do that.

use godot_mcp_domain::CapabilityProfile;
use godot_mcp_domain::contracts::SchedulerMode;

use crate::audit::AuditDigest;
use crate::observability::SafeField;
use crate::router::{EnginePresence, RouterHealth, RuntimePresence};
use crate::scheduler::JournalBinding;

/// How many digests a sample keeps. A defect list is unbounded in principle, so
/// the report keeps a fixed-size sample plus the true total rather than growing
/// with the number of defects.
pub const MAX_DIGEST_SAMPLE: usize = 8;

/// A bounded sample of digests, plus the true total they were drawn from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestSample {
    total: u32,
    sample: Vec<AuditDigest>,
}

impl DigestSample {
    /// A sample of nothing.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            total: 0,
            sample: Vec::new(),
        }
    }

    /// A sample of at most [`MAX_DIGEST_SAMPLE`] digests drawn from `total` items.
    #[must_use]
    pub fn of(total: u32, mut sample: Vec<AuditDigest>) -> Self {
        sample.truncate(MAX_DIGEST_SAMPLE);
        Self { total, sample }
    }

    /// How many items exist, which may exceed the sample length.
    #[must_use]
    pub const fn total(&self) -> u32 {
        self.total
    }

    /// Whether anything was observed at all.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.total == 0
    }

    /// The retained digests.
    #[must_use]
    pub fn sample(&self) -> &[AuditDigest] {
        &self.sample
    }

    /// The first retained digest, used as a finding's subject.
    #[must_use]
    pub fn first(&self) -> Option<&AuditDigest> {
        self.sample.first()
    }
}

/// Which transport this server is answering on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TransportSurface {
    /// Newline-delimited JSON-RPC over stdio.
    Stdio,
    /// Streamable HTTP on a loopback socket.
    Http,
}

impl TransportSurface {
    /// Stable snake_case name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::Http => "http",
        }
    }
}

/// Who holds the project's object-identity lease.
///
/// The owner is a digest, never a path: the lease is keyed on a held object
/// identity, and the operator only needs to know whether it is *this* server's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseOwnership {
    /// No lease is held.
    NotHeld,
    /// This server holds it.
    HeldHere {
        /// Digest of the owning identity.
        owner: AuditDigest,
    },
    /// Another owner holds it.
    HeldElsewhere {
        /// Digest of the owning identity.
        owner: AuditDigest,
    },
}

/// The project's identity, lease and safe-root generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectFacts {
    /// Digest of the project identity.
    pub identity: AuditDigest,
    /// Who holds the object-identity lease.
    pub lease: LeaseOwnership,
    /// The `SafeRoot` generation the lease published.
    pub safe_root_generation: u64,
}

/// The analysis index snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexFacts {
    /// The index agrees with the bytes on disk.
    Fresh {
        /// Snapshot generation.
        generation: u64,
    },
    /// The index is behind disk.
    Stale {
        /// Snapshot generation.
        generation: u64,
        /// Bounded coherence code explaining how it diverged.
        coherence: SafeField,
    },
    /// The index could not be read.
    Unreadable,
}

/// A detected Godot version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EngineVersion {
    major: u16,
    minor: u16,
    patch: u16,
}

impl EngineVersion {
    /// Record one detected version.
    #[must_use]
    pub const fn new(major: u16, minor: u16, patch: u16) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// Major component.
    #[must_use]
    pub const fn major(self) -> u16 {
        self.major
    }

    /// Minor component.
    #[must_use]
    pub const fn minor(self) -> u16 {
        self.minor
    }

    /// Patch component.
    #[must_use]
    pub const fn patch(self) -> u16 {
        self.patch
    }

    /// A single bounded number a finding can carry as its measure.
    #[must_use]
    pub const fn as_measure(self) -> u64 {
        (self.major as u64) * 1_000_000 + (self.minor as u64) * 1_000 + (self.patch as u64)
    }
}

/// Whether the detected engine version is one this build supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EngineCompatibility {
    /// Inside the supported range.
    Supported,
    /// Outside the supported range.
    Unsupported,
    /// Not determined.
    Unknown,
}

impl EngineCompatibility {
    /// Stable snake_case name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::Unsupported => "unsupported",
            Self::Unknown => "unknown",
        }
    }
}

/// The Godot binary and its capability snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineFacts {
    /// Whether an engine is reachable, and in which shape.
    pub presence: EnginePresence,
    /// Digest of the binary, never its path.
    pub binary: Option<AuditDigest>,
    /// Detected version, when one was read.
    pub version: Option<EngineVersion>,
    /// Whether the capability snapshot was actually probed.
    pub capabilities_probed: bool,
}

/// The editor plugin protocol this build negotiated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginProtocol {
    /// This server attaches no editor bridge, so nothing is negotiated.
    NotShipped,
    /// The editor bridge is attached and no plugin is connected to it.
    NotConnected,
    /// The attached plugin speaks the expected version.
    Matched {
        /// Negotiated version.
        version: u32,
    },
    /// The attached plugin speaks a different version.
    Mismatch {
        /// The version this build speaks.
        expected: u32,
        /// The version the plugin speaks.
        observed: u32,
    },
}

/// Version compatibility across the contract, the engine and the plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VersionFacts {
    /// Whether the caller's contract version matches this build's.
    pub contract_matched: bool,
    /// Whether the detected engine version is supported.
    pub engine: EngineCompatibility,
    /// The plugin protocol state.
    pub plugin: PluginProtocol,
}

/// The scheduler's admission state and its declared limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueFacts {
    /// Current admission mode.
    pub mode: SchedulerMode,
    /// Work in flight.
    pub running: u32,
    /// Work queued.
    pub queued: u32,
    /// Declared read concurrency limit.
    pub concurrency_limit: u32,
    /// Declared queue limit.
    pub queue_limit: u32,
    /// Which durable journal components are bound.
    pub journal: JournalBinding,
}

impl QueueFacts {
    /// Whether the queue is at its declared limit.
    #[must_use]
    pub const fn is_saturated(&self) -> bool {
        self.queue_limit > 0 && self.queued >= self.queue_limit
    }
}

/// Whether temporary project settings were reconciled back to their originals.
///
/// `Unknown` is treated exactly like `Pending`: a reconciliation that cannot be
/// proven complete is not complete, and assuming otherwise is how a temporary
/// setting outlives the run that introduced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsReconciliation {
    /// Every temporary setting was restored.
    Reconciled,
    /// Settings are still displaced.
    Pending {
        /// How many remain displaced.
        count: u32,
    },
    /// The reconciliation state could not be read.
    Unknown,
}

impl SettingsReconciliation {
    /// Whether reconciliation is proven complete.
    #[must_use]
    pub const fn is_complete(self) -> bool {
        matches!(self, Self::Reconciled)
    }

    /// How many settings remain displaced, when that is known.
    #[must_use]
    pub const fn pending(self) -> Option<u64> {
        match self {
            Self::Pending { count } => Some(count as u64),
            Self::Reconciled | Self::Unknown => None,
        }
    }
}

/// The durable journal's quarantine, survivor and settings state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalFacts {
    /// Quarantined transactions.
    pub quarantined: DigestSample,
    /// Supervised processes that survived a launch without a resolution.
    pub unresolved_survivors: DigestSample,
    /// Whether temporary settings were reconciled.
    pub settings: SettingsReconciliation,
    /// Whether the write-block state could be read at all.
    pub block_readable: bool,
}

/// How recently a periodic signal was observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// The signal does not exist.
    Absent,
    /// The signal is inside its freshness budget.
    Fresh {
        /// Age of the last observation.
        age_ms: u64,
    },
    /// The signal is past its freshness budget.
    Stale {
        /// Age of the last observation.
        age_ms: u64,
    },
}

impl Freshness {
    /// Stable snake_case name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Fresh { .. } => "fresh",
            Self::Stale { .. } => "stale",
        }
    }

    /// Whether the signal is present and inside its budget.
    #[must_use]
    pub const fn is_fresh(self) -> bool {
        matches!(self, Self::Fresh { .. })
    }

    /// The observed age, when there is one.
    #[must_use]
    pub const fn age_ms(self) -> Option<u64> {
        match self {
            Self::Absent => None,
            Self::Fresh { age_ms } | Self::Stale { age_ms } => Some(age_ms),
        }
    }
}

/// Whether autoloads displaced for a run were restored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoloadRecovery {
    /// No autoload was ever displaced.
    NotRequired,
    /// Every displaced autoload was restored.
    Restored,
    /// At least one displaced autoload is still missing.
    Pending,
}

impl AutoloadRecovery {
    /// Stable snake_case name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotRequired => "not_required",
            Self::Restored => "restored",
            Self::Pending => "pending",
        }
    }

    /// Whether nothing is outstanding.
    #[must_use]
    pub const fn is_settled(self) -> bool {
        matches!(self, Self::NotRequired | Self::Restored)
    }
}

/// The running instance's liveness facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeFacts {
    /// Whether a game instance is running.
    pub presence: RuntimePresence,
    /// The instance's heartbeat freshness.
    pub heartbeat: Freshness,
    /// Whether displaced autoloads were restored.
    pub autoload: AutoloadRecovery,
    /// The rendezvous record's freshness.
    pub rendezvous: Freshness,
}

/// The per-launch local secret's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalSecretState {
    /// Present, owner-only and readable.
    Ready,
    /// Never created.
    Missing,
    /// Present but unreadable.
    Unreadable,
    /// Present but readable beyond its owner.
    InsecurePermissions {
        /// The observed mode. The mode is a permission bit set, never a secret.
        mode: u32,
    },
}

/// The authorization server's key set state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JwksState {
    /// Fetched, with at least one usable key.
    Ready {
        /// How many usable keys were parsed.
        keys: u32,
    },
    /// Fetched, but no usable key was present.
    Empty,
    /// The endpoint did not answer.
    Unreachable,
    /// The cached document is past its freshness budget.
    Stale {
        /// Age of the cached document.
        age_ms: u64,
    },
}

/// The HTTP surface's readiness, including which authentication mode is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpReadiness {
    /// The HTTP transport is not enabled. This is the default.
    Disabled,
    /// Enabled with a user-scoped per-launch secret.
    TrustedLocal {
        /// The secret's state.
        secret: LocalSecretState,
    },
    /// Enabled behind an authenticated proxy hop with one frozen OIDC principal.
    ProxiedOidc {
        /// The key set's state.
        jwks: JwksState,
    },
}

impl HttpReadiness {
    /// Stable snake_case mode name.
    #[must_use]
    pub const fn mode(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::TrustedLocal { .. } => "trusted_local",
            Self::ProxiedOidc { .. } => "proxied_oidc",
        }
    }

    /// Whether the transport is enabled at all.
    #[must_use]
    pub const fn is_enabled(self) -> bool {
        !matches!(self, Self::Disabled)
    }
}

/// Everything one health assembly reads.
///
/// The plane facts are *borrowed* from the router's own published health rather
/// than derived again, so the diagnostics surface can never contradict the router
/// it describes.
#[derive(Debug)]
pub struct HealthInputs<'planes> {
    /// The advertised capability profile.
    pub profile: CapabilityProfile,
    /// The transport answering right now.
    pub transport: TransportSurface,
    /// Project identity, lease and safe-root generation.
    pub project: ProjectFacts,
    /// The analysis index snapshot.
    pub index: IndexFacts,
    /// The Godot binary and its capability snapshot.
    pub engine: EngineFacts,
    /// The router's own plane health.
    pub planes: &'planes RouterHealth,
    /// Scheduler admission state and limits.
    pub queue: QueueFacts,
    /// Durable journal quarantine, survivor and settings state.
    pub journal: JournalFacts,
    /// The running instance's liveness.
    pub runtime: RuntimeFacts,
    /// The HTTP surface's readiness.
    pub http: HttpReadiness,
    /// Contract, engine and plugin version compatibility.
    pub versions: VersionFacts,
    /// The trusted observation timestamp.
    pub observed_at_unix_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sample never grows with the number of defects: a project with thousands
    /// of quarantined transactions must not produce a thousand-entry report.
    #[test]
    fn a_digest_sample_is_bounded_by_construction() {
        let digests: Vec<AuditDigest> = (0..1_000)
            .map(|index| AuditDigest::of_str("transaction", &format!("txn-{index}")))
            .collect();
        let sample = DigestSample::of(1_000, digests);
        assert_eq!(sample.total(), 1_000);
        assert_eq!(sample.sample().len(), MAX_DIGEST_SAMPLE);
        assert!(sample.first().is_some());
        assert!(!sample.is_empty());
        assert!(DigestSample::empty().is_empty());
        assert!(DigestSample::empty().first().is_none());
    }

    /// An unproven reconciliation is never treated as complete.
    #[test]
    fn an_unknown_reconciliation_is_not_complete() {
        assert!(SettingsReconciliation::Reconciled.is_complete());
        assert!(!SettingsReconciliation::Unknown.is_complete());
        assert!(!SettingsReconciliation::Pending { count: 1 }.is_complete());
        assert_eq!(
            SettingsReconciliation::Pending { count: 2 }.pending(),
            Some(2)
        );
        assert!(SettingsReconciliation::Unknown.pending().is_none());
    }

    /// Saturation is decided from the declared limit, never guessed.
    #[test]
    fn saturation_follows_the_declared_queue_limit() {
        let facts = QueueFacts {
            mode: SchedulerMode::Accepting,
            running: 1,
            queued: 64,
            concurrency_limit: 4,
            queue_limit: 64,
            journal: JournalBinding {
                idempotency: true,
                store: true,
                survivors: true,
            },
        };
        assert!(facts.is_saturated());
        assert!(
            !QueueFacts {
                queued: 63,
                ..facts
            }
            .is_saturated()
        );
    }

    /// Freshness keeps the observed age so a finding can carry it as a bounded
    /// number rather than a formatted string.
    #[test]
    fn freshness_exposes_a_bounded_age() {
        assert!(Freshness::Fresh { age_ms: 10 }.is_fresh());
        assert!(!Freshness::Stale { age_ms: 10 }.is_fresh());
        assert_eq!(Freshness::Stale { age_ms: 90_000 }.age_ms(), Some(90_000));
        assert!(Freshness::Absent.age_ms().is_none());
        assert_eq!(Freshness::Absent.as_str(), "absent");
        assert!(AutoloadRecovery::Restored.is_settled());
        assert!(!AutoloadRecovery::Pending.is_settled());
        assert_eq!(EngineVersion::new(4, 3, 1).as_measure(), 4_003_001);
        assert_eq!(TransportSurface::Http.as_str(), "http");
        assert_eq!(EngineCompatibility::Unsupported.as_str(), "unsupported");
        assert!(HttpReadiness::Disabled.mode() == "disabled");
        assert!(!HttpReadiness::Disabled.is_enabled());
    }
}
