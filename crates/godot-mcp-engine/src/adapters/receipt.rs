//! What makes a loopback endpoint **owned**, and every way ownership fails.
//!
//! # Why a listening port proves nothing
//!
//! Godot's GDScript language server binds `127.0.0.1:6005` by default and its debug
//! adapter binds `127.0.0.1:6006` — the DAP address is hardcoded to loopback with no
//! host setting at all. Both are `EditorPlugin`s registered under `TOOLS_ENABLED`,
//! so only an editor build hosts them.
//!
//! That means the observation "something accepts connections on 6005" is compatible
//! with all of these:
//!
//! * the editor this server launched, still running — the only acceptable case;
//! * a *different* editor the operator opened by hand, on a different project;
//! * an editor from an earlier launch of ours that has since exited, with an
//!   unrelated process now holding the port;
//! * any local process at all, since the port is unauthenticated.
//!
//! Todo 14 already established the shape of the answer for processes: a bare PGID is
//! never an identity, and identity is a tuple that includes the process start time.
//! The same rule applies here, one level up: **a bare port number is never an
//! endpoint identity**. An endpoint is identified by the tuple
//!
//! > `(launch transaction, child PID, child start time, binary SHA-256,
//! >  project root URI, project generation, host:port)`
//!
//! and it is usable only while every element still matches what the launch receipt
//! recorded. [`EndpointReceipt::verify`] is that check. It is called before first
//! use **and again after every reconnect**, because reconnecting to the same port is
//! precisely the moment PID reuse becomes indistinguishable from continuity.
//!
//! # Where the port itself comes from
//!
//! Not from a scan, and not from a default. The server passes `--lsp-port` /
//! `--dap-port` in the argument array it journals at `intent_fsynced`, so the port
//! is a fact this server *chose* and durably recorded before the editor existed.
//! [`EndpointReceipt::from_launch`] reads it back out of that recorded argv; an
//! endpoint with no corresponding recorded flag is not owned, full stop.
//!
//! # The token
//!
//! [`OwnedEndpoint`] has private fields and no public constructor, so it can only be
//! produced by a successful [`EndpointReceipt::verify`]. Every adapter entry point
//! takes one. A caller holding only a host and a port therefore *cannot* reach the
//! adapters at all — the refusal is structural rather than asserted.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::supervision::journal::{BoundaryMembership, LaunchPhase, LaunchRecord, ReleaseStatus};
use crate::supervision::protocol::facts;

/// Which of Godot's two editor-hosted protocols an endpoint speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EndpointKind {
    /// The GDScript language server (`modules/gdscript/language_server`).
    LanguageServer,
    /// The editor debug adapter (`editor/debugger/debug_adapter`).
    DebugAdapter,
}

impl EndpointKind {
    /// Both kinds. There are two.
    pub const ALL: [Self; 2] = [Self::LanguageServer, Self::DebugAdapter];

    /// Stable wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LanguageServer => "lsp",
            Self::DebugAdapter => "dap",
        }
    }

    /// The command-line flag this server passes to choose the port. Recording the
    /// port in the launch argv is what makes the endpoint attributable later.
    #[must_use]
    pub const fn port_flag(self) -> &'static str {
        match self {
            Self::LanguageServer => "--lsp-port",
            Self::DebugAdapter => "--dap-port",
        }
    }

    /// The engine's own default port, for diagnostics only. Never a substitute for
    /// a recorded one.
    #[must_use]
    pub const fn engine_default_port(self) -> u16 {
        match self {
            // `int port = 6005;` in gdscript_language_server.h.
            Self::LanguageServer => 6005,
            // `int remote_port = 6006;` in debug_adapter_server.h.
            Self::DebugAdapter => 6006,
        }
    }
}

/// The only host either protocol is ever reached on.
///
/// The DAP server calls `protocol.start(remote_port, IPAddress("127.0.0.1"))` with
/// no setting to change it. The LSP defaults to the same address. This adapter never
/// dials a non-loopback address for either.
pub const LOOPBACK_HOST: &str = "127.0.0.1";

/// The flag that makes an editor build run as an editor. Both protocols are editor
/// plugins, so a launch without it can never host either.
pub const EDITOR_FLAG: &str = "--editor";

/// One `host:port` an editor protocol is reached on.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EditorEndpoint {
    /// Which protocol.
    pub kind: EndpointKind,
    /// The host. Always loopback for a usable endpoint.
    pub host: String,
    /// The TCP port.
    pub port: u16,
}

