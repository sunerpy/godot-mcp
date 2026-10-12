//! The loopback-only listener and one authenticated session.
//!
//! # The port never leaves loopback
//!
//! [`BridgeListener::bind_loopback`] binds the IPv4 loopback host on an ephemeral
//! port, as the plan requires, and [`BridgeListener::bind_address`] **refuses** any
//! address that is not a loopback address — the IPv4 and IPv6 wildcard addresses
//! included. There is no
//! configuration knob, no environment variable and no argument that widens this;
//! binding publicly is not reachable from this API. A unit test scans this
//! module's own shipped source to prove no wildcard bind address appears in it.
//!
//! Loopback is necessary and **not sufficient**: every accepted connection must
//! still authenticate with the per-launch rendezvous secret before one request is
//! dispatched. See [`super::handshake`].
//!
//! # One session, framed and bounded
//!
//! [`BridgeSession`] owns the socket after the upgrade and the authenticated
//! `hello`. It reads whole WebSocket text frames through [`super::ws`], so a
//! coalesced or short read is handled by the codec rather than by ad-hoc buffering
//! here, and a frame declaring more than [`MAX_FRAME_BYTES`] is refused from its
//! header — the session then closes rather than resynchronising onto
//! attacker-chosen bytes, exactly as Todo 18's codec does.
//!
//! # Admission is bounded
//!
//! [`MAX_SESSIONS`] is 1. Unlike the LSP/DAP plane, where Godot's own
//! `LSP_MAX_CLIENTS` is 8, the editor bridge has exactly one legitimate peer: the
//! one `EditorPlugin` in the one editor this server is paired with. A second
//! concurrent session would mean either a second editor claiming the same project
//! or an impersonator, and neither should be served.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use godot_mcp_domain::ErrorKind;

use super::envelope::{Envelope, EnvelopeError, PluginIdentity, canonical_bytes};
use super::handshake::{Handshake, HandshakeError, HandshakeOutcome};
use super::rendezvous::{RendezvousError, RendezvousFile, ephemeral_loopback, is_loopback};
use super::ws::{
    MAX_FRAME_BYTES, Opcode, WsError, WsFrame, encode_close, encode_server_frame,
    encode_server_text, handshake_head_len, parse_client_handshake, server_handshake_response,
};

/// How many concurrent authenticated sessions the bridge serves. Exactly one.
pub const MAX_SESSIONS: usize = 1;

/// How long a session waits for one frame before giving up.
pub const SESSION_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// How often a bounded accept re-polls its non-blocking listener.
const ACCEPT_POLL: Duration = Duration::from_millis(20);

/// Why the listener or a session failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ListenerError {
    /// A bind was attempted on an address that is not loopback.
    #[error("the bridge listener binds loopback only; refused a non-loopback address")]
    NotLoopback,

    /// The socket operation failed.
    #[error("bridge socket failure: {detail}")]
    Io {
        /// What the OS reported.
        detail: String,
    },

    /// The peer violated the WebSocket layer.
    #[error("bridge framing failure: {0}")]
    Frame(WsError),

    /// The peer failed the authenticated handshake.
    #[error("bridge handshake refused: {0}")]
    Handshake(HandshakeError),

    /// A message was not a canonical envelope.
    #[error("bridge message rejected: {0}")]
    Envelope(EnvelopeError),

    /// The session could not clean up its rendezvous file.
    #[error("bridge rendezvous cleanup failed: {0}")]
    Rendezvous(RendezvousError),

    /// The peer closed the connection.
    #[error("the peer closed the bridge connection")]
    PeerClosed,

    /// A second concurrent session was attempted.
    #[error("the bridge serves {MAX_SESSIONS} session; a further connection was refused")]
    TooManySessions,
}

impl ListenerError {
    /// The classified error kind this failure is reported as.
    #[must_use]
    pub fn error_kind(&self) -> ErrorKind {
        match self {
            Self::NotLoopback => ErrorKind::InvalidInput,
            Self::Io { .. } | Self::PeerClosed | Self::Rendezvous(_) => ErrorKind::Unavailable,
            Self::Frame(error) => {
                if error.is_protocol_violation() {
                    ErrorKind::InvalidInput
                } else {
                    ErrorKind::Unavailable
                }
            }
            Self::Handshake(error) => error.error_kind(),
            Self::Envelope(_) => ErrorKind::InvalidInput,
            Self::TooManySessions => ErrorKind::Conflict,
        }
    }

    fn io(error: &std::io::Error) -> Self {
        Self::Io {
            detail: error.to_string(),
        }
    }
}

/// The bridge's loopback listener.
#[derive(Debug)]
pub struct BridgeListener {
    listener: TcpListener,
    endpoint: SocketAddr,
}

impl BridgeListener {
    /// Bind an ephemeral loopback port.
    ///
    /// # Errors
    /// Returns [`ListenerError::Io`] when the bind fails.
    pub fn bind_loopback() -> Result<Self, ListenerError> {
        Self::bind_address(ephemeral_loopback())
    }

