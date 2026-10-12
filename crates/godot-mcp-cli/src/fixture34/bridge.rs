//! The deterministic local fake editor bridge.
//!
//! Todo 34's `editor_runtime` proof kind is proven by an authoritative bridge
//! receipt plus a live state probe, and a null receipt or a null probe always
//! fails proof by design — a file hash can never substitute. So the fixture needs
//! a bridge that actually answers, and the qualification host has no Godot editor.
//!
//! This module is that bridge, and it is **not** a second protocol. It speaks
//! exactly Todo 19's frozen wire contract: [`BridgeListener`] binds an ephemeral
//! loopback port, [`RendezvousFile`] writes the mode-0600 file with a fresh
//! 256-bit per-launch secret inside the fixture tree, and [`Handshake`]
//! authenticates the peer's `hello` against that secret, the same project object
//! identity and generation, and a compatible protocol MAJOR. Nothing here relaxes
//! any of those checks.
//!
//! # Which fixture a caller is asking about
//!
//! The peer names the transaction it wants a receipt for in the `hello` nonce,
//! spelled `task34-<transaction-id>` (or `task34-probe-<transaction-id>` for the
//! observation setup makes for itself). Reusing the frozen `auth.nonce` field for
//! this has a useful consequence: the handshake consumes each nonce exactly once,
//! so "answers exactly one authoritative receipt/live-probe request per
//! `editor_runtime` fixture" is enforced by the protocol's own replay refusal
//! rather than by bookkeeping here.
//!
//! # Why it runs as its own process
//!
//! `setup` has to return while the bridge keeps listening, so the bridge cannot be
//! a thread. It is this same binary re-executed with
//! [`BRIDGE_CONFIG_VARIABLE`] set, which keeps the command-line surface at exactly
//! the two subcommands the contract allows.

use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use godot_mcp_bridge::protocol::compat::PROTOCOL_VERSION;
use godot_mcp_bridge::protocol::envelope::{
    Deadline, Envelope, MessageKind, OperationClass, PluginIdentity, ProjectClaim, ServerIdentity,
    canonical_bytes,
};
use godot_mcp_bridge::protocol::handshake::{Handshake, HandshakeOutcome};
use godot_mcp_bridge::protocol::listener::BridgeListener;
use godot_mcp_bridge::protocol::rendezvous::RendezvousFile;
use godot_mcp_bridge::protocol::ws::{
    Opcode, WsFrame, client_handshake_request, encode_client_text, handshake_head_len,
    parse_server_handshake,
};
use godot_mcp_engine::supervision::procfs;
use serde::{Deserialize, Serialize};

use super::FixtureError;
use super::state::BridgeRecord;
use super::tree::FixturePaths;

/// The environment variable that turns this binary into the fake bridge.
pub const BRIDGE_CONFIG_VARIABLE: &str = "GODOT_MCP_TASK34_BRIDGE_CONFIG";

/// The method the bridge presents its authoritative receipt under.
pub const RECEIPT_METHOD: &str = "bridge.repair_receipt";

/// The session identity the fake bridge assigns.
pub const BRIDGE_SESSION_ID: &str = "task34-fixture-bridge";

/// The plugin identity the fixture's own probe presents.
pub const PROBE_PLUGIN_VERSION: &str = "task34-fixture-probe";

/// The Godot version the fixture's own probe presents. There is no editor here,
/// and the fixture says so rather than impersonating a release.
pub const PROBE_GODOT_VERSION: &str = "0.0.0-task34-fixture";

/// How long setup waits for the bridge process to report that it is listening.
pub const READY_DEADLINE: Duration = Duration::from_secs(30);

/// How long a probe round trip may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// How far ahead the bridge sets a request deadline.
const REQUEST_DEADLINE_MS: u64 = 30_000;

/// The masking key the fixture's probe uses. RFC 6455 requires client frames to
/// be masked; the value is not a security property (the server unmasks it
/// unconditionally), and a fixed key keeps the fixture deterministic.
const PROBE_MASK: [u8; 4] = [0x2f, 0x8b, 0xc1, 0x74];

