//! Stable, machine-readable diagnostic codes.
//!
//! Every observable defect this server can report has exactly one code here, and
//! the code — not a prose message — is the contract. A caller automating against
//! health must be able to branch on a string that will not change when the human
//! wording is improved.
//!
//! ## An optional absence is never a server failure
//!
//! The editor plugin bridge and the running-instance bridge are not part of this
//! release, and a headless deployment may legitimately have no Godot binary at
//! all. Those facts are reported, never hidden — but they are classified through
//! [`DiagnosticCode::is_optional_absence`], and
//! [`DiagnosticSeverity::clamp_optional`] makes it structurally impossible for
//! such a code to reach [`DiagnosticSeverity::Blocked`]. A profile that
//! *advertises* the plane may raise the finding to `Degraded`, which is honest:
//! part of the advertised surface cannot execute. It can never become a server
//! failure.
//!
//! ## Why remediation is a compiled-in constant
//!
//! [`DiagnosticCode::remediation`] returns `&'static str`. Remediation text
//! therefore cannot interpolate a path, a secret or a source line, because there
//! is no runtime value in the type at all. That is the whole reason it is not a
//! `String`.

use godot_mcp_domain::contracts::ComponentName;

/// How much a finding matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DiagnosticSeverity {
    /// Worth reporting, but nothing is wrong.
    Info,
    /// Part of the advertised surface cannot execute, or an answer is narrower
    /// than usual. The server still serves.
    Degraded,
    /// The server cannot serve the surface this finding belongs to.
    Blocked,
}

impl DiagnosticSeverity {
    /// Every severity, weakest first.
    pub const ALL: [Self; 3] = [Self::Info, Self::Degraded, Self::Blocked];

    /// Stable snake_case name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Degraded => "degraded",
            Self::Blocked => "blocked",
        }
    }

    /// The stronger of two severities.
    #[must_use]
    pub const fn max_of(self, other: Self) -> Self {
        if (self as u8) >= (other as u8) {
            self
        } else {
            other
        }
    }

    /// Cap a severity that describes an optional absence.
    ///
    /// An absent optional plane may be raised to `Degraded` by a profile that
    /// advertises it, but it can never become `Blocked`: that would report a
    /// deliberately unshipped bridge as a server failure.
    #[must_use]
    pub const fn clamp_optional(self) -> Self {
        match self {
            Self::Blocked => Self::Degraded,
            other => other,
        }
    }
}

/// One stable diagnostic code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DiagnosticCode {
    /// No project lease is held, so no descriptor-relative write has a safe root.
    ProjectLeaseNotHeld,
    /// The lease on this project's object identity is held by another owner.
    ProjectLeaseForeignOwner,
    /// The published contract version does not match this build's.
    ContractVersionMismatch,
    /// The analysis index is behind the bytes on disk.
    IndexStale,
    /// The analysis index could not be read at all.
    IndexUnreadable,
    /// No Godot binary is reachable.
    GodotBinaryAbsent,
    /// A Godot binary is reachable but its version is outside the supported set.
    GodotVersionUnsupported,
    /// A Godot binary is reachable but its capability snapshot was never probed.
    GodotCapabilitiesUnprobed,
    /// This build has no supervised-process boundary.
    ProcessPlaneUnavailable,
    /// This build has no language/debug adapter boundary.
    LspDapPlaneNotShipped,
    /// This build has no live editor plugin bridge.
    EditorPlaneNotShipped,
    /// The editor bridge exists but no editor is attached.
    EditorNotAttached,
    /// This build has no running-instance bridge.
    RuntimePlaneNotShipped,
    /// The runtime bridge exists but no game instance is running.
    GameNotRunning,
    /// The attached plugin speaks a protocol version this build does not.
    PluginProtocolMismatch,
    /// One or more transactions are quarantined.
    TransactionQuarantined,
    /// A supervised process survived a launch without a resolution.
    UnresolvedLaunchSurvivor,
    /// The write-block state itself could not be read.
    WriteBlockUnreadable,
    /// Temporary project settings were not reconciled back to their originals.
    TemporarySettingsUnreconciled,
    /// The running instance stopped answering heartbeats.
    RuntimeHeartbeatStale,
    /// Autoloads displaced for a run were not restored.
    AutoloadRestorationPending,
    /// The runtime rendezvous record is older than its freshness budget.
    RendezvousStale,
    /// The scheduler is draining and admits no new work.
    SchedulerDraining,
    /// The scheduler is quarantined.
    SchedulerQuarantined,
    /// The scheduler is stopped.
    SchedulerStopped,
    /// The scheduler is missing at least one durable journal component.
    SchedulerJournalUnbound,
    /// The admission queue is at its declared limit.
    QueueSaturated,
    /// The HTTP transport is not enabled.
    HttpDisabled,
    /// The per-launch local secret is absent.
    LocalSecretMissing,
    /// The per-launch local secret could not be read.
    LocalSecretUnreadable,
    /// The per-launch local secret is readable by more than its owner.
    LocalSecretInsecurePermissions,
    /// The authorization server's JWKS endpoint did not answer.
    JwksUnreachable,
    /// The JWKS document carried no usable verification key.
    JwksEmpty,
    /// The cached JWKS document is older than its freshness budget.
    JwksStale,
}

