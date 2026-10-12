//! The authenticated first handshake.
//!
//! # What `hello` must prove, in order
//!
//! A connection that has completed the RFC 6455 upgrade has proven exactly one
//! thing: something on this machine can open a loopback socket. The `hello`
//! envelope must then prove three more, and this module checks them in a fixed
//! order so a diagnostic always names the most fundamental disagreement:
//!
//! 1. **An authenticator is present at all.** A `hello` with no `auth` block is
//!    [`HandshakeError::MissingSecret`]. This is checked *before* schema validation
//!    on purpose: "you did not attempt to authenticate" is a more useful and more
//!    honest refusal than "your message failed field validation", and it is the one
//!    case where the schema violation and the security violation coincide.
//! 2. **The authenticator is this launch's secret.** Compared with
//!    [`constant_time_eq`](super::rendezvous::constant_time_eq) against the
//!    256-bit secret from the mode-0600 rendezvous file. Every successfully published
//!    launch has a fresh secret; Windows refuses unsafe replacement of a stale file
//!    until the operator removes it, rather than reusing its secret.
//! 3. **The pairing is compatible.** Same protocol MAJOR, same project object
//!    identities, same generation — [`super::compat::CompatibilityMatrix`].
//!
//! Only after all three pass is the handshake's **nonce** consumed. That ordering
//! matters: a failed attempt must not burn a nonce, or an attacker could deny a
//! legitimate plugin its handshake by replaying its `hello` with a wrong secret.
//!
//! # Why a nonce at all, when the secret is already per-launch
//!
//! The secret is per *launch*; the nonce is per *handshake attempt*. Without it, a
//! captured `hello` frame could be replayed to open a second session for as long as
//! the launch lives. [`Handshake::authenticate_value`] records every accepted nonce
//! and refuses a repeat with [`HandshakeError::ReplayedNonce`] even when the secret
//! is correct.
//!
//! # This is not an MCP handshake
//!
//! Nothing here reads or writes an MCP message. The plugin's whole vocabulary is
//! [`super::envelope::MessageKind`]; MCP initialisation, capabilities negotiation
//! and tool listing all happen in `godot-mcp-server`, a crate this one cannot even
//! import.

use std::collections::BTreeSet;
use std::sync::Mutex;

use godot_mcp_domain::ErrorKind;
use serde_json::Value;

use super::compat::{CompatibilityDecision, CompatibilityMatrix};
use super::envelope::{
    Envelope, EnvelopeError, MessageKind, PluginIdentity, ProjectClaim, ServerIdentity,
    validate_canonical,
};
use super::rendezvous::RendezvousFile;

/// The methods a freshly accepted session is told it may call.
///
/// Deliberately small and frozen by Todo 19. The plug-in and peer capabilities
/// planned for Todos 21–25 were cancelled. Advertising a name no shipped peer can
/// serve would be the mistake Todo 15 recorded — "connected" is not "supports
/// this method".
pub const BASE_METHODS: [&str; 2] = ["bridge.describe_session", "bridge.ping"];

/// Why a handshake was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HandshakeError {
    /// The message carried no authenticator at all.
    #[error("the hello message carries no authenticator")]
    MissingSecret,

    /// The authenticator did not match this launch's secret.
    #[error("the hello authenticator does not match this launch's rendezvous secret")]
    SecretMismatch,

    /// The nonce had already been accepted in this launch.
    #[error("the hello nonce has already been used in this launch")]
    ReplayedNonce,

    /// The message was not a `hello`.
    #[error("the first message was {observed:?}, not a hello")]
    NotHello {
        /// What arrived instead.
        observed: MessageKind,
    },

    /// The message named no project.
    #[error("the hello message names no project")]
    MissingProject,

    /// The pairing is not admitted.
    #[error("the handshake is incompatible: {0:?}")]
    Incompatible(CompatibilityDecision),

    /// The message was not a canonical envelope.
    #[error("the hello message is not a canonical envelope: {0}")]
    MalformedEnvelope(EnvelopeError),
}

impl HandshakeError {
    /// The classified error kind this refusal is reported as.
    ///
    /// Every authentication failure is `permission_denied`; a compatibility
    /// failure keeps the matrix's own classification, so an unsupported MAJOR
    /// stays `unsupported` rather than being flattened into a permission error.
    #[must_use]
    pub fn error_kind(&self) -> ErrorKind {
        match self {
            Self::MissingSecret | Self::SecretMismatch | Self::ReplayedNonce => {
                ErrorKind::PermissionDenied
            }
            Self::NotHello { .. } | Self::MissingProject | Self::MalformedEnvelope(_) => {
                ErrorKind::InvalidInput
            }
            Self::Incompatible(decision) => decision.error_kind().unwrap_or(ErrorKind::Internal),
        }
    }
}