impl EditorEndpoint {
    /// A loopback endpoint of `kind` on `port`.
    #[must_use]
    pub fn loopback(kind: EndpointKind, port: u16) -> Self {
        Self {
            kind,
            host: LOOPBACK_HOST.to_owned(),
            port,
        }
    }

    /// `host:port`, for dialling and for diagnostics.
    #[must_use]
    pub fn authority(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// Whether this endpoint is on the loopback interface.
    #[must_use]
    pub fn is_loopback(&self) -> bool {
        self.host == LOOPBACK_HOST || self.host == "::1" || self.host == "localhost"
    }
}

/// Every way an endpoint fails to be owned by this server.
///
/// The variants are deliberately distinct rather than folded into one "rejected":
/// a caller and an operator need to tell "we never launched this" from "we launched
/// it and the process underneath was replaced".
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OwnershipFailure {
    /// No launch receipt names this endpoint at all. This is the arbitrary-loopback
    /// -listener case, and it is refused first.
    #[error("{authority} is not named by any launch this server performed: {detail}")]
    NotOwned {
        /// The refused authority.
        authority: String,
        /// Exactly what was missing.
        detail: String,
    },

    /// The launch exists but never reached `committed_fsynced`, so nothing proves
    /// the editor was ever released to run.
    #[error(
        "launch {transaction_id} is at phase {phase}, so no released editor is proven to be \
         hosting {kind}"
    )]
    LaunchNotCommitted {
        /// The launch transaction.
        transaction_id: String,
        /// The furthest durable phase.
        phase: String,
        /// Which protocol was asked for.
        kind: &'static str,
    },

    /// Containment was never confirmed, or the launch is not in a live, resumed
    /// state. A live editor is required; an exited one cannot host anything.
    #[error("launch {transaction_id} does not describe a live contained editor: {detail}")]
    NotLive {
        /// The launch transaction.
        transaction_id: String,
        /// What the proof actually said.
        detail: String,
    },

    /// A required receipt fact is absent, so the tuple cannot be formed. Refused
    /// rather than defaulted.
    #[error(
        "launch {transaction_id} receipt carries no {fact}, so ownership cannot be established"
    )]
    MissingFact {
        /// The launch transaction.
        transaction_id: String,
        /// The absent fact key.
        fact: String,
    },

    /// The launch never asked for this protocol, so the editor is not hosting it on
    /// a port this server chose.
    #[error(
        "launch {transaction_id} argv carries no {flag}, so this server never asked that editor \
         to host {kind}"
    )]
    ProtocolNotRequested {
        /// The launch transaction.
        transaction_id: String,
        /// The flag that was absent.
        flag: &'static str,
        /// Which protocol was asked for.
        kind: &'static str,
    },

    /// The launch was not an editor launch, so it cannot host either protocol.
    #[error(
        "launch {transaction_id} argv carries no {EDITOR_FLAG}; both protocols are editor plugins \
         and an export template has no language server compiled in at all"
    )]
    NotAnEditorLaunch {
        /// The launch transaction.
        transaction_id: String,
    },

    /// The endpoint is not on loopback.
    #[error(
        "{authority} is not a loopback address; neither Godot protocol is ever dialled remotely"
    )]
    NotLoopback {
        /// The refused authority.
        authority: String,
    },

    /// The endpoint presented does not match the recorded one.
    #[error(
        "endpoint changed: launch {transaction_id} recorded {recorded}, but {observed} was presented"
    )]
    EndpointChanged {
        /// The launch transaction.
        transaction_id: String,
        /// The recorded authority.
        recorded: String,
        /// The presented authority.
        observed: String,
    },

    /// The recorded process is gone. Whatever is listening now is somebody else.
    #[error("the editor recorded by launch {transaction_id} (pid {pid}) is no longer live")]
    ProcessGone {
        /// The launch transaction.
        transaction_id: String,
        /// The recorded PID.
        pid: u32,
    },

    /// A live process holds the recorded PID but not the recorded start time. This
    /// is PID reuse and is the single most important refusal in this module.
    #[error(
        "pid {pid} is live but its start time is {observed:?}, not the {recorded:?} launch \
         {transaction_id} recorded: this is PID reuse, not continuity"
    )]
    PidReused {
        /// The launch transaction.
        transaction_id: String,
        /// The recorded (and observed) PID.
        pid: u32,
        /// The recorded start time.
        recorded: String,
        /// The start time observed now.
        observed: Option<String>,
    },

    /// The executable behind the endpoint changed identity.
    #[error(
        "the engine binary changed: launch {transaction_id} recorded {recorded}, observed {observed}"
    )]
    BinaryChanged {
        /// The launch transaction.
        transaction_id: String,
        /// The recorded hash.
        recorded: String,
        /// The hash observed now.
        observed: String,
    },

    /// The project root behind the endpoint changed. Godot answers a mismatched
    /// `rootUri` with a `gdscript_client/changeWorkspace` notification instead of
    /// failing, so an unchecked client would silently operate on another project.
    #[error(
        "the project root changed: launch {transaction_id} recorded {recorded}, observed {observed}"
    )]
    RootChanged {
        /// The launch transaction.
        transaction_id: String,
        /// The recorded root URI.
        recorded: String,
        /// The root URI observed now.
        observed: String,
    },

    /// The held project's generation advanced, so every permit and every cached
    /// answer minted against the old generation is stale.
    #[error(
        "project generation advanced from {recorded} to {observed} since launch {transaction_id}"
    )]
    GenerationChanged {
        /// The launch transaction.
        transaction_id: String,
        /// The recorded generation.
        recorded: u64,
        /// The generation observed now.
        observed: u64,
    },
}

