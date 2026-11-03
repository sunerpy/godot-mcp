//! The server end of the godot-mcp bridge.
//!
//! [`EditorBridge`] binds the loopback listener, writes the private rendezvous
//! file the peer reads, and serves exactly one authenticated session at a
//! time: a newer authenticated session supersedes the older one. The live
//! editor's `godot_mcp` plugin is one peer ([`BridgeConfig::editor`]); the
//! runtime probe of a managed game session is the other, one bridge per
//! session ([`BridgeConfig::runtime`]). Each session has a reader thread that delivers
//! answers to waiting callers, answers pings, sends heartbeats and closes the
//! session when the plugin falls silent while it runs no request. A plugin
//! runs requests on its main thread and answers no heartbeat meanwhile, so a
//! request in progress may keep it silent until that request's deadline.
//!
//! A call is dispatched only when a session is live and the plugin reported,
//! in `bridge.describe_session`, that it implements the method. Every call has
//! one of four endings, and the difference matters for mutations:
//!
//! * an answer (`result` or `error`, each carrying its mutation status);
//! * [`BridgeFailure::NotConnected`] / [`BridgeFailure::Unsupported`] — nothing
//!   was sent, so nothing can have changed;
//! * [`BridgeFailure::Lost`] / [`BridgeFailure::TimedOut`] — the request was
//!   sent and no answer came back. Whether a mutation ran is not decidable
//!   here; the caller records it as unresolved and never sends it again.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use godot_mcp_bridge::protocol::compat::PROTOCOL_VERSION;
use godot_mcp_bridge::protocol::envelope::{
    Deadline, Envelope, MessageKind, OperationClass, PluginIdentity, ProjectClaim, ServerIdentity,
};
use godot_mcp_bridge::protocol::handshake::Handshake;
use godot_mcp_bridge::protocol::listener::{BridgeListener, SessionReader, SessionWriter};
use godot_mcp_bridge::protocol::mutation::MutationStatus;
use godot_mcp_bridge::protocol::rendezvous::RendezvousFile;
use godot_mcp_domain::ErrorKind;
use serde_json::{Value, json};

use crate::effect::boundary::CancelSignal;

/// Name of the rendezvous file inside the project's private state root. The
/// plugin reads it as `res://.godot-mcp/editor-bridge.rendezvous`.
pub(crate) const RENDEZVOUS_FILE: &str = "editor-bridge.rendezvous";

/// The bridge methods this server may send, as advertised in `capabilities`.
pub(crate) const BRIDGE_METHODS: &[&str] = &[
    "editor.addon.reload",
    "editor.capture.chunk",
    "editor.capture.viewport",
    "editor.filesystem.scan",
    "editor.history.redo",
    "editor.history.undo",
    "editor.log.read",
    "editor.main_screen.set",
    "editor.node",
    "editor.play.start",
    "editor.play.stop",
    "editor.preferences.read",
    "editor.preferences.set",
    "editor.scene.batch",
    "editor.scene.close",
    "editor.scene.edit",
    "editor.scene.open",
    "editor.scene.save",
    "editor.scene.tree",
    "editor.script.open",
    "editor.selection.set",
    "editor.settings.apply",
    "editor.shortcuts.read",
    "editor.shortcuts.set",
    "editor.state",
    "editor.types.class",
    "editor.types.search",
];

/// What distinguishes one bridge from another: where its rendezvous file is
/// written, which methods it may send, and how its threads and events are
/// named.
#[derive(Debug, Clone)]
pub(crate) struct BridgeConfig {
    pub(crate) rendezvous: PathBuf,
    pub(crate) methods: &'static [&'static str],
    label: &'static str,
    events: BridgeEvents,
    liveness: Liveness,
}

/// How a bridge tells a live peer from a gone one.
#[derive(Debug, Clone, Copy)]
struct Liveness {
    /// How often the server proves its own liveness to the peer.
    heartbeat: Duration,
    /// How long the peer may stay silent while it runs no request.
    silence: Duration,
}

/// A peer answers heartbeats from its main thread, which can block for a
/// while (an import, a large save) without the peer being gone.
const LIVENESS: Liveness = Liveness {
    heartbeat: Duration::from_secs(5),
    silence: Duration::from_secs(30),
};

#[derive(Debug, Clone, Copy)]
struct BridgeEvents {
    refused: &'static str,
    undescribed: &'static str,
    connected: &'static str,
    disconnected: &'static str,
}

impl BridgeConfig {
    /// The live editor's bridge, rendezvous in the project's state root.
    pub(crate) fn editor(state_root: &Path) -> Self {
        Self {
            rendezvous: state_root.join(RENDEZVOUS_FILE),
            methods: BRIDGE_METHODS,
            label: "editor",
            events: BridgeEvents {
                refused: "editor_bridge_refused",
                undescribed: "editor_bridge_undescribed",
                connected: "editor_bridge_connected",
                disconnected: "editor_bridge_disconnected",
            },
            liveness: LIVENESS,
        }
    }

    /// One managed game session's runtime-probe bridge.
    pub(crate) fn runtime(rendezvous: PathBuf, methods: &'static [&'static str]) -> Self {
        Self {
            rendezvous,
            methods,
            label: "runtime",
            events: BridgeEvents {
                refused: "runtime_bridge_refused",
                undescribed: "runtime_bridge_undescribed",
                connected: "runtime_bridge_connected",
                disconnected: "runtime_bridge_disconnected",
            },
            liveness: LIVENESS,
        }
    }

    /// This bridge, sending heartbeats every `heartbeat` and closing a peer
    /// silent for `silence`.
    #[cfg(test)]
    pub(crate) const fn with_liveness(mut self, heartbeat: Duration, silence: Duration) -> Self {
        self.liveness = Liveness { heartbeat, silence };
        self
    }
}

