//! The five effect-boundary traits.
//!
//! Each boundary is the *only* way its class of side effect can be produced, and
//! each method requires an [`EffectTicket`] — a value the gateway alone can mint
//! from a validated permit. A caller holding a boundary implementation but no
//! ticket therefore cannot invoke a direct effect, which is what makes "direct
//! effect invocation" impossible rather than merely discouraged.
//!
//! Todo 6 ships the `file` boundary as a working `SafeRoot`-backed
//! implementation because the durable journal must be provable before any Godot
//! or editor code exists. The `process` boundary received its real
//! implementation in Todos 13-14 and writes to *this* journal. The shipped
//! `serve` assembly installs the bridge to the live editor plugin as the
//! `editor` boundary, and the managed-session manager's implementations as
//! the `runtime` boundary (commands to a managed game's probe) and the
//! `lsp_dap` boundary (the bounded adapters, dialled only through a managed
//! editor's server-owned launch receipt). Assemblies without them keep the
//! fail-closed stand-ins: `ProductionLspDapBoundary` in `crate::engine_plane`
//! and [`UnavailableBoundaries`] report `available = false` and refuse every
//! effect.

use std::path::Path;

use godot_mcp_domain::safe_root::{SafeRoot, SafeRootError};
use godot_mcp_engine::supervision::gate::CapturedStream;
use godot_mcp_engine::supervision::protocol::{LaunchDiagnostics, LaunchRequest, TerminationKind};
use godot_mcp_engine::validation::diagnostic::Diagnostic;
use godot_mcp_engine::validation::outcome::Observed;
use serde::Serialize;

use godot_mcp_bridge::protocol::mutation::MutationStatus;

use super::permit::AuthoritySeal;
use crate::journal::{EffectBoundaryKind, TypedProof};

/// Proof that the gateway validated a permit and journaled
/// `effect_started_fsynced` for this exact transaction.
///
/// The seal field makes the ticket unconstructable outside the permit module's
/// authority types, so no caller can fabricate one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EffectTicket {
    transaction_id: String,
    boundary: EffectBoundaryKind,
    seal: AuthoritySeal,
}

impl EffectTicket {
    /// Mint a ticket. Callable only from the gateway module.
    pub(super) const fn new(
        transaction_id: String,
        boundary: EffectBoundaryKind,
        seal: AuthoritySeal,
    ) -> Self {
        Self {
            transaction_id,
            boundary,
            seal,
        }
    }

    pub(crate) fn transaction_id(&self) -> &str {
        &self.transaction_id
    }

    pub(crate) const fn boundary(&self) -> EffectBoundaryKind {
        self.boundary
    }
}

/// One staged file mutation to publish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileWrite {
    /// Project-relative path, resolved only through `SafeRoot`.
    pub(crate) path: String,
    /// Expected preimage for a compare-and-swap replacement; `None` for a create.
    pub(crate) expected: Option<Vec<u8>>,
    /// Bytes to publish.
    pub(crate) bytes: Vec<u8>,
}

/// Descriptor-relative project file writes. Todo 6 ships a working
/// `SafeRoot`-backed implementation because the durable journal must be provable
/// before any Godot or editor code exists.
pub(crate) trait FileEffectBoundary: Send + Sync {
    /// Whether a real implementation is compiled in.
    fn available(&self) -> bool;

    /// Publish one staged write. Requires a gateway-minted ticket.
    fn publish(&self, ticket: &EffectTicket, write: &FileWrite) -> Result<(), SafeRootError>;

    /// Remove one file that must still hold exactly `expected`. Requires a
    /// gateway-minted ticket.
    fn delete(
        &self,
        ticket: &EffectTicket,
        path: &str,
        expected: &[u8],
    ) -> Result<(), SafeRootError>;
}

/// A supervised child-process launch request (implemented by Todo 14).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcessLaunchRequest {
    pub(crate) launch: LaunchRequest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub(crate) struct ProcessStreamOutputMetadata {
    pub(crate) retained_bytes: u64,
    pub(crate) total_bytes: u64,
    pub(crate) truncated: bool,
}

