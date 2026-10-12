//! The frozen versioned envelope every bridge message is wrapped in.
//!
//! # One envelope, ten kinds, no eleventh
//!
//! [`MessageKind`] is closed: `hello`, `capabilities`, `request`, `result`,
//! `error`, `progress`, `cancel`, `heartbeat`, `event`, `shutdown`. Adding an
//! eleventh kind is a `MAJOR` protocol bump by construction, because the
//! canonical schema in `schemas/bridge/protocol-v1.json` enumerates exactly these
//! ten and every message is validated against it.
//!
//! # Why every field is always present
//!
//! The canonical rendering has **the same key set on every message**, with
//! inapplicable fields explicitly `null` rather than omitted. That is what makes
//! byte stability achievable across two very different JSON implementations:
//! `serde_json` on the Rust side and Godot's `JSON.stringify` on the GDScript
//! side. If a field could be *absent* on one side and `null` on the other, the
//! two halves would render different bytes for the same logical message and the
//! golden vectors could not be shared. Sorted keys plus a fixed key set plus
//! compact separators is a rendering both sides can reproduce exactly.
//!
//! # These are not MCP messages
//!
//! `method` names a **bridge** method (`editor.describe_selection`), never an MCP
//! tool name. There is no protocol-marker member naming another RPC dialect, no
//! borrowed request-ID semantics and no foreign error object anywhere in this
//! envelope. The plugin never sees an MCP wire message; a unit test in [`super`]
//! asserts that structurally by scanning this module's own source for every MCP
//! spelling.
//!
//! # What the envelope binds
//!
//! * **identity** — server version + session, or plugin version + Godot version;
//! * **project** — the public opaque identity, the OS object identity of both the
//!   root directory and `project.godot`, and the generation, so "same project" is
//!   an object-identity fact rather than a path string comparison;
//! * **request/idempotency IDs** — the idempotency ID is reused verbatim in the
//!   Todo 6 journal record, so one mutation has one identity across both planes;
//! * **deadlines** — absolute wall-clock, so a request whose deadline has already
//!   passed is refused instead of started;
//! * **operation class** — `read`, `mutation` or `control`, which is what decides
//!   whether a result or error must carry a [`MutationStatus`];
//! * **mutation status** — `not_started | committed | unresolved`, three states and
//!   no fourth.

use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::jsonschema::{SchemaError, validate};
use serde_json::{Map, Value, json};

use super::mutation::MutationStatus;

/// The canonical envelope schema, embedded from the repository at compile time.
pub const PROTOCOL_SCHEMA_JSON: &str = include_str!("../../../../schemas/bridge/protocol-v1.json");

/// Envelope schema version. Distinct from the protocol version.
pub const ENVELOPE_SCHEMA_VERSION: &str = "1";

/// Which side sent a message. Never inferred from the socket direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Origin {
    /// This server.
    Server,
    /// The GDScript `EditorPlugin`.
    Plugin,
}

impl Origin {
    /// Canonical wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Server => "server",
            Self::Plugin => "plugin",
        }
    }

    /// Parse the canonical wire spelling.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "server" => Some(Self::Server),
            "plugin" => Some(Self::Plugin),
            _ => None,
        }
    }
}

/// The ten frozen message kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MessageKind {
    /// The plugin's opening message, carrying identity, project and the secret.
    Hello,
    /// The server's answer to `hello`, naming the methods it will accept.
    Capabilities,
    /// A unit of work for the plugin.
    Request,
    /// A successful answer to a request.
    Result,
    /// A failed answer to a request.
    Error,
    /// Partial completion of an in-flight request.
    Progress,
    /// Withdrawal of a request that has not started.
    Cancel,
    /// Liveness, in either direction.
    Heartbeat,
    /// An unsolicited notification from the plugin.
    Event,
    /// End of session.
    Shutdown,
}

impl MessageKind {
    /// Every kind. There are ten, permanently.
    pub const ALL: [Self; 10] = [
        Self::Hello,
        Self::Capabilities,
        Self::Request,
        Self::Result,
        Self::Error,
        Self::Progress,
        Self::Cancel,
        Self::Heartbeat,
        Self::Event,
        Self::Shutdown,
    ];

    /// Canonical wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hello => "hello",
            Self::Capabilities => "capabilities",
            Self::Request => "request",
            Self::Result => "result",
            Self::Error => "error",
            Self::Progress => "progress",
            Self::Cancel => "cancel",
            Self::Heartbeat => "heartbeat",
            Self::Event => "event",
            Self::Shutdown => "shutdown",
        }
    }

    /// Parse the canonical wire spelling.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == text)
    }
}

/// What class of work a request performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OperationClass {
    /// Never changes editor state.
    Read,
    /// Changes editor state, so its result must carry a [`MutationStatus`].
    Mutation,
    /// Protocol bookkeeping: heartbeat, cancel, shutdown.
    Control,
}

impl OperationClass {
    /// Every class. There are three.
    pub const ALL: [Self; 3] = [Self::Read, Self::Mutation, Self::Control];

    /// Canonical wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Mutation => "mutation",
            Self::Control => "control",
        }
    }

    /// Parse the canonical wire spelling.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|class| class.as_str() == text)
    }
}

