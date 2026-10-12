//! The deterministic fake GDScript plugin, and a dependency-free temporary
//! directory.
//!
//! The fake plugin is what a real `addons/godot_mcp/` client would do, minus the
//! editor: read the mode-0600 rendezvous file, dial the loopback endpoint it
//! names, perform the RFC 6455 upgrade, present a `hello` carrying the per-launch
//! secret, then answer whatever the server sends. It speaks the *production*
//! codec and the *production* envelope, so what the acceptance suite exercises is
//! the shipped protocol rather than a mock of it.
//!
//! Nothing here spawns a process and nothing here needs Godot.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use godot_mcp_bridge::protocol::envelope::{
    Envelope, MessageKind, OperationClass, PluginIdentity, canonical_bytes,
};
use godot_mcp_bridge::protocol::mutation::MutationStatus;
use godot_mcp_bridge::protocol::rendezvous::RendezvousFile;
use godot_mcp_bridge::protocol::ws::{
    Opcode, WsFrame, client_handshake_request, encode_client_text, parse_server_handshake,
};
use godot_mcp_domain::ErrorKind;
use serde_json::json;

// ---------------------------------------------------------------------------
// TempDir
// ---------------------------------------------------------------------------

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A unique temporary directory, removed on drop. No external crate needed.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// Create a fresh directory named after `label`, this process and a counter.
    #[must_use]
    pub fn new(label: &str) -> Self {
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("godot-mcp-{label}-{}-{unique}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create the temporary directory");
        Self { path }
    }

    /// The directory's path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

// ---------------------------------------------------------------------------
// The fake plugin
// ---------------------------------------------------------------------------

/// What the fake plugin does after it is connected and authenticated.
#[derive(Debug, Clone)]
pub enum PluginScript {
    /// Answer every server message the way a healthy plugin would.
    Happy {
        /// The identity presented in `hello`.
        identity: PluginIdentity,
    },
}

impl PluginScript {
    /// The healthy script.
    #[must_use]
    pub const fn happy(identity: PluginIdentity) -> Self {
        Self::Happy { identity }
    }

    const fn identity(&self) -> &PluginIdentity {
        match self {
            Self::Happy { identity } => identity,
        }
    }
}

/// A running fake plugin.
pub struct FakePlugin {
    handle: JoinHandle<Result<(), String>>,
}

impl FakePlugin {
    /// Read `rendezvous_path`, dial the endpoint it names and run `script`.
    #[must_use]
    pub fn spawn(rendezvous_path: &Path, script: PluginScript) -> Self {
        let path = rendezvous_path.to_path_buf();
        let handle = std::thread::spawn(move || run(&path, &script));
        Self { handle }
    }

    /// Wait for the plugin thread and surface whatever it reported.
    ///
    /// # Errors
    /// Returns the plugin's own diagnostic when its script did not complete.
    pub fn join(self) -> Result<(), String> {
        match self.handle.join() {
            Ok(result) => result,
            Err(_panic) => Err("the fake plugin thread panicked".to_owned()),
        }
    }
}

fn run(rendezvous_path: &Path, script: &PluginScript) -> Result<(), String> {
    let rendezvous =
        RendezvousFile::read(rendezvous_path).map_err(|error| format!("rendezvous: {error}"))?;
    let endpoint = rendezvous.endpoint();

    let mut stream = TcpStream::connect(endpoint).map_err(|error| format!("connect: {error}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|error| format!("read timeout: {error}"))?;

    // RFC 6455 upgrade, client side.
    let key = "dGhlIHNhbXBsZSBub25jZQ==";
    let request = client_handshake_request(endpoint, key);
    stream
        .write_all(request.as_bytes())
        .map_err(|error| format!("write upgrade: {error}"))?;

    let mut buffer = Vec::new();
    let head = read_until_double_crlf(&mut stream, &mut buffer)?;
    parse_server_handshake(&head, key).map_err(|error| format!("upgrade response: {error}"))?;

    // hello, carrying the per-launch secret read from the 0600 file.
    let hello = Envelope::hello(
        script.identity(),
        rendezvous.project(),
        rendezvous.secret_hex(),
        "nonce-0001",
    );
    send(&mut stream, &hello)?;

    // capabilities comes back first.
    let capabilities = recv(&mut stream, &mut buffer)?;
    if capabilities.kind() != MessageKind::Capabilities {
        return Err(format!(
            "expected capabilities, observed {:?}",
            capabilities.kind()
        ));
    }

    let identity = script.identity().clone();
    loop {
        let message = recv(&mut stream, &mut buffer)?;
        match message.kind() {
            MessageKind::Heartbeat => {
                let reply = Envelope::heartbeat_reply(&identity, message.message_id());
                send(&mut stream, &reply)?;
            }
            MessageKind::Request => {
                let reply = Envelope::result(
                    &identity,
                    message.message_id(),
                    match message.operation_class() {
                        Some(OperationClass::Mutation) => MutationStatus::Committed,
                        _ => MutationStatus::NotStarted,
                    },
                    &json!({"selected": ["res://main.tscn::Player"]}),
                );
                send(&mut stream, &reply)?;
            }
            MessageKind::Cancel => {
                let target = message
                    .correlation_id()
                    .unwrap_or_else(|| message.message_id())
                    .to_owned();
                let reply = Envelope::error(
                    &identity,
                    &target,
                    ErrorKind::Cancelled,
                    "the request was cancelled before it started",
                );
                send(&mut stream, &reply)?;
            }
            MessageKind::Shutdown => return Ok(()),
            other => return Err(format!("unexpected server message {other:?}")),
        }
    }
}

fn send(stream: &mut TcpStream, envelope: &Envelope) -> Result<(), String> {
    let text = String::from_utf8(canonical_bytes(envelope))
        .map_err(|error| format!("canonical bytes: {error}"))?;
    let frame = encode_client_text(&text, [0x37, 0xfa, 0x21, 0x3d])
        .map_err(|error| format!("encode: {error}"))?;
    stream
        .write_all(&frame)
        .map_err(|error| format!("write frame: {error}"))
}

fn recv(stream: &mut TcpStream, buffer: &mut Vec<u8>) -> Result<Envelope, String> {
    loop {
        match WsFrame::parse(buffer) {
            Ok(frame) => {
                let consumed = frame.consumed;
                let payload = frame.payload.clone();
                let opcode = frame.opcode;
                buffer.drain(..consumed);
                if opcode == Opcode::Close {
                    return Err("the server closed the connection".to_owned());
                }
                if opcode != Opcode::Text {
                    continue;
                }
                let text =
                    String::from_utf8(payload).map_err(|error| format!("payload utf8: {error}"))?;
                return Envelope::parse(&text).map_err(|error| format!("parse: {error}"));
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

fn read_until_double_crlf(stream: &mut TcpStream, buffer: &mut Vec<u8>) -> Result<Vec<u8>, String> {
    loop {
        if let Some(offset) = find(buffer, b"\r\n\r\n") {
            let head = buffer[..offset + 4].to_vec();
            buffer.drain(..offset + 4);
            return Ok(head);
        }
        let mut chunk = [0_u8; 1024];
        let read = stream
            .read(&mut chunk)
            .map_err(|error| format!("read head: {error}"))?;
        if read == 0 {
            return Err("the server closed before completing the upgrade".to_owned());
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