    /// Bind `address`, which must be a loopback address.
    ///
    /// # Errors
    /// Returns [`ListenerError::NotLoopback`] for any non-loopback address —
    /// including the wildcard addresses — and [`ListenerError::Io`] when the bind
    /// itself fails.
    pub fn bind_address(address: SocketAddr) -> Result<Self, ListenerError> {
        if !is_loopback(address) {
            return Err(ListenerError::NotLoopback);
        }
        let listener = TcpListener::bind(address).map_err(|error| ListenerError::io(&error))?;
        let endpoint = listener
            .local_addr()
            .map_err(|error| ListenerError::io(&error))?;
        // The kernel assigns the port; re-check the resolved address, because a
        // bind is only as loopback as what it actually ended up bound to.
        if !is_loopback(endpoint) {
            return Err(ListenerError::NotLoopback);
        }
        Ok(Self { listener, endpoint })
    }

    /// The endpoint to write into the rendezvous file.
    #[must_use]
    pub const fn endpoint(&self) -> SocketAddr {
        self.endpoint
    }

    /// Accept one connection, upgrade it, and authenticate its `hello`.
    ///
    /// # Errors
    /// Returns [`ListenerError::Io`] on a socket failure,
    /// [`ListenerError::Frame`] when the peer violates the WebSocket layer, and
    /// [`ListenerError::Handshake`] when it fails to authenticate. In the handshake
    /// case a classified `error` envelope and a close frame are written first, so
    /// the peer learns it was refused rather than seeing a bare disconnect.
    pub fn accept_authenticated(
        &self,
        handshake: &Handshake,
    ) -> Result<BridgeSession, ListenerError> {
        let (stream, peer) = self
            .listener
            .accept()
            .map_err(|error| ListenerError::io(&error))?;
        Self::authenticate(stream, peer, handshake)
    }

    /// [`Self::accept_authenticated`], waiting at most `wait` for a peer to
    /// dial. `Ok(None)` means nobody dialled in time, so the owning thread can
    /// decide between waits whether to keep serving.
    ///
    /// # Errors
    /// The same as [`Self::accept_authenticated`].
    pub fn accept_authenticated_within(
        &self,
        handshake: &Handshake,
        wait: Duration,
    ) -> Result<Option<BridgeSession>, ListenerError> {
        self.listener
            .set_nonblocking(true)
            .map_err(|error| ListenerError::io(&error))?;
        let deadline = Instant::now() + wait;
        let accepted = loop {
            match self.listener.accept() {
                Ok(pair) => break Ok(pair),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        break Err(None);
                    }
                    std::thread::sleep(ACCEPT_POLL);
                }
                Err(error) => break Err(Some(ListenerError::io(&error))),
            }
        };
        self.listener
            .set_nonblocking(false)
            .map_err(|error| ListenerError::io(&error))?;
        match accepted {
            Ok((stream, peer)) => {
                // An accepted socket may inherit the listener's mode.
                stream
                    .set_nonblocking(false)
                    .map_err(|error| ListenerError::io(&error))?;
                Self::authenticate(stream, peer, handshake).map(Some)
            }
            Err(None) => Ok(None),
            Err(Some(error)) => Err(error),
        }
    }

    fn authenticate(
        mut stream: TcpStream,
        peer: SocketAddr,
        handshake: &Handshake,
    ) -> Result<BridgeSession, ListenerError> {
        if !is_loopback(peer) {
            // Unreachable through a loopback-bound socket, but a listener that
            // trusted its bind rather than its peer is exactly the class of bug
            // Todo 18 catalogued.
            let _ = stream.write_all(&encode_close(1008, "non-loopback peer").unwrap_or_default());
            return Err(ListenerError::NotLoopback);
        }
        stream
            .set_read_timeout(Some(SESSION_READ_TIMEOUT))
            .map_err(|error| ListenerError::io(&error))?;

        let mut buffer = Vec::new();
        let head = read_handshake_head(&mut stream, &mut buffer)?;
        let key = parse_client_handshake(&head).map_err(ListenerError::Frame)?;
        stream
            .write_all(server_handshake_response(&key).as_bytes())
            .map_err(|error| ListenerError::io(&error))?;

        let hello = read_text_envelope(&mut stream, &mut buffer)?;
        let text = String::from_utf8(canonical_bytes(&hello)).unwrap_or_default();
        match handshake.authenticate_text(&text) {
            Ok(outcome) => {
                let capabilities = handshake.capabilities();
                write_envelope(&mut stream, &capabilities)?;
                Ok(BridgeSession {
                    stream,
                    buffer,
                    outcome,
                    rendezvous: Some(handshake.rendezvous().clone()),
                    data_writes_enabled: true,
                })
            }
            Err(error) => {
                let refusal = handshake.refusal(&error);
                let _ = write_envelope(&mut stream, &refusal);
                let _ =
                    stream.write_all(&encode_close(1008, "handshake refused").unwrap_or_default());
                Err(ListenerError::Handshake(error))
            }
        }
    }

    /// Classify an oversize declared frame length without touching the socket.
    ///
    /// Exists so the refusal a 16 MiB declaration receives can be asserted
    /// directly, proving the listener survives the attempt: the classification is
    /// derived from the codec's header-only check, not from having read the
    /// payload.
    ///
    /// # Errors
    /// Returns [`ListenerError::Frame`] when `declared` is *not* oversize, since a
    /// caller asking this question about an acceptable length has a bug.
    pub fn refuse_oversize_probe(&self, declared: u64) -> Result<ErrorKind, ListenerError> {
        match WsFrame::validate_declared_length(declared) {
            Err(error @ WsError::FrameTooLarge { .. }) => {
                Ok(ListenerError::Frame(error).error_kind())
            }
            Err(other) => Err(ListenerError::Frame(other)),
            Ok(_accepted) => Err(ListenerError::Frame(WsError::FrameTooLarge { declared })),
        }
    }
}