impl OwnershipFailure {
    /// The stable domain error kind. Every ownership failure is a refusal to act on
    /// something unproven, so none of them is `internal`.
    #[must_use]
    pub const fn error_kind(&self) -> godot_mcp_domain::ErrorKind {
        use godot_mcp_domain::ErrorKind;
        match self {
            // Refusals: this server has no authority over what is listening there.
            Self::NotOwned { .. } | Self::NotLoopback { .. } | Self::PidReused { .. } => {
                ErrorKind::PermissionDenied
            }
            // The world moved under a recorded launch.
            Self::EndpointChanged { .. }
            | Self::BinaryChanged { .. }
            | Self::RootChanged { .. }
            | Self::GenerationChanged { .. } => ErrorKind::Conflict,
            // Nothing is hosting the protocol right now.
            Self::LaunchNotCommitted { .. } | Self::NotLive { .. } | Self::ProcessGone { .. } => {
                ErrorKind::Unavailable
            }
            // The build or the launch cannot offer the protocol at all.
            Self::ProtocolNotRequested { .. } | Self::NotAnEditorLaunch { .. } => {
                ErrorKind::Unsupported
            }
            // A receipt that cannot form the tuple leaves the answer unknown.
            Self::MissingFact { .. } => ErrorKind::Uncertain,
        }
    }
}

/// What is observable about a candidate endpoint right now.
///
/// Supplied by the caller so this module makes no live syscalls of its own and every
/// test is deterministic. Production callers fill `pid`/`start_time` from
/// [`crate::supervision::procfs::process_identity`] and `binary_sha256` from the
/// executable on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedEndpoint {
    /// The authority actually being dialled.
    pub endpoint: EditorEndpoint,
    /// The live process holding the recorded PID, if any.
    pub pid: Option<u32>,
    /// That process's start time, if observable.
    pub start_time: Option<String>,
    /// The engine executable's hash as it is now.
    pub binary_sha256: String,
    /// The held project's root URI as it is now.
    pub project_root_uri: String,
    /// The held project's generation as it is now.
    pub project_generation: u64,
}

/// The verified capability to talk to one owned endpoint.
///
/// Private fields, no public constructor: the only way to obtain one is a successful
/// [`EndpointReceipt::verify`]. Every adapter entry point requires one, so a caller
/// that knows only a host and a port cannot reach an adapter at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedEndpoint {
    endpoint: EditorEndpoint,
    launch_transaction_id: String,
    pid: u32,
    start_time: String,
    project_root_uri: String,
    project_generation: u64,
    verified_at_reconnect: u32,
}

impl OwnedEndpoint {
    /// The authority to dial.
    #[must_use]
    pub const fn endpoint(&self) -> &EditorEndpoint {
        &self.endpoint
    }

    /// Which protocol this endpoint speaks.
    #[must_use]
    pub const fn kind(&self) -> EndpointKind {
        self.endpoint.kind
    }

    /// The launch transaction that owns it.
    #[must_use]
    pub fn launch_transaction_id(&self) -> &str {
        &self.launch_transaction_id
    }

    /// The verified editor PID.
    #[must_use]
    pub const fn pid(&self) -> u32 {
        self.pid
    }

    /// The verified editor start time.
    #[must_use]
    pub fn start_time(&self) -> &str {
        &self.start_time
    }

    /// The verified project root URI, as sent in `initialize`.
    #[must_use]
    pub fn project_root_uri(&self) -> &str {
        &self.project_root_uri
    }

