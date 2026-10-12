//! A scripted stand-in for the `godot_mcp` editor plugin.
//!
//! It is what the GDScript `bridge_client.gd` does, minus the editor: read the
//! private rendezvous file, dial the loopback endpoint, upgrade to WebSocket,
//! present `hello` with the per-launch secret and the project claim, then
//! answer the server. It speaks the production codec and envelope, so the
//! server code under test is the shipped bridge. Its behaviour per method is
//! scripted, which is how a disconnect in the middle of a mutation or a plugin
//! that never answers is reproduced deterministically.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use godot_mcp_bridge::protocol::compat::PROTOCOL_VERSION;
use godot_mcp_bridge::protocol::envelope::{
    Envelope, MessageKind, OperationClass, PluginIdentity, canonical_bytes,
};
use godot_mcp_bridge::protocol::mutation::MutationStatus;
use godot_mcp_bridge::protocol::rendezvous::RendezvousFile;
use godot_mcp_bridge::protocol::ws::{
    Opcode, WsFrame, client_handshake_request, encode_client_text, parse_server_handshake,
};
use godot_mcp_domain::ErrorKind;
use serde_json::{Value, json};

/// What the fake plugin does when a request for one method arrives.
#[derive(Debug, Clone, PartialEq)]
pub enum FakeAnswer {
    /// Answer with this payload (a mutation reports `committed`).
    Payload(Value),
    /// Answer with a classified error and, for a mutation, this status.
    Error(ErrorKind, MutationStatus),
    /// Write `contents` to `path`, as an editor saving a scene does, then
    /// answer with `payload` plus the written bytes' `sha256`.
    SaveThenAnswer {
        path: PathBuf,
        contents: Vec<u8>,
        payload: Value,
    },
    /// Close the connection without answering, as an editor that crashed or
    /// lost its socket mid-request would.
    Disconnect,
    /// Answer with this payload, then end the whole process with `code`, as a
    /// game's runtime probe does when told to stop. Only a fake running as its
    /// own process (the `qa-fake-game` stand-in) may use it.
    PayloadThenExit { payload: Value, code: i32 },
    /// Answer with `cancelled` as soon as the server withdraws the request,
    /// as a runtime probe stopping an input sequence does; answer with
    /// `finished` when no withdrawal arrives `within` that long. A named
    /// `marker` file reads `received <method>` once the request arrived and
    /// `withdrawn <method>` once it was withdrawn.
    PayloadOnCancel {
        cancelled: Value,
        finished: Value,
        within: Duration,
        marker: Option<PathBuf>,
    },
    /// Refuse with `kind` as soon as the server withdraws the request, as a
    /// runtime probe abandoning a recording does; answer with `finished` when
    /// no withdrawal arrives `within` that long. `marker` as for
    /// [`FakeAnswer::PayloadOnCancel`].
    ErrorOnCancel {
        kind: ErrorKind,
        finished: Value,
        within: Duration,
        marker: Option<PathBuf>,
    },
    /// Never answer.
    Ignore,
}

/// One scripted plugin.
#[derive(Debug, Clone)]
pub struct FakePluginScript {
    /// The bridge methods the plugin reports in `bridge.describe_session`.
    pub methods: Vec<String>,
    /// Per-method answers; a method not listed answers `{}`.
    pub answers: BTreeMap<String, FakeAnswer>,
    /// Whether heartbeats are answered.
    pub answer_heartbeats: bool,
}

impl FakePluginScript {
    /// A healthy plugin implementing `methods`.
    #[must_use]
    pub fn implementing(methods: &[&str]) -> Self {
        Self {
            methods: methods.iter().map(|method| (*method).to_owned()).collect(),
            answers: BTreeMap::new(),
            answer_heartbeats: true,
        }
    }

    /// Script `method`'s answer.
    #[must_use]
    pub fn answering(mut self, method: &str, answer: FakeAnswer) -> Self {
        self.answers.insert(method.to_owned(), answer);
        self
    }
}