/// What an accepted handshake established.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandshakeOutcome {
    /// The peer authenticated and is compatible.
    Accepted {
        /// The session identity the server assigned.
        session_id: String,
        /// The peer's identity, as presented.
        plugin: PluginIdentity,
        /// The nonce that was consumed.
        nonce: String,
    },
}

impl HandshakeOutcome {
    /// The authenticated peer.
    #[must_use]
    pub const fn plugin(&self) -> &PluginIdentity {
        match self {
            Self::Accepted { plugin, .. } => plugin,
        }
    }

    /// The session identity.
    #[must_use]
    pub fn session_id(&self) -> &str {
        match self {
            Self::Accepted { session_id, .. } => session_id,
        }
    }
}

/// The server side of the handshake: what it will accept, and what it has seen.
#[derive(Debug)]
pub struct Handshake {
    server: ServerIdentity,
    matrix: CompatibilityMatrix,
    rendezvous: RendezvousFile,
    methods: Vec<String>,
    seen_nonces: Mutex<BTreeSet<String>>,
}

impl Handshake {
    /// A handshake pinned to this server identity, project and rendezvous file.
    #[must_use]
    pub fn new(server: ServerIdentity, project: ProjectClaim, rendezvous: RendezvousFile) -> Self {
        let matrix = CompatibilityMatrix::new(&server.protocol_version, project);
        Self {
            server,
            matrix,
            rendezvous,
            methods: BASE_METHODS
                .iter()
                .map(|method| (*method).to_owned())
                .collect(),
            seen_nonces: Mutex::new(BTreeSet::new()),
        }
    }

    /// The same handshake advertising `methods` in addition to [`BASE_METHODS`]
    /// in its `capabilities` answer: the bridge methods this server may send to
    /// a plugin that implements them.
    #[must_use]
    pub fn with_methods(mut self, methods: &[&str]) -> Self {
        self.methods
            .extend(methods.iter().map(|method| (*method).to_owned()));
        self.methods.sort();
        self.methods.dedup();
        self
    }

    /// This server's identity.
    #[must_use]
    pub const fn server(&self) -> &ServerIdentity {
        &self.server
    }

    /// The project this handshake admits, and no other.
    #[must_use]
    pub const fn project(&self) -> &ProjectClaim {
        self.matrix.project()
    }

    /// The compatibility matrix in force.
    #[must_use]
    pub const fn matrix(&self) -> &CompatibilityMatrix {
        &self.matrix
    }

    pub(crate) const fn rendezvous(&self) -> &RendezvousFile {
        &self.rendezvous
    }

    /// Authenticate one already-decoded `hello` value.
    ///
    /// # Errors
    /// Returns the specific [`HandshakeError`] the message committed. On success
    /// the nonce is consumed, so a second identical call returns
    /// [`HandshakeError::ReplayedNonce`].
    pub fn authenticate_value(&self, value: &Value) -> Result<HandshakeOutcome, HandshakeError> {
        // 1. An authenticator, before anything else. See the module header.
        let auth = value.get("auth").filter(|auth| !auth.is_null());
        let Some(auth) = auth else {
            return Err(HandshakeError::MissingSecret);
        };
        let Some(secret_hex) = auth.get("secret_hex").and_then(Value::as_str) else {
            return Err(HandshakeError::MissingSecret);
        };
        let Some(nonce) = auth.get("nonce").and_then(Value::as_str) else {
            return Err(HandshakeError::MissingSecret);
        };

        // 2. The secret, in constant time.
        if !self.rendezvous.matches_secret_hex(secret_hex) {
            return Err(HandshakeError::SecretMismatch);
        }

        if let Some(observed) = value
            .get("kind")
            .and_then(Value::as_str)
            .and_then(MessageKind::parse)
            && observed != MessageKind::Hello
        {
            return Err(HandshakeError::NotHello { observed });
        }
        validate_canonical(value).map_err(HandshakeError::MalformedEnvelope)?;
        let envelope =
            Envelope::from_canonical_value(value).map_err(HandshakeError::MalformedEnvelope)?;
        if envelope.kind() != MessageKind::Hello {
            return Err(HandshakeError::NotHello {
                observed: envelope.kind(),
            });
        }
        let claimed = envelope
            .project()
            .ok_or(HandshakeError::MissingProject)?
            .clone();
        let plugin = PluginIdentity {
            plugin_version: envelope
                .identity()
                .plugin_version
                .clone()
                .unwrap_or_default(),
            protocol_version: envelope.protocol_version().to_owned(),
            godot_version: envelope
                .identity()
                .godot_version
                .clone()
                .unwrap_or_default(),
        };

        // 4. Compatibility.
        let decision = self.matrix.decide(&plugin, &claimed);
        if !decision.is_compatible() {
            return Err(HandshakeError::Incompatible(decision));
        }

        // 5. Only now is the nonce consumed.
        let mut seen = self
            .seen_nonces
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !seen.insert(nonce.to_owned()) {
            return Err(HandshakeError::ReplayedNonce);
        }

        Ok(HandshakeOutcome::Accepted {
            session_id: self.server.session_id.clone(),
            plugin,
            nonce: nonce.to_owned(),
        })
    }