/// The `Sec-WebSocket-Key` the fixture's probe sends. Also not a security
/// property: it only proves the server computed `Sec-WebSocket-Accept` correctly.
const PROBE_WS_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";

/// What setup hands the bridge process before spawning it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeConfig {
    /// The project both sides must agree on, exactly.
    pub project_object_identity: String,
    /// OS object identity of the project root directory.
    pub project_root_object: String,
    /// OS object identity of `project.godot`.
    pub project_file_object: String,
    /// The project generation in force.
    pub project_generation: u64,
    /// Where to write the mode-0600 rendezvous file.
    pub rendezvous_path: String,
    /// Where to report readiness once the port is bound.
    pub ready_path: String,
    /// The `editor_runtime` transactions this bridge will answer for.
    pub transactions: Vec<String>,
}

impl BridgeConfig {
    fn claim(&self) -> ProjectClaim {
        ProjectClaim {
            object_identity: self.project_object_identity.clone(),
            root_object: self.project_root_object.clone(),
            file_object: self.project_file_object.clone(),
            generation: self.project_generation,
        }
    }
}

/// What the bridge process reports once it is listening.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeReady {
    /// The bridge process's own PID.
    pub pid: u32,
    /// The loopback endpoint it bound.
    pub endpoint: String,
    /// The inode of its listening socket, when the platform reports one.
    pub socket_inode: Option<u64>,
}

/// One authoritative receipt, as the live bridge reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedReceipt {
    /// Whether the bridge answered at all.
    pub reachable: bool,
    /// The transaction the receipt is bound to.
    pub transaction_id: Option<String>,
    /// The plugin-acknowledged action/operation identifier.
    pub action_id: Option<String>,
    /// The live state-probe result.
    pub live_state_probe: Option<String>,
}

/// The action identifier the bridge binds to one transaction.
#[must_use]
pub fn action_id_for(transaction_id: &str) -> String {
    format!("task34-editor-action-{transaction_id}")
}

/// The live state-probe answer the bridge reports for one transaction.
#[must_use]
pub fn live_state_for(transaction_id: &str) -> String {
    format!("node:/root/Task34Fixture:{transaction_id}")
}

/// The `hello` nonce a repair-time caller uses for one transaction.
#[must_use]
pub fn repair_nonce(transaction_id: &str) -> String {
    format!("task34-{transaction_id}")
}

/// The `hello` nonce setup uses for its own live observation, kept distinct so it
/// never consumes the one a later repair needs.
#[must_use]
pub fn probe_nonce(transaction_id: &str) -> String {
    format!("task34-probe-{transaction_id}")
}

/// Recover the transaction a nonce names, or `None` for a foreign nonce.
#[must_use]
pub fn transaction_of_nonce(nonce: &str) -> Option<&str> {
    let rest = nonce.strip_prefix("task34-")?;
    Some(rest.strip_prefix("probe-").unwrap_or(rest))
}

