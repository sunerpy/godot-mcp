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
//!
//! The editor bridge also tells a plugin that lists [`SERVER_STATUS_METHOD`]
//! what this server serves, for the add-on's panel: once the session is
//! described, and again whenever what it would say changes. The status comes
//! from the [`StatusSource`] the server attaches; the runtime probe's bridge
//! never sends it.
//!
//! An answer can still come after the server stopped waiting for it. Once a
//! [`LateAnswerSink`] is attached, the bridge keeps every mutation request sent
//! for a transaction that ended [`BridgeFailure::Lost`] or
//! [`BridgeFailure::TimedOut`], and hands the sink each [`LateAnswer`] that it
//! can bind exactly to one of them: a `result` or `error` that arrives later on
//! the same session, or a [`LATE_ANSWER_EVENT`] the peer sends on a later one.
//! A report about a request another server sent is handed on unverified, to be
//! bound to its transaction by the journal; one that comes before the sink is
//! attached waits for it. The bridge itself never resends a request and never
//! answers a caller twice.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
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
use crate::journal::EditorProcess;

/// Name of the rendezvous file inside the project's private state root. The
/// plugin reads it as `res://.godot-mcp/editor-bridge.rendezvous`.
pub(crate) const RENDEZVOUS_FILE: &str = "editor-bridge.rendezvous";

/// The bridge methods this server may send, as advertised in `capabilities`.
pub(crate) const BRIDGE_METHODS: &[&str] = &[
    "editor.addon.reload",
    "editor.camera.read",
    "editor.camera.set",
    "editor.capture.chunk",
    "editor.capture.viewport",
    "editor.dialog.respond",
    "editor.dialogs.read",
    "editor.filesystem.scan",
    "editor.history.redo",
    "editor.history.undo",
    "editor.log.read",
    "editor.main_screen.set",
    "editor.node",
    "editor.performance.read",
    "editor.play.start",
    "editor.play.stop",
    "editor.preferences.read",
    "editor.preferences.set",
    "editor.preview.resource",
    "editor.restart",
    "editor.scene.batch",
    "editor.scene.close",
    "editor.scene.edit",
    "editor.scene.open",
    "editor.scene.read",
    "editor.scene.save",
    "editor.scene.tree",
    "editor.script.close",
    "editor.script.open",
    "editor.selection.set",
    "editor.settings.apply",
    "editor.shortcuts.read",
    "editor.shortcuts.set",
    "editor.state",
    "editor.types.class",
    "editor.types.search",
];

/// The bridge method that runs caller-written GDScript in the editor
/// (ADR 0012). A server offers it only when its operator turned script
/// execution on, and the add-on runs it only in a session whose server
/// offered it.
pub(crate) const SCRIPT_RUN_METHOD: &str = "editor.script.run";

/// The bridge method that restarts the editor. While a restart is awaited
/// ([`EditorBridge::begin_restart`]) it is the only mutation the bridge sends.
pub(crate) const RESTART_METHOD: &str = "editor.restart";

/// [`BRIDGE_METHODS`] and [`SCRIPT_RUN_METHOD`]: what a server whose operator
/// turned script execution on may send.
pub(crate) const BRIDGE_METHODS_WITH_SCRIPTS: &[&str] = &[
    "editor.addon.reload",
    "editor.camera.read",
    "editor.camera.set",
    "editor.capture.chunk",
    "editor.capture.viewport",
    "editor.dialog.respond",
    "editor.dialogs.read",
    "editor.filesystem.scan",
    "editor.history.redo",
    "editor.history.undo",
    "editor.log.read",
    "editor.main_screen.set",
    "editor.node",
    "editor.performance.read",
    "editor.play.start",
    "editor.play.stop",
    "editor.preferences.read",
    "editor.preferences.set",
    "editor.preview.resource",
    "editor.restart",
    "editor.scene.batch",
    "editor.scene.close",
    "editor.scene.edit",
    "editor.scene.open",
    "editor.scene.read",
    "editor.scene.save",
    "editor.scene.tree",
    "editor.script.close",
    "editor.script.open",
    SCRIPT_RUN_METHOD,
    "editor.selection.set",
    "editor.settings.apply",
    "editor.shortcuts.read",
    "editor.shortcuts.set",
    "editor.state",
    "editor.types.class",
    "editor.types.search",
];

/// The bridge method that tells a connected editor what this server serves:
/// its version, profile and transport, and the profile's tools with what each
/// needs and whether that is available now. A `control` request, sent only to
/// an editor plugin that lists it; the add-on shows it in its panel.
pub(crate) const SERVER_STATUS_METHOD: &str = "bridge.server_status";

/// What the server tells a connected editor about itself, sampled each time:
/// `None` when it cannot say right now.
pub(crate) type StatusSource = Arc<dyn Fn() -> Option<Value> + Send + Sync>;

/// How often a session that takes the server status samples it again.
const STATUS_SAMPLE: Duration = Duration::from_secs(10);
/// How long the plugin has to take one status.
const STATUS_DEADLINE: Duration = Duration::from_secs(30);

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
    /// Whether a peer that lists [`SERVER_STATUS_METHOD`] is sent the server
    /// status, and how often it is sampled again.
    status: Option<Duration>,
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
    /// This bridge, also offering [`SCRIPT_RUN_METHOD`] when `scripts`.
    pub(crate) fn allowing_scripts(mut self, scripts: bool) -> Self {
        if scripts {
            self.methods = BRIDGE_METHODS_WITH_SCRIPTS;
        }
        self
    }

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
            status: Some(STATUS_SAMPLE),
        }
    }

    /// One managed game session's runtime-probe bridge. It never sends the
    /// server status, whatever the probe lists.
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
            status: None,
        }
    }

    /// Every method this bridge may send, as its `capabilities` name them
    /// beside the base methods.
    pub(crate) fn advertised(&self) -> Vec<&'static str> {
        let mut methods = self.methods.to_vec();
        if self.status.is_some() {
            methods.push(SERVER_STATUS_METHOD);
        }
        methods.sort_unstable();
        methods
    }

    /// This bridge, sending heartbeats every `heartbeat` and closing a peer
    /// silent for `silence`.
    #[cfg(test)]
    pub(crate) const fn with_liveness(mut self, heartbeat: Duration, silence: Duration) -> Self {
        self.liveness = Liveness { heartbeat, silence };
        self
    }

    /// This bridge, sampling the server status every `sample` for a peer
    /// that takes it.
    #[cfg(test)]
    pub(crate) const fn with_status_sample(mut self, sample: Duration) -> Self {
        if self.status.is_some() {
            self.status = Some(sample);
        }
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

/// The event a peer sends, once a later session is described, for each
/// mutation it answered while no server was waiting for the answer: the
/// session had ended, or the request's deadline had passed.
pub(crate) const LATE_ANSWER_EVENT: &str = "mutation.late_answer";
/// How many mutation requests the bridge keeps after it stopped waiting for
/// their answers; the oldest is forgotten first.
const MAX_ABANDONED: usize = 64;
/// The longest error message a late answer keeps, as the add-on caps it.
const MAX_LATE_MESSAGE_CHARS: usize = 2048;
/// How many reported late answers the bridge keeps while no sink is attached
/// yet: the bridge listens before the server attaches one, and a plugin may
/// connect and report in between. The add-on keeps at most this many.
const MAX_UNCLAIMED_LATE: usize = 16;

/// Where late mutation answers go: the sink, once the server attached it, and
/// until then the answers reported meanwhile.
#[derive(Default)]
struct LateAnswers {
    sink: Option<LateAnswerSink>,
    unclaimed: VecDeque<LateAnswer>,
}

/// One mutation request, sent for a transaction, that the bridge stopped
/// waiting for.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Abandoned {
    /// The session generation it was sent on.
    generation: u64,
    message_id: String,
    idempotency_id: String,
    method: String,
    /// The transaction its payload named.
    transaction_id: String,
}