/// How long a fresh session has to describe itself.
const DESCRIBE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the acceptor waits for a dial before checking whether to stop.
const ACCEPT_WAIT: Duration = Duration::from_millis(250);
/// How long a reader waits for a frame before its housekeeping.
const READ_WAIT: Duration = Duration::from_millis(250);
/// Slack past a request's own deadline before the server gives up on it.
const ANSWER_GRACE: Duration = Duration::from_secs(2);
/// How often a caller waiting on an answer looks at its cancellation.
const CANCEL_POLL: Duration = Duration::from_millis(50);

/// One answer from the plugin.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum BridgeAnswer {
    /// A `result`: the method's payload and its mutation status.
    Result {
        status: MutationStatus,
        payload: Value,
    },
    /// An `error`: the classified failure and, for a mutation, its status.
    Error {
        kind: ErrorKind,
        message: String,
        status: Option<MutationStatus>,
    },
}

/// A call that ended without an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BridgeFailure {
    /// No plugin session is live. Nothing was sent.
    NotConnected,
    /// The connected plugin does not implement the method. Nothing was sent.
    Unsupported(String),
    /// The session ended after the request was sent and before an answer.
    Lost,
    /// No answer arrived before the request's deadline.
    TimedOut,
}

impl BridgeFailure {
    /// Whether the request reached the plugin, so a mutation may have run.
    pub(crate) const fn dispatched(&self) -> bool {
        matches!(self, Self::Lost | Self::TimedOut)
    }

    pub(crate) fn message(&self) -> String {
        match self {
            Self::NotConnected => "no editor is connected to the godot-mcp bridge".to_owned(),
            Self::Unsupported(method) => {
                format!("the connected editor plugin does not implement {method}")
            }
            Self::Lost => "the editor connection was lost before it answered".to_owned(),
            Self::TimedOut => "the editor did not answer before the deadline".to_owned(),
        }
    }

    /// The same failure, said of a game session's runtime probe.
    pub(crate) fn probe_message(&self) -> String {
        match self {
            Self::NotConnected => "the game session's runtime probe is not connected".to_owned(),
            Self::Unsupported(method) => {
                format!("the game session's runtime probe does not implement {method}")
            }
            Self::Lost => {
                "the game session's probe connection was lost before it answered".to_owned()
            }
            Self::TimedOut => {
                "the game session's probe did not answer before the deadline".to_owned()
            }
        }
    }
}

/// An editor is connected, so the bridge cannot be held free of one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EditorConnected;

/// While this is held, the bridge installs no editor session.
pub(crate) struct ConnectionExclusion {
    inner: Arc<Inner>,
}

impl Drop for ConnectionExclusion {
    fn drop(&mut self) {
        let mut exclusions = lock(&self.inner.admission);
        *exclusions = exclusions.saturating_sub(1);
    }
}

/// What the bridge can say about its connection, path-free.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BridgeStatus {
    pub(crate) connected: bool,
    pub(crate) plugin_version: Option<String>,
    pub(crate) godot_version: Option<String>,
    pub(crate) methods: Vec<String>,
    pub(crate) description: Option<Value>,
    pub(crate) sessions_opened: u64,
    pub(crate) sessions_closed: u64,
    pub(crate) last_close_reason: Option<String>,
}

impl BridgeStatus {
    pub(crate) fn to_value(&self) -> Value {
        json!({
            "connected": self.connected,
            "plugin_version": self.plugin_version,
            "godot_version": self.godot_version,
            "methods": self.methods,
            "sessions_opened": self.sessions_opened,
            "sessions_closed": self.sessions_closed,
            "last_close_reason": self.last_close_reason,
        })
    }
}

type AnswerSender = SyncSender<Result<BridgeAnswer, BridgeFailure>>;

struct Session {
    generation: u64,
    writer: SessionWriter,
    plugin: PluginIdentity,
    description: Value,
    methods: BTreeSet<String>,
}

#[derive(Default)]
struct Counters {
    opened: u64,
    closed: u64,
    last_close_reason: Option<String>,
    /// Generations that were described and served, so a session that ends
    /// before it was ever served is not counted as a closed one.
    served: BTreeSet<u64>,
}

struct Inner {
    stop: AtomicBool,
    server: ServerIdentity,
    methods: &'static [&'static str],
    label: &'static str,
    events: BridgeEvents,
    liveness: Liveness,
    /// How many exclusions are held. While one is, no session is installed.
    /// Taken before `current` wherever both are held.
    admission: Mutex<usize>,
    current: Mutex<Option<Session>>,
    /// Waiting callers by request message id.
    pending: Mutex<HashMap<String, Pending>>,
    next_message: AtomicU64,
    next_generation: AtomicU64,
    counters: Mutex<Counters>,
}

/// One request awaiting its answer. Its caller removes it when it stops
/// waiting, at its deadline at the latest.
struct Pending {
    /// The session generation the request was sent on.
    generation: u64,
    sender: AnswerSender,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// The live-editor bridge of one bound project.
pub(crate) struct EditorBridge {
    inner: Arc<Inner>,
    rendezvous: RendezvousFile,
    acceptor: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for EditorBridge {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EditorBridge")
            .field("connected", &self.status().connected)
            .finish_non_exhaustive()
    }
}

impl EditorBridge {
    /// Bind the loopback listener, write the rendezvous file into
    /// `state_root` and start accepting plugin sessions for `project`.
    ///
    /// # Errors
    /// Returns a description when the listener cannot bind or the rendezvous
    /// file cannot be written.
    pub(crate) fn start(state_root: &Path, project: ProjectClaim) -> Result<Arc<Self>, String> {
        let config = BridgeConfig::editor(state_root);
        remove_stale_rendezvous(&config.rendezvous)?;
        Self::start_with(config, project)
    }