    /// The verified project generation.
    #[must_use]
    pub const fn project_generation(&self) -> u64 {
        self.project_generation
    }

    /// How many reconnect verifications this token has passed. Zero is first use.
    #[must_use]
    pub const fn verified_at_reconnect(&self) -> u32 {
        self.verified_at_reconnect
    }
}

/// One endpoint's ownership record, derived from a server-owned launch receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointReceipt {
    /// Which protocol.
    pub kind: EndpointKind,
    /// The launch transaction this endpoint belongs to.
    pub launch_transaction_id: String,
    /// The endpoint this server asked the editor to host.
    pub endpoint: EditorEndpoint,
    /// The editor's PID, from the launch receipt.
    pub pid: u32,
    /// The editor's process start time, from the launch receipt.
    pub start_time: String,
    /// The engine binary's SHA-256, revalidated by the launch immediately before use.
    pub binary_sha256: String,
    /// The project root URI the editor was launched against.
    pub project_root_uri: String,
    /// The project generation at launch.
    pub project_generation: u64,
}

impl EndpointReceipt {
    /// Derive an endpoint's ownership from a committed Todo 14 launch record.
    ///
    /// The port is read from the argv the launch durably recorded at
    /// `intent_fsynced`, never from a default and never from a scan.
    ///
    /// # Errors
    /// Returns the [`OwnershipFailure`] that applies: the launch must be
    /// `committed_fsynced`, must carry a confirmed-containment live proof, must have
    /// asked for `--editor` and for this protocol's port flag, and must carry the
    /// PID, start time, binary hash and project root facts the tuple needs.
    pub fn from_launch(
        record: &LaunchRecord,
        kind: EndpointKind,
        project_root_uri: &str,
    ) -> Result<Self, OwnershipFailure> {
        let transaction_id = record.envelope.transaction_id.clone();
        if record.phase != LaunchPhase::CommittedFsynced {
            return Err(OwnershipFailure::LaunchNotCommitted {
                transaction_id,
                phase: record.phase.as_str().to_owned(),
                kind: kind.as_str(),
            });
        }
        let proof = record
            .proof
            .as_ref()
            .ok_or_else(|| OwnershipFailure::NotLive {
                transaction_id: transaction_id.clone(),
                detail: "the committed record carries no process_launch proof".to_owned(),
            })?;
        if proof.boundary_membership != BoundaryMembership::Confirmed {
            return Err(OwnershipFailure::NotLive {
                transaction_id,
                detail: "boundary membership was never confirmed by live inspection".to_owned(),
            });
        }
        if proof.release_status != ReleaseStatus::ResumedContained {
            return Err(OwnershipFailure::NotLive {
                transaction_id,
                detail: format!(
                    "release status is {:?}, and only a resumed-contained editor can host a \
                     protocol",
                    proof.release_status
                ),
            });
        }

        let intent = record
            .facts(LaunchRecord::INTENT_PAYLOAD)
            .map_err(|error| OwnershipFailure::NotOwned {
                authority: format!("{}:?", kind.as_str()),
                detail: format!("the launch intent payload is unreadable: {error}"),
            })?;
        let argv = argv_of(&intent);
        if !argv.iter().any(|item| item == EDITOR_FLAG) {
            return Err(OwnershipFailure::NotAnEditorLaunch { transaction_id });
        }
        let port = port_from_argv(&argv, kind.port_flag()).ok_or(
            OwnershipFailure::ProtocolNotRequested {
                transaction_id: transaction_id.clone(),
                flag: kind.port_flag(),
                kind: kind.as_str(),
            },
        )?;

        let receipt = record
            .facts(LaunchRecord::RECEIPT_PAYLOAD)
            .map_err(|error| OwnershipFailure::NotOwned {
                authority: EditorEndpoint::loopback(kind, port).authority(),
                detail: format!("the launch receipt payload is unreadable: {error}"),
            })?;
        let required = |fact: &str| -> Result<String, OwnershipFailure> {
            receipt
                .get(fact)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| OwnershipFailure::MissingFact {
                    transaction_id: transaction_id.clone(),
                    fact: fact.to_owned(),
                })
        };
        let pid = receipt
            .get(facts::CHILD_PID)
            .and_then(Value::as_u64)
            .and_then(|pid| u32::try_from(pid).ok())
            .ok_or_else(|| OwnershipFailure::MissingFact {
                transaction_id: transaction_id.clone(),
                fact: facts::CHILD_PID.to_owned(),
            })?;
        let start_time = required(facts::CHILD_START_TIME)?;
        let binary_sha256 = required(facts::BINARY_SHA256)?;