/// An absolute wall-clock deadline, in Unix milliseconds.
///
/// Absolute rather than a duration so a queued request cannot silently gain time
/// by sitting in the plugin's deferred queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Deadline {
    unix_ms: u64,
}

impl Deadline {
    /// A deadline at `unix_ms`.
    #[must_use]
    pub const fn from_unix_ms(unix_ms: u64) -> Self {
        Self { unix_ms }
    }

    /// The absolute deadline in Unix milliseconds.
    #[must_use]
    pub const fn unix_ms(self) -> u64 {
        self.unix_ms
    }

    /// Whether this deadline has already passed at `now_unix_ms`.
    #[must_use]
    pub const fn expired_at(self, now_unix_ms: u64) -> bool {
        now_unix_ms >= self.unix_ms
    }
}

/// The server's identity, as presented in `capabilities` and every server frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerIdentity {
    /// The server binary's version.
    pub server_version: String,
    /// The bridge protocol version the server speaks.
    pub protocol_version: String,
    /// The per-connection session identity.
    pub session_id: String,
}

/// The plugin's identity, as presented in `hello` and every plugin frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginIdentity {
    /// The editor-side client's own version: the bundled `godot_mcp` add-on's
    /// version (see [`crate::addon::ADDON_VERSION`]).
    pub plugin_version: String,
    /// The bridge protocol version the plugin speaks.
    pub protocol_version: String,
    /// The Godot build hosting the plugin, e.g. `4.4.1.stable`.
    pub godot_version: String,
}

/// The project both sides must agree on, by OS object identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectClaim {
    /// Public opaque `project_<64hex>` identity.
    pub object_identity: String,
    /// Encoded root-directory OS object identity.
    pub root_object: String,
    /// Encoded `project.godot` OS object identity.
    pub file_object: String,
    /// Current project generation.
    pub generation: u64,
}

/// The `hello` authenticator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthClaim {
    /// Hex spelling of the 256-bit per-launch rendezvous secret.
    pub secret_hex: String,
    /// Single-use nonce for this handshake attempt.
    pub nonce: String,
}

/// A classified failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorBody {
    /// One of Todo 3's ten fixed error kinds.
    pub kind: ErrorKind,
    /// A human-readable, payload-free explanation.
    pub message: String,
}

/// Partial completion of an in-flight request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressBody {
    /// Units finished.
    pub completed: u64,
    /// Units expected.
    pub total: u64,
    /// What is happening.
    pub note: String,
}

/// The sender's identity, as it appears on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IdentityBlock {
    /// Server binary version, when a server sent this.
    pub server_version: Option<String>,
    /// Plugin version, when a plugin sent this.
    pub plugin_version: Option<String>,
    /// Godot build, when a plugin sent this.
    pub godot_version: Option<String>,
    /// Session identity, when a server sent this.
    pub session_id: Option<String>,
}

impl From<&ServerIdentity> for IdentityBlock {
    fn from(identity: &ServerIdentity) -> Self {
        Self {
            server_version: Some(identity.server_version.clone()),
            plugin_version: None,
            godot_version: None,
            session_id: Some(identity.session_id.clone()),
        }
    }
}

impl From<&PluginIdentity> for IdentityBlock {
    fn from(identity: &PluginIdentity) -> Self {
        Self {
            server_version: None,
            plugin_version: Some(identity.plugin_version.clone()),
            godot_version: Some(identity.godot_version.clone()),
            session_id: None,
        }
    }
}

/// Why an envelope was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EnvelopeError {
    /// The text was not parsable JSON.
    #[error("the message is not parsable JSON")]
    Malformed,

    /// The value violated the canonical envelope schema.
    #[error("the message violates the canonical envelope schema: {0}")]
    Schema(SchemaError),

    /// The canonical schema itself could not be parsed.
    #[error("the canonical envelope schema is unusable")]
    SchemaUnusable,

    /// A field the schema admits carried a value this protocol does not define.
    #[error("field `{field}` carries the undefined value {value:?}")]
    UndefinedValue {
        /// Which field.
        field: &'static str,
        /// What it carried.
        value: String,
    },
}

/// One bridge message.
///
/// Constructed through the per-kind constructors so an envelope with a
/// contradictory field combination (a `heartbeat` carrying a `mutation_status`,
/// say) is not reachable from this API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    protocol_version: String,
    kind: MessageKind,
    origin: Origin,
    message_id: String,
    correlation_id: Option<String>,
    idempotency_id: Option<String>,
    operation_class: Option<OperationClass>,
    deadline: Option<Deadline>,
    method: Option<String>,
    event_name: Option<String>,
    reason: Option<String>,
    sent_unix_ms: Option<u64>,
    mutation_status: Option<MutationStatus>,
    identity: IdentityBlock,
    project: Option<ProjectClaim>,
    auth: Option<AuthClaim>,
    error: Option<ErrorBody>,
    progress: Option<ProgressBody>,
    payload: Option<Value>,
}