impl From<&CapturedStream> for ProcessStreamOutputMetadata {
    fn from(stream: &CapturedStream) -> Self {
        Self {
            retained_bytes: stream.bytes.len() as u64,
            total_bytes: stream.total_bytes,
            truncated: stream.truncated,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct ProcessOutputMetadata {
    pub(crate) stdout: ProcessStreamOutputMetadata,
    pub(crate) stderr: ProcessStreamOutputMetadata,
}

impl ProcessOutputMetadata {
    pub(crate) fn from_diagnostics(diagnostics: &LaunchDiagnostics) -> Self {
        Self {
            stdout: ProcessStreamOutputMetadata::from(&diagnostics.stdout),
            stderr: ProcessStreamOutputMetadata::from(&diagnostics.stderr),
        }
    }

    pub(crate) const fn diagnostics_truncated(self) -> bool {
        self.stdout.truncated || self.stderr.truncated
    }
}

/// The largest captured-stream excerpt returned inline in a public result.
/// The complete capture still drives diagnostics; the excerpt is what a caller
/// reads, and it says when it was shortened.
pub(crate) const PUBLIC_STREAM_EXCERPT_BYTES: usize = 16 * 1024;

/// Bounded, public text excerpts of a supervised process's captured streams.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ProcessOutputExcerpt {
    pub(crate) stdout: StreamExcerpt,
    pub(crate) stderr: StreamExcerpt,
}

/// The tail of one captured stream, decoded lossily as UTF-8.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct StreamExcerpt {
    pub(crate) text: String,
    /// Whether the excerpt is shorter than the retained capture.
    pub(crate) shortened: bool,
}

impl StreamExcerpt {
    pub(crate) fn of(stream: &CapturedStream) -> Self {
        let decoded = String::from_utf8_lossy(&stream.bytes);
        if decoded.len() <= PUBLIC_STREAM_EXCERPT_BYTES {
            return Self {
                text: decoded.into_owned(),
                shortened: false,
            };
        }
        let mut start = decoded.len() - PUBLIC_STREAM_EXCERPT_BYTES;
        while !decoded.is_char_boundary(start) {
            start += 1;
        }
        Self {
            text: decoded[start..].to_owned(),
            shortened: true,
        }
    }
}

impl ProcessOutputExcerpt {
    pub(crate) fn from_diagnostics(diagnostics: &LaunchDiagnostics) -> Self {
        Self {
            stdout: StreamExcerpt::of(&diagnostics.stdout),
            stderr: StreamExcerpt::of(&diagnostics.stderr),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcessEffectOutcome {
    pub(crate) proof: TypedProof,
    pub(crate) termination: TerminationKind,
    pub(crate) observed: Observed,
    pub(crate) diagnostics: Vec<Diagnostic>,
    pub(crate) output: ProcessOutputMetadata,
    pub(crate) excerpt: ProcessOutputExcerpt,
    /// Public-safe containment facts: the mechanism and its strength, never its
    /// concrete identity.
    pub(crate) boundary_kind: &'static str,
    pub(crate) boundary_strength: &'static str,
}

/// What a managed session runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SessionKind {
    /// The project's game, with the runtime probe.
    Game,
    /// The Godot editor, hosting the language server and debug adapter.
    Editor,
    /// One export of one preset.
    Export,
}

impl SessionKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Game => "game",
            Self::Editor => "editor",
            Self::Export => "export",
        }
    }

    /// The prefix of this kind's opaque session IDs.
    pub(crate) const fn prefix(self) -> &'static str {
        match self {
            Self::Game => "run_",
            Self::Editor => "editor_",
            Self::Export => "export_",
        }
    }
}

/// One managed session to start: a supervised launch that keeps running after
/// the call that started it returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionStartRequest {
    /// The opaque session ID the server chose.
    pub(crate) session_id: String,
    pub(crate) kind: SessionKind,
    /// The supervised launch. For a game it runs the session's launcher.
    pub(crate) launch: LaunchRequest,
    /// The scene a game runs, when not the project's main scene.
    pub(crate) scene: Option<String>,
    /// `window` or `headless`.
    pub(crate) display: String,
    /// How long the session may run before its boundary is terminated.
    pub(crate) lifetime: std::time::Duration,
    /// How long the session may take to become ready.
    pub(crate) readiness: std::time::Duration,
    pub(crate) godot_version: Option<String>,
    /// The ports an editor session hosts its language server and debug
    /// adapter on, as its argument array names them.
    pub(crate) lsp_port: Option<u16>,
    pub(crate) dap_port: Option<u16>,
    /// What an export session exports.
    pub(crate) export: Option<crate::engine_plane::session::export::ExportPlan>,
}