    /// Bind a bridge described by `config` and start accepting sessions.
    ///
    /// # Errors
    /// Returns a description when the listener cannot bind or the rendezvous
    /// file cannot be written.
    pub(crate) fn start_with(
        config: BridgeConfig,
        project: ProjectClaim,
    ) -> Result<Arc<Self>, String> {
        let listener = BridgeListener::bind_loopback().map_err(|error| error.to_string())?;
        let rendezvous =
            RendezvousFile::write_new(&config.rendezvous, listener.endpoint(), project.clone())
                .map_err(|error| error.to_string())?;
        let server = ServerIdentity {
            server_version: crate::SERVER_VERSION.to_owned(),
            protocol_version: PROTOCOL_VERSION.to_owned(),
            session_id: session_id(),
        };
        let handshake = Handshake::new(server.clone(), project, rendezvous.clone())
            .with_methods(config.methods);
        let inner = Arc::new(Inner {
            stop: AtomicBool::new(false),
            server,
            methods: config.methods,
            label: config.label,
            events: config.events,
            liveness: config.liveness,
            admission: Mutex::new(0),
            current: Mutex::new(None),
            pending: Mutex::new(HashMap::new()),
            next_message: AtomicU64::new(1),
            next_generation: AtomicU64::new(1),
            counters: Mutex::new(Counters::default()),
        });
        let accepting = Arc::clone(&inner);
        let acceptor = std::thread::Builder::new()
            .name(format!("godot-mcp-{}-bridge", config.label))
            .spawn(move || accept_loop(&accepting, &listener, &handshake))
            .map_err(|error| error.to_string())?;
        Ok(Arc::new(Self {
            inner,
            rendezvous,
            acceptor: Mutex::new(Some(acceptor)),
        }))
    }

    /// The connection, as the plugin described it.
    pub(crate) fn status(&self) -> BridgeStatus {
        let counters = lock(&self.inner.counters);
        let current = lock(&self.inner.current);
        BridgeStatus {
            connected: current.is_some(),
            plugin_version: current
                .as_ref()
                .map(|session| session.plugin.plugin_version.clone()),
            godot_version: current
                .as_ref()
                .map(|session| session.plugin.godot_version.clone()),
            methods: current
                .as_ref()
                .map(|session| session.methods.iter().cloned().collect())
                .unwrap_or_default(),
            description: current.as_ref().map(|session| session.description.clone()),
            sessions_opened: counters.opened,
            sessions_closed: counters.closed,
            last_close_reason: counters.last_close_reason.clone(),
        }
    }

    /// Keep every editor out while the returned exclusion is held: a headless
    /// writer of the editor's import and UID caches must not run beside an
    /// editor that writes them too. A plugin that dials in meanwhile is closed
    /// and connects on a later dial.
    ///
    /// # Errors
    /// [`EditorConnected`] when an editor is connected already.
    pub(crate) fn exclude_connections(&self) -> Result<ConnectionExclusion, EditorConnected> {
        let mut exclusions = lock(&self.inner.admission);
        if lock(&self.inner.current).is_some() {
            return Err(EditorConnected);
        }
        *exclusions += 1;
        Ok(ConnectionExclusion {
            inner: Arc::clone(&self.inner),
        })
    }

    /// Send one request and wait for its answer, at most `timeout`.
    ///
    /// Blocks the calling thread; async callers run it on a blocking thread.
    pub(crate) fn call(
        &self,
        method: &str,
        class: OperationClass,
        payload: &Value,
        idempotency_id: &str,
        timeout: Duration,
    ) -> Result<BridgeAnswer, BridgeFailure> {
        call(
            &self.inner,
            method,
            class,
            payload,
            idempotency_id,
            timeout,
            &CancelSignal::default(),
        )
    }

    /// [`Self::call`], withdrawing the request from the peer as soon as
    /// `cancellation` fires. The peer's answer is still awaited: a request
    /// withdrawn mid-way answers what it did before it stopped.
    pub(crate) fn call_cancellable(
        &self,
        method: &str,
        class: OperationClass,
        payload: &Value,
        idempotency_id: &str,
        timeout: Duration,
        cancellation: &CancelSignal,
    ) -> Result<BridgeAnswer, BridgeFailure> {
        call(
            &self.inner,
            method,
            class,
            payload,
            idempotency_id,
            timeout,
            cancellation,
        )
    }

    /// Stop serving: tell the plugin, close the session and remove the
    /// rendezvous file. Idempotent.
    pub(crate) fn stop(&self) {
        self.inner.stop.store(true, Ordering::SeqCst);
        if let Some(session) = lock(&self.inner.current).take() {
            let shutdown = Envelope::shutdown(
                &self.inner.server,
                &format!("shutdown-{}", session.generation),
                "the godot-mcp server is stopping",
            );
            let _ = session.writer.send(&shutdown);
            let _ = session.writer.close(1001, "server is stopping");
        }
        if let Some(acceptor) = lock(&self.acceptor).take() {
            let _ = acceptor.join();
        }
        fail_pending(&self.inner, None);
        let _ = self.rendezvous.remove();
    }
}

impl Drop for EditorBridge {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The editor's rendezvous is written by the one process that holds the
/// project's lease, at its startup, so one already on disk was left by a server
/// that stopped without its cleanup, and no live bridge reads it. Unix replaces
/// it in one atomic step. Windows cannot replace a file under the rendezvous CAS
/// contract (`godot_mcp_bridge::protocol::rendezvous`), so the stale file is
/// removed first and the new one is then created.
#[cfg(windows)]
fn remove_stale_rendezvous(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => {
            tracing::info!(
                event = "stale_rendezvous_removed",
                "the rendezvous a stopped server left was removed before this one was written"
            );
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "the rendezvous a stopped server left cannot be removed: {error}"
        )),
    }
}

#[cfg(not(windows))]
#[allow(
    clippy::unnecessary_wraps,
    reason = "the same signature as the Windows removal, which can fail"
)]
const fn remove_stale_rendezvous(_path: &Path) -> Result<(), String> {
    Ok(())
}