impl DiagnosticCode {
    /// Every code, in a stable order.
    pub const ALL: [Self; 34] = [
        Self::ProjectLeaseNotHeld,
        Self::ProjectLeaseForeignOwner,
        Self::ContractVersionMismatch,
        Self::IndexStale,
        Self::IndexUnreadable,
        Self::GodotBinaryAbsent,
        Self::GodotVersionUnsupported,
        Self::GodotCapabilitiesUnprobed,
        Self::ProcessPlaneUnavailable,
        Self::LspDapPlaneNotShipped,
        Self::EditorPlaneNotShipped,
        Self::EditorNotAttached,
        Self::RuntimePlaneNotShipped,
        Self::GameNotRunning,
        Self::PluginProtocolMismatch,
        Self::TransactionQuarantined,
        Self::UnresolvedLaunchSurvivor,
        Self::WriteBlockUnreadable,
        Self::TemporarySettingsUnreconciled,
        Self::RuntimeHeartbeatStale,
        Self::AutoloadRestorationPending,
        Self::RendezvousStale,
        Self::SchedulerDraining,
        Self::SchedulerQuarantined,
        Self::SchedulerStopped,
        Self::SchedulerJournalUnbound,
        Self::QueueSaturated,
        Self::HttpDisabled,
        Self::LocalSecretMissing,
        Self::LocalSecretUnreadable,
        Self::LocalSecretInsecurePermissions,
        Self::JwksUnreachable,
        Self::JwksEmpty,
        Self::JwksStale,
    ];