    /// Authenticate one `hello` message as it arrived on the wire.
    ///
    /// # Errors
    /// Returns [`HandshakeError::MalformedEnvelope`] for unparsable JSON, plus
    /// every error [`Handshake::authenticate_value`] can return.
    pub fn authenticate_text(&self, text: &str) -> Result<HandshakeOutcome, HandshakeError> {
        let value: Value = serde_json::from_str(text)
            .map_err(|_json| HandshakeError::MalformedEnvelope(EnvelopeError::Malformed))?;
        self.authenticate_value(&value)
    }

    /// The `capabilities` answer for an accepted handshake.
    #[must_use]
    pub fn capabilities(&self) -> Envelope {
        let methods: Vec<&str> = self.methods.iter().map(String::as_str).collect();
        Envelope::capabilities(&self.server, self.matrix.project(), &methods)
    }

    /// The classified `error` answer for a refused handshake.
    ///
    /// A refused peer is told *that* it was refused and under which classified
    /// kind, never which of the three checks failed in detail — the detail goes to
    /// this server's own diagnostics.
    #[must_use]
    pub fn refusal(&self, error: &HandshakeError) -> Envelope {
        Envelope::error_from_server(
            &self.server,
            "hello",
            error.error_kind(),
            "the bridge handshake was refused",
        )
    }

    /// Every nonce this handshake has accepted, for diagnostics.
    #[must_use]
    pub fn accepted_nonces(&self) -> Vec<String> {
        self.seen_nonces
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    use godot_mcp_domain::ErrorKind;
    use serde_json::{Value, json};

    use super::super::compat::{CompatibilityDecision, PROTOCOL_MAJOR, PROTOCOL_VERSION};
    use super::super::envelope::{
        Envelope, MessageKind, PluginIdentity, ProjectClaim, ServerIdentity, canonical_bytes,
    };
    use super::super::rendezvous::RendezvousFile;
    use super::{BASE_METHODS, Handshake, HandshakeError, HandshakeOutcome};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    struct Scratch {
        dir: PathBuf,
    }

    impl Scratch {
        fn new() -> Self {
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "godot-mcp-handshake-{}-{unique}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create scratch");
            Self { dir }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
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

    fn server() -> ServerIdentity {
        ServerIdentity {
            server_version: "0.1.0".to_owned(),
            protocol_version: PROTOCOL_VERSION.to_owned(),
            session_id: "session-1".to_owned(),
        }
    }

    fn plugin() -> PluginIdentity {
        PluginIdentity {
            plugin_version: "0.1.0".to_owned(),
            protocol_version: PROTOCOL_VERSION.to_owned(),
            godot_version: "4.4.1.stable".to_owned(),
        }
    }

    fn endpoint() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 45_871)
    }

    fn rendezvous(scratch: &Scratch, name: &str) -> RendezvousFile {
        RendezvousFile::write_new(&scratch.dir.join(name), endpoint(), project()).expect("write")
    }

    fn hello_value(
        plugin: &PluginIdentity,
        claimed: &ProjectClaim,
        secret_hex: String,
        nonce: &str,
    ) -> Value {
        let envelope = Envelope::hello(plugin, claimed, secret_hex, nonce);
        serde_json::from_slice(&canonical_bytes(&envelope)).expect("canonical hello")
    }