fn session_id() -> String {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    digest.update(std::process::id().to_le_bytes());
    digest.update(now_unix_ms().to_le_bytes());
    digest.update(format!("{:?}", Instant::now()).as_bytes());
    let hex: String = digest
        .finalize()
        .iter()
        .take(12)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("bridge-{hex}")
}

fn call(
    inner: &Arc<Inner>,
    method: &str,
    class: OperationClass,
    payload: &Value,
    idempotency_id: &str,
    timeout: Duration,
    cancellation: &CancelSignal,
) -> Result<BridgeAnswer, BridgeFailure> {
    let (writer, generation) = {
        let current = lock(&inner.current);
        let session = current.as_ref().ok_or(BridgeFailure::NotConnected)?;
        if !session.methods.contains(method) {
            return Err(BridgeFailure::Unsupported(method.to_owned()));
        }
        (session.writer.clone(), session.generation)
    };
    send_request(
        inner,
        &writer,
        generation,
        method,
        class,
        payload,
        idempotency_id,
        timeout,
        cancellation,
    )
}

#[allow(clippy::too_many_arguments)]
fn send_request(
    inner: &Arc<Inner>,
    writer: &SessionWriter,
    generation: u64,
    method: &str,
    class: OperationClass,
    payload: &Value,
    idempotency_id: &str,
    timeout: Duration,
    cancellation: &CancelSignal,
) -> Result<BridgeAnswer, BridgeFailure> {
    let message_id = format!(
        "request-{}",
        inner.next_message.fetch_add(1, Ordering::Relaxed)
    );
    let deadline_ms =
        now_unix_ms().saturating_add(u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX));
    let envelope = Envelope::request(
        &inner.server,
        &message_id,
        idempotency_id,
        method,
        class,
        Deadline::from_unix_ms(deadline_ms),
        payload,
    );
    let (sender, receiver) = mpsc::sync_channel(1);
    let deadline = Instant::now() + timeout + ANSWER_GRACE;
    lock(&inner.pending).insert(message_id.clone(), Pending { generation, sender });
    if writer.send(&envelope).is_err() {
        // A write that failed delivered no complete frame: nothing can run.
        lock(&inner.pending).remove(&message_id);
        return Err(BridgeFailure::NotConnected);
    }
    let withdraw = || {
        let _ = writer.send(&Envelope::cancel(
            &inner.server,
            &format!("cancel-{message_id}"),
            &message_id,
        ));
    };
    let mut withdrawn = false;
    loop {
        let now = Instant::now();
        if now >= deadline {
            lock(&inner.pending).remove(&message_id);
            withdraw();
            return Err(BridgeFailure::TimedOut);
        }
        match receiver.recv_timeout((deadline - now).min(CANCEL_POLL)) {
            Ok(answer) => return answer,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if !withdrawn && cancellation.is_cancelled() {
                    withdrawn = true;
                    withdraw();
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                lock(&inner.pending).remove(&message_id);
                return Err(BridgeFailure::Lost);
            }
        }
    }
}

/// Fail every caller waiting on `generation` (every caller, when `None`).
fn fail_pending(inner: &Inner, generation: Option<u64>) {
    let mut pending = lock(&inner.pending);
    let lost: Vec<String> = pending
        .iter()
        .filter(|(_, waiting)| generation.is_none_or(|ended| waiting.generation == ended))
        .map(|(id, _)| id.clone())
        .collect();
    for id in lost {
        if let Some(waiting) = pending.remove(&id) {
            let _ = waiting.sender.try_send(Err(BridgeFailure::Lost));
        }
    }
}