/// A managed session that started and became ready.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionStartOutcome {
    /// The session's public projection.
    pub(crate) session: serde_json::Value,
    /// The running launch's proof: released, contained, no exit yet.
    pub(crate) proof: TypedProof,
    pub(crate) boundary_kind: &'static str,
    pub(crate) boundary_strength: &'static str,
}

/// Supervised process launches. Todo 14 owns the real implementation the shipped
/// `serve` assembly installs.
pub(crate) trait ProcessEffectBoundary: Send + Sync {
    /// Whether a real implementation is compiled in. A fail-closed stand-in
    /// reports `false` so health reporting never claims it is ready.
    fn available(&self) -> bool;

    /// Why the boundary cannot launch, when it is compiled in but this host
    /// or configuration offers no containment for a launch.
    fn unavailable_reason(&self) -> Option<String> {
        None
    }

    fn launch(
        &self,
        ticket: &EffectTicket,
        request: &ProcessLaunchRequest,
    ) -> Result<ProcessEffectOutcome, BoundaryUnavailable>;

    /// Start one managed session and return once it is ready. `Err` carries
    /// why it did not start; whatever was launched has then been terminated.
    fn start_session(
        &self,
        ticket: &EffectTicket,
        request: &SessionStartRequest,
    ) -> Result<SessionStartOutcome, godot_mcp_domain::contracts::OutcomeError> {
        let _ = (ticket, request);
        Err(godot_mcp_domain::contracts::OutcomeError::new(
            godot_mcp_domain::ErrorKind::Unavailable,
            "managed Godot sessions are not available in this build",
            false,
            std::collections::BTreeMap::new(),
        ))
    }

    /// Stop one managed session whose stop is a process effect — an export's
    /// cancellation — and report its three-state answer. The command's
    /// `payload` names its `session`. `Err` means nothing was stopped.
    fn stop_session(
        &self,
        ticket: &EffectTicket,
        command: &EditorMutation,
    ) -> Result<EditorEffectOutcome, BoundaryUnavailable> {
        let _ = (ticket, command);
        Err(BoundaryUnavailable {
            boundary: "process",
        })
    }
}

/// One LSP/DAP request/response exchange. Todos 15/16 implement the bounded
/// adapter this shape addresses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LspDapExchange {
    pub(crate) method: String,
    pub(crate) payload: String,
}

/// Language/debug adapter channels. `serve` installs the managed editor
/// sessions' implementation, which dials an endpoint only through the
/// server-owned launch receipt of the editor that hosts it; other assemblies
/// keep a fail-closed stand-in.
pub(crate) trait LspDapEffectBoundary: Send + Sync {
    /// Whether a real implementation is compiled in.
    fn available(&self) -> bool;

    /// Whether the boundary can serve now: whether this server knows an
    /// editor session to answer for.
    fn connected(&self) -> bool {
        self.available()
    }

    /// Whether an editor session hosting the protocols is running now, for
    /// health reporting.
    fn running(&self) -> bool {
        self.connected()
    }

    fn exchange(
        &self,
        ticket: &EffectTicket,
        exchange: &LspDapExchange,
    ) -> Result<String, BoundaryUnavailable>;

    /// Carry out one journaled debug command and report its three-state
    /// answer. The command's `payload` names its editor `session`. `Err`
    /// means nothing reached the editor.
    fn apply(
        &self,
        ticket: &EffectTicket,
        command: &EditorMutation,
    ) -> Result<EditorEffectOutcome, BoundaryUnavailable> {
        let _ = (ticket, command);
        Err(BoundaryUnavailable {
            boundary: "lsp_dap",
        })
    }
}