/// An answer to a mutation request that came after the server stopped
/// waiting for it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LateAnswer {
    /// The transaction the request was sent for.
    pub(crate) transaction_id: String,
    pub(crate) idempotency_id: String,
    pub(crate) method: String,
    /// Whether this server sent the request and bound the answer to it from
    /// its own record. Otherwise an earlier server sent it, and only the
    /// peer's report names the transaction.
    pub(crate) sent_here: bool,
    pub(crate) answer: BridgeAnswer,
}

/// Where late answers go once the server attached it. It is called on a
/// session's reader thread and must not call back into the bridge.
pub(crate) type LateAnswerSink = Arc<dyn Fn(LateAnswer) + Send + Sync>;

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
    /// The editor is being restarted, so no other change is sent to it until
    /// the restarted editor connected. Nothing was sent.
    Restarting,
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
            Self::Restarting => "the editor is being restarted (editor.restart), so nothing else \
                                 is sent to it until the restarted editor connected; retry then"
                .to_owned(),
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
            Self::Restarting => "the game session's probe is not taking changes".to_owned(),
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

/// While this is held, the bridge awaits a restarted editor
/// ([`EditorBridge::begin_restart`]).
pub(crate) struct RestartGuard {
    inner: Arc<Inner>,
}

impl Drop for RestartGuard {
    fn drop(&mut self) {
        self.inner.restarting.fetch_sub(1, Ordering::SeqCst);
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
    /// The editor process the peer runs in, when the host proved it from the
    /// description ([`super::process::observe`]).
    editor_process: Option<EditorProcess>,
}

/// Whether a peer described by `description` is sent the server status: this
/// bridge sends it, and the peer listed it.
fn takes_status(status: Option<Duration>, description: &Value) -> bool {
    status.is_some()
        && description
            .get("methods")
            .and_then(Value::as_array)
            .is_some_and(|methods| {
                methods
                    .iter()
                    .any(|method| method.as_str() == Some(SERVER_STATUS_METHOD))
            })
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
    /// How often the server status is sampled for a peer that takes it;
    /// `None` for a bridge that never sends it.
    status: Option<Duration>,
    /// Where the server status comes from, once the server attached it.
    status_source: Mutex<Option<StatusSource>>,
    /// How many exclusions are held. While one is, no session is installed.
    /// Taken before `current` wherever both are held.
    admission: Mutex<usize>,
    /// How many restarts are awaited. While one is, the bridge sends no
    /// mutation but the restart and grants no exclusion.
    restarting: AtomicUsize,
    current: Mutex<Option<Session>>,
    /// Waiting callers by request message id.
    pending: Mutex<HashMap<String, Pending>>,
    /// Mutation requests sent for a transaction that the bridge stopped
    /// waiting for, oldest first, at most [`MAX_ABANDONED`]. Kept only once a
    /// late-answer sink is attached.
    abandoned: Mutex<VecDeque<Abandoned>>,
    /// Where late mutation answers go, once the server attached it, and the
    /// answers reported before it did.
    late: Mutex<LateAnswers>,
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
    /// What the bridge keeps of the request if it stops waiting for it.
    late: Option<Abandoned>,
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
    pub(crate) fn start(
        state_root: &Path,
        project: ProjectClaim,
        scripts: bool,
    ) -> Result<Arc<Self>, String> {
        let config = BridgeConfig::editor(state_root).allowing_scripts(scripts);
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
            .with_methods(&config.advertised());
        let inner = Arc::new(Inner {
            stop: AtomicBool::new(false),
            server,
            methods: config.methods,
            label: config.label,
            events: config.events,
            liveness: config.liveness,
            status: config.status,
            status_source: Mutex::new(None),
            admission: Mutex::new(0),
            restarting: AtomicUsize::new(0),
            current: Mutex::new(None),
            pending: Mutex::new(HashMap::new()),
            abandoned: Mutex::new(VecDeque::new()),
            late: Mutex::new(LateAnswers::default()),
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

    /// Tell every editor session that takes the server status what `source`
    /// says, from now on. The server attaches it once its own owners exist,
    /// which is after the bridge started: a session that opened before waits
    /// for it. The source must not hold this bridge, or neither is ever
    /// freed; it reads the server through weak references.
    pub(crate) fn attach_status_source(&self, source: StatusSource) {
        *lock(&self.inner.status_source) = Some(source);
    }

    /// Hand `sink` every late answer the bridge can bind, from now on: an
    /// answer to a mutation request sent for a transaction after the bridge
    /// stopped waiting for it, whether it comes on the same session or is
    /// reported on a later one. Answers reported before the sink was attached
    /// are handed to it now. The sink must not hold this bridge; it reads the
    /// server through weak references.
    pub(crate) fn attach_late_answers(&self, sink: LateAnswerSink) {
        let unclaimed = {
            let mut late = lock(&self.inner.late);
            late.sink = Some(Arc::clone(&sink));
            std::mem::take(&mut late.unclaimed)
        };
        for answer in unclaimed {
            sink(answer);
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
        // An editor this server restarts is running even while its add-on
        // has not connected yet, and writes the caches as it starts.
        if lock(&self.inner.current).is_some() || self.restarting() {
            return Err(EditorConnected);
        }
        *exclusions += 1;
        Ok(ConnectionExclusion {
            inner: Arc::clone(&self.inner),
        })
    }

    /// Hold the bridge for a restart until the returned guard is dropped:
    /// meanwhile it sends no mutation but [`RESTART_METHOD`], so no change is
    /// sent to an editor that is about to exit, and grants no
    /// [`ConnectionExclusion`], so no headless cache writer starts beside the
    /// editor that is starting.
    pub(crate) fn begin_restart(&self) -> RestartGuard {
        self.inner.restarting.fetch_add(1, Ordering::SeqCst);
        RestartGuard {
            inner: Arc::clone(&self.inner),
        }
    }

    /// Whether a restart is awaited.
    pub(crate) fn restarting(&self) -> bool {
        self.inner.restarting.load(Ordering::SeqCst) > 0
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
        .0
    }

    /// [`Self::call`], also answering the editor process of the session the
    /// request was sent on, as that session's description proved it. The
    /// session is chosen and its process read under one lock, so a newer
    /// session that supersedes it meanwhile is never named instead. `None`
    /// when nothing was sent or the session's process was not proven.
    pub(crate) fn call_with_process(
        &self,
        method: &str,
        class: OperationClass,
        payload: &Value,
        idempotency_id: &str,
        timeout: Duration,
    ) -> (Result<BridgeAnswer, BridgeFailure>, Option<EditorProcess>) {
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
        .0
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

/// Send one request on the current session and wait for its answer, with the
/// editor process of the session it was sent on. Nothing is sent, and no
/// process named, when no session takes the request.
fn call(
    inner: &Arc<Inner>,
    method: &str,
    class: OperationClass,
    payload: &Value,
    idempotency_id: &str,
    timeout: Duration,
    cancellation: &CancelSignal,
) -> (Result<BridgeAnswer, BridgeFailure>, Option<EditorProcess>) {
    let selected = {
        let current = lock(&inner.current);
        select(inner, current.as_ref(), method, class).map(|session| {
            (
                session.writer.clone(),
                session.generation,
                session.editor_process.clone(),
            )
        })
    };
    let (writer, generation, editor_process) = match selected {
        Ok(selected) => selected,
        Err(failure) => return (Err(failure), None),
    };
    let answer = send_request(
        inner,
        &writer,
        generation,
        method,
        class,
        payload,
        idempotency_id,
        timeout,
        cancellation,
    );
    (answer, editor_process)
}

/// The session that takes `method`, or why none does.
fn select<'a>(
    inner: &Inner,
    current: Option<&'a Session>,
    method: &str,
    class: OperationClass,
) -> Result<&'a Session, BridgeFailure> {
    let session = current.ok_or(BridgeFailure::NotConnected)?;
    if !session.methods.contains(method) {
        return Err(BridgeFailure::Unsupported(method.to_owned()));
    }
    if class == OperationClass::Mutation
        && method != RESTART_METHOD
        && inner.restarting.load(Ordering::SeqCst) > 0
    {
        return Err(BridgeFailure::Restarting);
    }
    Ok(session)
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
    let late = abandonable(
        inner,
        generation,
        &message_id,
        idempotency_id,
        method,
        class,
        payload,
    );
    lock(&inner.pending).insert(
        message_id.clone(),
        Pending {
            generation,
            sender,
            late: late.clone(),
        },
    );
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
            withdraw();
            return give_up(inner, &message_id, &receiver, late, BridgeFailure::TimedOut);
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
                return give_up(inner, &message_id, &receiver, late, BridgeFailure::Lost);
            }
        }
    }
}

/// Stop waiting for request `message_id` and report `failure`, keeping the
/// request so that an answer that comes later can still be bound to it.
///
/// The request is taken under the lock that delivers answers, so it either
/// still waits — and is kept before the lock is released, where an answer
/// arriving the moment after finds it — or its answer, or the loss of its
/// session, was already delivered to `receiver` and is returned instead.
fn give_up(
    inner: &Inner,
    message_id: &str,
    receiver: &mpsc::Receiver<Result<BridgeAnswer, BridgeFailure>>,
    late: Option<Abandoned>,
    failure: BridgeFailure,
) -> Result<BridgeAnswer, BridgeFailure> {
    {
        let mut pending = lock(&inner.pending);
        if let Some(waiting) = pending.remove(message_id) {
            keep_abandoned(inner, waiting.late);
            return Err(failure);
        }
    }
    match receiver.try_recv() {
        Ok(answer) => answer,
        Err(_) => {
            keep_abandoned(inner, late);
            Err(failure)
        }
    }
}

/// What the bridge keeps of a request if it stops waiting for it: a mutation
/// sent for a transaction, so its late answer can still be bound to it. Only a
/// bridge with a late-answer sink keeps one.
fn abandonable(
    inner: &Inner,
    generation: u64,
    message_id: &str,
    idempotency_id: &str,
    method: &str,
    class: OperationClass,
    payload: &Value,
) -> Option<Abandoned> {
    if class != OperationClass::Mutation || lock(&inner.late).sink.is_none() {
        return None;
    }
    let transaction_id = payload
        .get("transaction_id")
        .and_then(Value::as_str)
        .filter(|transaction| !transaction.is_empty())?;
    Some(Abandoned {
        generation,
        message_id: message_id.to_owned(),
        idempotency_id: idempotency_id.to_owned(),
        method: method.to_owned(),
        transaction_id: transaction_id.to_owned(),
    })
}

/// Keep one abandoned request, dropping the oldest beyond the bound.
fn keep_abandoned(inner: &Inner, late: Option<Abandoned>) {
    let Some(late) = late else {
        return;
    };
    let mut abandoned = lock(&inner.abandoned);
    if abandoned.len() >= MAX_ABANDONED {
        abandoned.pop_front();
    }
    abandoned.push_back(late);
}

/// Remove and return the abandoned request `matches` selects.
fn take_abandoned(inner: &Inner, matches: impl Fn(&Abandoned) -> bool) -> Option<Abandoned> {
    let mut abandoned = lock(&inner.abandoned);
    let index = abandoned.iter().position(matches)?;
    abandoned.remove(index)
}

/// Hand `late` to the sink, or keep it for the sink until one is attached,
/// dropping the oldest beyond the bound. The sink runs with no bridge lock
/// held.
fn hand_late(inner: &Inner, late: LateAnswer) {
    let sink = {
        let mut answers = lock(&inner.late);
        match &answers.sink {
            Some(sink) => Arc::clone(sink),
            None => {
                if answers.unclaimed.len() >= MAX_UNCLAIMED_LATE {
                    answers.unclaimed.pop_front();
                }
                answers.unclaimed.push_back(late);
                return;
            }
        }
    };
    sink(late);
}

/// The answer one `result` or `error` envelope carries.
fn answer_of(envelope: &Envelope) -> BridgeAnswer {
    if envelope.kind() == MessageKind::Result {
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
    }
}

/// The answer a late-answer report carries, or `None` when it does not say
/// what happened: its mutation status, whether it succeeded and, for a
/// failure, its error kind. A result keeps only the action and digest a
/// receipt names.
fn reported_answer(answer: &Value) -> Option<BridgeAnswer> {
    let status = MutationStatus::parse(answer.get("mutation_status")?.as_str()?)?;
    if answer.get("ok")?.as_bool()? {
        let mut payload = serde_json::Map::new();
        for key in ["action", "digest"] {
            if let Some(text) = answer
                .get(key)
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                payload.insert(key.to_owned(), Value::String(text.to_owned()));
            }
        }
        return Some(BridgeAnswer::Result {
            status,
            payload: Value::Object(payload),
        });
    }
    let kind: ErrorKind = serde_json::from_value(answer.get("error_kind")?.clone()).ok()?;
    let message = answer
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .chars()
        .take(MAX_LATE_MESSAGE_CHARS)
        .collect();
    Some(BridgeAnswer::Error {
        kind,
        message,
        status: Some(status),
    })
}

/// The late answer one [`LATE_ANSWER_EVENT`] reports, bound to its request.
/// A request this server sent must be one it stopped waiting for, named
/// exactly; one an earlier server sent is handed on for the journal to bind.
fn reported_late(inner: &Inner, payload: Option<&Value>) -> Option<LateAnswer> {
    let payload = payload?;
    let request = payload.get("request")?;
    let text = |key: &str| {
        request
            .get(key)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    };
    let session_id = text("session_id")?;
    let message_id = text("message_id")?;
    let idempotency_id = text("idempotency_id")?;
    let method = text("method")?;
    let transaction_id = text("transaction_id")?;
    let answer = reported_answer(payload.get("answer")?)?;
    let sent_here = session_id == inner.server.session_id;
    if sent_here {
        take_abandoned(inner, |entry| {
            entry.message_id == message_id
                && entry.idempotency_id == idempotency_id
                && entry.method == method
                && entry.transaction_id == transaction_id
        })?;
    }
    Some(LateAnswer {
        transaction_id,
        idempotency_id,
        method,
        sent_here,
        answer,
    })
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
            // Kept before its caller hears of the loss: its answer may be
            // reported on the next session.
            keep_abandoned(inner, waiting.late);
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
        let feeds_status = takes_status(inner.status, &description);
        let editor_process = super::process::observe(&description);
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
            editor_process,
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
        if feeds_status {
            let feeding = Arc::clone(inner);
            let writer = writer.clone();
            // The status is sampled and awaited on a thread of its own, so a
            // slow sample never delays the answers the reader delivers.
            let _ = std::thread::Builder::new()
                .name(format!("godot-mcp-{}-status-{generation}", inner.label))
                .spawn(move || feed_status(&feeding, &writer, generation));
        }
    }
}