    /// The stable machine-readable name. This is the contract.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProjectLeaseNotHeld => "project_lease_not_held",
            Self::ProjectLeaseForeignOwner => "project_lease_foreign_owner",
            Self::ContractVersionMismatch => "contract_version_mismatch",
            Self::IndexStale => "index_stale",
            Self::IndexUnreadable => "index_unreadable",
            Self::GodotBinaryAbsent => "godot_binary_absent",
            Self::GodotVersionUnsupported => "godot_version_unsupported",
            Self::GodotCapabilitiesUnprobed => "godot_capabilities_unprobed",
            Self::ProcessPlaneUnavailable => "process_plane_unavailable",
            Self::LspDapPlaneNotShipped => "lsp_dap_plane_not_shipped",
            Self::EditorPlaneNotShipped => "editor_plane_not_shipped",
            Self::EditorNotAttached => "editor_not_attached",
            Self::RuntimePlaneNotShipped => "runtime_plane_not_shipped",
            Self::GameNotRunning => "game_not_running",
            Self::PluginProtocolMismatch => "plugin_protocol_mismatch",
            Self::TransactionQuarantined => "transaction_quarantined",
            Self::UnresolvedLaunchSurvivor => "unresolved_launch_survivor",
            Self::WriteBlockUnreadable => "write_block_unreadable",
            Self::TemporarySettingsUnreconciled => "temporary_settings_unreconciled",
            Self::RuntimeHeartbeatStale => "runtime_heartbeat_stale",
            Self::AutoloadRestorationPending => "autoload_restoration_pending",
            Self::RendezvousStale => "rendezvous_stale",
            Self::SchedulerDraining => "scheduler_draining",
            Self::SchedulerQuarantined => "scheduler_quarantined",
            Self::SchedulerStopped => "scheduler_stopped",
            Self::SchedulerJournalUnbound => "scheduler_journal_unbound",
            Self::QueueSaturated => "queue_saturated",
            Self::HttpDisabled => "http_disabled",
            Self::LocalSecretMissing => "local_secret_missing",
            Self::LocalSecretUnreadable => "local_secret_unreadable",
            Self::LocalSecretInsecurePermissions => "local_secret_insecure_permissions",
            Self::JwksUnreachable => "jwks_unreachable",
            Self::JwksEmpty => "jwks_empty",
            Self::JwksStale => "jwks_stale",
        }
    }

    /// Which public health component this code belongs to.
    #[must_use]
    pub const fn component(self) -> ComponentName {
        match self {
            Self::ProjectLeaseNotHeld
            | Self::ProjectLeaseForeignOwner
            | Self::ContractVersionMismatch
            | Self::TemporarySettingsUnreconciled => ComponentName::Project,
            Self::IndexStale | Self::IndexUnreadable => ComponentName::Index,
            Self::GodotBinaryAbsent
            | Self::GodotVersionUnsupported
            | Self::GodotCapabilitiesUnprobed
            | Self::ProcessPlaneUnavailable
            | Self::LspDapPlaneNotShipped => ComponentName::Godot,
            Self::EditorPlaneNotShipped
            | Self::EditorNotAttached
            | Self::PluginProtocolMismatch => ComponentName::Editor,
            Self::RuntimePlaneNotShipped
            | Self::GameNotRunning
            | Self::RuntimeHeartbeatStale
            | Self::AutoloadRestorationPending
            | Self::RendezvousStale => ComponentName::Runtime,
            Self::TransactionQuarantined
            | Self::UnresolvedLaunchSurvivor
            | Self::WriteBlockUnreadable
            | Self::SchedulerDraining
            | Self::SchedulerQuarantined
            | Self::SchedulerStopped
            | Self::SchedulerJournalUnbound
            | Self::QueueSaturated => ComponentName::Scheduler,
            Self::HttpDisabled
            | Self::LocalSecretMissing
            | Self::LocalSecretUnreadable
            | Self::LocalSecretInsecurePermissions
            | Self::JwksUnreachable
            | Self::JwksEmpty
            | Self::JwksStale => ComponentName::Authorization,
        }
    }

    /// The severity this code carries before any profile-specific raise.
    #[must_use]
    pub const fn baseline_severity(self) -> DiagnosticSeverity {
        match self {
            Self::GodotBinaryAbsent
            | Self::GodotCapabilitiesUnprobed
            | Self::ProcessPlaneUnavailable
            | Self::LspDapPlaneNotShipped
            | Self::EditorPlaneNotShipped
            | Self::EditorNotAttached
            | Self::RuntimePlaneNotShipped
            | Self::GameNotRunning
            | Self::HttpDisabled => DiagnosticSeverity::Info,
            Self::ProjectLeaseNotHeld
            | Self::ContractVersionMismatch
            | Self::SchedulerStopped
            | Self::LocalSecretInsecurePermissions
            | Self::JwksUnreachable
            | Self::JwksEmpty => DiagnosticSeverity::Blocked,
            Self::ProjectLeaseForeignOwner
            | Self::IndexStale
            | Self::IndexUnreadable
            | Self::GodotVersionUnsupported
            | Self::PluginProtocolMismatch
            | Self::TransactionQuarantined
            | Self::UnresolvedLaunchSurvivor
            | Self::WriteBlockUnreadable
            | Self::TemporarySettingsUnreconciled
            | Self::RuntimeHeartbeatStale
            | Self::AutoloadRestorationPending
            | Self::RendezvousStale
            | Self::SchedulerDraining
            | Self::SchedulerQuarantined
            | Self::SchedulerJournalUnbound
            | Self::QueueSaturated
            | Self::LocalSecretMissing
            | Self::LocalSecretUnreadable
            | Self::JwksStale => DiagnosticSeverity::Degraded,
        }
    }

    /// Whether this code describes the absence of an optional plane or peer.
    ///
    /// Such a code is never allowed to reach [`DiagnosticSeverity::Blocked`].
    #[must_use]
    pub const fn is_optional_absence(self) -> bool {
        matches!(
            self,
            Self::GodotBinaryAbsent
                | Self::GodotCapabilitiesUnprobed
                | Self::ProcessPlaneUnavailable
                | Self::LspDapPlaneNotShipped
                | Self::EditorPlaneNotShipped
                | Self::EditorNotAttached
                | Self::RuntimePlaneNotShipped
                | Self::GameNotRunning
                | Self::HttpDisabled
        )
    }

    /// Whether this code holds mutations closed while it is present.
    #[must_use]
    pub const fn blocks_mutation(self) -> bool {
        matches!(
            self,
            Self::ProjectLeaseNotHeld
                | Self::ProjectLeaseForeignOwner
                | Self::ContractVersionMismatch
                | Self::TransactionQuarantined
                | Self::UnresolvedLaunchSurvivor
                | Self::WriteBlockUnreadable
                | Self::TemporarySettingsUnreconciled
                | Self::SchedulerQuarantined
                | Self::SchedulerStopped
                | Self::SchedulerJournalUnbound
        )
    }

    /// What an operator can do about this code.
    ///
    /// The text is deliberately honest about the two planes this release does not
    /// ship: it says the plane is not part of the build rather than implying a
    /// plugin exists and merely needs starting.
    #[must_use]
    pub const fn remediation(self) -> &'static str {
        match self {
            Self::ProjectLeaseNotHeld => {
                "point the server at a directory containing project.godot so it can hold the \
                 project lease"
            }
            Self::ProjectLeaseForeignOwner => {
                "another process holds this project's lease; stop it, or run against a different \
                 project"
            }
            Self::ContractVersionMismatch => {
                "the caller and this build disagree on the contract version; upgrade the client"
            }
            Self::IndexStale => "re-index the project so the analysis snapshot matches disk",
            Self::IndexUnreadable => {
                "the analysis index could not be read; check the state directory is writable and \
                 re-index"
            }
            Self::GodotBinaryAbsent => {
                "install a Godot binary and declare it in configuration if process-backed \
                 outcomes are wanted; a read-only deployment does not need one"
            }
            Self::GodotVersionUnsupported => {
                "install a Godot version this build supports; the detected one is outside the \
                 supported range"
            }
            Self::GodotCapabilitiesUnprobed => {
                "run a capability probe so version-specific behaviour is known rather than assumed"
            }
            Self::ProcessPlaneUnavailable => {
                "no process boundary can contain a Godot launch for this server — none is \
                 attached to the transport, or this host and the [process] configuration offer \
                 none, as the godot component says — so process-backed outcomes are refused \
                 with `unavailable`"
            }
            Self::LspDapPlaneNotShipped => {
                "this server attaches no language-server or debug-adapter boundary, so those \
                 outcomes are refused honestly; the runtime and full profiles host them in \
                 managed editor sessions"
            }
            Self::EditorPlaneNotShipped => {
                "this server attaches no live editor bridge, so no run-time change here enables \
                 it; the editor and full profiles serve the godot_mcp editor add-on"
            }
            Self::EditorNotAttached => {
                "install the godot_mcp add-on with editor.addon.install and open the project in \
                 the Godot editor so the add-on connects"
            }
            Self::RuntimePlaneNotShipped => {
                "this server attaches no managed game sessions, so no run-time change here \
                 enables runtime outcomes; the runtime and full profiles serve them"
            }
            Self::GameNotRunning => {
                "start a managed game session with project.run.start so a running game can answer"
            }
            Self::PluginProtocolMismatch => {
                "the attached plugin speaks a different protocol version; align the plugin with \
                 this build"
            }
            Self::TransactionQuarantined => {
                "resolve the quarantined transactions through the proof-driven repair path before \
                 writing again"
            }
            Self::UnresolvedLaunchSurvivor => {
                "resolve the surviving supervised process so its effect becomes decidable"
            }
            Self::WriteBlockUnreadable => {
                "the write-block state could not be read, so writes stay closed rather than \
                 assumed clear; check the journal store"
            }
            Self::TemporarySettingsUnreconciled => {
                "reconcile the temporary project settings back to their recorded originals before \
                 writing again"
            }
            Self::RuntimeHeartbeatStale => {
                "the running instance stopped answering; restart it or let the supervisor reap it"
            }
            Self::AutoloadRestorationPending => {
                "restore the autoloads displaced for the last run before relying on runtime \
                 outcomes"
            }
            Self::RendezvousStale => {
                "the runtime rendezvous record is past its freshness budget; treat runtime \
                 observations as unproven until it refreshes"
            }
            Self::SchedulerDraining => "the scheduler is draining; wait for in-flight work to end",
            Self::SchedulerQuarantined => {
                "the scheduler is quarantined; resolve the durable block it is holding"
            }
            Self::SchedulerStopped => "the scheduler is stopped; restart the server",
            Self::SchedulerJournalUnbound => {
                "at least one durable journal component is unbound, which silently removes a \
                 guarantee; restart with a writable state directory"
            }
            Self::QueueSaturated => {
                "the admission queue is at its limit; reduce concurrency or raise the configured \
                 queue limit"
            }
            Self::HttpDisabled => {
                "the HTTP transport is off by default; enable it in configuration if a remote \
                 caller is needed"
            }
            Self::LocalSecretMissing => {
                "the per-launch local secret was not created; restart the HTTP transport"
            }
            Self::LocalSecretUnreadable => {
                "the per-launch local secret could not be read; check the state directory"
            }
            Self::LocalSecretInsecurePermissions => {
                "the per-launch local secret is readable beyond its owner; tighten it to \
                 owner-only before serving"
            }
            Self::JwksUnreachable => {
                "the authorization server's key set did not answer; restore reachability before \
                 serving authenticated callers"
            }
            Self::JwksEmpty => {
                "the key set carried no usable verification key; publish one at the configured \
                 endpoint"
            }
            Self::JwksStale => {
                "the cached key set is past its freshness budget; allow a refresh before trusting \
                 it"
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The code list is complete and every name is unique: a duplicated code
    /// would make two different defects indistinguishable to an automated caller.
    #[test]
    fn every_code_has_a_unique_stable_name() {
        let mut names: Vec<&'static str> = DiagnosticCode::ALL
            .into_iter()
            .map(DiagnosticCode::as_str)
            .collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "diagnostic code names must be unique");
        for code in DiagnosticCode::ALL {
            assert!(!code.as_str().is_empty());
            assert!(!code.remediation().is_empty());
            assert!(
                code.as_str()
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'_' || byte.is_ascii_digit()),
                "{} must be snake_case",
                code.as_str()
            );
        }
    }

    /// An optional absence can never be classified as a server failure, in any
    /// direction: neither its baseline nor a clamped raise may reach `Blocked`.
    #[test]
    fn an_optional_absence_can_never_be_blocked() {
        for code in DiagnosticCode::ALL {
            if !code.is_optional_absence() {
                continue;
            }
            assert_ne!(
                code.baseline_severity(),
                DiagnosticSeverity::Blocked,
                "{} is optional and must not be blocking",
                code.as_str()
            );
            assert!(
                !code.blocks_mutation(),
                "{} is optional and must not block mutations",
                code.as_str()
            );
            for severity in DiagnosticSeverity::ALL {
                assert_ne!(severity.clamp_optional(), DiagnosticSeverity::Blocked);
            }
        }
    }

    /// A server that attaches no bridge for a plane must not tell an operator
    /// to start a plugin it could never reach; the hint has to name the
    /// assembly fact instead.
    #[test]
    fn unshipped_bridges_advertise_the_build_fact_not_a_phantom_plugin() {
        for code in [
            DiagnosticCode::EditorPlaneNotShipped,
            DiagnosticCode::RuntimePlaneNotShipped,
            DiagnosticCode::LspDapPlaneNotShipped,
        ] {
            let hint = code.remediation();
            assert!(hint.contains("this server attaches no"), "{hint}");
            for dishonest in [
                "install the plugin",
                "enable the plugin",
                "start the plugin",
            ] {
                assert!(!hint.contains(dishonest), "{hint}");
            }
        }
    }
}