/// One live-editor mutation: the bridge method (`action`), its payload as
/// compact JSON text, the caller's idempotency key, how long the editor has
/// to answer and the caller's cancellation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EditorMutation {
    pub(crate) action: String,
    pub(crate) payload: String,
    pub(crate) idempotency_id: String,
    pub(crate) timeout: std::time::Duration,
    pub(crate) cancellation: CancelSignal,
}

/// The caller's cancellation of one command in flight, for a boundary that
/// can withdraw the command from its peer. It is not part of the command's
/// identity: two commands that differ only in it are the same command.
#[derive(Debug, Clone, Default)]
pub(crate) struct CancelSignal(Option<tokio_util::sync::CancellationToken>);

impl CancelSignal {
    /// The signal of `token`.
    pub(crate) fn of(token: &tokio_util::sync::CancellationToken) -> Self {
        Self(Some(token.clone()))
    }

    /// Whether the caller cancelled.
    pub(crate) fn is_cancelled(&self) -> bool {
        self.0
            .as_ref()
            .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
    }
}

impl PartialEq for CancelSignal {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Eq for CancelSignal {}

/// The editor's three-state answer to one mutation, with the proof the journal
/// records for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EditorEffectOutcome {
    /// `committed`, `not_started` or `unresolved`.
    pub(crate) status: MutationStatus,
    /// The plugin's result payload, when it answered with a result.
    pub(crate) payload: serde_json::Value,
    /// The classified failure, when it answered with an error or no answer
    /// came back.
    pub(crate) error: Option<(godot_mcp_domain::ErrorKind, String)>,
    /// The `editor_runtime` proof derived from the answer.
    pub(crate) proof: TypedProof,
}

/// The live-editor boundary. `serve` installs the bridge to the `godot_mcp`
/// editor plugin; other assemblies keep the fail-closed stand-in.
pub(crate) trait EditorEffectBoundary: Send + Sync {
    /// Whether a real implementation is compiled in.
    fn available(&self) -> bool;

    /// Whether the boundary can serve right now: for the live editor, whether
    /// an editor is connected.
    fn connected(&self) -> bool {
        self.available()
    }

    fn mutate(
        &self,
        ticket: &EffectTicket,
        mutation: &EditorMutation,
    ) -> Result<TypedProof, BoundaryUnavailable>;

    /// Apply one mutation and report the editor's three-state answer.
    /// `Err` means nothing reached the editor.
    fn apply(
        &self,
        ticket: &EffectTicket,
        mutation: &EditorMutation,
    ) -> Result<EditorEffectOutcome, BoundaryUnavailable> {
        self.mutate(ticket, mutation)
            .map(|proof| EditorEffectOutcome {
                status: MutationStatus::Committed,
                payload: serde_json::json!({}),
                error: None,
                proof,
            })
    }
}

/// One unavailable running-instance operation shape retained after Todo 25 was cancelled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeOperation {
    pub(crate) operation: String,
    pub(crate) payload: String,
}

/// The running-instance boundary: commands to a managed game session's
/// runtime probe. `serve` installs the session manager's implementation;
/// other assemblies keep the fail-closed stand-in.
pub(crate) trait RuntimeEffectBoundary: Send + Sync {
    /// Whether a real implementation is compiled in.
    fn available(&self) -> bool;

    /// Whether the boundary can serve now: whether this server knows a game
    /// session to answer for.
    fn connected(&self) -> bool {
        false
    }

    /// Whether a game session is running now, for health reporting.
    fn running(&self) -> bool {
        self.connected()
    }

    fn invoke(
        &self,
        ticket: &EffectTicket,
        operation: &RuntimeOperation,
    ) -> Result<TypedProof, BoundaryUnavailable>;

    /// Carry out one runtime command and report its three-state answer. The
    /// command's `payload` names its `session`. `Err` means nothing reached
    /// the session.
    fn apply(
        &self,
        ticket: &EffectTicket,
        command: &EditorMutation,
    ) -> Result<EditorEffectOutcome, BoundaryUnavailable> {
        let _ = (ticket, command);
        Err(BoundaryUnavailable {
            boundary: "runtime",
        })
    }
}

/// A boundary with no real implementation compiled in. It fails closed: it never
/// returns a fabricated proof and never performs a partial effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the {boundary} effect boundary is not implemented in this build")]
pub(crate) struct BoundaryUnavailable {
    /// Which boundary was requested.
    pub(crate) boundary: &'static str,
}