impl Envelope {
    fn blank(protocol_version: &str, kind: MessageKind, origin: Origin, message_id: &str) -> Self {
        Self {
            protocol_version: protocol_version.to_owned(),
            kind,
            origin,
            message_id: message_id.to_owned(),
            correlation_id: None,
            idempotency_id: None,
            operation_class: None,
            deadline: None,
            method: None,
            event_name: None,
            reason: None,
            sent_unix_ms: None,
            mutation_status: None,
            identity: IdentityBlock::default(),
            project: None,
            auth: None,
            error: None,
            progress: None,
            payload: None,
        }
    }

    /// The plugin's opening message.
    #[must_use]
    pub fn hello(
        plugin: &PluginIdentity,
        project: &ProjectClaim,
        secret_hex: String,
        nonce: &str,
    ) -> Self {
        let mut envelope = Self::blank(
            &plugin.protocol_version,
            MessageKind::Hello,
            Origin::Plugin,
            &format!("hello-{nonce}"),
        );
        envelope.identity = IdentityBlock::from(plugin);
        envelope.project = Some(project.clone());
        envelope.auth = Some(AuthClaim {
            secret_hex,
            nonce: nonce.to_owned(),
        });
        envelope.operation_class = Some(OperationClass::Control);
        envelope
    }

    /// The server's answer to `hello`.
    #[must_use]
    pub fn capabilities(server: &ServerIdentity, project: &ProjectClaim, methods: &[&str]) -> Self {
        let mut envelope = Self::blank(
            &server.protocol_version,
            MessageKind::Capabilities,
            Origin::Server,
            &format!("capabilities-{}", server.session_id),
        );
        envelope.identity = IdentityBlock::from(server);
        envelope.project = Some(project.clone());
        envelope.operation_class = Some(OperationClass::Control);
        let mut sorted: Vec<&str> = methods.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        envelope.payload = Some(json!({"methods": sorted}));
        envelope
    }

    /// A unit of work for the plugin.
    #[must_use]
    pub fn request(
        server: &ServerIdentity,
        message_id: &str,
        idempotency_id: &str,
        method: &str,
        operation_class: OperationClass,
        deadline: Deadline,
        payload: &Value,
    ) -> Self {
        let mut envelope = Self::blank(
            &server.protocol_version,
            MessageKind::Request,
            Origin::Server,
            message_id,
        );
        envelope.identity = IdentityBlock::from(server);
        envelope.idempotency_id = Some(idempotency_id.to_owned());
        envelope.method = Some(method.to_owned());
        envelope.operation_class = Some(operation_class);
        envelope.deadline = Some(deadline);
        envelope.payload = Some(payload.clone());
        envelope
    }

    /// A successful answer to the request `correlation_id`.
    #[must_use]
    pub fn result(
        plugin: &PluginIdentity,
        correlation_id: &str,
        mutation_status: MutationStatus,
        payload: &Value,
    ) -> Self {
        let mut envelope = Self::blank(
            &plugin.protocol_version,
            MessageKind::Result,
            Origin::Plugin,
            &format!("result-{correlation_id}"),
        );
        envelope.identity = IdentityBlock::from(plugin);
        envelope.correlation_id = Some(correlation_id.to_owned());
        envelope.mutation_status = Some(mutation_status);
        envelope.payload = Some(payload.clone());
        envelope
    }

    /// A failed answer to the request `correlation_id`.
    #[must_use]
    pub fn error(
        plugin: &PluginIdentity,
        correlation_id: &str,
        kind: ErrorKind,
        message: &str,
    ) -> Self {
        let mut envelope = Self::blank(
            &plugin.protocol_version,
            MessageKind::Error,
            Origin::Plugin,
            &format!("error-{correlation_id}"),
        );
        envelope.identity = IdentityBlock::from(plugin);
        envelope.correlation_id = Some(correlation_id.to_owned());
        envelope.error = Some(ErrorBody {
            kind,
            message: message.to_owned(),
        });
        envelope
    }

    /// A failed mutation answer with the mutation's proven three-state outcome.
    #[must_use]
    pub fn mutation_error(
        plugin: &PluginIdentity,
        correlation_id: &str,
        mutation_status: MutationStatus,
        kind: ErrorKind,
        message: &str,
    ) -> Self {
        let mut envelope = Self::error(plugin, correlation_id, kind, message);
        envelope.operation_class = Some(OperationClass::Mutation);
        envelope.mutation_status = Some(mutation_status);
        envelope
    }

    /// A failed answer sent by the server rather than the plugin.
    ///
    /// The server answers with an error exactly once: when it refuses a handshake.
    /// Every other error on this bridge originates in the plugin, which is why this
    /// is a separate, narrowly documented constructor rather than an `origin`
    /// parameter on [`Envelope::error`].
    #[must_use]
    pub fn error_from_server(
        server: &ServerIdentity,
        correlation_id: &str,
        kind: ErrorKind,
        message: &str,
    ) -> Self {
        let mut envelope = Self::blank(
            &server.protocol_version,
            MessageKind::Error,
            Origin::Server,
            &format!("error-{correlation_id}"),
        );
        envelope.identity = IdentityBlock::from(server);
        envelope.correlation_id = Some(correlation_id.to_owned());
        envelope.error = Some(ErrorBody {
            kind,
            message: message.to_owned(),
        });
        envelope
    }