/// One authenticated session over an accepted loopback connection.
#[derive(Debug)]
pub struct BridgeSession {
    stream: TcpStream,
    buffer: Vec<u8>,
    outcome: HandshakeOutcome,
    rendezvous: Option<RendezvousFile>,
    data_writes_enabled: bool,
}

impl BridgeSession {
    /// What the handshake established.
    #[must_use]
    pub const fn outcome(&self) -> &HandshakeOutcome {
        &self.outcome
    }

    /// The authenticated peer's identity.
    #[must_use]
    pub const fn plugin(&self) -> &PluginIdentity {
        self.outcome.plugin()
    }

    /// The session identity the server assigned.
    #[must_use]
    pub fn session_id(&self) -> &str {
        self.outcome.session_id()
    }

    /// Send one message and read the next one the peer sends.
    ///
    /// # Errors
    /// Returns [`ListenerError::Io`] on a socket failure,
    /// [`ListenerError::Frame`] on a framing violation, and
    /// [`ListenerError::PeerClosed`] when the peer closes or has already closed.
    pub fn exchange(&mut self, envelope: &Envelope) -> Result<Envelope, ListenerError> {
        self.send(envelope)?;
        self.receive()
    }

    /// Send one message without waiting for an answer.
    ///
    /// # Errors
    /// Returns [`ListenerError::PeerClosed`] after a peer Close was observed, or
    /// [`ListenerError::Io`] on a socket failure.
    pub fn send(&mut self, envelope: &Envelope) -> Result<(), ListenerError> {
        self.ensure_data_writes_enabled()?;
        write_envelope(&mut self.stream, envelope)
    }

    /// Read the next message the peer sends.
    ///
    /// # Errors
    /// Returns [`ListenerError::Frame`], [`ListenerError::Envelope`],
    /// [`ListenerError::PeerClosed`] or [`ListenerError::Io`].
    pub fn receive(&mut self) -> Result<Envelope, ListenerError> {
        let result = read_text_envelope(&mut self.stream, &mut self.buffer);
        if matches!(result, Err(ListenerError::PeerClosed)) {
            self.data_writes_enabled = false;
        }
        result
    }

    /// Answer a ping with a pong, at the WebSocket layer.
    ///
    /// # Errors
    /// Returns [`ListenerError::Frame`] for an oversized control payload or
    /// [`ListenerError::Io`] on a socket failure, and
    /// [`ListenerError::PeerClosed`] after a peer Close was observed.
    pub fn pong(&mut self, payload: &[u8]) -> Result<(), ListenerError> {
        self.ensure_data_writes_enabled()?;
        write_pong(&mut self.stream, payload)
    }

    /// Send `shutdown`, close the frame stream and remove this launch's rendezvous.
    ///
    /// # Errors
    /// Returns [`ListenerError::Io`] on a socket failure or
    /// [`ListenerError::Rendezvous`] when the private rendezvous file cannot be
    /// removed. Rendezvous cleanup is attempted even when writing the shutdown
    /// frames fails.
    pub fn shutdown(&mut self) -> Result<(), ListenerError> {
        let session = self.session_id().to_owned();
        let shutdown = Envelope::shutdown(
            &super::envelope::ServerIdentity {
                server_version: env!("CARGO_PKG_VERSION").to_owned(),
                protocol_version: super::compat::PROTOCOL_VERSION.to_owned(),
                session_id: session,
            },
            "shutdown-1",
            "server is stopping",
        );
        let transport_result = if self.data_writes_enabled {
            self.data_writes_enabled = false;
            (|| {
                write_envelope(&mut self.stream, &shutdown)?;
                let close =
                    encode_close(1001, "server is stopping").map_err(ListenerError::Frame)?;
                self.stream
                    .write_all(&close)
                    .map_err(|error| ListenerError::io(&error))
            })()
        } else {
            Ok(())
        };
        let cleanup_result = self.remove_rendezvous();
        transport_result?;
        cleanup_result
    }

    fn remove_rendezvous(&mut self) -> Result<(), ListenerError> {
        let Some(rendezvous) = self.rendezvous.take() else {
            return Ok(());
        };
        rendezvous.remove().map_err(ListenerError::Rendezvous)
    }

    fn ensure_data_writes_enabled(&self) -> Result<(), ListenerError> {
        if self.data_writes_enabled {
            Ok(())
        } else {
            Err(ListenerError::PeerClosed)
        }
    }