        Ok(Self {
            kind,
            launch_transaction_id: transaction_id,
            endpoint: EditorEndpoint::loopback(kind, port),
            pid,
            start_time,
            binary_sha256,
            project_root_uri: project_root_uri.to_owned(),
            project_generation: record.envelope.project.generation,
        })
    }

    /// Verify the endpoint is still the one this receipt names, and mint the token
    /// the adapters require.
    ///
    /// `reconnect` is the number of reconnects this token has already survived; it
    /// is carried into [`OwnedEndpoint::verified_at_reconnect`] so a caller can
    /// prove the check was re-run rather than reused. Verification is identical on
    /// first use and after a reconnect **by design**: a reconnect to the same port
    /// is exactly when PID reuse looks like continuity.
    ///
    /// # Errors
    /// Returns the [`OwnershipFailure`] for the first element of the tuple that no
    /// longer matches: endpoint, loopback, liveness, start time, binary hash,
    /// project root or project generation.
    pub fn verify(
        &self,
        observed: &ObservedEndpoint,
        reconnect: u32,
    ) -> Result<OwnedEndpoint, OwnershipFailure> {
        if !observed.endpoint.is_loopback() {
            return Err(OwnershipFailure::NotLoopback {
                authority: observed.endpoint.authority(),
            });
        }
        if observed.endpoint != self.endpoint {
            return Err(OwnershipFailure::EndpointChanged {
                transaction_id: self.launch_transaction_id.clone(),
                recorded: self.endpoint.authority(),
                observed: observed.endpoint.authority(),
            });
        }
        match observed.pid {
            None => {
                return Err(OwnershipFailure::ProcessGone {
                    transaction_id: self.launch_transaction_id.clone(),
                    pid: self.pid,
                });
            }
            Some(pid) if pid != self.pid => {
                return Err(OwnershipFailure::ProcessGone {
                    transaction_id: self.launch_transaction_id.clone(),
                    pid: self.pid,
                });
            }
            Some(_) => {}
        }
        if observed.start_time.as_deref() != Some(self.start_time.as_str()) {
            return Err(OwnershipFailure::PidReused {
                transaction_id: self.launch_transaction_id.clone(),
                pid: self.pid,
                recorded: self.start_time.clone(),
                observed: observed.start_time.clone(),
            });
        }
        if observed.binary_sha256 != self.binary_sha256 {
            return Err(OwnershipFailure::BinaryChanged {
                transaction_id: self.launch_transaction_id.clone(),
                recorded: self.binary_sha256.clone(),
                observed: observed.binary_sha256.clone(),
            });
        }
        if observed.project_root_uri != self.project_root_uri {
            return Err(OwnershipFailure::RootChanged {
                transaction_id: self.launch_transaction_id.clone(),
                recorded: self.project_root_uri.clone(),
                observed: observed.project_root_uri.clone(),
            });
        }
        if observed.project_generation != self.project_generation {
            return Err(OwnershipFailure::GenerationChanged {
                transaction_id: self.launch_transaction_id.clone(),
                recorded: self.project_generation,
                observed: observed.project_generation,
            });
        }
        Ok(OwnedEndpoint {
            endpoint: self.endpoint.clone(),
            launch_transaction_id: self.launch_transaction_id.clone(),
            pid: self.pid,
            start_time: self.start_time.clone(),
            project_root_uri: self.project_root_uri.clone(),
            project_generation: self.project_generation,
            verified_at_reconnect: reconnect,
        })
    }
}

/// Refuse an endpoint that no launch receipt names.
///
/// This is the arbitrary-loopback-listener answer, expressed as a function so a
/// caller with a bare authority has something honest to call and no way to proceed.
#[must_use]
pub fn refuse_unowned(endpoint: &EditorEndpoint, detail: &str) -> OwnershipFailure {
    OwnershipFailure::NotOwned {
        authority: endpoint.authority(),
        detail: detail.to_owned(),
    }
}