    /// Partial completion of the request `correlation_id`.
    #[must_use]
    pub fn progress(
        plugin: &PluginIdentity,
        correlation_id: &str,
        completed: u64,
        total: u64,
        note: &str,
    ) -> Self {
        let mut envelope = Self::blank(
            &plugin.protocol_version,
            MessageKind::Progress,
            Origin::Plugin,
            &format!("progress-{correlation_id}-{completed}"),
        );
        envelope.identity = IdentityBlock::from(plugin);
        envelope.correlation_id = Some(correlation_id.to_owned());
        envelope.progress = Some(ProgressBody {
            completed,
            total,
            note: note.to_owned(),
        });
        envelope
    }

    /// Withdraw the request `target`, which must not have started.
    #[must_use]
    pub fn cancel(server: &ServerIdentity, message_id: &str, target: &str) -> Self {
        let mut envelope = Self::blank(
            &server.protocol_version,
            MessageKind::Cancel,
            Origin::Server,
            message_id,
        );
        envelope.identity = IdentityBlock::from(server);
        envelope.correlation_id = Some(target.to_owned());
        envelope.operation_class = Some(OperationClass::Control);
        envelope
    }

    /// Liveness, from the server.
    #[must_use]
    pub fn heartbeat(server: &ServerIdentity, message_id: &str, sent_unix_ms: u64) -> Self {
        let mut envelope = Self::blank(
            &server.protocol_version,
            MessageKind::Heartbeat,
            Origin::Server,
            message_id,
        );
        envelope.identity = IdentityBlock::from(server);
        envelope.operation_class = Some(OperationClass::Control);
        envelope.sent_unix_ms = Some(sent_unix_ms);
        envelope
    }

    /// Liveness, answered by the plugin.
    ///
    /// The reply echoes the server's `message_id` as its correlation, so a stale
    /// heartbeat answer is distinguishable from a current one.
    #[must_use]
    pub fn heartbeat_reply(plugin: &PluginIdentity, correlation_id: &str) -> Self {
        let mut envelope = Self::blank(
            &plugin.protocol_version,
            MessageKind::Heartbeat,
            Origin::Plugin,
            &format!("heartbeat-{correlation_id}"),
        );
        envelope.identity = IdentityBlock::from(plugin);
        envelope.correlation_id = Some(correlation_id.to_owned());
        envelope.operation_class = Some(OperationClass::Control);
        envelope
    }

    /// An unsolicited notification from the plugin.
    #[must_use]
    pub fn event(
        plugin: &PluginIdentity,
        message_id: &str,
        event_name: &str,
        payload: &Value,
    ) -> Self {
        let mut envelope = Self::blank(
            &plugin.protocol_version,
            MessageKind::Event,
            Origin::Plugin,
            message_id,
        );
        envelope.identity = IdentityBlock::from(plugin);
        envelope.event_name = Some(event_name.to_owned());
        envelope.payload = Some(payload.clone());
        envelope
    }

    /// End of session, from the server.
    #[must_use]
    pub fn shutdown(server: &ServerIdentity, message_id: &str, reason: &str) -> Self {
        let mut envelope = Self::blank(
            &server.protocol_version,
            MessageKind::Shutdown,
            Origin::Server,
            message_id,
        );
        envelope.identity = IdentityBlock::from(server);
        envelope.operation_class = Some(OperationClass::Control);
        envelope.reason = Some(reason.to_owned());
        envelope
    }

    /// Which kind this is.
    #[must_use]
    pub const fn kind(&self) -> MessageKind {
        self.kind
    }

    /// Which side sent it.
    #[must_use]
    pub const fn origin(&self) -> Origin {
        self.origin
    }

    /// This message's own identity.
    #[must_use]
    pub fn message_id(&self) -> &str {
        &self.message_id
    }

    /// The message this one answers or withdraws.
    #[must_use]
    pub fn correlation_id(&self) -> Option<&str> {
        self.correlation_id.as_deref()
    }

    /// The caller's idempotency key, on a request.
    #[must_use]
    pub fn idempotency_id(&self) -> Option<&str> {
        self.idempotency_id.as_deref()
    }

    /// The bridge method, on a request.
    #[must_use]
    pub fn method(&self) -> Option<&str> {
        self.method.as_deref()
    }

    /// The declared operation class.
    #[must_use]
    pub const fn operation_class(&self) -> Option<OperationClass> {
        self.operation_class
    }

    /// The absolute deadline, on a request.
    #[must_use]
    pub const fn deadline(&self) -> Option<Deadline> {
        self.deadline
    }

    /// The three-state mutation outcome, on a result or failed mutation.
    #[must_use]
    pub const fn mutation_status(&self) -> Option<MutationStatus> {
        self.mutation_status
    }

    /// The classified failure kind, on an error.
    #[must_use]
    pub fn error_kind(&self) -> Option<ErrorKind> {
        self.error.as_ref().map(|body| body.kind)
    }

    /// The failure body, on an error.
    #[must_use]
    pub const fn error_body(&self) -> Option<&ErrorBody> {
        self.error.as_ref()
    }