    /// Split the session into a reader one thread waits on and a writer any
    /// thread may send through, so requests can be written while the next
    /// answer is awaited. The rendezvous file stays with its owner: closing a
    /// split session never removes it.
    ///
    /// # Errors
    /// Returns [`ListenerError::Io`] when the socket cannot be duplicated.
    pub fn split(self) -> Result<(SessionReader, SessionWriter), ListenerError> {
        let read_half = self
            .stream
            .try_clone()
            .map_err(|error| ListenerError::io(&error))?;
        let writer = SessionWriter {
            inner: Arc::new(WriterInner {
                stream: Mutex::new(self.stream),
                open: AtomicBool::new(self.data_writes_enabled),
            }),
        };
        let reader = SessionReader {
            stream: read_half,
            buffer: self.buffer,
            outcome: self.outcome,
            writer: writer.clone(),
        };
        Ok((reader, writer))
    }
}

struct WriterInner {
    stream: Mutex<TcpStream>,
    open: AtomicBool,
}

/// The sending half of a split [`BridgeSession`]. Cheap to clone; every clone
/// writes whole frames under one lock, so frames never interleave.
#[derive(Clone)]
pub struct SessionWriter {
    inner: Arc<WriterInner>,
}

impl std::fmt::Debug for SessionWriter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionWriter")
            .field("open", &self.is_open())
            .finish_non_exhaustive()
    }
}

impl SessionWriter {
    /// Whether frames may still be written.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.inner.open.load(Ordering::SeqCst)
    }

    /// Send one message.
    ///
    /// # Errors
    /// Returns [`ListenerError::PeerClosed`] once either side closed, or
    /// [`ListenerError::Io`] on a socket failure (which also closes the writer).
    pub fn send(&self, envelope: &Envelope) -> Result<(), ListenerError> {
        let mut stream = self.lock_open()?;
        write_envelope(&mut stream, envelope).inspect_err(|_| self.mark_closed())
    }

    /// Close the frame stream with `code` and `reason`. Idempotent.
    ///
    /// # Errors
    /// Returns [`ListenerError::Frame`] for an oversized reason or
    /// [`ListenerError::Io`] on a socket failure.
    pub fn close(&self, code: u16, reason: &str) -> Result<(), ListenerError> {
        let Ok(mut stream) = self.lock_open() else {
            return Ok(());
        };
        self.mark_closed();
        let close = encode_close(code, reason).map_err(ListenerError::Frame)?;
        let written = stream
            .write_all(&close)
            .map_err(|error| ListenerError::io(&error));
        let _ = stream.shutdown(std::net::Shutdown::Both);
        written
    }

    fn pong(&self, payload: &[u8]) -> Result<(), ListenerError> {
        let mut stream = self.lock_open()?;
        write_pong(&mut stream, payload)
    }

    fn answer_close(&self, payload: &[u8]) -> Result<(), ListenerError> {
        let Ok(mut stream) = self.lock_open() else {
            return Ok(());
        };
        self.mark_closed();
        let response = encode_server_frame(Opcode::Close, payload).map_err(ListenerError::Frame)?;
        stream
            .write_all(&response)
            .map_err(|error| ListenerError::io(&error))
    }

    fn mark_closed(&self) {
        self.inner.open.store(false, Ordering::SeqCst);
    }

    fn lock_open(&self) -> Result<std::sync::MutexGuard<'_, TcpStream>, ListenerError> {
        let stream = self
            .inner
            .stream
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.is_open() {
            Ok(stream)
        } else {
            Err(ListenerError::PeerClosed)
        }
    }
}

/// The receiving half of a split [`BridgeSession`].
#[derive(Debug)]
pub struct SessionReader {
    stream: TcpStream,
    buffer: Vec<u8>,
    outcome: HandshakeOutcome,
    writer: SessionWriter,
}

impl SessionReader {
    /// What the handshake established.
    #[must_use]
    pub const fn outcome(&self) -> &HandshakeOutcome {
        &self.outcome
    }

    /// The next message the peer sends, waiting at most `wait`. `Ok(None)`
    /// means nothing complete arrived in time; a partially received frame is
    /// kept for the next call. A ping is answered through the writer and a
    /// close ends the session for both halves.
    ///
    /// # Errors
    /// Returns [`ListenerError::PeerClosed`] when the peer closed,
    /// [`ListenerError::Frame`] or [`ListenerError::Envelope`] for a protocol
    /// violation, or [`ListenerError::Io`] on a socket failure.
    pub fn receive_within(&mut self, wait: Duration) -> Result<Option<Envelope>, ListenerError> {
        let deadline = Instant::now() + wait;
        loop {
            match WsFrame::parse_from_client(&self.buffer) {
                Ok(frame) => {
                    self.buffer.drain(..frame.consumed);
                    match frame.opcode {
                        Opcode::Close => {
                            self.writer.answer_close(&frame.payload)?;
                            return Err(ListenerError::PeerClosed);
                        }
                        Opcode::Ping => self.writer.pong(&frame.payload)?,
                        Opcode::Pong => {}
                        Opcode::Text => {
                            let text = String::from_utf8(frame.payload)
                                .map_err(|_utf8| ListenerError::Frame(WsError::NotUtf8))?;
                            return Envelope::parse(&text)
                                .map(Some)
                                .map_err(ListenerError::Envelope);
                        }
                    }
                }
                Err(error) if error.is_incomplete() => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Ok(None);
                    }
                    self.stream
                        .set_read_timeout(Some(remaining))
                        .map_err(|error| ListenerError::io(&error))?;
                    let mut chunk = [0_u8; 8192];
                    match self.stream.read(&mut chunk) {
                        Ok(0) => {
                            self.writer.mark_closed();
                            return Err(ListenerError::PeerClosed);
                        }
                        Ok(read) => {
                            if self.buffer.len() + read > MAX_FRAME_BYTES + 64 * 1024 {
                                self.writer.mark_closed();
                                return Err(ListenerError::Frame(WsError::FrameTooLarge {
                                    declared: (self.buffer.len() + read) as u64,
                                }));
                            }
                            self.buffer.extend_from_slice(&chunk[..read]);
                        }
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) =>
                        {
                            return Ok(None);
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(error) => {
                            self.writer.mark_closed();
                            return Err(ListenerError::io(&error));
                        }
                    }
                }
                Err(error) => {
                    self.writer.mark_closed();
                    return Err(ListenerError::Frame(error));
                }
            }
        }
    }
}