/// A running fake plugin.
pub struct FakeEditorPlugin {
    received: Arc<Mutex<Vec<(String, Value)>>>,
    handle: Option<JoinHandle<Result<(), String>>>,
}

impl FakeEditorPlugin {
    /// Wait until `rendezvous` exists, then dial and serve `script`.
    #[must_use]
    pub fn spawn(rendezvous: &Path, script: FakePluginScript) -> Self {
        let received = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&received);
        let path: PathBuf = rendezvous.to_path_buf();
        let handle = std::thread::spawn(move || serve(&path, &script, &recorded));
        Self {
            received,
            handle: Some(handle),
        }
    }

    /// Every request received so far: its method and payload, in order.
    #[must_use]
    pub fn received(&self) -> Vec<(String, Value)> {
        self.received
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Wait for the plugin thread to end and return how it ended.
    ///
    /// # Errors
    /// Returns the plugin's own description of a failure.
    pub fn join(mut self) -> Result<(), String> {
        match self.handle.take().map(JoinHandle::join) {
            Some(Ok(result)) => result,
            Some(Err(_)) => Err("the fake plugin panicked".to_owned()),
            None => Ok(()),
        }
    }
}

fn identity() -> PluginIdentity {
    PluginIdentity {
        plugin_version: "0.2.0".to_owned(),
        protocol_version: PROTOCOL_VERSION.to_owned(),
        godot_version: "4.7.1.stable.fake".to_owned(),
    }
}