    #[test]
    fn a_correct_hello_is_accepted_once_and_never_twice() {
        let scratch = Scratch::new();
        let file = rendezvous(&scratch, "ok");
        let handshake = Handshake::new(server(), project(), file.clone());
        let hello = hello_value(&plugin(), &project(), file.secret_hex(), "n1");

        let outcome = handshake.authenticate_value(&hello).expect("accepted");
        assert_eq!(
            outcome,
            HandshakeOutcome::Accepted {
                session_id: "session-1".to_owned(),
                plugin: plugin(),
                nonce: "n1".to_owned()
            }
        );
        assert_eq!(outcome.plugin().godot_version, "4.4.1.stable");
        assert_eq!(outcome.session_id(), "session-1");
        assert_eq!(handshake.accepted_nonces(), vec!["n1".to_owned()]);

        assert_eq!(
            handshake.authenticate_value(&hello),
            Err(HandshakeError::ReplayedNonce)
        );
    }

    #[test]
    fn capabilities_name_the_base_methods_plus_the_ones_the_server_will_send() {
        let scratch = Scratch::new();
        let file = rendezvous(&scratch, "methods");
        let base = Handshake::new(server(), project(), file.clone());
        assert_eq!(
            base.capabilities().payload(),
            Some(&serde_json::json!({ "methods": ["bridge.describe_session", "bridge.ping"] }))
        );
        let extended = Handshake::new(server(), project(), file).with_methods(&[
            "editor.state",
            "bridge.ping",
            "editor.scene.tree",
        ]);
        assert_eq!(
            extended.capabilities().payload(),
            Some(&serde_json::json!({ "methods": [
                "bridge.describe_session", "bridge.ping", "editor.scene.tree", "editor.state"
            ] }))
        );
    }

    #[test]
    fn a_missing_authenticator_is_refused_before_anything_else() {
        let scratch = Scratch::new();
        let file = rendezvous(&scratch, "noauth");
        let handshake = Handshake::new(server(), project(), file.clone());

        let mut hello = hello_value(&plugin(), &project(), file.secret_hex(), "n1");
        hello.as_object_mut().expect("object").remove("auth");
        assert_eq!(
            handshake.authenticate_value(&hello),
            Err(HandshakeError::MissingSecret)
        );

        let mut null_auth = hello_value(&plugin(), &project(), file.secret_hex(), "n1");
        null_auth["auth"] = Value::Null;
        assert_eq!(
            handshake.authenticate_value(&null_auth),
            Err(HandshakeError::MissingSecret)
        );

        let mut no_nonce = hello_value(&plugin(), &project(), file.secret_hex(), "n1");
        no_nonce["auth"]
            .as_object_mut()
            .expect("object")
            .remove("nonce");
        assert_eq!(
            handshake.authenticate_value(&no_nonce),
            Err(HandshakeError::MissingSecret)
        );
    }

    #[test]
    fn a_wrong_secret_is_refused_without_burning_the_nonce() {
        let scratch = Scratch::new();
        let file = rendezvous(&scratch, "wrong-secret");
        let handshake = Handshake::new(server(), project(), file.clone());

        let wrong = hello_value(&plugin(), &project(), "0".repeat(64), "n1");
        assert_eq!(
            handshake.authenticate_value(&wrong),
            Err(HandshakeError::SecretMismatch)
        );

        // The same nonce still works with the correct secret.
        let good = hello_value(&plugin(), &project(), file.secret_hex(), "n1");
        handshake.authenticate_value(&good).expect("accepted");
    }

    #[cfg(not(windows))]
    #[test]
    fn a_rotated_secret_refuses_the_previous_launch_secret() {
        let scratch = Scratch::new();
        let file = rendezvous(&scratch, "rotate");
        let stale = file.secret_hex();
        let rotated = file.rotate().expect("rotate");
        let handshake = Handshake::new(server(), project(), rotated);
        let replayed = hello_value(&plugin(), &project(), stale, "n1");

        assert_eq!(
            handshake.authenticate_value(&replayed),
            Err(HandshakeError::SecretMismatch)
        );
        assert!(handshake.accepted_nonces().is_empty());
    }

    #[test]
    fn an_incompatible_pairing_is_refused_without_burning_the_nonce() {
        let scratch = Scratch::new();
        let file = rendezvous(&scratch, "compat");
        let handshake = Handshake::new(server(), project(), file.clone());

        let mut future = plugin();
        future.protocol_version = format!("{}.0.0", PROTOCOL_MAJOR + 1);
        assert_eq!(
            handshake.authenticate_value(&hello_value(
                &future,
                &project(),
                file.secret_hex(),
                "n1"
            )),
            Err(HandshakeError::Incompatible(
                CompatibilityDecision::MajorVersionUnsupported {
                    expected: PROTOCOL_MAJOR,
                    observed: PROTOCOL_MAJOR + 1
                }
            ))
        );

        let mut other = project();
        other.root_object = "unix:dev=9,ino=9".to_owned();
        assert_eq!(
            handshake.authenticate_value(&hello_value(&plugin(), &other, file.secret_hex(), "n1")),
            Err(HandshakeError::Incompatible(
                CompatibilityDecision::ProjectRootMismatch
            ))
        );
        assert!(handshake.accepted_nonces().is_empty());
    }