/// The production `file` boundary: every write goes through Todo 2's held-lease
/// `SafeRoot`, so path authority stays bound to the held descriptor.
pub(crate) struct SafeRootFileBoundary {
    root: SafeRoot,
}

impl SafeRootFileBoundary {
    pub(crate) const fn new(root: SafeRoot) -> Self {
        Self { root }
    }
}

impl FileEffectBoundary for SafeRootFileBoundary {
    fn available(&self) -> bool {
        true
    }

    fn publish(&self, ticket: &EffectTicket, write: &FileWrite) -> Result<(), SafeRootError> {
        authorize_file_ticket(ticket)?;
        if let Some(expected) = write.expected.as_deref()
            && write.path == godot_mcp_domain::sysfs::PROJECT_MARKER
        {
            // The lease is bound to the marker's object identity, so the
            // marker is rewritten in place: a replacing publication would read
            // as a retarget of the held project and close every later effect.
            return self
                .root
                .cas_overwrite(Path::new(&write.path), expected, &write.bytes);
        }
        if write.expected.is_none() {
            // A create may name directories that do not exist yet. They are
            // created handle-relative, symlinks refused, only after the gateway
            // validated the permit and journaled `effect_started_fsynced`.
            self.root.create_parent_dirs(Path::new(&write.path))?;
        }
        self.root.cas_write(
            Path::new(&write.path),
            write.expected.as_deref(),
            &write.bytes,
        )
    }

    fn delete(
        &self,
        ticket: &EffectTicket,
        path: &str,
        expected: &[u8],
    ) -> Result<(), SafeRootError> {
        authorize_file_ticket(ticket)?;
        self.root.cas_delete(Path::new(path), expected)
    }
}

/// The ticket is the boundary's own proof that the gateway validated a permit
/// and journaled `effect_started_fsynced` for this exact transaction. A
/// mismatched or empty ticket is refused rather than acted on, so even a
/// crate-internal misuse cannot produce an unjournaled effect.
fn authorize_file_ticket(ticket: &EffectTicket) -> Result<(), SafeRootError> {
    if ticket.boundary() != EffectBoundaryKind::File || ticket.transaction_id().is_empty() {
        return Err(SafeRootError::Io(
            "file effect ticket does not authorize this boundary".to_owned(),
        ));
    }
    Ok(())
}

/// Fail-closed stand-ins for boundaries with no compiled implementation. The
/// editor and runtime boundaries retain these stand-ins because Todos 23 and 25
/// were cancelled.
pub(crate) struct UnavailableBoundaries;

impl ProcessEffectBoundary for UnavailableBoundaries {
    fn available(&self) -> bool {
        false
    }

    fn launch(
        &self,
        _ticket: &EffectTicket,
        _request: &ProcessLaunchRequest,
    ) -> Result<ProcessEffectOutcome, BoundaryUnavailable> {
        Err(BoundaryUnavailable {
            boundary: "process",
        })
    }
}

impl LspDapEffectBoundary for UnavailableBoundaries {
    fn available(&self) -> bool {
        false
    }

    fn exchange(
        &self,
        _ticket: &EffectTicket,
        _exchange: &LspDapExchange,
    ) -> Result<String, BoundaryUnavailable> {
        Err(BoundaryUnavailable {
            boundary: "lsp_dap",
        })
    }
}

impl EditorEffectBoundary for UnavailableBoundaries {
    fn available(&self) -> bool {
        false
    }

    fn mutate(
        &self,
        _ticket: &EffectTicket,
        _mutation: &EditorMutation,
    ) -> Result<TypedProof, BoundaryUnavailable> {
        Err(BoundaryUnavailable { boundary: "editor" })
    }
}

impl RuntimeEffectBoundary for UnavailableBoundaries {
    fn available(&self) -> bool {
        false
    }

    fn invoke(
        &self,
        _ticket: &EffectTicket,
        _operation: &RuntimeOperation,
    ) -> Result<TypedProof, BoundaryUnavailable> {
        Err(BoundaryUnavailable {
            boundary: "runtime",
        })
    }
}