fn write_envelope(stream: &mut TcpStream, envelope: &Envelope) -> Result<(), ListenerError> {
    let text = String::from_utf8(canonical_bytes(envelope)).unwrap_or_default();
    let frame = encode_server_text(&text).map_err(ListenerError::Frame)?;
    stream
        .write_all(&frame)
        .map_err(|error| ListenerError::io(&error))?;
    stream.flush().map_err(|error| ListenerError::io(&error))
}

fn read_handshake_head(
    stream: &mut TcpStream,
    buffer: &mut Vec<u8>,
) -> Result<Vec<u8>, ListenerError> {
    loop {
        if let Some(length) = handshake_head_len(buffer).map_err(ListenerError::Frame)? {
            let head = buffer[..length].to_vec();
            buffer.drain(..length);
            return Ok(head);
        }
        fill(stream, buffer)?;
    }
}

fn read_text_envelope(
    stream: &mut TcpStream,
    buffer: &mut Vec<u8>,
) -> Result<Envelope, ListenerError> {
    loop {
        match WsFrame::parse_from_client(buffer) {
            Ok(frame) => {
                buffer.drain(..frame.consumed);
                match frame.opcode {
                    Opcode::Close => {
                        let response = encode_server_frame(Opcode::Close, &frame.payload)
                            .map_err(ListenerError::Frame)?;
                        stream
                            .write_all(&response)
                            .map_err(|error| ListenerError::io(&error))?;
                        return Err(ListenerError::PeerClosed);
                    }
                    Opcode::Ping => {
                        write_pong(stream, &frame.payload)?;
                    }
                    Opcode::Pong => {}
                    Opcode::Text => {
                        let text = String::from_utf8(frame.payload)
                            .map_err(|_utf8| ListenerError::Frame(WsError::NotUtf8))?;
                        return Envelope::parse(&text).map_err(ListenerError::Envelope);
                    }
                }
            }
            Err(error) if error.is_incomplete() => fill(stream, buffer)?,
            Err(error) => return Err(ListenerError::Frame(error)),
        }
    }
}

fn write_pong(stream: &mut TcpStream, payload: &[u8]) -> Result<(), ListenerError> {
    let frame = encode_server_frame(Opcode::Pong, payload).map_err(ListenerError::Frame)?;
    stream
        .write_all(&frame)
        .map_err(|error| ListenerError::io(&error))
}