fn read_rendezvous(path: &Path) -> Result<RendezvousFile, String> {
    let started = Instant::now();
    loop {
        match RendezvousFile::read(path) {
            Ok(file) => return Ok(file),
            Err(error) if started.elapsed() > Duration::from_secs(20) => {
                return Err(format!("rendezvous: {error}"));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

fn serve(
    rendezvous: &Path,
    script: &FakePluginScript,
    received: &Mutex<Vec<(String, Value)>>,
) -> Result<(), String> {
    let file = read_rendezvous(rendezvous)?;
    let endpoint = file.endpoint();
    let mut stream = TcpStream::connect(endpoint).map_err(|error| format!("connect: {error}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .map_err(|error| format!("timeout: {error}"))?;
    let key = "dGhlIHNhbXBsZSBub25jZQ==";
    stream
        .write_all(client_handshake_request(endpoint, key).as_bytes())
        .map_err(|error| format!("upgrade: {error}"))?;
    let mut buffer = Vec::new();
    let head = read_head(&mut stream, &mut buffer)?;
    parse_server_handshake(&head, key).map_err(|error| format!("upgrade answer: {error}"))?;

    let plugin = identity();
    // A nonce is single-use per server launch, so every dial presents a new one.
    static DIALS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nonce = format!(
        "fake-{}-{}",
        std::process::id(),
        DIALS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    send(
        &mut stream,
        &Envelope::hello(&plugin, file.project(), file.secret_hex(), &nonce),
    )?;
    let capabilities = receive(&mut stream, &mut buffer)?;
    if capabilities.kind() != MessageKind::Capabilities {
        return Err(format!(
            "expected capabilities, got {:?}",
            capabilities.kind()
        ));
    }

    loop {
        let message = match receive(&mut stream, &mut buffer) {
            Ok(message) => message,
            Err(closed) if closed.contains("closed") => return Ok(()),
            Err(error) => return Err(error),
        };
        match message.kind() {
            MessageKind::Heartbeat => {
                if script.answer_heartbeats {
                    send(
                        &mut stream,
                        &Envelope::heartbeat_reply(&plugin, message.message_id()),
                    )?;
                }
            }
            MessageKind::Request => {
                let method = message.method().unwrap_or_default().to_owned();
                let payload = message.payload().cloned().unwrap_or(Value::Null);
                received
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push((method.clone(), payload));
                let mutation = message.operation_class() == Some(OperationClass::Mutation);
                if method == "bridge.describe_session" {
                    send(
                        &mut stream,
                        &Envelope::result(
                            &plugin,
                            message.message_id(),
                            MutationStatus::NotStarted,
                            &json!({ "methods": script.methods, "plugin_version": "0.2.0" }),
                        ),
                    )?;
                    continue;
                }
                match script
                    .answers
                    .get(&method)
                    .cloned()
                    .unwrap_or(FakeAnswer::Payload(json!({})))
                {
                    FakeAnswer::Payload(payload) => send(
                        &mut stream,
                        &Envelope::result(
                            &plugin,
                            message.message_id(),
                            if mutation {
                                MutationStatus::Committed
                            } else {
                                MutationStatus::NotStarted
                            },
                            &payload,
                        ),
                    )?,
                    FakeAnswer::Error(kind, status) => {
                        let envelope = if mutation {
                            Envelope::mutation_error(
                                &plugin,
                                message.message_id(),
                                status,
                                kind,
                                "scripted failure",
                            )
                        } else {
                            Envelope::error(&plugin, message.message_id(), kind, "scripted failure")
                        };
                        send(&mut stream, &envelope)?;
                    }
                    FakeAnswer::SaveThenAnswer {
                        path,
                        contents,
                        payload,
                    } => {
                        std::fs::write(&path, &contents)
                            .map_err(|error| format!("save {}: {error}", path.display()))?;
                        let mut payload = payload;
                        payload["sha256"] = Value::String(sha256_hex(&contents));
                        send(
                            &mut stream,
                            &Envelope::result(
                                &plugin,
                                message.message_id(),
                                MutationStatus::Committed,
                                &payload,
                            ),
                        )?;
                    }
                    FakeAnswer::Disconnect => {
                        let _ = stream.shutdown(std::net::Shutdown::Both);
                        return Ok(());
                    }
                    FakeAnswer::PayloadThenExit { payload, code } => {
                        send(
                            &mut stream,
                            &Envelope::result(
                                &plugin,
                                message.message_id(),
                                MutationStatus::Committed,
                                &payload,
                            ),
                        )?;
                        let _ = stream.flush();
                        let _ = stream.shutdown(std::net::Shutdown::Write);
                        std::process::exit(code);
                    }
                    FakeAnswer::PayloadOnCancel {
                        cancelled,
                        finished,
                        within,
                        marker,
                    } => {
                        if let Some(marker) = &marker {
                            std::fs::write(marker, format!("received {method}"))
                                .map_err(|error| format!("marker: {error}"))?;
                        }
                        let withdrawn = await_withdrawal(
                            &mut stream,
                            &mut buffer,
                            &plugin,
                            script,
                            message.message_id(),
                            within,
                        )?;
                        if withdrawn && let Some(marker) = &marker {
                            std::fs::write(marker, format!("withdrawn {method}"))
                                .map_err(|error| format!("marker: {error}"))?;
                        }
                        send(
                            &mut stream,
                            &Envelope::result(
                                &plugin,
                                message.message_id(),
                                MutationStatus::Committed,
                                if withdrawn { &cancelled } else { &finished },
                            ),
                        )?;
                    }
                    FakeAnswer::ErrorOnCancel {
                        kind,
                        finished,
                        within,
                        marker,
                    } => {
                        if let Some(marker) = &marker {
                            std::fs::write(marker, format!("received {method}"))
                                .map_err(|error| format!("marker: {error}"))?;
                        }
                        let withdrawn = await_withdrawal(
                            &mut stream,
                            &mut buffer,
                            &plugin,
                            script,
                            message.message_id(),
                            within,
                        )?;
                        if withdrawn {
                            if let Some(marker) = &marker {
                                std::fs::write(marker, format!("withdrawn {method}"))
                                    .map_err(|error| format!("marker: {error}"))?;
                            }
                            send(
                                &mut stream,
                                &Envelope::error(
                                    &plugin,
                                    message.message_id(),
                                    kind,
                                    "the request was withdrawn",
                                ),
                            )?;
                        } else {
                            send(
                                &mut stream,
                                &Envelope::result(
                                    &plugin,
                                    message.message_id(),
                                    MutationStatus::Committed,
                                    &finished,
                                ),
                            )?;
                        }
                    }
                    FakeAnswer::Ignore => {}
                }
            }
            MessageKind::Cancel | MessageKind::Event => {}
            MessageKind::Shutdown => return Ok(()),
            other => return Err(format!("unexpected server message {other:?}")),
        }
    }
}

/// Read until the server withdraws request `message_id` (true) or `within`
/// passes (false), answering heartbeats meanwhile.
fn await_withdrawal(
    stream: &mut TcpStream,
    buffer: &mut Vec<u8>,
    plugin: &PluginIdentity,
    script: &FakePluginScript,
    message_id: &str,
    within: Duration,
) -> Result<bool, String> {
    let deadline = Instant::now() + within;
    let withdrawn = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break false;
        }
        stream
            .set_read_timeout(Some(left))
            .map_err(|error| format!("timeout: {error}"))?;
        let message = match receive(stream, buffer) {
            Ok(message) => message,
            // The read timed out with the wait: nothing was withdrawn.
            Err(error)
                if error.starts_with("read: ")
                    && Instant::now() + Duration::from_millis(50) >= deadline =>
            {
                break false;
            }
            Err(error) => return Err(error),
        };
        match message.kind() {
            MessageKind::Cancel if message.correlation_id() == Some(message_id) => break true,
            MessageKind::Heartbeat if script.answer_heartbeats => send(
                stream,
                &Envelope::heartbeat_reply(plugin, message.message_id()),
            )?,
            _ => {}
        }
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .map_err(|error| format!("timeout: {error}"))?;
    Ok(withdrawn)
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn send(stream: &mut TcpStream, envelope: &Envelope) -> Result<(), String> {
    let text = String::from_utf8(canonical_bytes(envelope)).map_err(|error| error.to_string())?;
    let frame =
        encode_client_text(&text, [0x37, 0xfa, 0x21, 0x3d]).map_err(|error| error.to_string())?;
    stream
        .write_all(&frame)
        .map_err(|error| format!("write: {error}"))
}

fn receive(stream: &mut TcpStream, buffer: &mut Vec<u8>) -> Result<Envelope, String> {
    loop {
        match WsFrame::parse(buffer) {
            Ok(frame) => {
                let consumed = frame.consumed;
                let opcode = frame.opcode;
                let payload = frame.payload.clone();
                buffer.drain(..consumed);
                match opcode {
                    Opcode::Close => return Err("the server closed the connection".to_owned()),
                    Opcode::Text => {
                        let text = String::from_utf8(payload).map_err(|error| error.to_string())?;
                        return Envelope::parse(&text).map_err(|error| format!("parse: {error}"));
                    }
                    _ => {}
                }
            }
            Err(error) if error.is_incomplete() => {
                let mut chunk = [0_u8; 4096];
                let read = stream
                    .read(&mut chunk)
                    .map_err(|error| format!("read: {error}"))?;
                if read == 0 {
                    return Err("the server closed the stream".to_owned());
                }
                buffer.extend_from_slice(&chunk[..read]);
            }
            Err(error) => return Err(format!("frame: {error}")),
        }
    }
}

fn read_head(stream: &mut TcpStream, buffer: &mut Vec<u8>) -> Result<Vec<u8>, String> {
    loop {
        if let Some(offset) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            let head = buffer[..offset + 4].to_vec();
            buffer.drain(..offset + 4);
            return Ok(head);
        }
        let mut chunk = [0_u8; 1024];
        let read = stream
            .read(&mut chunk)
            .map_err(|error| format!("read head: {error}"))?;
        if read == 0 {
            return Err("the server closed before the upgrade".to_owned());
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
}
