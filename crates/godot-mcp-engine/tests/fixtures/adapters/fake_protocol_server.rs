//! A deterministic in-process fake of Godot's two editor-hosted protocol servers.
//!
//! # Why a real socket
//!
//! Godot's language server and debug adapter are editor plugins, so exercising the
//! real ones needs a running editor. This fake stands in for them at the level that
//! matters: it binds a real loopback `TcpListener`, accepts real connections and
//! speaks the real `Content-Length` framing, so the code under test is the production
//! codec and the production channel — not a mock of them.
//!
//! # Why it is deterministic
//!
//! Every behaviour is scripted up front, and the script is a committed JSON fixture.
//! There is no sleeping, no polling loop with a timeout, and no dependence on how the
//! kernel happens to segment a write:
//!
//! * [`Delivery::Whole`] writes a frame in one `write_all`;
//! * [`Delivery::Fragmented`] writes it in fixed-size pieces, each flushed, so the
//!   reader provably sees a partial header and a partial body;
//! * [`Delivery::Coalesced`] concatenates the next several frames into one
//!   `write_all`, so the reader provably sees more than one frame per read.
//!
//! Fragmentation and coalescing are therefore properties of the script, not of
//! timing. A test that asserts them cannot pass by accident on a fast machine and
//! fail on a slow one.

#![allow(
    dead_code,
    reason = "one committed fake serves both acceptance cases; each uses the subset it needs"
)]

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::{Value, json};

/// How one scripted reply reaches the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// One `write_all` per frame.
    Whole,
    /// The frame split into pieces of this many bytes, each flushed separately.
    Fragmented(usize),
    /// This frame plus the next `n - 1` replies written together in one `write_all`.
    Coalesced(usize),
    /// Nothing is written for the inbound frame this reply answers: Godot's
    /// `req_launch` answers only once `configurationDone` arrives.
    Withheld,
}

/// One scripted reply.
#[derive(Debug, Clone)]
pub struct Reply {
    /// The JSON payload, already in the protocol's own shape.
    pub payload: Value,
    /// How it is written.
    pub delivery: Delivery,
}

impl Reply {
    /// A reply written in one piece.
    pub fn whole(payload: Value) -> Self {
        Self {
            payload,
            delivery: Delivery::Whole,
        }
    }

    /// A reply split into `piece` byte pieces.
    pub fn fragmented(payload: Value, piece: usize) -> Self {
        Self {
            payload,
            delivery: Delivery::Fragmented(piece),
        }
    }

    /// A reply written together with the next `count - 1` replies.
    pub fn coalesced(payload: Value, count: usize) -> Self {
        Self {
            payload,
            delivery: Delivery::Coalesced(count),
        }
    }

    /// No reply at all to the inbound frame it is scripted for.
    pub fn withheld() -> Self {
        Self {
            payload: Value::Null,
            delivery: Delivery::Withheld,
        }
    }
}

/// A raw byte sequence the fake emits instead of a well-formed frame.
#[derive(Debug, Clone)]
pub enum Malformed {
    /// A header declaring more than the engine's 4 MiB ceiling.
    OversizeDeclaration(u64),
    /// A header section with no `Name: value` structure.
    BadHeader,
    /// A header with no `Content-Length` at all.
    NoContentLength,
    /// Close the connection without answering.
    CloseImmediately,
}

/// What the fake does when a connection is made.
#[derive(Debug, Clone)]
pub enum Script {
    /// Answer each inbound frame from this queue, in order.
    Replies(Vec<Reply>),
    /// Emit raw malformed bytes after the first inbound frame.
    Malformed(Malformed),
    /// Accept the connection and never answer anything.
    Silent,
}

/// A running fake protocol server.
pub struct FakeProtocolServer {
    address: SocketAddr,
    accepted: Arc<AtomicUsize>,
    received: Arc<Mutex<Vec<Value>>>,
    handle: Option<JoinHandle<()>>,
    shutdown: Arc<Mutex<Option<TcpStream>>>,
}