fn fill(stream: &mut TcpStream, buffer: &mut Vec<u8>) -> Result<(), ListenerError> {
    let mut chunk = [0_u8; 8192];
    let read = stream
        .read(&mut chunk)
        .map_err(|error| ListenerError::io(&error))?;
    if read == 0 {
        return Err(ListenerError::PeerClosed);
    }
    if buffer.len() + read > MAX_FRAME_BYTES + 64 * 1024 {
        return Err(ListenerError::Frame(WsError::FrameTooLarge {
            declared: (buffer.len() + read) as u64,
        }));
    }
    buffer.extend_from_slice(&chunk[..read]);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::net::{Shutdown, TcpListener, TcpStream};
    use std::time::Duration;

    use godot_mcp_domain::ErrorKind;

    use super::super::compat::PROTOCOL_VERSION;
    use super::super::envelope::{
        Envelope, MessageKind, PluginIdentity, ServerIdentity, canonical_bytes,
    };
    use super::super::handshake::HandshakeOutcome;
    use super::super::ws::{MAX_FRAME_BYTES, Opcode, WsError, WsFrame, encode_client_text};
    use super::{BridgeListener, BridgeSession, ListenerError, MAX_SESSIONS, read_text_envelope};

    fn loopback_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let endpoint = listener.local_addr().expect("resolve loopback endpoint");
        let client = TcpStream::connect(endpoint).expect("connect loopback client");
        let (server, peer) = listener.accept().expect("accept loopback client");
        assert!(peer.ip().is_loopback());
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("set client read timeout");
        (client, server)
    }

    fn authenticated_session(stream: TcpStream) -> BridgeSession {
        BridgeSession {
            stream,
            buffer: Vec::new(),
            outcome: HandshakeOutcome::Accepted {
                session_id: "session-close".to_owned(),
                plugin: PluginIdentity {
                    plugin_version: "0.1.0".to_owned(),
                    protocol_version: PROTOCOL_VERSION.to_owned(),
                    godot_version: "4.7.1.stable".to_owned(),
                },
                nonce: "nonce-close".to_owned(),
            },
            rendezvous: None,
            data_writes_enabled: true,
        }
    }

    fn masked_client_control_frame(opcode: Opcode, payload: &[u8]) -> Vec<u8> {
        assert!(opcode != Opcode::Text);
        assert!(payload.len() <= 125);
        let mask = [0x37, 0xfa, 0x21, 0x3d];
        let mut frame = Vec::with_capacity(payload.len() + 6);
        frame.push(0x80 | opcode.as_u8());
        frame.push(0x80 | u8::try_from(payload.len()).expect("control payload fits in u8"));
        frame.extend_from_slice(&mask);
        frame.extend(
            payload
                .iter()
                .enumerate()
                .map(|(index, byte)| byte ^ mask[index % mask.len()]),
        );
        frame
    }

    fn read_server_frame(stream: &mut TcpStream) -> WsFrame {
        let mut buffer = Vec::new();
        loop {
            match WsFrame::parse(&buffer) {
                Ok(frame) => return frame,
                Err(error) if error.is_incomplete() => {
                    let mut chunk = [0_u8; 128];
                    let count = stream.read(&mut chunk).expect("read server frame");
                    assert_ne!(count, 0, "server closed before sending a complete frame");
                    buffer.extend_from_slice(&chunk[..count]);
                }
                Err(error) => panic!("server sent an invalid frame: {error}"),
            }
        }
    }

    fn shipped_source() -> String {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/protocol/listener.rs");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        let marker = format!("#[cfg({})]", "test");
        match text.find(&marker) {
            Some(offset) => text[..offset].to_owned(),
            None => text,
        }
    }

    fn production_source(source: &str) -> &str {
        source
            .find("#[cfg(test)]")
            .map_or(source, |index| &source[..index])
    }

    fn temp_rendezvous_path(label: &str) -> std::path::PathBuf {
        let directory =
            std::env::temp_dir().join(format!("godot-mcp-listener-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("create rendezvous directory");
        directory.join("editor.rendezvous")
    }

    fn test_handshake(
        listener: &BridgeListener,
        label: &str,
    ) -> super::super::handshake::Handshake {
        let project = super::super::envelope::ProjectClaim {
            object_identity: format!("project_{}", "a".repeat(64)),
            root_object: "unix:dev=1,ino=2".to_owned(),
            file_object: "unix:dev=1,ino=3".to_owned(),
            generation: 0,
        };
        let rendezvous = super::super::rendezvous::RendezvousFile::write_new(
            &temp_rendezvous_path(label),
            listener.endpoint(),
            project.clone(),
        )
        .expect("write rendezvous");
        super::super::handshake::Handshake::new(
            ServerIdentity {
                server_version: "0.2.0".to_owned(),
                protocol_version: PROTOCOL_VERSION.to_owned(),
                session_id: format!("session-{label}"),
            },
            project,
            rendezvous,
        )
    }

    #[test]
    fn a_bounded_accept_answers_none_when_no_peer_dials() {
        let listener = BridgeListener::bind_loopback().expect("bind");
        let handshake = test_handshake(&listener, "bounded-accept");
        let started = std::time::Instant::now();
        let accepted = listener
            .accept_authenticated_within(&handshake, Duration::from_millis(80))
            .expect("no failure without a peer");
        assert!(accepted.is_none(), "nobody dialled");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_split_session_writes_while_its_reader_waits_and_sees_a_close() {
        let (mut client, server) = loopback_pair();
        let (mut reader, writer) = authenticated_session(server).split().expect("split");
        let server_identity = ServerIdentity {
            server_version: "0.2.0".to_owned(),
            protocol_version: PROTOCOL_VERSION.to_owned(),
            session_id: "session-split".to_owned(),
        };

        // Nothing arrives within the wait: the reader answers None, not an error.
        assert_eq!(
            reader.receive_within(Duration::from_millis(50)),
            Ok(None),
            "an idle wait is not a failure"
        );

        // The writer sends from another thread while the reader is free to wait.
        let sender = writer.clone();
        let envelope = Envelope::heartbeat(&server_identity, "heartbeat-1", 1_700_000_000_000);
        std::thread::spawn(move || sender.send(&envelope).expect("send from another thread"))
            .join()
            .expect("sender thread");
        let frame = read_server_frame(&mut client);
        assert_eq!(frame.opcode, Opcode::Text);
        let text = String::from_utf8(frame.payload).expect("utf-8");
        assert!(text.contains("\"heartbeat-1\""), "{text}");

        // A ping is answered through the writer; a text frame reaches the reader.
        let reply = Envelope::heartbeat_reply(
            &PluginIdentity {
                plugin_version: "0.2.0".to_owned(),
                protocol_version: PROTOCOL_VERSION.to_owned(),
                godot_version: "4.7.1.stable".to_owned(),
            },
            "heartbeat-1",
        );
        let reply_text = String::from_utf8(canonical_bytes(&reply)).expect("utf-8");
        let mut bytes = masked_client_control_frame(Opcode::Ping, b"ping");
        bytes.extend_from_slice(
            &encode_client_text(&reply_text, [0x01, 0x02, 0x03, 0x04]).expect("encode"),
        );
        client.write_all(&bytes).expect("send ping and text");
        let received = reader
            .receive_within(Duration::from_secs(2))
            .expect("read")
            .expect("an envelope arrived");
        assert_eq!(received.kind(), MessageKind::Heartbeat);
        assert_eq!(received.correlation_id(), Some("heartbeat-1"));
        let pong = read_server_frame(&mut client);
        assert_eq!(pong.opcode, Opcode::Pong);

        // A close from the peer ends the session for both halves.
        client
            .write_all(&masked_client_control_frame(Opcode::Close, &[0x03, 0xe8]))
            .expect("send close");
        assert_eq!(
            reader.receive_within(Duration::from_secs(2)),
            Err(ListenerError::PeerClosed)
        );
        assert!(!writer.is_open());
        assert_eq!(
            writer.send(&Envelope::heartbeat(&server_identity, "heartbeat-2", 1)),
            Err(ListenerError::PeerClosed)
        );
    }

    #[test]
    fn a_loopback_bind_gets_an_ephemeral_port() {
        let listener = BridgeListener::bind_loopback().expect("bind");
        let endpoint = listener.endpoint();
        assert!(endpoint.ip().is_loopback());
        assert_ne!(endpoint.port(), 0, "the kernel assigned a real port");
    }

    #[test]
    fn a_ping_received_while_reading_an_envelope_gets_an_immediate_matching_pong() {
        let (mut client, mut server) = loopback_pair();
        let payload = b"round-trip-ping";
        let pending = Envelope::shutdown(
            &ServerIdentity {
                server_version: "0.1.0".to_owned(),
                protocol_version: super::super::compat::PROTOCOL_VERSION.to_owned(),
                session_id: "session-ping".to_owned(),
            },
            "shutdown-after-ping",
            "test envelope",
        );
        let text = String::from_utf8(canonical_bytes(&pending)).expect("canonical UTF-8");
        let mut bytes = masked_client_control_frame(Opcode::Ping, payload);
        bytes.extend_from_slice(
            &encode_client_text(&text, [0x11, 0x22, 0x33, 0x44]).expect("encode text"),
        );

        client.write_all(&bytes).expect("send real client frames");
        let received = read_text_envelope(&mut server, &mut Vec::new()).expect("read envelope");

        assert_eq!(received.kind(), MessageKind::Shutdown);
        let pong = read_server_frame(&mut client);
        assert_eq!(pong.opcode, Opcode::Pong);
        assert_eq!(pong.payload, payload);
    }

    #[test]
    fn a_fragmented_frame_from_a_non_contract_peer_is_refused_over_loopback() {
        let (mut client, mut server) = loopback_pair();
        let mask = [0x37, 0xfa, 0x21, 0x3d];
        let payload = b"{}";
        let mut frame = vec![
            0x01,
            0x80 | u8::try_from(payload.len()).expect("short payload"),
        ];
        frame.extend_from_slice(&mask);
        frame.extend(
            payload
                .iter()
                .enumerate()
                .map(|(index, byte)| byte ^ mask[index % mask.len()]),
        );

        client
            .write_all(&frame)
            .expect("send fragmented text frame");

        assert_eq!(
            read_text_envelope(&mut server, &mut Vec::new()),
            Err(ListenerError::Frame(WsError::Fragmented))
        );
    }

    #[test]
    fn a_received_close_is_answered_once_before_a_later_ping_is_ignored() {
        let (mut client, mut server) = loopback_pair();
        let close_payload = [0x03, 0xe8, b'd', b'o', b'n', b'e'];
        let mut bytes = masked_client_control_frame(Opcode::Close, &close_payload);
        bytes.extend_from_slice(&masked_client_control_frame(Opcode::Ping, b"too late"));

        client.write_all(&bytes).expect("send close then ping");
        assert_eq!(
            read_text_envelope(&mut server, &mut Vec::new()),
            Err(ListenerError::PeerClosed)
        );
        server
            .shutdown(Shutdown::Both)
            .expect("close server socket after peer close");
        let mut response = Vec::new();
        client
            .read_to_end(&mut response)
            .expect("read the server Close response");
        let frame = WsFrame::parse(&response).expect("parse server Close response");
        assert_eq!(frame.opcode, Opcode::Close);
        assert_eq!(frame.payload, close_payload);
        assert_eq!(frame.consumed, response.len(), "no frame follows Close");
    }

    #[test]
    fn a_session_refuses_data_after_it_answers_close() {
        let (mut client, server) = loopback_pair();
        client
            .write_all(&masked_client_control_frame(Opcode::Close, &[]))
            .expect("send Close");
        let mut session = authenticated_session(server);
        let outbound = Envelope::shutdown(
            &ServerIdentity {
                server_version: "0.1.0".to_owned(),
                protocol_version: PROTOCOL_VERSION.to_owned(),
                session_id: "session-close".to_owned(),
            },
            "after-close",
            "must not be sent",
        );

        assert_eq!(session.receive(), Err(ListenerError::PeerClosed));

        assert_eq!(session.send(&outbound), Err(ListenerError::PeerClosed));
        assert_eq!(session.pong(b"too late"), Err(ListenerError::PeerClosed));
    }

    #[test]
    fn every_non_loopback_address_is_refused() {
        for address in ["0.0.0.0:0", "[::]:0", "192.168.1.1:0", "203.0.113.5:0"] {
            assert_eq!(
                BridgeListener::bind_address(address.parse().expect("addr"))
                    .expect_err("a non-loopback bind is refused"),
                ListenerError::NotLoopback,
                "address {address}"
            );
        }
        BridgeListener::bind_address("127.0.0.1:0".parse().expect("addr")).expect("loopback binds");
    }

    /// The shipped code must not contain a wildcard bind address at all, so
    /// widening the listener is not a one-character change away.
    #[test]
    fn the_shipped_listener_names_no_wildcard_address() {
        let source = shipped_source();
        // Assembled at runtime so this test's own source cannot satisfy the search.
        let wildcards = [
            format!("0.0{}.0.0", ""),
            format!("[{}::]", ""),
            format!("UNSPECI{}FIED", ""),
        ];
        for wildcard in wildcards {
            assert!(
                !source.contains(&wildcard),
                "the shipped listener must not name {wildcard}"
            );
        }
        assert!(
            source.contains("is_loopback"),
            "the shipped listener checks loopback explicitly"
        );
    }

    /// The plugin's whole vocabulary is this bridge's envelope. No MCP wire type
    /// may appear in the shipped listener.
    #[test]
    fn the_shipped_listener_speaks_no_mcp_wire_type() {
        let source = shipped_source();
        let forbidden = [
            format!("json{}rpc", ""),
            format!("tools/{}call", ""),
            format!("initi{}alize", ""),
            format!("rm{}cp", ""),
        ];
        for spelling in forbidden {
            assert!(
                !source.contains(&spelling),
                "the shipped listener must not reference the MCP wire type {spelling}"
            );
        }
    }

    #[test]
    fn the_bridge_serves_exactly_one_session() {
        assert_eq!(MAX_SESSIONS, 1);
        assert_eq!(
            ListenerError::TooManySessions.error_kind(),
            ErrorKind::Conflict
        );
    }

    #[test]
    fn rendezvous_rotation_and_removal_have_production_lifecycle_callers() {
        const LISTENER_SOURCE: &str = include_str!("listener.rs");
        const FIXTURE_SOURCE: &str = include_str!("../../../godot-mcp-cli/src/fixture34/bridge.rs");
        const ACCEPTANCE_SOURCE: &str = include_str!("../../tests/bridge_protocol.rs");

        let remove_call = format!("rendezvous.rem{}ove()", "");
        assert_eq!(
            production_source(LISTENER_SOURCE)
                .matches(&remove_call)
                .count(),
            1,
            "the production session lifecycle must own exactly one rendezvous removal site"
        );
        assert_eq!(
            ACCEPTANCE_SOURCE
                .matches(&format!(".rem{}ove()", ""))
                .count(),
            0,
            "the acceptance harness must not perform lifecycle cleanup itself"
        );

        let startup_call = format!("RendezvousFile::write_{}new(", "");
        assert_eq!(
            production_source(FIXTURE_SOURCE)
                .matches(&startup_call)
                .count(),
            1,
            "production startup must apply write_new's platform-specific stale-file policy"
        );
    }

    #[test]
    fn an_oversize_declaration_is_classified_as_invalid_input() {
        let listener = BridgeListener::bind_loopback().expect("bind");
        assert_eq!(
            listener
                .refuse_oversize_probe(16 * 1024 * 1024)
                .expect("classified"),
            ErrorKind::InvalidInput
        );
        assert!(
            listener.refuse_oversize_probe(1024).is_err(),
            "an acceptable length is not an oversize refusal"
        );
        assert!(
            listener
                .refuse_oversize_probe(MAX_FRAME_BYTES as u64)
                .is_err()
        );
    }

    #[test]
    fn every_listener_failure_is_classified() {
        assert_eq!(
            ListenerError::NotLoopback.error_kind(),
            ErrorKind::InvalidInput
        );
        assert_eq!(
            ListenerError::PeerClosed.error_kind(),
            ErrorKind::Unavailable
        );
        assert_eq!(
            ListenerError::Io {
                detail: "broken pipe".to_owned()
            }
            .error_kind(),
            ErrorKind::Unavailable
        );
        assert_eq!(
            ListenerError::Frame(WsError::Fragmented).error_kind(),
            ErrorKind::InvalidInput
        );
        assert_eq!(
            ListenerError::Frame(WsError::Incomplete { needed: 1 }).error_kind(),
            ErrorKind::Unavailable,
            "a short read is a transport condition, not a protocol violation"
        );
    }
}