    #[test]
    fn a_message_that_is_not_a_hello_is_refused() {
        let scratch = Scratch::new();
        let file = rendezvous(&scratch, "kind");
        let handshake = Handshake::new(server(), project(), file.clone());

        let mut heartbeat: Value =
            serde_json::from_slice(&canonical_bytes(&Envelope::heartbeat(&server(), "h1", 5)))
                .expect("canonical");
        heartbeat["auth"] = json!({"secret_hex": file.secret_hex(), "nonce": "n1"});
        assert_eq!(
            handshake.authenticate_value(&heartbeat),
            Err(HandshakeError::NotHello {
                observed: MessageKind::Heartbeat
            })
        );
    }

    #[test]
    fn a_non_canonical_message_is_refused_as_malformed() {
        let scratch = Scratch::new();
        let file = rendezvous(&scratch, "malformed");
        let handshake = Handshake::new(server(), project(), file.clone());

        let mut hello = hello_value(&plugin(), &project(), file.secret_hex(), "n1");
        hello.as_object_mut().expect("object").remove("kind");
        assert!(matches!(
            handshake.authenticate_value(&hello),
            Err(HandshakeError::MalformedEnvelope(_))
        ));

        assert!(matches!(
            handshake.authenticate_text("{not json"),
            Err(HandshakeError::MalformedEnvelope(_))
        ));
    }

    #[test]
    fn the_wire_path_and_the_value_path_agree() {
        let scratch = Scratch::new();
        let file = rendezvous(&scratch, "wire");
        let handshake = Handshake::new(server(), project(), file.clone());
        let envelope = Envelope::hello(&plugin(), &project(), file.secret_hex(), "n1");
        let text = String::from_utf8(canonical_bytes(&envelope)).expect("utf8");
        handshake.authenticate_text(&text).expect("accepted");
        assert_eq!(handshake.accepted_nonces(), vec!["n1".to_owned()]);
    }

    #[test]
    fn every_refusal_is_classified() {
        assert_eq!(
            HandshakeError::MissingSecret.error_kind(),
            ErrorKind::PermissionDenied
        );
        assert_eq!(
            HandshakeError::SecretMismatch.error_kind(),
            ErrorKind::PermissionDenied
        );
        assert_eq!(
            HandshakeError::ReplayedNonce.error_kind(),
            ErrorKind::PermissionDenied
        );
        assert_eq!(
            HandshakeError::NotHello {
                observed: MessageKind::Event
            }
            .error_kind(),
            ErrorKind::InvalidInput
        );
        assert_eq!(
            HandshakeError::Incompatible(CompatibilityDecision::MajorVersionUnsupported {
                expected: 1,
                observed: 2
            })
            .error_kind(),
            ErrorKind::Unsupported,
            "a version refusal keeps its own classification"
        );
        assert_eq!(
            HandshakeError::Incompatible(CompatibilityDecision::ProjectRootMismatch).error_kind(),
            ErrorKind::Conflict
        );
    }

    #[test]
    fn the_capabilities_answer_advertises_only_frozen_methods() {
        let scratch = Scratch::new();
        let file = rendezvous(&scratch, "caps");
        let handshake = Handshake::new(server(), project(), file);
        let capabilities = handshake.capabilities();
        assert_eq!(capabilities.kind(), MessageKind::Capabilities);
        let methods = capabilities.payload().expect("payload")["methods"]
            .as_array()
            .expect("array")
            .len();
        assert_eq!(methods, BASE_METHODS.len());
    }

    #[test]
    fn a_refusal_answer_carries_the_classification_and_no_detail() {
        let scratch = Scratch::new();
        let file = rendezvous(&scratch, "refuse");
        let handshake = Handshake::new(server(), project(), file);
        let refusal = handshake.refusal(&HandshakeError::SecretMismatch);
        assert_eq!(refusal.kind(), MessageKind::Error);
        assert_eq!(refusal.error_kind(), Some(ErrorKind::PermissionDenied));
        let message = &refusal.error_body().expect("body").message;
        assert!(
            !message.contains("secret") && !message.contains("nonce"),
            "the refusal does not tell the peer which check failed: {message}"
        );
    }
}