/// Whether `generation` is still the session this bridge serves.
fn is_current(inner: &Inner, generation: u64) -> bool {
    lock(&inner.current)
        .as_ref()
        .is_some_and(|session| session.generation == generation)
}

/// What a status says, apart from when it was observed: a status that says
/// the same again is not sent again.
fn status_key(status: &Value) -> String {
    let mut key = status.clone();
    if let Some(object) = key.as_object_mut() {
        object.remove("observed_at_unix_ms");
    }
    key.to_string()
}

/// Tell the peer of session `generation` the server status: as soon as there
/// is a source, and again whenever a sample differs from the status the peer
/// last took. A status the peer refused, or did not answer in time, is sent
/// again with the next sample. Ends with the session.
fn feed_status(inner: &Arc<Inner>, writer: &SessionWriter, generation: u64) {
    let Some(sample) = inner.status else {
        return;
    };
    let mut taken: Option<String> = None;
    let mut due = Instant::now();
    loop {
        if inner.stop.load(Ordering::SeqCst) || !is_current(inner, generation) {
            return;
        }
        let source = lock(&inner.status_source).clone();
        let Some(source) = source.filter(|_| Instant::now() >= due) else {
            std::thread::sleep(READ_WAIT.min(sample));
            continue;
        };
        due = Instant::now() + sample;
        let Some(status) = source() else {
            continue;
        };
        let key = status_key(&status);
        if taken.as_ref() == Some(&key) {
            continue;
        }
        match send_request(
            inner,
            writer,
            generation,
            SERVER_STATUS_METHOD,
            OperationClass::Control,
            &status,
            &format!("server-status-{generation}"),
            STATUS_DEADLINE,
            &CancelSignal::default(),
        ) {
            Ok(BridgeAnswer::Result { .. }) => taken = Some(key),
            Ok(BridgeAnswer::Error { .. }) | Err(BridgeFailure::TimedOut) => {}
            Err(_) => return,
        }
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
                if let Some(reason) = deliver(inner, generation, &envelope) {
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

/// Hand one message the plugin sent on session `generation` to whoever waits
/// for it: an answer to its caller, or, when the caller stopped waiting for a
/// mutation it sent for a transaction, to the late-answer sink, as is a late
/// answer the plugin reports. Returns a reason to close the session when the
/// message has no place in the protocol.
fn deliver(inner: &Inner, generation: u64, envelope: &Envelope) -> Option<String> {
    match envelope.kind() {
        MessageKind::Result | MessageKind::Error => {
            let correlation = envelope.correlation_id()?.to_owned();
            {
                // Delivered under the lock a caller gives up under, so the
                // caller either reads the answer or kept its request first.
                let mut pending = lock(&inner.pending);
                if let Some(Pending { sender, .. }) = pending.remove(&correlation) {
                    let _ = sender.try_send(Ok(answer_of(envelope)));
                    return None;
                }
            }
            if let Some(abandoned) = take_abandoned(inner, |entry| {
                entry.generation == generation && entry.message_id == correlation
            }) {
                hand_late(
                    inner,
                    LateAnswer {
                        transaction_id: abandoned.transaction_id,
                        idempotency_id: abandoned.idempotency_id,
                        method: abandoned.method,
                        sent_here: true,
                        answer: answer_of(envelope),
                    },
                );
            }
            None
        }
        MessageKind::Event => {
            if envelope.event_name() == Some(LATE_ANSWER_EVENT)
                && let Some(late) = reported_late(inner, envelope.payload())
            {
                hand_late(inner, late);
            }
            None
        }
        MessageKind::Heartbeat | MessageKind::Progress => None,
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
            let bridge = EditorBridge::start(&state, project(), false).expect("start");
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
        let first = EditorBridge::start(&scratch.0, project(), false).expect("start");
        let left = std::fs::read(&path).expect("the first rendezvous");
        // What a killed server leaves: its file, and none of its cleanup.
        std::mem::forget(first);
        let second = EditorBridge::start(&scratch.0, project(), false)
            .expect("the next server starts over a stale rendezvous");
        let written = std::fs::read(&path).expect("the second rendezvous");
        assert_ne!(written, left, "the next server writes its own secret");
        second.stop();
        assert!(!path.exists(), "stop removes the rendezvous");
    }

    #[test]
    fn nothing_is_sent_without_a_session() {
        let scratch = Scratch::new("idle");
        let bridge = EditorBridge::start(&scratch.0, project(), false).expect("start");
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
        let bridge = EditorBridge::start(&scratch.0, project(), false).expect("start");
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

    /// While a restart is awaited the bridge sends the restart and reads, and
    /// nothing else that could change the editor that is about to exit; no
    /// exclusion is granted either, because the restarted editor writes the
    /// caches while it starts, before its add-on connects.
    #[test]
    fn no_file_is_free_to_write_while_a_restarted_editor_is_awaited() {
        use crate::router::LiveEditorView as _;
        let scratch = Scratch::new("restart-held");
        let bridge = EditorBridge::start(&scratch.0, project(), false).expect("start");
        let boundary = super::super::boundary::BridgeEditorBoundary::new(Arc::clone(&bridge));
        assert_eq!(
            boundary.held_paths(),
            Ok(None),
            "with no editor, a headless writer holds the files"
        );
        let guard = bridge.begin_restart();
        let refused = boundary.held_paths().expect_err("a restart is awaited");
        assert!(refused.contains("editor.restart"), "{refused}");
        drop(guard);
        assert_eq!(boundary.held_paths(), Ok(None));
        bridge.stop();
    }

    #[test]
    fn an_awaited_restart_holds_every_other_change_back() {
        let scratch = Scratch::new("restart-guard");
        let bridge = EditorBridge::start(&scratch.0, project(), false).expect("start");
        let plugin = FakeEditorPlugin::spawn(
            &scratch.0.join(RENDEZVOUS_FILE),
            FakePluginScript::implementing(&[
                "editor.state",
                "editor.selection.set",
                RESTART_METHOD,
            ]),
        );
        wait_until("the plugin to connect", || bridge.status().connected);
        let ask = |method: &str, class: OperationClass| {
            bridge.call(method, class, &json!({}), "guard", Duration::from_secs(10))
        };
        let guard = bridge.begin_restart();
        assert!(bridge.restarting());
        assert!(matches!(
            ask("editor.selection.set", OperationClass::Mutation),
            Err(BridgeFailure::Restarting)
        ));
        assert!(!BridgeFailure::Restarting.dispatched());
        assert!(ask("editor.state", OperationClass::Read).is_ok());
        assert!(ask(RESTART_METHOD, OperationClass::Mutation).is_ok());
        assert!(
            bridge.exclude_connections().is_err(),
            "no exclusion while a restart is awaited"
        );
        drop(guard);
        assert!(!bridge.restarting());
        assert!(ask("editor.selection.set", OperationClass::Mutation).is_ok());
        let methods: Vec<String> = plugin
            .received()
            .into_iter()
            .map(|(method, _)| method)
            .collect();
        assert_eq!(
            methods,
            vec![
                "bridge.describe_session",
                "editor.state",
                RESTART_METHOD,
                "editor.selection.set"
            ],
            "the held-back change was never sent"
        );
        bridge.stop();
        plugin.join().expect("the plugin saw a clean shutdown");
    }

    #[test]
    fn a_described_plugin_answers_only_the_methods_it_reported() {
        let scratch = Scratch::new("answers");
        let bridge = EditorBridge::start(&scratch.0, project(), false).expect("start");
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
        let bridge = EditorBridge::start(&scratch.0, project(), false).expect("start");
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
        let bridge = EditorBridge::start(&scratch.0, project(), false).expect("start");
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
                late: None,
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
        let bridge = EditorBridge::start(&scratch.0, project(), false).expect("start");
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

    /// A fake editor that reports this test process as the editor: its id and
    /// the executable it runs, both of which this host can check.
    fn as_this_process(script: FakePluginScript) -> FakePluginScript {
        let executable = std::env::current_exe().expect("this test's executable");
        script
            .describing("pid", json!(std::process::id()))
            .describing("executable", json!(executable.display().to_string()))
    }

    /// This test process as the bridge records it, where the host can observe it.
    fn this_process() -> Option<EditorProcess> {
        use godot_mcp_engine::supervision::procfs;
        procfs::observations_available().then(|| {
            let start_time = procfs::process_identity(std::process::id())
                .expect("this process is observable")
                .expect("this process is live")
                .start_time
                .expect("a start time is reported");
            EditorProcess {
                pid: std::process::id(),
                start_time,
            }
        })
    }

    #[test]
    fn an_unanswered_mutation_names_the_editor_process_its_session_runs_in() {
        let scratch = Scratch::new("editor-process");
        let bridge = EditorBridge::start(&scratch.0, project(), false).expect("start");
        let plugin = FakeEditorPlugin::spawn(
            &scratch.0.join(RENDEZVOUS_FILE),
            as_this_process(FakePluginScript::implementing(&["editor.scene.batch"]))
                .answering("editor.scene.batch", FakeAnswer::Disconnect),
        );
        wait_until("the plugin to connect", || bridge.status().connected);
        let (answer, process) = bridge.call_with_process(
            "editor.scene.batch",
            OperationClass::Mutation,
            &json!({ "path": "res://main.tscn" }),
            "batch-1",
            Duration::from_secs(10),
        );
        assert_eq!(answer, Err(BridgeFailure::Lost));
        assert_eq!(process, this_process());
        plugin.join().expect("the plugin ended");
        bridge.stop();
    }

    #[test]
    fn a_session_whose_editor_cannot_be_checked_names_no_process() {
        let elsewhere = tempfile::NamedTempFile::new().expect("another file");
        for script in [
            // An add-on older than the `executable` field.
            FakePluginScript::implementing(&["editor.state"])
                .describing("pid", json!(std::process::id())),
            // A process that does not run the executable the editor named.
            FakePluginScript::implementing(&["editor.state"])
                .describing("pid", json!(std::process::id()))
                .describing("executable", json!(elsewhere.path().display().to_string())),
        ] {
            let scratch = Scratch::new("no-editor-process");
            let bridge = EditorBridge::start(&scratch.0, project(), false).expect("start");
            let plugin = FakeEditorPlugin::spawn(&scratch.0.join(RENDEZVOUS_FILE), script);
            wait_until("the plugin to connect", || bridge.status().connected);
            let (answer, process) = bridge.call_with_process(
                "editor.state",
                OperationClass::Read,
                &json!({}),
                "state-1",
                Duration::from_secs(5),
            );
            assert!(answer.is_ok(), "{answer:?}");
            assert_eq!(process, None);
            bridge.stop();
            plugin.join().expect("the plugin ended");
        }
    }

    #[test]
    fn a_request_names_the_process_of_the_session_it_was_sent_on() {
        let scratch = Scratch::new("superseded-process");
        let bridge = EditorBridge::start(&scratch.0, project(), false).expect("start");
        let first = FakeEditorPlugin::spawn(
            &scratch.0.join(RENDEZVOUS_FILE),
            as_this_process(FakePluginScript::implementing(&["editor.state"])),
        );
        wait_until("the first plugin", || bridge.status().sessions_opened == 1);
        let (answer, process) = bridge.call_with_process(
            "editor.state",
            OperationClass::Read,
            &json!({}),
            "state-1",
            Duration::from_secs(5),
        );
        assert!(answer.is_ok(), "{answer:?}");
        assert_eq!(process, this_process());
        let second = FakeEditorPlugin::spawn(
            &scratch.0.join(RENDEZVOUS_FILE),
            FakePluginScript::implementing(&["editor.state"]),
        );
        wait_until("the second plugin", || bridge.status().sessions_opened == 2);
        first.join().expect("the superseded plugin was closed");
        let (answer, process) = bridge.call_with_process(
            "editor.state",
            OperationClass::Read,
            &json!({}),
            "state-2",
            Duration::from_secs(5),
        );
        assert!(answer.is_ok(), "{answer:?}");
        assert_eq!(process, None, "the newer session described no process");
        bridge.stop();
        second.join().expect("the second plugin ended");
    }

    /// A status source whose answer a test changes, counting how often it is
    /// asked.
    fn status_source(status: Value) -> (StatusSource, Arc<Mutex<Value>>, Arc<AtomicU64>) {
        let shared = Arc::new(Mutex::new(status));
        let asked = Arc::new(AtomicU64::new(0));
        let (reading, counting) = (Arc::clone(&shared), Arc::clone(&asked));
        let source: StatusSource = Arc::new(move || {
            counting.fetch_add(1, Ordering::SeqCst);
            Some(lock(&reading).clone())
        });
        (source, shared, asked)
    }

    fn sent(plugin: &FakeEditorPlugin, method: &str) -> Vec<Value> {
        plugin
            .received()
            .into_iter()
            .filter(|(received, _)| received == method)
            .map(|(_, payload)| payload)
            .collect()
    }

    #[test]
    fn the_server_status_reaches_only_an_editor_plugin_that_lists_it() {
        let scratch = Scratch::new("status-listed");
        let bridge = EditorBridge::start(&scratch.0, project(), false).expect("start");
        let status = json!({ "server_version": "9.9.9", "observed_at_unix_ms": 1, "tools": [] });
        let (source, _, _) = status_source(status.clone());
        bridge.attach_status_source(source);
        let listing = FakeEditorPlugin::spawn(
            &scratch.0.join(RENDEZVOUS_FILE),
            FakePluginScript::implementing(&[SERVER_STATUS_METHOD, "editor.state"]),
        );
        wait_until("the status to reach the plugin", || {
            !sent(&listing, SERVER_STATUS_METHOD).is_empty()
        });
        assert_eq!(sent(&listing, SERVER_STATUS_METHOD), vec![status]);
        assert_eq!(
            listing.received()[0].0,
            "bridge.describe_session",
            "the session is described before anything is sent"
        );
        assert_eq!(
            bridge.status().methods,
            vec!["editor.state".to_owned()],
            "the status is the session's own bookkeeping, not a method tools call"
        );
        bridge.stop();
        listing.join().expect("the plugin ended");

        let scratch = Scratch::new("status-unlisted");
        let bridge = EditorBridge::start(&scratch.0, project(), false).expect("start");
        let (source, _, asked) = status_source(json!({}));
        bridge.attach_status_source(source);
        let quiet = FakeEditorPlugin::spawn(
            &scratch.0.join(RENDEZVOUS_FILE),
            FakePluginScript::implementing(&["editor.state"]),
        );
        wait_until("the plugin to connect", || bridge.status().connected);
        // Several of the reader's housekeeping turns, then a round trip: a
        // status sent at all would have reached the plugin before this answer.
        std::thread::sleep(Duration::from_millis(600));
        bridge
            .call(
                "editor.state",
                OperationClass::Read,
                &json!({}),
                "after-status",
                Duration::from_secs(5),
            )
            .expect("answered");
        let methods: Vec<String> = quiet
            .received()
            .into_iter()
            .map(|(method, _)| method)
            .collect();
        assert_eq!(methods, vec!["bridge.describe_session", "editor.state"]);
        assert_eq!(
            asked.load(Ordering::SeqCst),
            0,
            "nothing is sampled for a plugin that does not list the method"
        );
        bridge.stop();
        quiet.join().expect("the plugin ended");
    }

    #[test]
    fn a_status_is_sent_again_when_it_changes_and_not_while_it_holds() {
        let scratch = Scratch::new("status-changes");
        let bridge = EditorBridge::start_with(
            BridgeConfig::editor(&scratch.0).with_status_sample(Duration::from_millis(50)),
            project(),
        )
        .expect("start");
        let (source, shared, asked) =
            status_source(json!({ "profile": "editor", "observed_at_unix_ms": 1 }));
        bridge.attach_status_source(source);
        let plugin = FakeEditorPlugin::spawn(
            &scratch.0.join(RENDEZVOUS_FILE),
            FakePluginScript::implementing(&[SERVER_STATUS_METHOD]),
        );
        wait_until("the first status", || {
            sent(&plugin, SERVER_STATUS_METHOD).len() == 1
        });
        // The same status observed later is not news: it is not sent again.
        *lock(&shared) = json!({ "profile": "editor", "observed_at_unix_ms": 2 });
        let sampled = asked.load(Ordering::SeqCst);
        wait_until("three more samples", || {
            asked.load(Ordering::SeqCst) >= sampled + 3
        });
        assert_eq!(sent(&plugin, SERVER_STATUS_METHOD).len(), 1);
        // A changed one is.
        *lock(&shared) = json!({ "profile": "full", "observed_at_unix_ms": 3 });
        wait_until("the changed status", || {
            sent(&plugin, SERVER_STATUS_METHOD).len() == 2
        });
        assert_eq!(sent(&plugin, SERVER_STATUS_METHOD)[1]["profile"], "full");
        bridge.stop();
        plugin.join().expect("the plugin ended");
    }

    #[test]
    fn a_status_the_plugin_refused_is_sent_again() {
        let scratch = Scratch::new("status-refused");
        let bridge = EditorBridge::start_with(
            BridgeConfig::editor(&scratch.0).with_status_sample(Duration::from_millis(50)),
            project(),
        )
        .expect("start");
        let (source, _, _) = status_source(json!({ "profile": "editor" }));
        bridge.attach_status_source(source);
        let plugin = FakeEditorPlugin::spawn(
            &scratch.0.join(RENDEZVOUS_FILE),
            FakePluginScript::implementing(&[SERVER_STATUS_METHOD]).answering(
                SERVER_STATUS_METHOD,
                FakeAnswer::Error(ErrorKind::InvalidInput, MutationStatus::NotStarted),
            ),
        );
        wait_until("a second attempt", || {
            sent(&plugin, SERVER_STATUS_METHOD).len() >= 2
        });
        bridge.stop();
        plugin.join().expect("the plugin ended");
    }

    #[test]
    fn a_status_source_attached_after_the_session_opened_still_reaches_it() {
        let scratch = Scratch::new("status-late");
        let bridge = EditorBridge::start(&scratch.0, project(), false).expect("start");
        let plugin = FakeEditorPlugin::spawn(
            &scratch.0.join(RENDEZVOUS_FILE),
            FakePluginScript::implementing(&[SERVER_STATUS_METHOD]),
        );
        wait_until("the plugin to connect", || bridge.status().connected);
        let (source, _, _) = status_source(json!({ "profile": "late" }));
        bridge.attach_status_source(source);
        wait_until("the status", || {
            sent(&plugin, SERVER_STATUS_METHOD) == vec![json!({ "profile": "late" })]
        });
        bridge.stop();
        plugin.join().expect("the plugin ended");
    }

    #[test]
    fn the_runtime_probe_is_never_told_the_server_status() {
        let scratch = Scratch::new("status-runtime");
        let rendezvous = scratch.0.join("runtime.rendezvous");
        let bridge = EditorBridge::start_with(
            BridgeConfig::runtime(rendezvous.clone(), &["runtime.tree"])
                .with_status_sample(Duration::from_millis(50)),
            project(),
        )
        .expect("start");
        let (source, _, asked) = status_source(json!({ "profile": "runtime" }));
        bridge.attach_status_source(source);
        let probe = FakeEditorPlugin::spawn(
            &rendezvous,
            FakePluginScript::implementing(&[SERVER_STATUS_METHOD, "runtime.tree"]),
        );
        wait_until("the probe to connect", || bridge.status().connected);
        std::thread::sleep(Duration::from_millis(500));
        bridge
            .call(
                "runtime.tree",
                OperationClass::Read,
                &json!({}),
                "tree",
                Duration::from_secs(5),
            )
            .expect("answered");
        let methods: Vec<String> = probe
            .received()
            .into_iter()
            .map(|(method, _)| method)
            .collect();
        assert_eq!(methods, vec!["bridge.describe_session", "runtime.tree"]);
        assert_eq!(asked.load(Ordering::SeqCst), 0);
        bridge.stop();
        probe.join().expect("the probe ended");
    }

    /// A sink that keeps every late answer it is handed.
    fn late_sink() -> (LateAnswerSink, Arc<Mutex<Vec<LateAnswer>>>) {
        let kept = Arc::new(Mutex::new(Vec::new()));
        let keeping = Arc::clone(&kept);
        let sink: LateAnswerSink = Arc::new(move |late| lock(&keeping).push(late));
        (sink, kept)
    }

    const LATE_TRANSACTION: &str = "00000000-0000-4000-8000-000000000501";

    /// A bridge with a late-answer sink, and a plugin serving `script`.
    fn late_bridge(
        label: &str,
        script: FakePluginScript,
    ) -> (
        Scratch,
        Arc<EditorBridge>,
        Arc<Mutex<Vec<LateAnswer>>>,
        FakeEditorPlugin,
    ) {
        let scratch = Scratch::new(label);
        let bridge = EditorBridge::start(&scratch.0, project(), false).expect("start");
        let (sink, kept) = late_sink();
        bridge.attach_late_answers(sink);
        let plugin = FakeEditorPlugin::spawn(&scratch.0.join(RENDEZVOUS_FILE), script);
        wait_until("the plugin to connect", || bridge.status().connected);
        (scratch, bridge, kept, plugin)
    }

    /// A round trip on the session: every frame the plugin sent before it
    /// answers this read has been delivered once it returns.
    fn round_trip(bridge: &EditorBridge) {
        bridge
            .call(
                "editor.state",
                OperationClass::Read,
                &json!({}),
                "round-trip",
                Duration::from_secs(10),
            )
            .expect("the round trip is answered");
    }

    #[test]
    fn a_mutation_answered_after_the_server_gave_up_reaches_the_sink() {
        let (_scratch, bridge, kept, plugin) = late_bridge(
            "late-same",
            FakePluginScript::implementing(&["editor.state", "editor.scene.batch"]).answering(
                "editor.scene.batch",
                FakeAnswer::PayloadAfter {
                    delay: Duration::from_millis(2800),
                    payload: json!({ "action": "godot-mcp: scene.batch.apply", "digest": "after" }),
                },
            ),
        );
        assert_eq!(
            bridge.call(
                "editor.scene.batch",
                OperationClass::Mutation,
                &json!({ "path": "res://main.tscn", "transaction_id": LATE_TRANSACTION }),
                "late-1",
                Duration::from_millis(200),
            ),
            Err(BridgeFailure::TimedOut),
            "the caller's deadline and the grace pass before the answer"
        );
        round_trip(&bridge);
        assert_eq!(
            *lock(&kept),
            vec![LateAnswer {
                transaction_id: LATE_TRANSACTION.to_owned(),
                idempotency_id: "late-1".to_owned(),
                method: "editor.scene.batch".to_owned(),
                sent_here: true,
                answer: BridgeAnswer::Result {
                    status: MutationStatus::Committed,
                    payload: json!({ "action": "godot-mcp: scene.batch.apply", "digest": "after" }),
                },
            }]
        );
        bridge.stop();
        plugin.join().expect("the plugin ended");
    }

    #[test]
    fn a_late_read_and_a_late_mutation_sent_for_no_transaction_reach_no_sink() {
        let late = FakeAnswer::PayloadAfter {
            delay: Duration::from_millis(2800),
            payload: json!({ "digest": "late" }),
        };
        let (_scratch, bridge, kept, plugin) = late_bridge(
            "late-ignored",
            FakePluginScript::implementing(&[
                "editor.state",
                "editor.scene.tree",
                "editor.selection.set",
            ])
            .answering("editor.scene.tree", late.clone())
            .answering("editor.selection.set", late),
        );
        assert_eq!(
            bridge.call(
                "editor.scene.tree",
                OperationClass::Read,
                &json!({ "transaction_id": LATE_TRANSACTION }),
                "late-read",
                Duration::from_millis(200),
            ),
            Err(BridgeFailure::TimedOut)
        );
        assert_eq!(
            bridge.call(
                "editor.selection.set",
                OperationClass::Mutation,
                &json!({ "nodes": [] }),
                "late-untracked",
                Duration::from_millis(200),
            ),
            Err(BridgeFailure::TimedOut)
        );
        round_trip(&bridge);
        assert!(lock(&kept).is_empty(), "{:?}", lock(&kept));
        bridge.stop();
        plugin.join().expect("the plugin ended");
    }

    /// The editor lost its session while the request waited in its queue,
    /// reconnected, and reports that the request never started.
    #[test]
    fn a_lost_mutation_reported_on_a_later_session_reaches_the_sink_once() {
        let (scratch, bridge, kept, first) = late_bridge(
            "late-reported",
            FakePluginScript::implementing(&["editor.state", "editor.scene.batch"])
                .answering("editor.scene.batch", FakeAnswer::Disconnect),
        );
        assert_eq!(
            bridge.call(
                "editor.scene.batch",
                OperationClass::Mutation,
                &json!({ "path": "res://main.tscn", "transaction_id": LATE_TRANSACTION }),
                "lost-1",
                Duration::from_secs(10),
            ),
            Err(BridgeFailure::Lost)
        );
        let seen = first
            .seen()
            .into_iter()
            .find(|request| request.method == "editor.scene.batch")
            .expect("the plugin received the batch");
        first
            .join()
            .expect("the first plugin dropped its connection");
        wait_until("the session to end", || !bridge.status().connected);
        let never_started = json!({
            "mutation_status": "not_started",
            "ok": false,
            "error_kind": "unavailable",
            "message": "the connection was lost before the request started"
        });
        let mut unknown = seen.clone();
        unknown.message_id = "request-999".to_owned();
        let second = FakeEditorPlugin::spawn(
            &scratch.0.join(RENDEZVOUS_FILE),
            FakePluginScript::implementing(&["editor.state", "editor.scene.batch"])
                .reporting_late(seen.late_report(never_started.clone()))
                .reporting_late(seen.late_report(never_started.clone()))
                .reporting_late(unknown.late_report(never_started)),
        );
        wait_until("the plugin to reconnect", || bridge.status().connected);
        round_trip(&bridge);
        assert_eq!(
            *lock(&kept),
            vec![LateAnswer {
                transaction_id: LATE_TRANSACTION.to_owned(),
                idempotency_id: "lost-1".to_owned(),
                method: "editor.scene.batch".to_owned(),
                sent_here: true,
                answer: BridgeAnswer::Error {
                    kind: ErrorKind::Unavailable,
                    message: "the connection was lost before the request started".to_owned(),
                    status: Some(MutationStatus::NotStarted),
                },
            }],
            "the request is bound once; a repeated report and one naming a request this \
             server never sent are dropped"
        );
        bridge.stop();
        second.join().expect("the second plugin ended");
    }

    #[test]
    fn a_late_answer_reported_for_an_earlier_server_is_handed_on_unverified() {
        let report = json!({
            "request": {
                "session_id": "bridge-000000000000000000000000",
                "message_id": "request-7",
                "idempotency_id": "earlier-1",
                "method": "editor.scene.save",
                "transaction_id": LATE_TRANSACTION
            },
            "answer": {
                "mutation_status": "committed",
                "ok": true,
                "action": "godot-mcp: editor.scene.save",
                "digest": "saved"
            }
        });
        let (_scratch, bridge, kept, plugin) = late_bridge(
            "late-earlier",
            FakePluginScript::implementing(&["editor.state"]).reporting_late(report),
        );
        round_trip(&bridge);
        assert_eq!(
            *lock(&kept),
            vec![LateAnswer {
                transaction_id: LATE_TRANSACTION.to_owned(),
                idempotency_id: "earlier-1".to_owned(),
                method: "editor.scene.save".to_owned(),
                sent_here: false,
                answer: BridgeAnswer::Result {
                    status: MutationStatus::Committed,
                    payload: json!({ "action": "godot-mcp: editor.scene.save", "digest": "saved" }),
                },
            }]
        );
        bridge.stop();
        plugin.join().expect("the plugin ended");
    }

    #[test]
    fn a_late_answer_reported_before_the_sink_is_attached_reaches_it_once() {
        let report = json!({
            "request": {
                "session_id": "bridge-000000000000000000000000",
                "message_id": "request-8",
                "idempotency_id": "early-1",
                "method": "editor.filesystem.scan",
                "transaction_id": LATE_TRANSACTION
            },
            "answer": { "mutation_status": "not_started", "ok": false,
                        "error_kind": "unavailable", "message": "never started" }
        });
        let scratch = Scratch::new("late-early");
        // The server listens before it has a sink: the plugin may connect and
        // report in between.
        let bridge = EditorBridge::start(&scratch.0, project(), false).expect("start");
        let plugin = FakeEditorPlugin::spawn(
            &scratch.0.join(RENDEZVOUS_FILE),
            FakePluginScript::implementing(&["editor.state"]).reporting_late(report),
        );
        wait_until("the plugin to connect", || bridge.status().connected);
        round_trip(&bridge);
        let (sink, kept) = late_sink();
        bridge.attach_late_answers(sink);
        let expected = vec![LateAnswer {
            transaction_id: LATE_TRANSACTION.to_owned(),
            idempotency_id: "early-1".to_owned(),
            method: "editor.filesystem.scan".to_owned(),
            sent_here: false,
            answer: BridgeAnswer::Error {
                kind: ErrorKind::Unavailable,
                message: "never started".to_owned(),
                status: Some(MutationStatus::NotStarted),
            },
        }];
        assert_eq!(*lock(&kept), expected, "the report waited for the sink");
        let (again, kept_again) = late_sink();
        bridge.attach_late_answers(again);
        assert!(lock(&kept_again).is_empty(), "a report is handed on once");
        bridge.stop();
        plugin.join().expect("the plugin ended");
    }

    #[test]
    fn the_bridge_keeps_only_so_many_reports_for_a_sink_not_yet_attached() {
        let scratch = Scratch::new("late-unclaimed");
        let bridge = EditorBridge::start(&scratch.0, project(), false).expect("start");
        let reported = |index: usize| LateAnswer {
            transaction_id: LATE_TRANSACTION.to_owned(),
            idempotency_id: format!("early-{index}"),
            method: "editor.scene.save".to_owned(),
            sent_here: false,
            answer: BridgeAnswer::Result {
                status: MutationStatus::Committed,
                payload: json!({}),
            },
        };
        for index in 0..MAX_UNCLAIMED_LATE + 4 {
            hand_late(&bridge.inner, reported(index));
        }
        let (sink, kept) = late_sink();
        bridge.attach_late_answers(sink);
        let expected: Vec<LateAnswer> = (4..MAX_UNCLAIMED_LATE + 4).map(reported).collect();
        assert_eq!(*lock(&kept), expected, "the newest reports, oldest first");
        hand_late(&bridge.inner, reported(99));
        assert_eq!(
            lock(&kept).last(),
            Some(&reported(99)),
            "an attached sink takes it at once"
        );
        bridge.stop();
    }

    #[test]
    fn a_reported_answer_must_say_what_happened() {
        let committed = json!({ "mutation_status": "committed", "ok": true });
        assert_eq!(
            reported_answer(&committed),
            Some(BridgeAnswer::Result {
                status: MutationStatus::Committed,
                payload: json!({}),
            })
        );
        let long = "x".repeat(MAX_LATE_MESSAGE_CHARS + 10);
        assert_eq!(
            reported_answer(&json!({
                "mutation_status": "committed",
                "ok": false,
                "error_kind": "internal",
                "message": long
            })),
            Some(BridgeAnswer::Error {
                kind: ErrorKind::Internal,
                message: "x".repeat(MAX_LATE_MESSAGE_CHARS),
                status: Some(MutationStatus::Committed),
            })
        );
        for malformed in [
            json!({ "ok": true }),
            json!({ "mutation_status": "done", "ok": true }),
            json!({ "mutation_status": "committed" }),
            json!({ "mutation_status": "not_started", "ok": false }),
            json!({ "mutation_status": "not_started", "ok": false, "error_kind": "gone" }),
            json!("committed"),
        ] {
            assert_eq!(reported_answer(&malformed), None, "{malformed}");
        }
    }

    /// The caller's deadline and the answer can meet at one instant. Whichever
    /// takes the request first wins: the answer reaches the caller, or the
    /// caller gives up first and the answer reaches the sink. It is never
    /// dropped between the two.
    #[test]
    fn an_answer_that_meets_the_deadline_reaches_the_caller_or_the_sink() {
        let scratch = Scratch::new("late-race");
        let bridge = EditorBridge::start(&scratch.0, project(), false).expect("start");
        let (sink, kept) = late_sink();
        bridge.attach_late_answers(sink);
        let request = |id: &str| {
            abandonable(
                &bridge.inner,
                9,
                id,
                "idem-race",
                "editor.scene.batch",
                OperationClass::Mutation,
                &json!({ "transaction_id": LATE_TRANSACTION }),
            )
        };
        let plugin = PluginIdentity {
            plugin_version: godot_mcp_bridge::addon::ADDON_VERSION.to_owned(),
            protocol_version: PROTOCOL_VERSION.to_owned(),
            godot_version: "4.4.1.stable".to_owned(),
        };
        let answer = |id: &str| {
            Envelope::result(
                &plugin,
                id,
                MutationStatus::Committed,
                &json!({ "action": "godot-mcp: scene.batch.apply" }),
            )
        };

        // The answer takes the request first: the caller who then gives up
        // finds it waiting.
        let (sender, receiver) = mpsc::sync_channel(1);
        lock(&bridge.inner.pending).insert(
            "request-first".to_owned(),
            Pending {
                generation: 9,
                sender,
                late: request("request-first"),
            },
        );
        assert_eq!(deliver(&bridge.inner, 9, &answer("request-first")), None);
        assert!(
            matches!(
                give_up(
                    &bridge.inner,
                    "request-first",
                    &receiver,
                    request("request-first"),
                    BridgeFailure::TimedOut
                ),
                Ok(BridgeAnswer::Result {
                    status: MutationStatus::Committed,
                    ..
                })
            ),
            "the caller reads the answer that arrived as it gave up"
        );
        assert!(lock(&kept).is_empty());
        assert!(lock(&bridge.inner.abandoned).is_empty());

        // The caller gives up first: the answer that then arrives is late.
        let (sender, receiver) = mpsc::sync_channel(1);
        lock(&bridge.inner.pending).insert(
            "request-second".to_owned(),
            Pending {
                generation: 9,
                sender,
                late: request("request-second"),
            },
        );
        assert!(matches!(
            give_up(
                &bridge.inner,
                "request-second",
                &receiver,
                request("request-second"),
                BridgeFailure::TimedOut
            ),
            Err(BridgeFailure::TimedOut)
        ));
        assert_eq!(deliver(&bridge.inner, 9, &answer("request-second")), None);
        assert_eq!(
            lock(&kept)
                .iter()
                .map(|late| (late.transaction_id.as_str(), late.sent_here))
                .collect::<Vec<_>>(),
            vec![(LATE_TRANSACTION, true)]
        );

        // A session that ends keeps the requests it loses, before their
        // callers hear of the loss.
        let (sender, receiver) = mpsc::sync_channel(1);
        lock(&bridge.inner.pending).insert(
            "request-third".to_owned(),
            Pending {
                generation: 9,
                sender,
                late: request("request-third"),
            },
        );
        fail_pending(&bridge.inner, Some(9));
        assert_eq!(
            lock(&bridge.inner.abandoned)
                .iter()
                .map(|entry| entry.message_id.as_str())
                .collect::<Vec<_>>(),
            vec!["request-third"]
        );
        assert!(matches!(receiver.try_recv(), Ok(Err(BridgeFailure::Lost))));
        bridge.stop();
    }

    #[test]
    fn the_bridge_keeps_abandoned_requests_only_for_a_sink_and_only_so_many() {
        let scratch = Scratch::new("late-bounded");
        let bridge = EditorBridge::start(&scratch.0, project(), false).expect("start");
        let payload = |index: usize| json!({ "transaction_id": format!("00000000-0000-4000-8000-{index:012}") });
        keep_abandoned(
            &bridge.inner,
            abandonable(
                &bridge.inner,
                1,
                "request-0",
                "idem-0",
                "editor.scene.batch",
                OperationClass::Mutation,
                &payload(0),
            ),
        );
        assert!(
            lock(&bridge.inner.abandoned).is_empty(),
            "no sink, nothing kept"
        );
        let (sink, _) = late_sink();
        bridge.attach_late_answers(sink);
        for index in 0..=MAX_ABANDONED {
            keep_abandoned(
                &bridge.inner,
                abandonable(
                    &bridge.inner,
                    1,
                    &format!("request-{index}"),
                    &format!("idem-{index}"),
                    "editor.scene.batch",
                    OperationClass::Mutation,
                    &payload(index),
                ),
            );
        }
        keep_abandoned(
            &bridge.inner,
            abandonable(
                &bridge.inner,
                1,
                "request-read",
                "idem-read",
                "editor.state",
                OperationClass::Read,
                &payload(999),
            ),
        );
        let kept = lock(&bridge.inner.abandoned);
        assert_eq!(kept.len(), MAX_ABANDONED);
        assert_eq!(
            kept.front().map(|entry| entry.message_id.as_str()),
            Some("request-1")
        );
        assert_eq!(
            kept.back().map(|entry| entry.message_id.as_str()),
            Some(format!("request-{MAX_ABANDONED}").as_str())
        );
        drop(kept);
        bridge.stop();
    }

    #[test]
    fn only_a_server_allowing_scripts_offers_the_script_method() {
        let mut with_scripts: Vec<&str> = BRIDGE_METHODS.to_vec();
        with_scripts.push(SCRIPT_RUN_METHOD);
        with_scripts.sort_unstable();
        assert_eq!(BRIDGE_METHODS_WITH_SCRIPTS, with_scripts.as_slice());
        assert!(!BRIDGE_METHODS.contains(&SCRIPT_RUN_METHOD));
        let plain = BridgeConfig::editor(Path::new("state")).allowing_scripts(false);
        assert!(!plain.advertised().contains(&SCRIPT_RUN_METHOD));
        let scripting = BridgeConfig::editor(Path::new("state")).allowing_scripts(true);
        assert!(scripting.advertised().contains(&SCRIPT_RUN_METHOD));
        assert!(scripting.advertised().contains(&SERVER_STATUS_METHOD));
    }

    #[test]
    fn only_the_editor_bridge_advertises_the_server_status() {
        let editor = BridgeConfig::editor(Path::new("state"));
        assert!(editor.advertised().contains(&SERVER_STATUS_METHOD));
        assert!(
            BRIDGE_METHODS
                .iter()
                .all(|method| editor.advertised().contains(method))
        );
        let runtime = BridgeConfig::runtime(PathBuf::from("runtime.rendezvous"), &["runtime.tree"]);
        assert_eq!(runtime.advertised(), vec!["runtime.tree"]);
    }
}