impl FakeProtocolServer {
    /// Bind a loopback listener and serve `script` to every connection.
    ///
    /// `max_connections` bounds the accept loop so the thread always terminates
    /// without needing a timeout.
    pub fn start(script: Script, max_connections: usize) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind a loopback listener");
        let address = listener.local_addr().expect("the bound address");
        let accepted = Arc::new(AtomicUsize::new(0));
        let received = Arc::new(Mutex::new(Vec::new()));
        let accepted_thread = Arc::clone(&accepted);
        let received_thread = Arc::clone(&received);
        let handle = std::thread::spawn(move || {
            for _ in 0..max_connections {
                let Ok((stream, _peer)) = listener.accept() else {
                    return;
                };
                accepted_thread.fetch_add(1, Ordering::SeqCst);
                serve(stream, &script, &received_thread);
            }
        });
        Self {
            address,
            accepted,
            received,
            handle: Some(handle),
            shutdown: Arc::new(Mutex::new(None)),
        }
    }

    /// The bound loopback address.
    pub const fn address(&self) -> SocketAddr {
        self.address
    }

    /// The bound port.
    pub fn port(&self) -> u16 {
        self.address.port()
    }

    /// How many connections have been accepted.
    pub fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }

    /// Every request payload the fake received, in order.
    pub fn received(&self) -> Vec<Value> {
        self.received.lock().expect("the request log").clone()
    }

    /// The `command` / `method` names received, in order.
    pub fn received_names(&self) -> Vec<String> {
        self.received()
            .into_iter()
            .filter_map(|message| {
                message
                    .get("command")
                    .or_else(|| message.get("method"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect()
    }

    /// Wait for the accept loop to finish. Called by `Drop` too.
    pub fn join(&mut self) {
        let _unused = self.shutdown.lock().map(|mut slot| slot.take());
        if let Some(handle) = self.handle.take() {
            let _ignored = handle.join();
        }
    }
}

impl Drop for FakeProtocolServer {
    fn drop(&mut self) {
        // Unblock a pending accept by connecting once, then join. Without this a
        // script that serves fewer connections than `max_connections` would leave the
        // thread parked in `accept`. A loop that already ended has closed its
        // listener, and a host that answers a closed loopback port with silence
        // instead of a reset (as some macOS CI hosts do) would hold a plain
        // connect for its whole timeout, so it is bounded and skipped then.
        if self
            .handle
            .as_ref()
            .is_some_and(|handle| !handle.is_finished())
        {
            let _ignored = TcpStream::connect_timeout(&self.address, Duration::from_secs(2));
        }
        self.join();
    }
}

/// A loopback address nothing is listening on.
///
/// Bind a listener, record its port, then drop it. On Linux loopback the port is not
/// immediately reassigned, so a connect to it is refused — which is exactly the "no
/// editor is hosting this protocol" case.
pub fn closed_loopback_address() -> SocketAddr {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind a loopback listener");
    let address = listener.local_addr().expect("the bound address");
    drop(listener);
    address
}

fn serve(mut stream: TcpStream, script: &Script, received: &Arc<Mutex<Vec<Value>>>) {
    stream.set_nodelay(true).ok();
    // Inbound bytes past one frame belong to the next: a client may send two
    // requests before reading either answer.
    let mut pending: Vec<u8> = Vec::new();
    match script {
        Script::Silent => {
            // Read one frame and answer nothing, so the client's own bound applies.
            let _ignored = read_frame(&mut stream, &mut pending, received);
        }
        Script::Malformed(malformed) => {
            let _ignored = read_frame(&mut stream, &mut pending, received);
            match malformed {
                Malformed::OversizeDeclaration(declared) => {
                    let header = format!("Content-Length: {declared}\r\n\r\n");
                    let _ignored = stream.write_all(header.as_bytes());
                }
                Malformed::BadHeader => {
                    let _ignored = stream.write_all(b"this-is-not-a-header\r\n\r\n{}");
                }
                Malformed::NoContentLength => {
                    let _ignored = stream.write_all(b"Content-Type: application/json\r\n\r\n{}");
                }
                Malformed::CloseImmediately => {}
            }
            let _ignored = stream.flush();
        }
        Script::Replies(replies) => {
            let mut index = 0_usize;
            while index < replies.len() {
                if read_frame(&mut stream, &mut pending, received).is_none() {
                    return;
                }
                let reply = &replies[index];
                match reply.delivery {
                    Delivery::Withheld => index += 1,
                    Delivery::Whole => {
                        let _ignored = stream.write_all(&frame_of(&reply.payload));
                        index += 1;
                    }
                    Delivery::Fragmented(piece) => {
                        let bytes = frame_of(&reply.payload);
                        for chunk in bytes.chunks(piece.max(1)) {
                            let _ignored = stream.write_all(chunk);
                            let _ignored = stream.flush();
                        }
                        index += 1;
                    }
                    Delivery::Coalesced(count) => {
                        let mut blob = Vec::new();
                        let end = (index + count.max(1)).min(replies.len());
                        for reply in &replies[index..end] {
                            blob.extend_from_slice(&frame_of(&reply.payload));
                        }
                        let _ignored = stream.write_all(&blob);
                        index = end;
                    }
                }
                let _ignored = stream.flush();
            }
            drain(&mut stream);
        }
    }
}

/// Read until the client closes, so this end never closes first while the
/// client still sends: Windows answers a close with unread bytes by resetting
/// the connection, which the client sees as aborted before it has read the
/// last reply. Bounded, so a client that never closes cannot hold the thread.
fn drain(stream: &mut TcpStream) {
    let _ignored = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let mut sink = [0_u8; 512];
    while matches!(stream.read(&mut sink), Ok(read) if read > 0) {}
}

fn read_frame(
    stream: &mut TcpStream,
    buffer: &mut Vec<u8>,
    received: &Arc<Mutex<Vec<Value>>>,
) -> Option<Value> {
    let mut chunk = [0_u8; 512];
    let header_end = loop {
        if let Some(offset) = find(buffer, b"\r\n\r\n") {
            break offset;
        }
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    let header = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let length: usize = header
        .lines()
        .find_map(|line| line.strip_prefix("Content-Length: "))
        .and_then(|value| value.trim().parse().ok())?;
    let body_start = header_end + 4;
    while buffer.len() < body_start + length {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let message: Value = serde_json::from_slice(&buffer[body_start..body_start + length]).ok()?;
    buffer.drain(..body_start + length);
    received
        .lock()
        .expect("the request log")
        .push(message.clone());
    Some(message)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn frame_of(payload: &Value) -> Vec<u8> {
    let body = serde_json::to_vec(payload).expect("a scripted payload serializes");
    let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    frame.extend_from_slice(&body);
    frame
}

// ---------------------------------------------------------------------------
// The committed fixture-editor receipt
// ---------------------------------------------------------------------------

/// The committed fixture-editor receipt, as stored on disk.
///
/// This is the whole point of the ownership check: the endpoint's legitimacy comes
/// from a launch **this server** performed, recorded in the launch journal. The
/// fixture carries the same facts Todo 14's protocol writes — the argv it chose
/// (including the port flags), the child's PID and start time, and the revalidated
/// binary hash.
#[derive(Debug, Clone)]
pub struct FixtureEditorReceipt {
    /// The launch transaction ID.
    pub transaction_id: String,
    /// The opaque project object identity.
    pub project_object_identity: String,
    /// The project root's OS object identity.
    pub project_root_object: String,
    /// The `project.godot` OS object identity.
    pub project_file_object: String,
    /// The project generation at launch.
    pub project_generation: u64,
    /// The project root URI the editor was launched against.
    pub project_root_uri: String,
    /// The argument array the launch recorded at `intent_fsynced`, with `{lsp_port}`
    /// and `{dap_port}` placeholders for the ports the test binds.
    pub argv_template: Vec<String>,
    /// The editor child's PID.
    pub child_pid: u32,
    /// The editor child's process start time.
    pub child_start_time: String,
    /// The revalidated engine binary hash.
    pub binary_sha256: String,
    /// The containment boundary's identity.
    pub boundary_identity: String,
}

impl FixtureEditorReceipt {
    /// Load the committed fixture.
    pub fn load(path: &Path) -> Self {
        let bytes =
            std::fs::read(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        let value: Value = serde_json::from_slice(&bytes)
            .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()));
        let text = |key: &str| -> String {
            value[key]
                .as_str()
                .unwrap_or_else(|| panic!("{} has no string {key}", path.display()))
                .to_owned()
        };
        Self {
            transaction_id: text("transaction_id"),
            project_object_identity: text("project_object_identity"),
            project_root_object: text("project_root_object"),
            project_file_object: text("project_file_object"),
            project_generation: value["project_generation"]
                .as_u64()
                .expect("project_generation is a number"),
            project_root_uri: text("project_root_uri"),
            argv_template: value["argv"]
                .as_array()
                .expect("argv is an array")
                .iter()
                .map(|item| item.as_str().expect("argv element is a string").to_owned())
                .collect(),
            child_pid: u32::try_from(value["child_pid"].as_u64().expect("child_pid is a number"))
                .expect("child_pid fits in u32"),
            child_start_time: text("child_start_time"),
            binary_sha256: text("binary_sha256"),
            boundary_identity: text("boundary_identity"),
        }
    }

    /// The recorded argv with the fixture's port placeholders filled in.
    pub fn argv(&self, lsp_port: u16, dap_port: u16) -> Vec<String> {
        self.argv_template
            .iter()
            .map(|item| {
                item.replace("{lsp_port}", &lsp_port.to_string())
                    .replace("{dap_port}", &dap_port.to_string())
            })
            .collect()
    }

    /// The launch intent facts, exactly as Todo 14's protocol records them.
    pub fn intent_facts(&self, lsp_port: u16, dap_port: u16) -> BTreeMap<String, Value> {
        let mut intent = BTreeMap::new();
        intent.insert("boundary_kind".to_owned(), json!("container_adapter"));
        intent.insert(
            "boundary_identity".to_owned(),
            json!(self.boundary_identity),
        );
        intent.insert("argv".to_owned(), json!(self.argv(lsp_port, dap_port)));
        intent.insert("binary_sha256".to_owned(), json!(self.binary_sha256));
        intent.insert(
            "project_generation".to_owned(),
            json!(self.project_generation),
        );
        intent.insert(
            "project_root_object".to_owned(),
            json!(self.project_root_object),
        );
        intent.insert(
            "project_file_object".to_owned(),
            json!(self.project_file_object),
        );
        intent
    }

    /// The launch receipt facts, exactly as Todo 14's protocol records them.
    pub fn receipt_facts(&self) -> BTreeMap<String, Value> {
        let mut receipt = BTreeMap::new();
        receipt.insert("child_pid".to_owned(), json!(self.child_pid));
        receipt.insert("child_start_time".to_owned(), json!(self.child_start_time));
        receipt.insert("boundary_membership_confirmed".to_owned(), json!(true));
        receipt.insert("binary_sha256".to_owned(), json!(self.binary_sha256));
        receipt.insert(
            "project_root_object".to_owned(),
            json!(self.project_root_object),
        );
        receipt
    }
}