    /// The project claim, on `hello` and `capabilities`.
    #[must_use]
    pub const fn project(&self) -> Option<&ProjectClaim> {
        self.project.as_ref()
    }

    /// The authenticator, on `hello`.
    #[must_use]
    pub const fn auth(&self) -> Option<&AuthClaim> {
        self.auth.as_ref()
    }

    /// The sender's identity block.
    #[must_use]
    pub const fn identity(&self) -> &IdentityBlock {
        &self.identity
    }

    /// The protocol version the sender declared.
    #[must_use]
    pub fn protocol_version(&self) -> &str {
        &self.protocol_version
    }

    /// The kind-specific body.
    #[must_use]
    pub const fn payload(&self) -> Option<&Value> {
        self.payload.as_ref()
    }

    /// The canonical value: the full fixed key set, inapplicable fields `null`.
    #[must_use]
    pub fn to_canonical_value(&self) -> Value {
        let identity = json!({
            "server_version": self.identity.server_version,
            "plugin_version": self.identity.plugin_version,
            "godot_version": self.identity.godot_version,
            "session_id": self.identity.session_id,
        });
        let project = self.project.as_ref().map_or(Value::Null, |project| {
            json!({
                "object_identity": project.object_identity,
                "root_object": project.root_object,
                "file_object": project.file_object,
                "generation": project.generation,
            })
        });
        let auth = self.auth.as_ref().map_or(
            Value::Null,
            |auth| json!({"secret_hex": auth.secret_hex, "nonce": auth.nonce}),
        );
        let error = self.error.as_ref().map_or(
            Value::Null,
            |error| json!({"kind": error.kind.code(), "message": error.message}),
        );
        let progress = self.progress.as_ref().map_or(Value::Null, |progress| {
            json!({
                "completed": progress.completed,
                "total": progress.total,
                "note": progress.note,
            })
        });
        json!({
            "schema_version": ENVELOPE_SCHEMA_VERSION,
            "protocol_version": self.protocol_version,
            "kind": self.kind.as_str(),
            "origin": self.origin.as_str(),
            "message_id": self.message_id,
            "correlation_id": self.correlation_id,
            "idempotency_id": self.idempotency_id,
            "operation_class": self.operation_class.map(OperationClass::as_str),
            "deadline_unix_ms": self.deadline.map(Deadline::unix_ms),
            "method": self.method,
            "event_name": self.event_name,
            "reason": self.reason,
            "sent_unix_ms": self.sent_unix_ms,
            "mutation_status": self.mutation_status.map(MutationStatus::as_str),
            "identity": identity,
            "project": project,
            "auth": auth,
            "error": error,
            "progress": progress,
            "payload": self.payload.clone(),
        })
    }

    /// Parse one canonical message.
    ///
    /// # Errors
    /// Returns [`EnvelopeError::Malformed`] for unparsable JSON,
    /// [`EnvelopeError::Schema`] when the canonical schema rejects it, and
    /// [`EnvelopeError::UndefinedValue`] for a schema-admitted spelling this
    /// protocol does not define.
    pub fn parse(text: &str) -> Result<Self, EnvelopeError> {
        let value: Value = serde_json::from_str(text).map_err(|_json| EnvelopeError::Malformed)?;
        Self::from_canonical_value(&value)
    }

    /// Parse one already-decoded canonical message value.
    ///
    /// # Errors
    /// Same as [`Envelope::parse`], minus the JSON-syntax case.
    pub fn from_canonical_value(value: &Value) -> Result<Self, EnvelopeError> {
        validate_canonical(value)?;

        let kind = MessageKind::parse(string(value, "kind").unwrap_or_default())
            .ok_or_else(|| undefined("kind", value))?;
        let origin = Origin::parse(string(value, "origin").unwrap_or_default())
            .ok_or_else(|| undefined("origin", value))?;
        let operation_class = match value.get("operation_class") {
            Some(Value::String(text)) => Some(
                OperationClass::parse(text).ok_or_else(|| undefined("operation_class", value))?,
            ),
            _ => None,
        };
        let mutation_status = match value.get("mutation_status") {
            Some(Value::String(text)) => Some(
                MutationStatus::parse(text).ok_or_else(|| undefined("mutation_status", value))?,
            ),
            _ => None,
        };
        let error = match value.get("error") {
            Some(Value::Object(body)) => {
                let code = body
                    .get("kind")
                    .and_then(Value::as_str)
                    .ok_or_else(|| undefined("error.kind", value))?;
                let kind = parse_error_kind(code).ok_or_else(|| undefined("error.kind", value))?;
                Some(ErrorBody {
                    kind,
                    message: body
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                })
            }
            _ => None,
        };
        let progress = match value.get("progress") {
            Some(Value::Object(body)) => Some(ProgressBody {
                completed: body.get("completed").and_then(Value::as_u64).unwrap_or(0),
                total: body.get("total").and_then(Value::as_u64).unwrap_or(0),
                note: body
                    .get("note")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            }),
            _ => None,
        };
        let project = match value.get("project") {
            Some(Value::Object(body)) => Some(ProjectClaim {
                object_identity: owned(body, "object_identity"),
                root_object: owned(body, "root_object"),
                file_object: owned(body, "file_object"),
                generation: body.get("generation").and_then(Value::as_u64).unwrap_or(0),
            }),
            _ => None,
        };
        let auth = match value.get("auth") {
            Some(Value::Object(body)) => Some(AuthClaim {
                secret_hex: owned(body, "secret_hex"),
                nonce: owned(body, "nonce"),
            }),
            _ => None,
        };
        let identity = match value.get("identity") {
            Some(Value::Object(body)) => IdentityBlock {
                server_version: optional(body, "server_version"),
                plugin_version: optional(body, "plugin_version"),
                godot_version: optional(body, "godot_version"),
                session_id: optional(body, "session_id"),
            },
            _ => IdentityBlock::default(),
        };

        Ok(Self {
            protocol_version: string(value, "protocol_version")
                .unwrap_or_default()
                .to_owned(),
            kind,
            origin,
            message_id: string(value, "message_id").unwrap_or_default().to_owned(),
            correlation_id: string(value, "correlation_id").map(ToOwned::to_owned),
            idempotency_id: string(value, "idempotency_id").map(ToOwned::to_owned),
            operation_class,
            deadline: value
                .get("deadline_unix_ms")
                .and_then(Value::as_u64)
                .map(Deadline::from_unix_ms),
            method: string(value, "method").map(ToOwned::to_owned),
            event_name: string(value, "event_name").map(ToOwned::to_owned),
            reason: string(value, "reason").map(ToOwned::to_owned),
            sent_unix_ms: value.get("sent_unix_ms").and_then(Value::as_u64),
            mutation_status,
            identity,
            project,
            auth,
            error,
            progress,
            payload: match value.get("payload") {
                Some(Value::Null) | None => None,
                Some(payload) => Some(payload.clone()),
            },
        })
    }
}