/// Select the receipt for `kind` from the launches this server owns.
///
/// Returns [`OwnershipFailure::NotOwned`] when no committed launch of ours asked an
/// editor to host that protocol on that authority — which is the only correct answer
/// for a listener we did not start.
///
/// # Errors
/// Returns the [`OwnershipFailure`] from the matching launch, or `NotOwned` when
/// nothing matches.
pub fn receipt_for(
    committed: &[LaunchRecord],
    kind: EndpointKind,
    project_root_uri: &str,
    endpoint: &EditorEndpoint,
) -> Result<EndpointReceipt, OwnershipFailure> {
    let mut refusals: Vec<String> = Vec::new();
    for record in committed {
        match EndpointReceipt::from_launch(record, kind, project_root_uri) {
            Ok(receipt) if &receipt.endpoint == endpoint => return Ok(receipt),
            Ok(receipt) => refusals.push(format!(
                "launch {} hosts {} on {}",
                receipt.launch_transaction_id,
                kind.as_str(),
                receipt.endpoint.authority()
            )),
            Err(failure) => refusals.push(failure.to_string()),
        }
    }
    Err(refuse_unowned(
        endpoint,
        &if refusals.is_empty() {
            "this server has no committed editor launch at all".to_owned()
        } else {
            refusals.join("; ")
        },
    ))
}