/// Spawn the fake bridge as its own process and wait until it is listening.
///
/// # Errors
/// Returns [`FixtureError::Bridge`] when the process cannot be started, does not
/// report readiness within [`READY_DEADLINE`], or reports an unusable endpoint.
pub fn spawn(
    paths: &FixturePaths,
    config: &BridgeConfig,
) -> Result<(BridgeRecord, SocketAddr), FixtureError> {
    let config_path = paths.bridge_config();
    let bytes = serde_json::to_vec_pretty(config).map_err(|error| FixtureError::Bridge {
        detail: format!("the bridge configuration could not be serialized: {error}"),
    })?;
    super::tree::write_file(&config_path, &bytes)?;
    let ready_path = PathBuf::from(&config.ready_path);
    let _ = std::fs::remove_file(&ready_path);

    let program = std::env::current_exe().map_err(|error| FixtureError::Bridge {
        detail: format!("the fixture binary's own path is unknown: {error}"),
    })?;
    let log = std::fs::File::create(paths.bridge_log()).map_err(|error| FixtureError::Bridge {
        detail: format!("the bridge log could not be created: {error}"),
    })?;
    let child = Command::new(&program)
        .arg("setup")
        .arg("--scenario")
        .arg(super::SCENARIO)
        .env(BRIDGE_CONFIG_VARIABLE, &config_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        .spawn()
        .map_err(|error| FixtureError::Bridge {
            detail: format!("the bridge process could not be started: {error}"),
        })?;
    let pid = child.id();
    drop(child);

    let ready = await_ready(&ready_path, pid)?;
    let endpoint: SocketAddr = ready
        .endpoint
        .parse()
        .map_err(|error| FixtureError::Bridge {
            detail: format!(
                "the bridge reported the unusable endpoint {}: {error}",
                ready.endpoint
            ),
        })?;
    let observed = super::survivor::observe(pid)?;
    Ok((
        BridgeRecord {
            pid,
            start_time: observed.start_time,
            endpoint: ready.endpoint,
            socket_inode: ready.socket_inode,
            rendezvous_path: config.rendezvous_path.clone(),
        },
        endpoint,
    ))
}

/// Whether the recorded bridge process is still the one holding its socket.
///
/// All of PID, start time and — where the platform reports it — the listening
/// socket's inode must match. A PID that no longer holds that exact socket is a
/// different process, and is never signalled.
///
/// # Errors
/// Returns [`FixtureError::Process`] when the platform cannot answer.
pub fn is_live(record: &BridgeRecord) -> Result<bool, FixtureError> {
    let Some(recorded) = record.start_time.as_deref() else {
        return Ok(false);
    };
    let identity = match procfs::process_identity(record.pid) {
        Ok(Some(identity)) => identity,
        Ok(None) => return Ok(false),
        Err(error) => {
            return Err(FixtureError::Process {
                detail: format!("bridge pid {} could not be observed: {error}", record.pid),
            });
        }
    };
    if !identity.matches_recorded(record.pid, Some(recorded), None) {
        return Ok(false);
    }
    match record.socket_inode {
        None => Ok(true),
        Some(inode) => {
            procfs::holds_socket_inode(record.pid, inode).map_err(|error| FixtureError::Process {
                detail: format!(
                    "bridge pid {} could not be checked against socket inode {inode}: {error}",
                    record.pid
                ),
            })
        }
    }
}

/// Terminate the exact bridge process the record identifies, and prove it is gone.
///
/// Returns `true` when a live listener was terminated. Nothing is signalled unless
/// the recorded PID, start time and socket inode all match.
///
/// # Errors
/// Returns [`FixtureError::NotTerminated`] when the listener is still there after
/// [`super::survivor::TERMINATION_DEADLINE`]; the wait is bounded.
pub fn terminate(record: &BridgeRecord) -> Result<bool, FixtureError> {
    if !is_live(record)? {
        return Ok(false);
    }
    if let Err(error) = procfs::kill_pid(record.pid) {
        return Err(FixtureError::NotTerminated {
            detail: format!(
                "the bridge listener (pid {}) could not be signalled: {error}",
                record.pid
            ),
        });
    }
    let deadline = Instant::now() + super::survivor::TERMINATION_DEADLINE;
    loop {
        if !is_live(record)? {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Err(FixtureError::NotTerminated {
                detail: format!(
                    "the bridge listener (pid {}) is still live {}s after being signalled",
                    record.pid,
                    super::survivor::TERMINATION_DEADLINE.as_secs()
                ),
            });
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Ask the live bridge for one transaction's authoritative receipt and live probe.
///
/// Every field of the returned observation comes from the bridge's answer on the
/// wire. Nothing is filled in locally, so a bridge that does not answer yields an
/// unreachable observation rather than a fabricated receipt.
///
/// # Errors
/// Returns [`FixtureError::Bridge`] when the rendezvous file is unusable or the
/// round trip fails.
pub fn probe(
    rendezvous_path: &Path,
    endpoint: SocketAddr,
    transaction_id: &str,
    nonce: &str,
) -> Result<ObservedReceipt, FixtureError> {
    let rendezvous =
        RendezvousFile::read(rendezvous_path).map_err(|error| FixtureError::Bridge {
            detail: format!(
                "the bridge rendezvous file {} is unusable: {error}",
                rendezvous_path.display()
            ),
        })?;
    let plugin = PluginIdentity {
        plugin_version: PROBE_PLUGIN_VERSION.to_owned(),
        protocol_version: PROTOCOL_VERSION.to_owned(),
        godot_version: PROBE_GODOT_VERSION.to_owned(),
    };
    let hello = Envelope::hello(
        &plugin,
        rendezvous.project(),
        rendezvous.secret_hex(),
        nonce,
    );

    let mut stream = TcpStream::connect(endpoint).map_err(|error| FixtureError::Bridge {
        detail: format!("the bridge at {endpoint} refused a connection: {error}"),
    })?;
    stream
        .set_read_timeout(Some(PROBE_TIMEOUT))
        .and_then(|()| stream.set_write_timeout(Some(PROBE_TIMEOUT)))
        .map_err(|error| FixtureError::Bridge {
            detail: format!("the bridge connection could not be bounded: {error}"),
        })?;
    write_all(
        &mut stream,
        client_handshake_request(endpoint, PROBE_WS_KEY).as_bytes(),
    )?;

    let mut buffer = Vec::new();
    let head = read_head(&mut stream, &mut buffer)?;
    parse_server_handshake(&head, PROBE_WS_KEY).map_err(|error| FixtureError::Bridge {
        detail: format!("the bridge's opening handshake was rejected: {error}"),
    })?;

    write_text(&mut stream, &hello)?;
    // The listener answers an accepted `hello` with `capabilities`, then presents
    // its receipt as one request. A refusal arrives as an `error` instead.
    let first = read_envelope(&mut stream, &mut buffer)?;
    if first.kind() != MessageKind::Capabilities {
        return Ok(ObservedReceipt {
            reachable: false,
            transaction_id: None,
            action_id: None,
            live_state_probe: None,
        });
    }
    let receipt = read_envelope(&mut stream, &mut buffer)?;
    if receipt.kind() != MessageKind::Request || receipt.method() != Some(RECEIPT_METHOD) {
        return Ok(ObservedReceipt {
            reachable: false,
            transaction_id: None,
            action_id: None,
            live_state_probe: None,
        });
    }
    let payload = receipt.payload().cloned().unwrap_or_default();
    let observed = ObservedReceipt {
        reachable: true,
        transaction_id: string_field(&payload, "receipt_transaction_id"),
        action_id: string_field(&payload, "receipt_action_id"),
        live_state_probe: string_field(&payload, "live_state_probe"),
    };
    if observed.transaction_id.as_deref() != Some(transaction_id) {
        return Err(FixtureError::Bridge {
            detail: format!(
                "the bridge answered for {:?}, not {transaction_id}",
                observed.transaction_id
            ),
        });
    }
    let acknowledgement = Envelope::result(
        &plugin,
        receipt.message_id(),
        godot_mcp_bridge::protocol::mutation::MutationStatus::NotStarted,
        &serde_json::json!({ "acknowledged": true }),
    );
    write_text(&mut stream, &acknowledgement)?;
    Ok(observed)
}

/// Run the fake bridge until it is terminated. Never returns on success.
///
/// # Errors
/// Returns [`FixtureError::Bridge`] when the configuration is unusable or the port
/// cannot be bound.
pub fn serve(config_path: &Path) -> Result<std::convert::Infallible, FixtureError> {
    let text = std::fs::read_to_string(config_path).map_err(|error| FixtureError::Bridge {
        detail: format!(
            "the bridge configuration {} is unreadable: {error}",
            config_path.display()
        ),
    })?;
    let config: BridgeConfig =
        serde_json::from_str(&text).map_err(|error| FixtureError::Bridge {
            detail: format!(
                "the bridge configuration {} is malformed: {error}",
                config_path.display()
            ),
        })?;

    let listener = BridgeListener::bind_loopback().map_err(|error| FixtureError::Bridge {
        detail: format!("the bridge could not bind a loopback port: {error}"),
    })?;
    let endpoint = listener.endpoint();
    let rendezvous_path = PathBuf::from(&config.rendezvous_path);
    let rendezvous = RendezvousFile::write_new(&rendezvous_path, endpoint, config.claim())
        .map_err(|error| FixtureError::Bridge {
            detail: format!("the bridge rendezvous file could not be written: {error}"),
        })?;
    let server = ServerIdentity {
        server_version: env!("CARGO_PKG_VERSION").to_owned(),
        protocol_version: PROTOCOL_VERSION.to_owned(),
        session_id: BRIDGE_SESSION_ID.to_owned(),
    };
    let handshake = Handshake::new(server.clone(), config.claim(), rendezvous);

    let ready = BridgeReady {
        pid: std::process::id(),
        endpoint: endpoint.to_string(),
        socket_inode: listening_socket_inode(endpoint),
    };
    let bytes = serde_json::to_vec_pretty(&ready).map_err(|error| FixtureError::Bridge {
        detail: format!("the bridge readiness record could not be serialized: {error}"),
    })?;
    super::tree::write_file(Path::new(&config.ready_path), &bytes)?;

    loop {
        let Ok(mut session) = listener.accept_authenticated(&handshake) else {
            // A refused peer has already been told, over the wire, that it was
            // refused. The bridge keeps listening: one bad dial is not a reason to
            // take the fixture's editor plane down.
            continue;
        };
        let HandshakeOutcome::Accepted { nonce, .. } = session.outcome();
        let transaction = transaction_of_nonce(nonce)
            .filter(|candidate| config.transactions.iter().any(|known| known == candidate))
            .map(str::to_owned);
        let envelope = match transaction {
            Some(transaction) => Envelope::request(
                &server,
                &format!("receipt-{transaction}"),
                &format!("task34-receipt-{transaction}"),
                RECEIPT_METHOD,
                OperationClass::Control,
                Deadline::from_unix_ms(now_unix_ms().saturating_add(REQUEST_DEADLINE_MS)),
                &serde_json::json!({
                    "live_state_probe": live_state_for(&transaction),
                    "receipt_action_id": action_id_for(&transaction),
                    "receipt_transaction_id": transaction,
                }),
            ),
            None => Envelope::error_from_server(
                &server,
                "hello",
                godot_mcp_domain::ErrorKind::NotFound,
                "this bridge holds no receipt for the requested transaction",
            ),
        };
        let _ = session.send(&envelope);
        let _ = session.receive();
        drop(session);
    }
}

fn await_ready(ready_path: &Path, pid: u32) -> Result<BridgeReady, FixtureError> {
    let deadline = Instant::now() + READY_DEADLINE;
    loop {
        if let Ok(text) = std::fs::read_to_string(ready_path)
            && let Ok(ready) = serde_json::from_str::<BridgeReady>(&text)
            && ready.pid == pid
        {
            return Ok(ready);
        }
        if Instant::now() >= deadline {
            return Err(FixtureError::Bridge {
                detail: format!(
                    "the bridge process (pid {pid}) did not report readiness within {}s",
                    READY_DEADLINE.as_secs()
                ),
            });
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

fn string_field(payload: &serde_json::Value, name: &str) -> Option<String> {
    payload
        .get(name)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

fn write_all(stream: &mut TcpStream, bytes: &[u8]) -> Result<(), FixtureError> {
    stream
        .write_all(bytes)
        .and_then(|()| stream.flush())
        .map_err(|error| FixtureError::Bridge {
            detail: format!("the bridge connection could not be written to: {error}"),
        })
}

fn write_text(stream: &mut TcpStream, envelope: &Envelope) -> Result<(), FixtureError> {
    let text = String::from_utf8(canonical_bytes(envelope)).unwrap_or_default();
    let frame = encode_client_text(&text, PROBE_MASK).map_err(|error| FixtureError::Bridge {
        detail: format!("a bridge message could not be framed: {error}"),
    })?;
    write_all(stream, &frame)
}

fn read_head(stream: &mut TcpStream, buffer: &mut Vec<u8>) -> Result<Vec<u8>, FixtureError> {
    loop {
        match handshake_head_len(buffer) {
            Ok(Some(length)) => {
                let head = buffer[..length].to_vec();
                buffer.drain(..length);
                return Ok(head);
            }
            Ok(None) => fill(stream, buffer)?,
            Err(error) => {
                return Err(FixtureError::Bridge {
                    detail: format!("the bridge's opening handshake was unusable: {error}"),
                });
            }
        }
    }
}

fn read_envelope(stream: &mut TcpStream, buffer: &mut Vec<u8>) -> Result<Envelope, FixtureError> {
    loop {
        match WsFrame::parse(buffer) {
            Ok(frame) => {
                buffer.drain(..frame.consumed);
                match frame.opcode {
                    Opcode::Close => {
                        return Err(FixtureError::Bridge {
                            detail: "the bridge closed the connection".to_owned(),
                        });
                    }
                    Opcode::Ping | Opcode::Pong => continue,
                    Opcode::Text => {
                        let text = String::from_utf8(frame.payload).map_err(|error| {
                            FixtureError::Bridge {
                                detail: format!("a bridge message was not UTF-8: {error}"),
                            }
                        })?;
                        return Envelope::parse(&text).map_err(|error| FixtureError::Bridge {
                            detail: format!(
                                "a bridge message was not a canonical envelope: {error}"
                            ),
                        });
                    }
                }
            }
            Err(error) if error.is_incomplete() => fill(stream, buffer)?,
            Err(error) => {
                return Err(FixtureError::Bridge {
                    detail: format!("a bridge frame was rejected: {error}"),
                });
            }
        }
    }
}

fn fill(stream: &mut TcpStream, buffer: &mut Vec<u8>) -> Result<(), FixtureError> {
    let mut chunk = [0_u8; 8192];
    let read = stream
        .read(&mut chunk)
        .map_err(|error| FixtureError::Bridge {
            detail: format!("the bridge connection could not be read: {error}"),
        })?;
    if read == 0 {
        return Err(FixtureError::Bridge {
            detail: "the bridge closed the connection mid-message".to_owned(),
        });
    }
    buffer.extend_from_slice(&chunk[..read]);
    Ok(())
}

/// The inode of the listening socket bound to `endpoint` in this process.
///
/// Cleanup uses this to prove the recorded PID is the process that owns that exact
/// socket. Reported as `None` on any platform that does not expose it, which makes
/// the identity check fall back to PID + start time alone.
#[cfg(target_os = "linux")]
fn listening_socket_inode(endpoint: SocketAddr) -> Option<u64> {
    const LISTEN: &str = "0A";
    let port = endpoint.port();
    for table in ["/proc/self/net/tcp", "/proc/self/net/tcp6"] {
        let Ok(text) = std::fs::read_to_string(table) else {
            continue;
        };
        for line in text.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let (Some(local), Some(state), Some(inode)) =
                (fields.first(), fields.get(3), fields.get(9))
            else {
                continue;
            };
            let _ = local;
            if *state != LISTEN {
                continue;
            }
            let Some(local_port) = fields
                .get(1)
                .and_then(|address| address.rsplit(':').next())
                .and_then(|hex| u16::from_str_radix(hex, 16).ok())
            else {
                continue;
            };
            if local_port != port {
                continue;
            }
            if let Ok(inode) = inode.parse::<u64>()
                && procfs::holds_socket_inode(std::process::id(), inode).unwrap_or(false)
            {
                return Some(inode);
            }
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn listening_socket_inode(endpoint: SocketAddr) -> Option<u64> {
    let _ = endpoint;
    None
}