fn string<'value>(value: &'value Value, field: &str) -> Option<&'value str> {
    value.get(field).and_then(Value::as_str)
}

fn owned(body: &Map<String, Value>, field: &str) -> String {
    body.get(field)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn optional(body: &Map<String, Value>, field: &str) -> Option<String> {
    body.get(field)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn undefined(field: &'static str, value: &Value) -> EnvelopeError {
    EnvelopeError::UndefinedValue {
        field,
        value: value
            .get(field)
            .map_or_else(|| "null".to_owned(), ToString::to_string),
    }
}

fn parse_error_kind(code: &str) -> Option<ErrorKind> {
    const ALL: [ErrorKind; 10] = [
        ErrorKind::InvalidInput,
        ErrorKind::NotFound,
        ErrorKind::Conflict,
        ErrorKind::Unsupported,
        ErrorKind::PermissionDenied,
        ErrorKind::Timeout,
        ErrorKind::Cancelled,
        ErrorKind::Unavailable,
        ErrorKind::Uncertain,
        ErrorKind::Internal,
    ];
    ALL.into_iter().find(|kind| kind.code() == code)
}

/// The canonical bytes of `envelope`: compact UTF-8 JSON with sorted keys.
///
/// `serde_json`'s default `Map` is a `BTreeMap`, so serialization emits keys in
/// lexicographic order, and `to_vec` emits no whitespace. Both properties are
/// asserted by this module's tests rather than assumed, because they are what the
/// GDScript half must reproduce.
#[must_use]
pub fn canonical_bytes(envelope: &Envelope) -> Vec<u8> {
    serde_json::to_vec(&envelope.to_canonical_value()).unwrap_or_default()
}

/// Validate one message value against the canonical envelope schema.
///
/// # Errors
/// Returns [`EnvelopeError::SchemaUnusable`] when the embedded schema cannot be
/// parsed, or [`EnvelopeError::Schema`] with the first violation.
pub fn validate_canonical(value: &Value) -> Result<(), EnvelopeError> {
    let schema: Value = serde_json::from_str(PROTOCOL_SCHEMA_JSON)
        .map_err(|_json| EnvelopeError::SchemaUnusable)?;
    validate(&schema, value).map_err(EnvelopeError::Schema)
}

#[cfg(test)]
mod tests {
    use godot_mcp_domain::ErrorKind;
    use serde_json::{Value, json};

    use super::super::mutation::MutationStatus;
    use super::{
        Deadline, ENVELOPE_SCHEMA_VERSION, Envelope, EnvelopeError, MessageKind, OperationClass,
        Origin, PROTOCOL_SCHEMA_JSON, PluginIdentity, ProjectClaim, ServerIdentity,
        canonical_bytes, parse_error_kind, validate_canonical,
    };

    fn server() -> ServerIdentity {
        ServerIdentity {
            server_version: "0.1.0".to_owned(),
            protocol_version: "1.0.0".to_owned(),
            session_id: "session-1".to_owned(),
        }
    }

    fn plugin() -> PluginIdentity {
        PluginIdentity {
            plugin_version: "0.1.0".to_owned(),
            protocol_version: "1.0.0".to_owned(),
            godot_version: "4.4.1.stable".to_owned(),
        }
    }

    fn project() -> ProjectClaim {
        ProjectClaim {
            object_identity: format!("project_{}", "a".repeat(64)),
            root_object: "unix:dev=1,ino=2".to_owned(),
            file_object: "unix:dev=1,ino=3".to_owned(),
            generation: 3,
        }
    }

    fn every_kind() -> Vec<Envelope> {
        vec![
            Envelope::hello(&plugin(), &project(), "b".repeat(64), "n1"),
            Envelope::capabilities(&server(), &project(), &["editor.read"]),
            Envelope::request(
                &server(),
                "r1",
                "i1",
                "editor.read",
                OperationClass::Read,
                Deadline::from_unix_ms(10),
                &json!({}),
            ),
            Envelope::result(&plugin(), "r1", MutationStatus::NotStarted, &json!({})),
            Envelope::error(&plugin(), "r1", ErrorKind::Timeout, "too slow"),
            Envelope::mutation_error(
                &plugin(),
                "r2",
                MutationStatus::Unresolved,
                ErrorKind::Uncertain,
                "outcome unknown",
            ),
            Envelope::progress(&plugin(), "r1", 1, 2, "half"),
            Envelope::cancel(&server(), "c1", "r1"),
            Envelope::heartbeat(&server(), "h1", 5),
            Envelope::heartbeat_reply(&plugin(), "h1"),
            Envelope::event(&plugin(), "e1", "editor.changed", &json!({})),
            Envelope::shutdown(&server(), "s1", "stopping"),
        ]
    }

    #[test]
    fn there_are_exactly_ten_kinds_and_the_schema_enumerates_the_same_ten() {
        assert_eq!(MessageKind::ALL.len(), 10);
        let schema: Value = serde_json::from_str(PROTOCOL_SCHEMA_JSON).expect("schema parses");
        let enumerated = schema["properties"]["kind"]["enum"]
            .as_array()
            .expect("kind enum");
        assert_eq!(enumerated.len(), 10);
        for kind in MessageKind::ALL {
            assert!(
                enumerated.contains(&json!(kind.as_str())),
                "schema enumerates {}",
                kind.as_str()
            );
        }
        assert_eq!(
            schema["oneOf"].as_array().expect("kind branches").len(),
            MessageKind::ALL.len(),
            "the kind-conditional oneOf has exactly one branch per kind"
        );
    }

    #[test]
    fn every_kind_validates_and_round_trips_byte_for_byte() {
        for envelope in every_kind() {
            let bytes = canonical_bytes(&envelope);
            let text = String::from_utf8(bytes.clone()).expect("utf8");
            validate_canonical(&serde_json::from_str::<Value>(&text).expect("parses"))
                .unwrap_or_else(|error| panic!("{:?}: {error}", envelope.kind()));
            let parsed = Envelope::parse(&text).expect("parses back");
            assert_eq!(canonical_bytes(&parsed), bytes, "{:?}", envelope.kind());
            assert_eq!(parsed, envelope, "{:?}", envelope.kind());
        }
    }

    #[test]
    fn the_canonical_rendering_has_sorted_keys_and_no_whitespace() {
        for envelope in every_kind() {
            let text = String::from_utf8(canonical_bytes(&envelope)).expect("utf8");
            assert!(!text.contains('\n'), "no newlines");
            assert!(!text.contains(": "), "no space after a colon");
            assert!(!text.contains(", "), "no space after a comma");

            let value: Value = serde_json::from_str(&text).expect("parses");
            let keys: Vec<&String> = value
                .as_object()
                .expect("object")
                .keys()
                .collect::<Vec<_>>();
            let mut sorted = keys.clone();
            sorted.sort();
            assert_eq!(keys, sorted, "top-level keys are lexicographically sorted");
        }
    }

    #[test]
    fn every_message_carries_the_same_key_set() {
        let mut reference: Option<Vec<String>> = None;
        for envelope in every_kind() {
            let value = envelope.to_canonical_value();
            let keys: Vec<String> = value.as_object().expect("object").keys().cloned().collect();
            match &reference {
                None => reference = Some(keys),
                Some(expected) => assert_eq!(
                    &keys,
                    expected,
                    "{:?} has the same key set as every other kind",
                    envelope.kind()
                ),
            }
        }
        assert_eq!(
            reference.expect("at least one kind").len(),
            20,
            "the envelope has twenty top-level fields"
        );
    }

    #[test]
    fn inapplicable_fields_are_null_and_never_absent() {
        let heartbeat = Envelope::heartbeat(&server(), "h1", 5).to_canonical_value();
        for field in [
            "mutation_status",
            "error",
            "progress",
            "auth",
            "project",
            "method",
            "event_name",
            "idempotency_id",
            "deadline_unix_ms",
            "reason",
        ] {
            assert_eq!(
                heartbeat.get(field),
                Some(&Value::Null),
                "{field} is present and null on a heartbeat"
            );
        }
    }

    #[test]
    fn the_origin_is_carried_and_never_inferred() {
        assert_eq!(
            Envelope::hello(&plugin(), &project(), "b".repeat(64), "n1").origin(),
            Origin::Plugin
        );
        assert_eq!(
            Envelope::heartbeat(&server(), "h1", 5).origin(),
            Origin::Server
        );
        assert_eq!(Origin::parse("server"), Some(Origin::Server));
        assert_eq!(Origin::parse("editor"), None);
    }

    #[test]
    fn malformed_and_undefined_messages_are_refused() {
        assert_eq!(Envelope::parse("{"), Err(EnvelopeError::Malformed));
        assert_eq!(Envelope::parse("[]"), {
            let value = json!([]);
            Err(validate_canonical(&value).expect_err("an array is not an envelope"))
        });

        let mut value = Envelope::heartbeat(&server(), "h1", 5).to_canonical_value();
        value["kind"] = json!("teleport");
        assert!(matches!(
            Envelope::from_canonical_value(&value),
            Err(EnvelopeError::Schema(_))
        ));

        let mut missing = Envelope::heartbeat(&server(), "h1", 5).to_canonical_value();
        missing.as_object_mut().expect("object").remove("kind");
        assert!(matches!(
            Envelope::from_canonical_value(&missing),
            Err(EnvelopeError::Schema(_))
        ));

        let mut extra = Envelope::heartbeat(&server(), "h1", 5).to_canonical_value();
        extra["surprise"] = json!(true);
        assert!(matches!(
            Envelope::from_canonical_value(&extra),
            Err(EnvelopeError::Schema(_))
        ));
    }

    #[test]
    fn every_kind_rejects_the_shape_of_every_other_kind() {
        for envelope in every_kind() {
            let original_kind = envelope.kind();
            for contradictory_kind in MessageKind::ALL {
                if contradictory_kind == original_kind {
                    continue;
                }
                let mut value = envelope.to_canonical_value();
                value["kind"] = json!(contradictory_kind.as_str());
                assert!(
                    validate_canonical(&value).is_err(),
                    "the schema admitted a {:?} envelope relabelled as {:?}",
                    original_kind,
                    contradictory_kind
                );
                assert!(
                    matches!(
                        Envelope::from_canonical_value(&value),
                        Err(EnvelopeError::Schema(_))
                    ),
                    "the parser admitted a {:?} envelope relabelled as {:?}",
                    original_kind,
                    contradictory_kind
                );
            }
        }
    }

    #[test]
    fn mutation_errors_require_and_round_trip_a_three_state_outcome() {
        let ordinary = Envelope::error(&plugin(), "r1", ErrorKind::Conflict, "failed");
        for status in MutationStatus::ALL {
            let value =
                Envelope::mutation_error(&plugin(), "r1", status, ErrorKind::Conflict, "failed")
                    .to_canonical_value();
            validate_canonical(&value)
                .unwrap_or_else(|error| panic!("mutation error {status} rejected: {error}"));
            let parsed = Envelope::from_canonical_value(&value).expect("parser agrees");
            assert_eq!(parsed.operation_class(), Some(OperationClass::Mutation));
            assert_eq!(parsed.mutation_status(), Some(status));
        }

        let mut missing_status = ordinary.to_canonical_value();
        missing_status["operation_class"] = json!(OperationClass::Mutation.as_str());
        assert!(validate_canonical(&missing_status).is_err());
        assert!(Envelope::from_canonical_value(&missing_status).is_err());

        let mut status_without_mutation = ordinary.to_canonical_value();
        status_without_mutation["mutation_status"] = json!(MutationStatus::Unresolved.as_str());
        assert!(validate_canonical(&status_without_mutation).is_err());
        assert!(Envelope::from_canonical_value(&status_without_mutation).is_err());
    }

    #[test]
    fn all_ten_error_kinds_round_trip_through_their_wire_codes() {
        for kind in [
            ErrorKind::InvalidInput,
            ErrorKind::NotFound,
            ErrorKind::Conflict,
            ErrorKind::Unsupported,
            ErrorKind::PermissionDenied,
            ErrorKind::Timeout,
            ErrorKind::Cancelled,
            ErrorKind::Unavailable,
            ErrorKind::Uncertain,
            ErrorKind::Internal,
        ] {
            assert_eq!(parse_error_kind(kind.code()), Some(kind));
            let envelope = Envelope::error(&plugin(), "r1", kind, "because");
            let text = String::from_utf8(canonical_bytes(&envelope)).expect("utf8");
            assert_eq!(
                Envelope::parse(&text).expect("parses").error_kind(),
                Some(kind)
            );
        }
        assert_eq!(parse_error_kind("teapot"), None);
    }

    #[test]
    fn a_deadline_is_absolute_and_expiry_is_decidable() {
        let deadline = Deadline::from_unix_ms(1_000);
        assert_eq!(deadline.unix_ms(), 1_000);
        assert!(!deadline.expired_at(999));
        assert!(deadline.expired_at(1_000));
        assert!(deadline.expired_at(1_001));
    }

    #[test]
    fn capabilities_sorts_and_dedups_its_method_list() {
        let envelope = Envelope::capabilities(&server(), &project(), &["b.two", "a.one", "b.two"]);
        assert_eq!(
            envelope.payload().expect("payload")["methods"],
            json!(["a.one", "b.two"])
        );
    }

    #[test]
    fn the_envelope_schema_version_is_pinned() {
        assert_eq!(ENVELOPE_SCHEMA_VERSION, "1");
        let value = Envelope::heartbeat(&server(), "h1", 5).to_canonical_value();
        assert_eq!(value["schema_version"], json!("1"));
    }
}