fn accept_loop(inner: &Arc<Inner>, listener: &BridgeListener, handshake: &Handshake) {
    while !inner.stop.load(Ordering::SeqCst) {
        let session = match listener.accept_authenticated_within(handshake, ACCEPT_WAIT) {
            Ok(Some(session)) => session,
            Ok(None) => continue,
            Err(error) => {
                tracing::warn!(
                    event = inner.events.refused,
                    reason = %error,
                    "a bridge connection was refused"
                );
                continue;
            }
        };
        let plugin = session.plugin().clone();
        let Ok((reader, writer)) = session.split() else {
            continue;
        };
        let generation = inner.next_generation.fetch_add(1, Ordering::Relaxed);
        let reading = Arc::clone(inner);
        let spawned = std::thread::Builder::new()
            .name(format!("godot-mcp-{}-session-{generation}", inner.label))
            .spawn({
                let writer = writer.clone();
                move || read_loop(&reading, reader, &writer, generation)
            });
        if spawned.is_err() {
            let _ = writer.close(1011, "the server could not serve the session");
            continue;
        }
        // The plugin says which methods it implements before anything is sent.
        let described = send_request(
            inner,
            &writer,
            generation,
            "bridge.describe_session",
            OperationClass::Read,
            &json!({}),
            &format!("describe-{generation}"),
            DESCRIBE_TIMEOUT,
            &CancelSignal::default(),
        );
        let description = match described {
            Ok(BridgeAnswer::Result { payload, .. }) => payload,
            other => {
                tracing::warn!(
                    event = inner.events.undescribed,
                    outcome = ?other,
                    "a bridge peer did not describe its session"
                );
                let _ = writer.close(1002, "the session was not described");
                continue;
            }
        };
        let methods: BTreeSet<String> = description
            .get("methods")
            .and_then(Value::as_array)
            .map(|methods| {
                methods
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|method| inner.methods.contains(method))
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        // An exclusion is checked and the session installed under one lock,
        // so no exclusion is granted between the check and the install.
        let admission = lock(&inner.admission);
        if *admission > 0 {
            drop(admission);
            tracing::info!(
                event = inner.events.refused,
                reason = "excluded",
                "a bridge peer was kept out while a headless cache writer ran"
            );
            let _ = writer.close(
                1013,
                "the server is running a headless import or UID update; retry shortly",
            );
            continue;
        }
        let replaced = lock(&inner.current).replace(Session {
            generation,
            writer: writer.clone(),
            plugin,
            description,
            methods,
        });
        drop(admission);
        {
            let mut counters = lock(&inner.counters);
            counters.opened += 1;
            counters.served.insert(generation);
        }
        if let Some(previous) = replaced {
            // A newer authenticated session supersedes the older one.
            let _ = previous.writer.close(1000, "superseded by a newer session");
        }
        tracing::info!(
            event = inner.events.connected,
            generation,
            "a bridge peer connected"
        );
    }
}

fn read_loop(
    inner: &Arc<Inner>,
    mut reader: SessionReader,
    writer: &SessionWriter,
    generation: u64,
) {
    let mut last_heard = Instant::now();
    let mut last_beat = Instant::now();
    let reason = loop {
        if inner.stop.load(Ordering::SeqCst) {
            break "the server is stopping".to_owned();
        }
        match reader.receive_within(READ_WAIT) {
            Ok(Some(envelope)) => {
                last_heard = Instant::now();
                if let Some(reason) = deliver(inner, &envelope) {
                    let _ = writer.close(1002, &reason);
                    break reason;
                }
            }
            Ok(None) => {}
            Err(error) => break error.to_string(),
        }
        if last_beat.elapsed() >= inner.liveness.heartbeat {
            last_beat = Instant::now();
            let beat = Envelope::heartbeat(
                &inner.server,
                &format!(
                    "heartbeat-{generation}-{}",
                    inner.next_message.fetch_add(1, Ordering::Relaxed)
                ),
                now_unix_ms(),
            );
            if writer.send(&beat).is_err() {
                break "the connection closed".to_owned();
            }
        }
        if last_heard.elapsed() >= inner.liveness.silence && !running_a_request(inner, generation) {
            let _ = writer.close(1001, "the plugin went silent");
            break "the plugin went silent".to_owned();
        }
    };
    let _ = writer.close(1000, "session ended");
    {
        let mut current = lock(&inner.current);
        if current
            .as_ref()
            .is_some_and(|session| session.generation == generation)
        {
            *current = None;
        }
    }
    {
        let mut counters = lock(&inner.counters);
        if counters.served.remove(&generation) {
            counters.closed += 1;
        }
        counters.last_close_reason = Some(reason.clone());
    }
    fail_pending(inner, Some(generation));
    tracing::info!(
        event = inner.events.disconnected,
        generation,
        reason = %reason,
        "a bridge session ended"
    );
}

/// Whether the peer of session `generation` holds a request whose caller still
/// waits for it. A peer runs requests on its main thread, which answers no
/// heartbeat until the request is done, so its silence then is not its absence;
/// once every such caller has given up, the silence limit applies again. A
/// caller gives up by removing its request, so the request's own timeout is
/// always what it reports, even when it wakes after its deadline.
fn running_a_request(inner: &Inner, generation: u64) -> bool {
    lock(&inner.pending)
        .values()
        .any(|waiting| waiting.generation == generation)
}

/// Hand one plugin message to whoever waits for it. Returns a reason to close
/// the session when the message has no place in the protocol.
fn deliver(inner: &Inner, envelope: &Envelope) -> Option<String> {
    match envelope.kind() {
        MessageKind::Result | MessageKind::Error => {
            let correlation = envelope.correlation_id()?.to_owned();
            let waiting = lock(&inner.pending).remove(&correlation);
            if let Some(Pending { sender, .. }) = waiting {
                let answer = if envelope.kind() == MessageKind::Result {
                    BridgeAnswer::Result {
                        status: envelope
                            .mutation_status()
                            .unwrap_or(MutationStatus::NotStarted),
                        payload: envelope.payload().cloned().unwrap_or_else(|| json!({})),
                    }
                } else {
                    let body = envelope.error_body();
                    BridgeAnswer::Error {
                        kind: body.map_or(ErrorKind::Internal, |body| body.kind),
                        message: body.map_or_else(String::new, |body| body.message.clone()),
                        status: envelope.mutation_status(),
                    }
                };
                let _ = sender.try_send(Ok(answer));
            }
            None
        }
        MessageKind::Heartbeat | MessageKind::Event | MessageKind::Progress => None,
        other => Some(format!("a plugin may not send {}", other.as_str())),
    }
}

#[cfg(all(test, feature = "test-hooks"))]
mod tests {
    use super::*;
    use crate::test_support::fake_editor_plugin::{FakeAnswer, FakeEditorPlugin, FakePluginScript};

    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "godot-mcp-editor-bridge-{label}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("scratch directory");
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn project() -> ProjectClaim {
        ProjectClaim {
            object_identity: format!("project_{}", "b".repeat(64)),
            root_object: "unix:dev=1,ino=20".to_owned(),
            file_object: "unix:dev=1,ino=21".to_owned(),
            generation: 0,
        }
    }

    fn wait_until(what: &str, condition: impl Fn() -> bool) {
        let started = Instant::now();
        while !condition() {
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "timed out waiting for {what}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The real `godot_mcp` plugin in real headless editors, when
    /// `GODOT_MCP_TEST_EDITORS` lists editor binaries (comma-separated).
    #[test]
    fn the_real_plugin_serves_reads_and_one_undoable_batch() {
        let Ok(binaries) = std::env::var("GODOT_MCP_TEST_EDITORS") else {
            eprintln!("GODOT_MCP_TEST_EDITORS is unset; the live-editor check did not run");
            return;
        };
        let addon =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../godot-mcp-bridge/addon/godot_mcp");
        for binary in binaries.split(',').filter(|binary| !binary.is_empty()) {
            let scratch = Scratch::new("live");
            let project_root = scratch.0.join("game");
            std::fs::create_dir_all(project_root.join("addons/godot_mcp")).expect("project");
            for entry in std::fs::read_dir(&addon).expect("addon") {
                let entry = entry.expect("entry");
                std::fs::copy(
                    entry.path(),
                    project_root
                        .join("addons/godot_mcp")
                        .join(entry.file_name()),
                )
                .expect("copy addon");
            }
            std::fs::write(
                project_root.join("project.godot"),
                "config_version=5\n\n[application]\n\nconfig/name=\"live\"\n\n[editor_plugins]\n\n\
                 enabled=PackedStringArray(\"res://addons/godot_mcp/plugin.cfg\")\n",
            )
            .expect("project.godot");
            std::fs::write(
                project_root.join("main.tscn"),
                "[gd_scene format=3]\n\n[node name=\"Main\" type=\"Node2D\"]\n\n\
                 [node name=\"Player\" type=\"Node2D\" parent=\".\"]\n",
            )
            .expect("scene");
            let state = project_root.join(".godot-mcp");
            std::fs::create_dir_all(&state).expect("state");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700))
                    .expect("mode");
            }
            let bridge = EditorBridge::start(&state, project()).expect("start");
            let home = scratch.0.join("home");
            std::fs::create_dir_all(&home).expect("home");
            let log_path = scratch.0.join("editor.log");
            let log = std::fs::File::create(&log_path).expect("log");
            let mut editor = std::process::Command::new(binary)
                .args(["--headless", "--editor", "--verbose", "--path"])
                .arg(&project_root)
                .env("HOME", &home)
                .stdout(log.try_clone().expect("log"))
                .stderr(log)
                .spawn()
                .expect("launch the editor");
            let started = Instant::now();
            while !bridge.status().connected {
                if started.elapsed() > Duration::from_secs(90) {
                    let _ = editor.kill();
                    let output = std::fs::read_to_string(&log_path).unwrap_or_default();
                    panic!(
                        "{binary}: the plugin never connected ({:?}):\n{output}",
                        bridge.status()
                    );
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            let call = |method: &str, class: OperationClass, payload: Value| {
                bridge
                    .call(method, class, &payload, "live", Duration::from_secs(30))
                    .unwrap_or_else(|failure| panic!("{binary}: {method}: {failure:?}"))
            };
            let payload = |answer: BridgeAnswer| match answer {
                BridgeAnswer::Result { payload, .. } => payload,
                other => panic!("{binary}: expected a result, got {other:?}"),
            };
            let opened = payload(call(
                "editor.scene.open",
                OperationClass::Mutation,
                json!({ "path": "res://main.tscn" }),
            ));
            assert_eq!(
                opened["edited_scene"], "res://main.tscn",
                "{binary}: {opened}"
            );
            let before = payload(call("editor.scene.tree", OperationClass::Read, json!({})));
            let state_after_read = payload(call("editor.state", OperationClass::Read, json!({})));
            assert_eq!(
                state_after_read["history"]["has_undo"],
                json!(false),
                "{binary}: a read adds no history"
            );
            let bytes = std::fs::read(project_root.join("main.tscn")).expect("scene bytes");
            let sha = {
                use sha2::{Digest, Sha256};
                Sha256::digest(&bytes)
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            };
            let batch = payload(call(
                "editor.scene.batch",
                OperationClass::Mutation,
                json!({
                    "path": "res://main.tscn",
                    "expected_sha256": sha,
                    "handles": {},
                    "operations": [
                        { "op": "set_property", "node": "Player", "property": "position",
                          "value": { "type": "vector2", "value": { "x": 12.0, "y": 34.0 } } },
                        { "op": "add_group", "node": "Player", "group": "players" }
                    ],
                    "label": "godot-mcp: live test"
                }),
            ));
            assert_eq!(batch["saved"], json!(true), "{binary}: {batch}");
            let saved =
                std::fs::read_to_string(project_root.join("main.tscn")).expect("saved scene");
            assert!(
                saved.contains("position = Vector2(12, 34)") && saved.contains("players"),
                "{binary}: {saved}"
            );
            let undone = payload(call(
                "editor.history.undo",
                OperationClass::Mutation,
                json!({}),
            ));
            assert_eq!(
                undone["digest"], before["digest"],
                "{binary}: undo restores the pre-state"
            );
            let redone = payload(call(
                "editor.history.redo",
                OperationClass::Mutation,
                json!({}),
            ));
            assert_eq!(
                redone["digest"], batch["digest"],
                "{binary}: redo restores the post-state"
            );
            bridge.stop();
            let _ = editor.kill();
            let _ = editor.wait();
        }
    }

    /// A killed server never removes its rendezvous. The next server holds the
    /// project's lease, so no live bridge can still be reading that file: it
    /// starts over it with its own endpoint and secret, on every platform.
    #[test]
    fn a_rendezvous_a_killed_server_left_does_not_stop_the_next_one() {
        let scratch = Scratch::new("stale");
        let path = scratch.0.join(RENDEZVOUS_FILE);
        let first = EditorBridge::start(&scratch.0, project()).expect("start");
        let left = std::fs::read(&path).expect("the first rendezvous");
        // What a killed server leaves: its file, and none of its cleanup.
        std::mem::forget(first);
        let second = EditorBridge::start(&scratch.0, project())
            .expect("the next server starts over a stale rendezvous");
        let written = std::fs::read(&path).expect("the second rendezvous");
        assert_ne!(written, left, "the next server writes its own secret");
        second.stop();
        assert!(!path.exists(), "stop removes the rendezvous");
    }

    #[test]
    fn nothing_is_sent_without_a_session() {
        let scratch = Scratch::new("idle");
        let bridge = EditorBridge::start(&scratch.0, project()).expect("start");
        assert!(scratch.0.join(RENDEZVOUS_FILE).is_file());
        assert!(!bridge.status().connected);
        assert_eq!(
            bridge.call(
                "editor.state",
                OperationClass::Read,
                &json!({}),
                "id-1",
                Duration::from_secs(1)
            ),
            Err(BridgeFailure::NotConnected)
        );
        bridge.stop();
        assert!(
            !scratch.0.join(RENDEZVOUS_FILE).exists(),
            "stop removes the rendezvous"
        );
    }

    /// While a headless writer of the editor's caches holds an exclusion, no
    /// editor session is installed: a plugin that dials in is closed after it
    /// described itself, and one that dials after the exclusion is released
    /// connects. A connected editor makes an exclusion impossible.
    #[test]
    fn an_exclusion_keeps_editors_out_until_it_is_released() {
        let scratch = Scratch::new("exclusion");
        let bridge = EditorBridge::start(&scratch.0, project()).expect("start");
        let exclusion = bridge
            .exclude_connections()
            .expect("no editor is connected");
        let refused = FakeEditorPlugin::spawn(
            &scratch.0.join(RENDEZVOUS_FILE),
            FakePluginScript::implementing(&["editor.state"]),
        );
        wait_until("the excluded plugin to be described", || {
            refused
                .received()
                .iter()
                .any(|(method, _)| method == "bridge.describe_session")
        });
        refused
            .join()
            .expect("the excluded session was closed by the server");
        let status = bridge.status();
        assert!(!status.connected, "{status:?}");
        assert_eq!(status.sessions_opened, 0, "{status:?}");

        drop(exclusion);
        let admitted = FakeEditorPlugin::spawn(
            &scratch.0.join(RENDEZVOUS_FILE),
            FakePluginScript::implementing(&["editor.state"]),
        );
        wait_until("the plugin to connect", || bridge.status().connected);
        assert!(
            bridge.exclude_connections().is_err(),
            "no exclusion is held while an editor is connected"
        );
        bridge.stop();
        admitted.join().expect("the plugin saw a clean shutdown");
    }

    #[test]
    fn a_described_plugin_answers_only_the_methods_it_reported() {
        let scratch = Scratch::new("answers");
        let bridge = EditorBridge::start(&scratch.0, project()).expect("start");
        let plugin = FakeEditorPlugin::spawn(
            &scratch.0.join(RENDEZVOUS_FILE),
            FakePluginScript::implementing(&["editor.state", "not.a.bridge.method"]).answering(
                "editor.state",
                FakeAnswer::Payload(json!({ "open_scenes": [] })),
            ),
        );
        wait_until("the plugin to connect", || bridge.status().connected);
        let status = bridge.status();
        assert_eq!(
            status.methods,
            vec!["editor.state".to_owned()],
            "{status:?}"
        );
        assert_eq!(status.plugin_version.as_deref(), Some("0.2.0"));

        let answer = bridge
            .call(
                "editor.state",
                OperationClass::Read,
                &json!({}),
                "id-2",
                Duration::from_secs(5),
            )
            .expect("answered");
        assert_eq!(
            answer,
            BridgeAnswer::Result {
                status: MutationStatus::NotStarted,
                payload: json!({ "open_scenes": [] })
            }
        );
        assert_eq!(
            bridge.call(
                "editor.scene.tree",
                OperationClass::Read,
                &json!({}),
                "id-3",
                Duration::from_secs(1)
            ),
            Err(BridgeFailure::Unsupported("editor.scene.tree".to_owned()))
        );
        let methods: Vec<String> = plugin
            .received()
            .into_iter()
            .map(|(method, _)| method)
            .collect();
        assert_eq!(methods, vec!["bridge.describe_session", "editor.state"]);
        bridge.stop();
        plugin.join().expect("the plugin saw a clean shutdown");
    }

    #[test]
    fn a_mutation_whose_connection_drops_is_lost_and_never_sent_again() {
        let scratch = Scratch::new("lost");
        let bridge = EditorBridge::start(&scratch.0, project()).expect("start");
        let first = FakeEditorPlugin::spawn(
            &scratch.0.join(RENDEZVOUS_FILE),
            FakePluginScript::implementing(&["editor.scene.batch"])
                .answering("editor.scene.batch", FakeAnswer::Disconnect),
        );
        wait_until("the first plugin to connect", || bridge.status().connected);
        let failure = bridge
            .call(
                "editor.scene.batch",
                OperationClass::Mutation,
                &json!({ "path": "res://main.tscn" }),
                "batch-1",
                Duration::from_secs(10),
            )
            .expect_err("no answer came back");
        assert_eq!(failure, BridgeFailure::Lost);
        assert!(failure.dispatched(), "the request reached the plugin");
        first.join().expect("the first plugin ended");
        wait_until("the session to end", || !bridge.status().connected);

        // The editor comes back: it is described afresh and sent nothing else.
        let second = FakeEditorPlugin::spawn(
            &scratch.0.join(RENDEZVOUS_FILE),
            FakePluginScript::implementing(&["editor.scene.batch"]),
        );
        wait_until("the second plugin to connect", || bridge.status().connected);
        let methods: Vec<String> = second
            .received()
            .into_iter()
            .map(|(method, _)| method)
            .collect();
        assert_eq!(
            methods,
            vec!["bridge.describe_session"],
            "no replay after a reconnect"
        );
        let status = bridge.status();
        assert_eq!((status.sessions_opened, status.sessions_closed), (2, 1));
        bridge.stop();
        second.join().expect("the second plugin ended");
    }

    #[test]
    fn an_unanswered_request_times_out() {
        let scratch = Scratch::new("silent");
        let bridge = EditorBridge::start(&scratch.0, project()).expect("start");
        let plugin = FakeEditorPlugin::spawn(
            &scratch.0.join(RENDEZVOUS_FILE),
            FakePluginScript::implementing(&["editor.state"])
                .answering("editor.state", FakeAnswer::Ignore),
        );
        wait_until("the plugin to connect", || bridge.status().connected);
        assert_eq!(
            bridge.call(
                "editor.state",
                OperationClass::Read,
                &json!({}),
                "id-4",
                Duration::from_millis(200)
            ),
            Err(BridgeFailure::TimedOut)
        );
        bridge.stop();
        plugin.join().expect("the plugin ended");
    }

    /// A bridge that takes a peer for gone after 300 ms of silence.
    fn impatient(scratch: &Scratch) -> Arc<EditorBridge> {
        EditorBridge::start_with(
            BridgeConfig::editor(&scratch.0)
                .with_liveness(Duration::from_millis(50), Duration::from_millis(300)),
            project(),
        )
        .expect("start")
    }

    #[test]
    fn a_silent_peer_running_no_request_is_closed() {
        let scratch = Scratch::new("quiet");
        let bridge = impatient(&scratch);
        let mut script = FakePluginScript::implementing(&["editor.state"]);
        script.answer_heartbeats = false;
        let plugin = FakeEditorPlugin::spawn(&scratch.0.join(RENDEZVOUS_FILE), script);
        wait_until("the plugin to connect", || {
            bridge.status().sessions_opened == 1
        });
        wait_until("the silent session to close", || {
            bridge.status().sessions_closed == 1
        });
        assert_eq!(
            bridge.status().last_close_reason.as_deref(),
            Some("the plugin went silent")
        );
        bridge.stop();
        plugin.join().expect("the plugin ended");
    }

    #[test]
    fn a_peer_busy_with_a_request_may_stay_silent_until_its_deadline() {
        let scratch = Scratch::new("busy");
        let bridge = impatient(&scratch);
        let plugin = FakeEditorPlugin::spawn(
            &scratch.0.join(RENDEZVOUS_FILE),
            FakePluginScript::implementing(&["editor.state"]).answering(
                "editor.state",
                FakeAnswer::PayloadAfter {
                    delay: Duration::from_millis(1500),
                    payload: json!({ "open_scenes": [] }),
                },
            ),
        );
        wait_until("the plugin to connect", || bridge.status().connected);
        let answer = bridge.call(
            "editor.state",
            OperationClass::Read,
            &json!({}),
            "busy-1",
            Duration::from_secs(10),
        );
        assert_eq!(
            answer,
            Ok(BridgeAnswer::Result {
                status: MutationStatus::NotStarted,
                payload: json!({ "open_scenes": [] }),
            }),
            "five times the silence limit, but within the request's deadline"
        );
        assert_eq!(bridge.status().sessions_closed, 0);
        bridge.stop();
        plugin.join().expect("the plugin ended");
    }

    #[test]
    fn a_peer_silent_past_its_request_deadline_is_closed() {
        let scratch = Scratch::new("overdue");
        let bridge = impatient(&scratch);
        let plugin = FakeEditorPlugin::spawn(
            &scratch.0.join(RENDEZVOUS_FILE),
            FakePluginScript::implementing(&["editor.state"]).answering(
                "editor.state",
                FakeAnswer::PayloadAfter {
                    delay: Duration::from_secs(4),
                    payload: json!({}),
                },
            ),
        );
        wait_until("the plugin to connect", || bridge.status().connected);
        assert_eq!(
            bridge.call(
                "editor.state",
                OperationClass::Read,
                &json!({}),
                "overdue-1",
                Duration::from_millis(100),
            ),
            Err(BridgeFailure::TimedOut)
        );
        wait_until("the silent session to close", || {
            bridge.status().sessions_closed == 1
        });
        assert_eq!(
            bridge.status().last_close_reason.as_deref(),
            Some("the plugin went silent")
        );
        bridge.stop();
        // The plugin's late answer finds the connection closed.
        let _ = plugin.join();
    }

    /// A caller's deadline and the reader's silence check fall on the same
    /// instant, so the silence rule must not apply again until the caller
    /// itself has given up on its request: otherwise a caller woken late
    /// receives the closed session's `Lost` instead of its own timeout.
    #[test]
    fn a_request_holds_the_silence_rule_until_its_caller_gives_up() {
        let scratch = Scratch::new("give-up");
        let bridge = impatient(&scratch);
        let (sender, _receiver) = mpsc::sync_channel(1);
        lock(&bridge.inner.pending).insert(
            "request-late".to_owned(),
            Pending {
                generation: 7,
                sender,
            },
        );
        assert!(
            running_a_request(&bridge.inner, 7),
            "the caller still waits, past the instant its deadline names"
        );
        lock(&bridge.inner.pending).remove("request-late");
        assert!(!running_a_request(&bridge.inner, 7), "the caller gave up");
        bridge.stop();
    }

    #[test]
    fn a_newer_session_supersedes_the_older_one() {
        let scratch = Scratch::new("supersede");
        let bridge = EditorBridge::start(&scratch.0, project()).expect("start");
        let first = FakeEditorPlugin::spawn(
            &scratch.0.join(RENDEZVOUS_FILE),
            FakePluginScript::implementing(&["editor.state"]),
        );
        wait_until("the first plugin", || bridge.status().sessions_opened == 1);
        let second = FakeEditorPlugin::spawn(
            &scratch.0.join(RENDEZVOUS_FILE),
            FakePluginScript::implementing(&["editor.state", "editor.scene.tree"]),
        );
        wait_until("the second plugin", || bridge.status().sessions_opened == 2);
        first.join().expect("the superseded plugin was closed");
        wait_until("the old session to close", || {
            bridge.status().sessions_closed == 1
        });
        let status = bridge.status();
        assert!(status.connected);
        assert_eq!(
            status.methods,
            vec!["editor.scene.tree".to_owned(), "editor.state".to_owned()]
        );
        bridge.stop();
        second.join().expect("the second plugin ended");
    }
}