/// The argv array a launch recorded at `intent_fsynced`.
fn argv_of(intent: &BTreeMap<String, Value>) -> Vec<String> {
    intent
        .get(facts::ARGV)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// The port value following `flag` in a recorded argv, as a separate element.
///
/// Only the array form is accepted, matching Todo 14's rule that arguments are never
/// a shell string: `--lsp-port 6005` is two elements, and `--lsp-port=6005` is not
/// how this server writes it.
fn port_from_argv(argv: &[String], flag: &str) -> Option<u16> {
    let position = argv.iter().position(|item| item == flag)?;
    argv.get(position + 1)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervision::journal::{
        LaunchPayload, ProcessLaunchProof, ProjectIdentityClaim, TransactionEnvelope, TransportMode,
    };
    use serde_json::json;

    const ROOT_URI: &str = "file:///projects/demo";

    fn envelope(generation: u64) -> TransactionEnvelope {
        TransactionEnvelope {
            transaction_id: "00000000-0000-4000-8000-000000001801".to_owned(),
            project: ProjectIdentityClaim {
                object_identity: format!("project_{}", "2".repeat(64)),
                root_object: "unix:dev=1,ino=20".to_owned(),
                file_object: "unix:dev=1,ino=21".to_owned(),
                generation,
            },
            transport_mode: TransportMode::Stdio,
            principal: None,
            session: None,
            operation_family: crate::supervision::journal::OperationFamily::RuntimeExecution,
            tool_name: "adapters.launch_editor".to_owned(),
            idempotency_id: "idem-1801".to_owned(),
            canonical_request_hash: "0".repeat(64),
            canonical_request_bytes: 2,
            opened_at_unix_ms: 1_700_000_000_000,
        }
    }

    fn live_proof() -> ProcessLaunchProof {
        ProcessLaunchProof {
            boundary_identity: "adapter:boundary-1801".to_owned(),
            pid: Some(4242),
            start_time: Some("991122".to_owned()),
            boundary_membership: BoundaryMembership::Confirmed,
            release_status: ReleaseStatus::ResumedContained,
            exit_code: None,
            signal: None,
            boundary_empty_after_exit: None,
        }
    }

    fn record(argv: Vec<&str>, generation: u64) -> LaunchRecord {
        let mut intent = BTreeMap::new();
        intent.insert(facts::ARGV.to_owned(), json!(argv));
        let mut receipt = BTreeMap::new();
        receipt.insert(facts::CHILD_PID.to_owned(), json!(4242));
        receipt.insert(facts::CHILD_START_TIME.to_owned(), json!("991122"));
        receipt.insert(facts::BINARY_SHA256.to_owned(), json!("a".repeat(64)));
        LaunchRecord {
            envelope: envelope(generation),
            phase: LaunchPhase::CommittedFsynced,
            payloads: vec![
                LaunchPayload::new(LaunchRecord::INTENT_PAYLOAD, &intent, Vec::new()),
                LaunchPayload::new(LaunchRecord::STAGED_PAYLOAD, &BTreeMap::new(), Vec::new()),
                LaunchPayload::new(LaunchRecord::RECEIPT_PAYLOAD, &receipt, Vec::new()),
            ],
            proof: Some(live_proof()),
        }
    }

    fn editor_record() -> LaunchRecord {
        record(
            vec![
                "/opt/godot",
                "--editor",
                "--path",
                "/projects/demo",
                "--lsp-port",
                "6005",
                "--dap-port",
                "6006",
            ],
            17,
        )
    }

    fn observed(receipt: &EndpointReceipt) -> ObservedEndpoint {
        ObservedEndpoint {
            endpoint: receipt.endpoint.clone(),
            pid: Some(receipt.pid),
            start_time: Some(receipt.start_time.clone()),
            binary_sha256: receipt.binary_sha256.clone(),
            project_root_uri: receipt.project_root_uri.clone(),
            project_generation: receipt.project_generation,
        }
    }

    #[test]
    fn the_port_comes_from_the_recorded_argv_not_from_the_engine_default() {
        let record = record(
            vec![
                "/opt/godot",
                "--editor",
                "--lsp-port",
                "7777",
                "--dap-port",
                "7778",
            ],
            17,
        );
        let receipt = EndpointReceipt::from_launch(&record, EndpointKind::LanguageServer, ROOT_URI)
            .expect("owned");
        assert_eq!(receipt.endpoint.port, 7777);
        assert_ne!(
            receipt.endpoint.port,
            EndpointKind::LanguageServer.engine_default_port(),
            "the default is never substituted for a recorded port"
        );
    }

    #[test]
    fn a_launch_that_never_asked_for_the_protocol_is_unsupported() {
        let record = record(vec!["/opt/godot", "--editor", "--lsp-port", "6005"], 17);
        let failure = EndpointReceipt::from_launch(&record, EndpointKind::DebugAdapter, ROOT_URI)
            .expect_err("no --dap-port");
        assert!(
            matches!(failure, OwnershipFailure::ProtocolNotRequested { .. }),
            "{failure}"
        );
        assert_eq!(
            failure.error_kind(),
            godot_mcp_domain::ErrorKind::Unsupported
        );
    }

    #[test]
    fn a_non_editor_launch_can_never_host_either_protocol() {
        let record = record(vec!["/opt/godot", "--headless", "--lsp-port", "6005"], 17);
        let failure = EndpointReceipt::from_launch(&record, EndpointKind::LanguageServer, ROOT_URI)
            .expect_err("no --editor");
        assert!(
            matches!(failure, OwnershipFailure::NotAnEditorLaunch { .. }),
            "{failure}"
        );
    }

    #[test]
    fn an_uncommitted_launch_is_not_hosting_anything_yet() {
        let mut record = editor_record();
        record.phase = LaunchPhase::EffectStartedFsynced;
        let failure = EndpointReceipt::from_launch(&record, EndpointKind::LanguageServer, ROOT_URI)
            .expect_err("not released");
        assert!(
            matches!(failure, OwnershipFailure::LaunchNotCommitted { .. }),
            "{failure}"
        );
        assert_eq!(
            failure.error_kind(),
            godot_mcp_domain::ErrorKind::Unavailable
        );
    }

    #[test]
    fn an_exited_launch_is_refused_as_not_live() {
        let mut record = editor_record();
        record.proof = Some(ProcessLaunchProof {
            release_status: ReleaseStatus::ExitedClean,
            exit_code: Some(0),
            boundary_empty_after_exit: Some(true),
            ..live_proof()
        });
        let failure = EndpointReceipt::from_launch(&record, EndpointKind::LanguageServer, ROOT_URI)
            .expect_err("exited");
        assert!(
            matches!(failure, OwnershipFailure::NotLive { .. }),
            "{failure}"
        );
    }

    #[test]
    fn unconfirmed_containment_is_refused() {
        let mut record = editor_record();
        record.proof = Some(ProcessLaunchProof {
            boundary_membership: BoundaryMembership::Unconfirmed,
            ..live_proof()
        });
        let failure = EndpointReceipt::from_launch(&record, EndpointKind::LanguageServer, ROOT_URI)
            .expect_err("unconfirmed");
        assert!(
            matches!(failure, OwnershipFailure::NotLive { .. }),
            "{failure}"
        );
    }

    #[test]
    fn pid_reuse_is_refused_rather_than_read_as_continuity() {
        let receipt =
            EndpointReceipt::from_launch(&editor_record(), EndpointKind::LanguageServer, ROOT_URI)
                .expect("owned");
        let mut state = observed(&receipt);
        state.start_time = Some("999999".to_owned());
        let failure = receipt.verify(&state, 1).expect_err("pid reuse");
        assert!(
            matches!(failure, OwnershipFailure::PidReused { .. }),
            "{failure}"
        );
        assert_eq!(
            failure.error_kind(),
            godot_mcp_domain::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn each_element_of_the_tuple_is_checked_independently() {
        let receipt =
            EndpointReceipt::from_launch(&editor_record(), EndpointKind::LanguageServer, ROOT_URI)
                .expect("owned");
        assert!(receipt.verify(&observed(&receipt), 0).is_ok());

        let gone = ObservedEndpoint {
            pid: None,
            ..observed(&receipt)
        };
        assert!(matches!(
            receipt.verify(&gone, 0),
            Err(OwnershipFailure::ProcessGone { .. })
        ));

        let swapped = ObservedEndpoint {
            binary_sha256: "b".repeat(64),
            ..observed(&receipt)
        };
        assert!(matches!(
            receipt.verify(&swapped, 0),
            Err(OwnershipFailure::BinaryChanged { .. })
        ));

        let moved = ObservedEndpoint {
            project_root_uri: "file:///projects/other".to_owned(),
            ..observed(&receipt)
        };
        assert!(matches!(
            receipt.verify(&moved, 0),
            Err(OwnershipFailure::RootChanged { .. })
        ));

        let advanced = ObservedEndpoint {
            project_generation: 18,
            ..observed(&receipt)
        };
        assert!(matches!(
            receipt.verify(&advanced, 0),
            Err(OwnershipFailure::GenerationChanged { .. })
        ));

        let elsewhere = ObservedEndpoint {
            endpoint: EditorEndpoint::loopback(EndpointKind::LanguageServer, 6006),
            ..observed(&receipt)
        };
        assert!(matches!(
            receipt.verify(&elsewhere, 0),
            Err(OwnershipFailure::EndpointChanged { .. })
        ));

        let remote = ObservedEndpoint {
            endpoint: EditorEndpoint {
                kind: EndpointKind::LanguageServer,
                host: "10.0.0.5".to_owned(),
                port: 6005,
            },
            ..observed(&receipt)
        };
        assert!(matches!(
            receipt.verify(&remote, 0),
            Err(OwnershipFailure::NotLoopback { .. })
        ));
    }

    #[test]
    fn an_endpoint_no_launch_names_is_not_owned() {
        let failure = receipt_for(
            &[editor_record()],
            EndpointKind::LanguageServer,
            ROOT_URI,
            &EditorEndpoint::loopback(EndpointKind::LanguageServer, 6100),
        )
        .expect_err("a listener we did not start");
        assert!(
            matches!(failure, OwnershipFailure::NotOwned { .. }),
            "{failure}"
        );
        assert_eq!(
            failure.error_kind(),
            godot_mcp_domain::ErrorKind::PermissionDenied
        );

        let none = receipt_for(
            &[],
            EndpointKind::LanguageServer,
            ROOT_URI,
            &EditorEndpoint::loopback(EndpointKind::LanguageServer, 6005),
        )
        .expect_err("no launches at all");
        assert!(
            none.to_string().contains("no committed editor launch"),
            "{none}"
        );
    }

    #[test]
    fn a_token_records_which_reconnect_verified_it() {
        let receipt =
            EndpointReceipt::from_launch(&editor_record(), EndpointKind::LanguageServer, ROOT_URI)
                .expect("owned");
        let first = receipt.verify(&observed(&receipt), 0).expect("first use");
        assert_eq!(first.verified_at_reconnect(), 0);
        let after = receipt.verify(&observed(&receipt), 3).expect("reconnect");
        assert_eq!(after.verified_at_reconnect(), 3);
        assert_eq!(after.launch_transaction_id(), receipt.launch_transaction_id);
    }

    #[test]
    fn a_missing_receipt_fact_is_uncertain_not_defaulted() {
        let mut record = editor_record();
        let mut receipt_facts = BTreeMap::new();
        receipt_facts.insert(facts::CHILD_PID.to_owned(), json!(4242));
        // No start time: the tuple cannot be formed.
        record.payloads[2] =
            LaunchPayload::new(LaunchRecord::RECEIPT_PAYLOAD, &receipt_facts, Vec::new());
        let failure = EndpointReceipt::from_launch(&record, EndpointKind::LanguageServer, ROOT_URI)
            .expect_err("no start time");
        assert!(
            matches!(failure, OwnershipFailure::MissingFact { .. }),
            "{failure}"
        );
        assert_eq!(failure.error_kind(), godot_mcp_domain::ErrorKind::Uncertain);
    }

    #[test]
    fn a_port_flag_is_only_read_as_a_separate_argv_element() {
        assert_eq!(
            port_from_argv(&["--lsp-port".to_owned(), "6005".to_owned()], "--lsp-port"),
            Some(6005)
        );
        assert_eq!(
            port_from_argv(&["--lsp-port=6005".to_owned()], "--lsp-port"),
            None,
            "this server never writes a joined flag, so a joined one is not ours"
        );
    }
}
