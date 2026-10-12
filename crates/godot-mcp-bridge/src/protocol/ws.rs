//! The minimal RFC 6455 WebSocket the editor bridge speaks.
//!
//! # Why WebSocket here, when Todo 18's plane is raw TCP
//!
//! These are two different planes and the distinction is load-bearing. Godot's
//! GDScript language server and debug adapter moved **off** WebSocket in commit
//! `24b27043f` ("Migrating language server from Websockets to raw TCP",
//! 2020-02-01), so Todo 18's adapters speak plain `Content-Length` framing. This
//! module is the *editor bridge*: a connection between this server and the
//! pure-GDScript `EditorPlugin`, where the plugin side is
//! [`WebSocketPeer`](https://docs.godotengine.org/en/stable/classes/class_websocketpeer.html)
//! — a documented, RFC 6455-compliant client built into the engine, with
//! `connect_to_url`, `poll`, `send_text` and `was_string_packet`. Choosing
//! WebSocket text frames means the plugin needs **no hand-rolled framing at all**:
//! one `send_text` call per message, one `get_packet` per received message.
//!
//! The Rust side is the server, so the Rust side is what must implement RFC 6455.
//! That is this module, and only the subset the bridge uses:
//!
//! | supported | not supported |
//! | --- | --- |
//! | client request: `GET`, HTTP/1.1, non-empty `Host`, upgrade tokens, version 13, canonical 16-byte key, fixed subprotocol | extensions and extension negotiation |
//! | server response: HTTP/1.1 `101`, upgrade tokens, matching accept key, fixed subprotocol | subprotocol negotiation beyond the one fixed name |
//! | unfragmented UTF-8 text frames, minimal length encoding, 4 MiB ceiling (§5.2, §5.6) | §5.4 fragmented-message reassembly (the fixed `WebSocketPeer` peer contract sends unfragmented messages), continuation and binary frames |
//! | final close, ping and pong frames of at most 125 payload bytes; Close payload shape/status/reason validation; received Close is answered once with the same payload and disables later session writes; received Ping is answered immediately with a matching Pong unless Close was received first; received Pong is consumed (§5.5–§5.5.3) | server-originated Ping scheduling |
//! | client frames require masks; server frames forbid masks (§5.1, §5.3) | reserved bits and every extension-dependent frame shape |
//!
//! RFC 6455 §5.4 requires a general-purpose receiver to handle fragmented messages.
//! This fixed two-peer protocol deliberately excludes that capability: every bridge
//! message is one JSON document sent with one `WebSocketPeer.send_text`. In the
//! pinned native Godot 4.7.1 implementation, `send_text` routes through
//! `WSLPeer::_send` to `wslay_event_queue_msg`; the bundled wslay API specifies that
//! this call sends the message without fragmentation and requires the distinct
//! `wslay_event_queue_fragmented_msg` API to fragment. A frame with FIN clear or a
//! continuation opcode therefore comes from outside the sole peer contract and is
//! refused with [`WsError::Fragmented`] or [`WsError::UnsupportedOpcode`].
//!
//! Reassembly would also change the allocation boundary below: the 4 MiB limit would
//! have to cover the sum of all fragments, not each fragment independently. Refusing
//! the excluded shape preserves the header-only, pre-allocation ceiling instead of
//! adding a buffer whose only use would be to admit a non-contract peer.
//!
//! # The 4 MiB ceiling
//!
//! [`MAX_FRAME_BYTES`] is 4 MiB, the same number Todo 18's codec takes from the
//! engine's own `LSP_MAX_BUFFER_SIZE` / `DAP_MAX_BUFFER_SIZE` (`4194304`, a fixed
//! inline `uint8_t req_buf[4194304]` per peer). Two independent reasons keep the
//! bridge on the same number rather than a larger one. First, the plugin side is
//! not unbounded either: `WebSocketPeer.inbound_buffer_size` defaults to 65535 and
//! is a *configured allocation*, so an arbitrarily large frame is a plugin-side
//! failure, not a slow success. Second, uniformity across both planes means one
//! reviewable limit instead of two, and the failure is the same shape in both:
//! refuse from the declared length, before allocating it.
//!
//! A declared length is checked **from the header alone**
//! ([`WsFrame::validate_declared_length`]), so a peer announcing 16 MiB is refused
//! having caused one 10-byte read, and the listener stays up.

use std::net::SocketAddr;

use super::digest::{base64_decode, base64_encode, sha1};

/// The GUID RFC 6455 §1.3 concatenates with `Sec-WebSocket-Key`.
pub const WEBSOCKET_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// Hard maximum payload bytes for one frame, in either direction.
///
/// Matches Todo 18's `MAX_PAYLOAD_BYTES`; see this module's header for why.
pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// Maximum bytes of an opening-handshake header section.
///
/// A legitimate upgrade is a few hundred bytes. This cap means a peer that never
/// sends the terminator cannot grow this process without bound.
pub const MAX_HANDSHAKE_BYTES: usize = 8 * 1024;

/// The subprotocol name the bridge negotiates.
pub const SUBPROTOCOL: &str = "godot-mcp-bridge.v1";

/// The frame opcodes this module handles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Opcode {
    /// A UTF-8 text frame (§5.6). Every bridge message is one of these.
    Text,
    /// A close frame (§5.5.1).
    Close,
    /// A ping frame (§5.5.2).
    Ping,
    /// A pong frame (§5.5.3).
    Pong,
}

impl Opcode {
    /// The wire nibble.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Text => 0x1,
            Self::Close => 0x8,
            Self::Ping => 0x9,
            Self::Pong => 0xa,
        }
    }

    /// Parse a wire nibble, rejecting everything this module does not handle.
    #[must_use]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0x1 => Some(Self::Text),
            0x8 => Some(Self::Close),
            0x9 => Some(Self::Ping),
            0xa => Some(Self::Pong),
            _ => None,
        }
    }

    const fn is_control(self) -> bool {
        matches!(self, Self::Close | Self::Ping | Self::Pong)
    }
}

/// Why a frame or handshake was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WsError {
    /// Not enough bytes have arrived yet to decide anything. Read more.
    #[error("the frame is incomplete; {needed} more bytes are required")]
    Incomplete {
        /// A lower bound on how many further bytes are needed.
        needed: usize,
    },

    /// The declared payload length exceeds [`MAX_FRAME_BYTES`].
    ///
    /// Decided from the header alone, before the payload is allocated or read.
    #[error(
        "frame declares {declared} payload bytes, over the {MAX_FRAME_BYTES}-byte bridge limit"
    )]
    FrameTooLarge {
        /// The length the peer declared, or the length a caller tried to send.
        declared: u64,
    },

    /// An opcode outside [`Opcode`], including binary and continuation frames.
    #[error("frame opcode 0x{opcode:x} is not one this bridge speaks")]
    UnsupportedOpcode {
        /// The nibble observed.
        opcode: u8,
    },

    /// An RFC 6455 §5.4 fragmented frame from outside the fixed peer contract.
    /// See the module header for the verified `WebSocketPeer` send path.
    #[error(
        "RFC 6455 §5.4 fragmented frames are outside the bridge's unfragmented WebSocketPeer contract"
    )]
    Fragmented,

    /// A reserved bit was set, which would imply an extension was negotiated.
    #[error("frame sets a reserved bit, implying an unnegotiated extension")]
    ReservedBitSet,

    /// A client-to-server frame arrived unmasked, which RFC 6455 §5.1 forbids.
    #[error("a client-to-server frame must be masked")]
    UnmaskedClientFrame,

    /// A server-to-client frame arrived masked, which RFC 6455 §5.1 forbids.
    #[error("a server-to-client frame must not be masked")]
    MaskedServerFrame,

    /// A control frame declared more than RFC 6455 §5.5's 125-byte maximum.
    #[error("control frame declares {declared} payload bytes; control frames are limited to 125")]
    ControlFrameTooLarge {
        /// The payload length declared by the control frame.
        declared: u64,
    },

    /// An extended payload length used a wider-than-necessary wire encoding.
    #[error(
        "frame uses extended length marker {marker} for {declared} payload bytes; the shortest encoding is required"
    )]
    NonMinimalLengthEncoding {
        /// The extended-length marker from the seven-bit length field.
        marker: u8,
        /// The payload length carried by that extended field.
        declared: u64,
    },

    /// A 64-bit payload length set the high bit reserved by RFC 6455 §5.2.
    #[error("frame's 64-bit payload length has its reserved high bit set")]
    Invalid64BitLength,

    /// A text frame's payload was not valid UTF-8.
    #[error("a text frame's payload is not valid UTF-8")]
    NotUtf8,

    /// A Close payload carried exactly one byte, so it could not contain a status code.
    #[error("a Close frame payload must be empty or contain a two-byte status code")]
    InvalidClosePayloadLength,

    /// A Close payload carried a status code that RFC 6455 forbids on the wire.
    #[error("Close frame status code {code} is not valid on the wire")]
    InvalidCloseStatusCode {
        /// The forbidden status code received or requested for encoding.
        code: u16,
    },

    /// A Close reason was not valid UTF-8.
    #[error("a Close frame reason is not valid UTF-8")]
    InvalidCloseReason,

    /// The opening handshake was not a `GET` upgrade request.
    #[error("the opening handshake is not a WebSocket GET upgrade")]
    NotAnUpgrade,

    /// The opening handshake did not use HTTP/1.1 as RFC 6455 requires.
    #[error("the WebSocket opening handshake must use HTTP/1.1")]
    WrongHttpVersion,

    /// The `Connection` header did not carry the `Upgrade` token.
    #[error("the WebSocket opening handshake has no Connection: Upgrade token")]
    MissingConnectionUpgrade,

    /// A client did not request RFC 6455 wire version 13.
    #[error("the WebSocket opening handshake must request Sec-WebSocket-Version: 13")]
    UnsupportedWebSocketVersion,

    /// The opening handshake carried no `Sec-WebSocket-Key`.
    #[error("the opening handshake carries no Sec-WebSocket-Key")]
    MissingKey,

    /// The client opening handshake carried no non-empty `Host` header.
    #[error("the WebSocket opening handshake carries no non-empty Host header")]
    MissingHost,

    /// The client's `Sec-WebSocket-Key` was not canonical base64 of 16 bytes.
    #[error("Sec-WebSocket-Key must be canonical base64 encoding exactly 16 bytes")]
    InvalidWebSocketKey,

    /// The client did not offer the bridge's fixed subprotocol.
    #[error("the client opening handshake must offer Sec-WebSocket-Protocol: {SUBPROTOCOL}")]
    ClientSubprotocolMismatch,

    /// The server did not echo the bridge's fixed subprotocol.
    #[error("the server opening handshake must echo Sec-WebSocket-Protocol: {SUBPROTOCOL}")]
    ServerSubprotocolMismatch,

    /// The server's `Sec-WebSocket-Accept` did not match the key sent.
    #[error("Sec-WebSocket-Accept does not match the key that was sent")]
    AcceptKeyMismatch,

    /// The handshake header section exceeded [`MAX_HANDSHAKE_BYTES`].
    #[error("the handshake header exceeded {MAX_HANDSHAKE_BYTES} bytes with no terminator")]
    HandshakeTooLarge,

    /// The handshake bytes were not UTF-8 or not `Name: value` lines.
    #[error("the handshake is malformed: {detail}")]
    MalformedHandshake {
        /// Exactly what was wrong, quotable in a diagnostic.
        detail: String,
    },
}

impl WsError {
    /// Whether this only means "read more bytes".
    #[must_use]
    pub const fn is_incomplete(&self) -> bool {
        matches!(self, Self::Incomplete { .. })
    }

    /// Whether the peer violated the protocol, as opposed to a short read.
    #[must_use]
    pub const fn is_protocol_violation(&self) -> bool {
        !self.is_incomplete()
    }
}

/// One decoded frame, plus how many bytes of the input it consumed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WsFrame {
    /// Which frame this is.
    pub opcode: Opcode,
    /// The unmasked payload.
    pub payload: Vec<u8>,
    /// How many bytes of the input this frame occupied, header included.
    pub consumed: usize,
}

impl WsFrame {
    /// Refuse a declared payload length that exceeds [`MAX_FRAME_BYTES`].
    ///
    /// Separated out so an oversize declaration can be refused from a parsed
    /// header with no payload in hand at all.
    ///
    /// # Errors
    /// Returns [`WsError::FrameTooLarge`] when `declared` is over the ceiling.
    pub const fn validate_declared_length(declared: u64) -> Result<usize, WsError> {
        if declared > MAX_FRAME_BYTES as u64 {
            return Err(WsError::FrameTooLarge { declared });
        }
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the guard above proves declared fits in usize on every supported target"
        )]
        Ok(declared as usize)
    }

    /// Decode one frame from the front of `input`, masked or not.
    ///
    /// # Errors
    /// Returns [`WsError::Incomplete`] when more bytes are needed;
    /// [`WsError::ReservedBitSet`] for an unnegotiated extension,
    /// [`WsError::Fragmented`] for a FIN-clear frame excluded by the fixed
    /// `WebSocketPeer` contract, or [`WsError::UnsupportedOpcode`] for an opcode
    /// outside the bridge contract;
    /// [`WsError::MaskedServerFrame`] for a masked server frame;
    /// [`WsError::NonMinimalLengthEncoding`] or [`WsError::Invalid64BitLength`]
    /// for a non-canonical length; [`WsError::ControlFrameTooLarge`] or
    /// [`WsError::FrameTooLarge`] for a declared length over its applicable cap;
    /// [`WsError::NotUtf8`] for invalid text, and the `InvalidClose*` variants for
    /// an invalid Close payload.
    pub fn parse(input: &[u8]) -> Result<Self, WsError> {
        Self::decode(input, false)
    }

    /// Decode one frame that must be masked, as every client-to-server frame is.
    ///
    /// # Errors
    /// Returns [`WsError::Incomplete`] when more bytes are needed;
    /// [`WsError::ReservedBitSet`] for an unnegotiated extension,
    /// [`WsError::Fragmented`] for a FIN-clear frame excluded by the fixed
    /// `WebSocketPeer` contract, or [`WsError::UnsupportedOpcode`] for an opcode
    /// outside the bridge contract;
    /// [`WsError::UnmaskedClientFrame`] for an unmasked client frame;
    /// [`WsError::NonMinimalLengthEncoding`] or [`WsError::Invalid64BitLength`]
    /// for a non-canonical length; [`WsError::ControlFrameTooLarge`] or
    /// [`WsError::FrameTooLarge`] for a declared length over its applicable cap;
    /// [`WsError::NotUtf8`] for invalid text, and the `InvalidClose*` variants for
    /// an invalid Close payload.
    pub fn parse_from_client(input: &[u8]) -> Result<Self, WsError> {
        Self::decode(input, true)
    }

    fn decode(input: &[u8], require_mask: bool) -> Result<Self, WsError> {
        if input.len() < 2 {
            return Err(WsError::Incomplete {
                needed: 2 - input.len(),
            });
        }
        let first = input[0];
        let second = input[1];

        if first & 0x70 != 0 {
            return Err(WsError::ReservedBitSet);
        }
        if first & 0x80 == 0 {
            return Err(WsError::Fragmented);
        }
        let opcode = Opcode::from_u8(first & 0x0f).ok_or(WsError::UnsupportedOpcode {
            opcode: first & 0x0f,
        })?;

        let masked = second & 0x80 != 0;
        if require_mask && !masked {
            return Err(WsError::UnmaskedClientFrame);
        }
        if !require_mask && masked {
            return Err(WsError::MaskedServerFrame);
        }

        let short = second & 0x7f;
        let (declared, mut offset) = match short {
            126 => {
                if input.len() < 4 {
                    return Err(WsError::Incomplete {
                        needed: 4 - input.len(),
                    });
                }
                let declared = u64::from(u16::from_be_bytes([input[2], input[3]]));
                if declared < 126 {
                    return Err(WsError::NonMinimalLengthEncoding {
                        marker: 126,
                        declared,
                    });
                }
                (declared, 4)
            }
            127 => {
                if input.len() < 10 {
                    return Err(WsError::Incomplete {
                        needed: 10 - input.len(),
                    });
                }
                let mut bytes = [0_u8; 8];
                bytes.copy_from_slice(&input[2..10]);
                if bytes[0] & 0x80 != 0 {
                    return Err(WsError::Invalid64BitLength);
                }
                let declared = u64::from_be_bytes(bytes);
                if declared < 65_536 {
                    return Err(WsError::NonMinimalLengthEncoding {
                        marker: 127,
                        declared,
                    });
                }
                (declared, 10)
            }
            other => (u64::from(other), 2),
        };

        if opcode.is_control() && declared > 125 {
            return Err(WsError::ControlFrameTooLarge { declared });
        }
        // Decided from the header alone: no payload allocation happens first.
        let length = Self::validate_declared_length(declared)?;

        let mask = if masked {
            if input.len() < offset + 4 {
                return Err(WsError::Incomplete {
                    needed: offset + 4 - input.len(),
                });
            }
            let mask = [
                input[offset],
                input[offset + 1],
                input[offset + 2],
                input[offset + 3],
            ];
            offset += 4;
            Some(mask)
        } else {
            None
        };

        if input.len() < offset + length {
            return Err(WsError::Incomplete {
                needed: offset + length - input.len(),
            });
        }

        let mut payload = input[offset..offset + length].to_vec();
        if let Some(mask) = mask {
            for (index, byte) in payload.iter_mut().enumerate() {
                *byte ^= mask[index % 4];
            }
        }
        if opcode == Opcode::Text && std::str::from_utf8(&payload).is_err() {
            return Err(WsError::NotUtf8);
        }
        if opcode == Opcode::Close {
            validate_close_payload(&payload)?;
        }

        Ok(Self {
            opcode,
            payload,
            consumed: offset + length,
        })
    }
}

/// Encode a server-to-client frame. Server frames are never masked (§5.1).
///
/// # Errors
/// Returns [`WsError::ControlFrameTooLarge`] for a control payload over 125 bytes,
/// an `InvalidClose*` variant for an invalid Close payload, or
/// [`WsError::FrameTooLarge`] for any payload over the bridge ceiling. Nothing is
/// produced, so an unsendable frame never reaches a socket.
pub fn encode_server_frame(opcode: Opcode, payload: &[u8]) -> Result<Vec<u8>, WsError> {
    encode(opcode, payload, None)
}

/// Encode a server-to-client text frame.
///
/// # Errors
/// Returns [`WsError::FrameTooLarge`] for an oversize payload.
pub fn encode_server_text(text: &str) -> Result<Vec<u8>, WsError> {
    encode_server_frame(Opcode::Text, text.as_bytes())
}

/// Encode a client-to-server text frame with an explicit mask.
///
/// The mask is a parameter rather than generated here so a test can produce
/// byte-identical frames; production clients are Godot's `WebSocketPeer`, which
/// generates its own.
///
/// # Errors
/// Returns [`WsError::FrameTooLarge`] for an oversize payload.
pub fn encode_client_text(text: &str, mask: [u8; 4]) -> Result<Vec<u8>, WsError> {
    encode(Opcode::Text, text.as_bytes(), Some(mask))
}

/// Encode a close frame carrying `code` and `reason` (§5.5.1).
///
/// # Errors
/// Returns [`WsError::InvalidCloseStatusCode`] for a code forbidden on the wire,
/// or [`WsError::ControlFrameTooLarge`] when the code and reason exceed a control
/// frame's 125-byte payload cap.
pub fn encode_close(code: u16, reason: &str) -> Result<Vec<u8>, WsError> {
    validate_close_status_code(code)?;
    let mut payload = Vec::with_capacity(2 + reason.len());
    payload.extend_from_slice(&code.to_be_bytes());
    payload.extend_from_slice(reason.as_bytes());
    encode_server_frame(Opcode::Close, &payload)
}

const fn validate_close_status_code(code: u16) -> Result<(), WsError> {
    if matches!(code, 1000..=1003 | 1007..=1014 | 3000..=4999) {
        Ok(())
    } else {
        Err(WsError::InvalidCloseStatusCode { code })
    }
}

fn validate_close_payload(payload: &[u8]) -> Result<(), WsError> {
    if payload.is_empty() {
        return Ok(());
    }
    if payload.len() == 1 {
        return Err(WsError::InvalidClosePayloadLength);
    }
    let code = u16::from_be_bytes([payload[0], payload[1]]);
    validate_close_status_code(code)?;
    std::str::from_utf8(&payload[2..])
        .map(|_reason| ())
        .map_err(|_utf8| WsError::InvalidCloseReason)
}

fn encode(opcode: Opcode, payload: &[u8], mask: Option<[u8; 4]>) -> Result<Vec<u8>, WsError> {
    if opcode == Opcode::Close {
        validate_close_payload(payload)?;
    }
    if opcode.is_control() && payload.len() > 125 {
        return Err(WsError::ControlFrameTooLarge {
            declared: payload.len() as u64,
        });
    }
    if payload.len() > MAX_FRAME_BYTES {
        return Err(WsError::FrameTooLarge {
            declared: payload.len() as u64,
        });
    }
    let mut frame = Vec::with_capacity(payload.len() + 14);
    frame.push(0x80 | opcode.as_u8());
    let mask_bit = if mask.is_some() { 0x80 } else { 0x00 };
    let length = payload.len();
    if length < 126 {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the branch proves length < 126"
        )]
        frame.push(mask_bit | length as u8);
    } else if length <= usize::from(u16::MAX) {
        frame.push(mask_bit | 126);
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the branch proves length fits in u16"
        )]
        frame.extend_from_slice(&(length as u16).to_be_bytes());
    } else {
        frame.push(mask_bit | 127);
        frame.extend_from_slice(&(length as u64).to_be_bytes());
    }
    match mask {
        Some(mask) => {
            frame.extend_from_slice(&mask);
            for (index, byte) in payload.iter().enumerate() {
                frame.push(byte ^ mask[index % 4]);
            }
        }
        None => frame.extend_from_slice(payload),
    }
    Ok(frame)
}

/// `Sec-WebSocket-Accept` for `key`: base64(SHA-1(key + GUID)) (§4.2.2).
#[must_use]
pub fn accept_key(key: &str) -> String {
    let mut input = String::with_capacity(key.len() + WEBSOCKET_GUID.len());
    input.push_str(key);
    input.push_str(WEBSOCKET_GUID);
    base64_encode(&sha1(input.as_bytes()))
}

/// The client's opening-handshake request for `endpoint`.
///
/// Written out here so a fixture can produce exactly what `WebSocketPeer` sends.
#[must_use]
pub fn client_handshake_request(endpoint: SocketAddr, key: &str) -> String {
    format!(
        "GET / HTTP/1.1\r\n\
         Host: {endpoint}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: {key}\r\n\
         Sec-WebSocket-Version: 13\r\n\
         Sec-WebSocket-Protocol: {SUBPROTOCOL}\r\n\
         \r\n"
    )
}

/// The server's `101 Switching Protocols` response for `key`.
#[must_use]
pub fn server_handshake_response(key: &str) -> String {
    format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {}\r\n\
         Sec-WebSocket-Protocol: {SUBPROTOCOL}\r\n\
         \r\n",
        accept_key(key)
    )
}

/// Extract `Sec-WebSocket-Key` from a client's opening handshake.
///
/// # Errors
/// Returns [`WsError::NotAnUpgrade`] when the request is not a WebSocket `GET`
/// upgrade, [`WsError::WrongHttpVersion`] unless the request uses HTTP/1.1,
/// [`WsError::MissingConnectionUpgrade`] when `Connection` lacks `Upgrade`,
/// [`WsError::UnsupportedWebSocketVersion`] unless the version is 13,
/// [`WsError::MissingHost`] when no non-empty `Host` header is present,
/// [`WsError::MissingKey`] when the key header is absent,
/// [`WsError::InvalidWebSocketKey`] unless the key is canonical base64 of 16 bytes,
/// [`WsError::ClientSubprotocolMismatch`] unless the client offers [`SUBPROTOCOL`],
/// [`WsError::HandshakeTooLarge`] past the size cap, or
/// [`WsError::MalformedHandshake`] for non-UTF-8 / unparsable header lines.
pub fn parse_client_handshake(bytes: &[u8]) -> Result<String, WsError> {
    let (start, headers) = parse_head(bytes)?;
    let mut request = start.split_ascii_whitespace();
    if request.next() != Some("GET") || request.next().is_none() {
        return Err(WsError::NotAnUpgrade);
    }
    if request.next() != Some("HTTP/1.1") || request.next().is_some() {
        return Err(WsError::WrongHttpVersion);
    }
    let upgrade = header(&headers, "upgrade").unwrap_or_default();
    if !upgrade.eq_ignore_ascii_case("websocket") {
        return Err(WsError::NotAnUpgrade);
    }
    if header(&headers, "host").is_none_or(|host| host.is_empty()) {
        return Err(WsError::MissingHost);
    }
    if !header_has_token(&headers, "connection", "upgrade") {
        return Err(WsError::MissingConnectionUpgrade);
    }
    if header(&headers, "sec-websocket-version").as_deref() != Some("13") {
        return Err(WsError::UnsupportedWebSocketVersion);
    }
    let key = header(&headers, "sec-websocket-key").ok_or(WsError::MissingKey)?;
    let Some(nonce) = base64_decode(&key) else {
        return Err(WsError::InvalidWebSocketKey);
    };
    if nonce.len() != 16 {
        return Err(WsError::InvalidWebSocketKey);
    }
    if !header_has_exact_token(&headers, "sec-websocket-protocol", SUBPROTOCOL) {
        return Err(WsError::ClientSubprotocolMismatch);
    }
    Ok(key)
}

/// Verify a server's opening-handshake response against the key that was sent.
///
/// # Errors
/// Returns [`WsError::WrongHttpVersion`] unless the response uses HTTP/1.1,
/// [`WsError::NotAnUpgrade`] unless the status line is `101` and the `Upgrade`
/// header names WebSocket, [`WsError::MissingConnectionUpgrade`] when
/// `Connection` lacks `Upgrade`, [`WsError::MissingKey`] when
/// `Sec-WebSocket-Accept` is absent,
/// [`WsError::AcceptKeyMismatch`] when it does not match,
/// [`WsError::ServerSubprotocolMismatch`] unless the server echoes [`SUBPROTOCOL`],
/// plus the malformed / oversize handshake errors.
pub fn parse_server_handshake(bytes: &[u8], key: &str) -> Result<(), WsError> {
    let (start, headers) = parse_head(bytes)?;
    let mut response = start.split_ascii_whitespace();
    if response.next() != Some("HTTP/1.1") {
        return Err(WsError::WrongHttpVersion);
    }
    if response.next() != Some("101") {
        return Err(WsError::NotAnUpgrade);
    }
    let upgrade = header(&headers, "upgrade").unwrap_or_default();
    if !upgrade.eq_ignore_ascii_case("websocket") {
        return Err(WsError::NotAnUpgrade);
    }
    if !header_has_token(&headers, "connection", "upgrade") {
        return Err(WsError::MissingConnectionUpgrade);
    }
    let accept = header(&headers, "sec-websocket-accept").ok_or(WsError::MissingKey)?;
    if accept != accept_key(key) {
        return Err(WsError::AcceptKeyMismatch);
    }
    if !header_has_exact_token(&headers, "sec-websocket-protocol", SUBPROTOCOL) {
        return Err(WsError::ServerSubprotocolMismatch);
    }
    Ok(())
}

fn parse_head(bytes: &[u8]) -> Result<(String, Vec<(String, String)>), WsError> {
    if bytes.len() > MAX_HANDSHAKE_BYTES {
        return Err(WsError::HandshakeTooLarge);
    }
    let text = std::str::from_utf8(bytes).map_err(|error| WsError::MalformedHandshake {
        detail: format!("handshake bytes are not UTF-8: {error}"),
    })?;
    let mut lines = text.split("\r\n");
    let start = lines
        .next()
        .ok_or_else(|| WsError::MalformedHandshake {
            detail: "handshake has no start line".to_owned(),
        })?
        .to_owned();
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(WsError::MalformedHandshake {
                detail: format!("header line {line:?} has no `Name: value` separator"),
            });
        };
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
    }
    Ok((start, headers))
}

fn header(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(key, _value)| key == name)
        .map(|(_key, value)| value.clone())
}

fn header_has_token(headers: &[(String, String)], name: &str, expected: &str) -> bool {
    header_has_token_matching(headers, name, |token| token.eq_ignore_ascii_case(expected))
}

fn header_has_exact_token(headers: &[(String, String)], name: &str, expected: &str) -> bool {
    header_has_token_matching(headers, name, |token| token == expected)
}

fn header_has_token_matching(
    headers: &[(String, String)],
    name: &str,
    predicate: impl FnMut(&str) -> bool,
) -> bool {
    headers
        .iter()
        .filter(|(key, _value)| key == name)
        .flat_map(|(_key, value)| value.split(','))
        .map(str::trim)
        .any(predicate)
}

/// Find the end of a handshake header section, if it has arrived.
///
/// # Errors
/// Returns [`WsError::HandshakeTooLarge`] when the cap is passed with no
/// terminator in sight.
pub fn handshake_head_len(buffer: &[u8]) -> Result<Option<usize>, WsError> {
    if let Some(offset) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
        return Ok(Some(offset + 4));
    }
    if buffer.len() > MAX_HANDSHAKE_BYTES {
        return Err(WsError::HandshakeTooLarge);
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use super::{
        MAX_FRAME_BYTES, Opcode, WEBSOCKET_GUID, WsError, WsFrame, accept_key,
        client_handshake_request, encode_client_text, encode_close, encode_server_text,
        handshake_head_len, parse_client_handshake, parse_server_handshake,
        server_handshake_response,
    };

    fn endpoint() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 45_871)
    }

    /// RFC 6455 §1.3's worked example, and the GUID it depends on.
    #[test]
    fn the_accept_key_matches_the_rfc_6455_worked_example() {
        assert_eq!(WEBSOCKET_GUID, "258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
        assert_eq!(
            accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    /// RFC 6455 §5.7's single-frame masked "Hello", byte for byte.
    #[test]
    fn a_masked_text_frame_matches_the_rfc_6455_example_bytes() {
        let frame = encode_client_text("Hello", [0x37, 0xfa, 0x21, 0x3d]).expect("encode");
        assert_eq!(
            frame,
            vec![
                0x81, 0x85, 0x37, 0xfa, 0x21, 0x3d, 0x7f, 0x9f, 0x4d, 0x51, 0x58
            ]
        );
        let parsed = WsFrame::parse_from_client(&frame).expect("parse");
        assert_eq!(parsed.opcode, Opcode::Text);
        assert_eq!(parsed.payload, b"Hello");
        assert_eq!(parsed.consumed, frame.len());
    }

    /// RFC 6455 §5.7's single-frame unmasked "Hello".
    #[test]
    fn an_unmasked_text_frame_matches_the_rfc_6455_example_bytes() {
        let frame = encode_server_text("Hello").expect("encode");
        assert_eq!(frame, vec![0x81, 0x05, b'H', b'e', b'l', b'l', b'o']);
        let parsed = WsFrame::parse(&frame).expect("parse");
        assert_eq!(parsed.payload, b"Hello");
    }

    #[test]
    fn the_three_length_encodings_all_round_trip() {
        for length in [0_usize, 125, 126, 65_535, 65_536] {
            let text = "a".repeat(length);
            let frame = encode_server_text(&text).expect("encode");
            let parsed = WsFrame::parse(&frame).expect("parse");
            assert_eq!(parsed.payload.len(), length, "length {length}");
            assert_eq!(parsed.consumed, frame.len(), "length {length}");
        }
    }

    #[test]
    fn an_oversize_declaration_is_refused_from_the_header_alone() {
        let declared = 16 * 1024 * 1024_u64;
        let mut header = vec![0x81_u8, 0x7f];
        header.extend_from_slice(&declared.to_be_bytes());
        assert_eq!(header.len(), 10, "only the header is in hand");
        assert_eq!(
            WsFrame::parse(&header),
            Err(WsError::FrameTooLarge { declared })
        );
        assert_eq!(
            WsFrame::validate_declared_length(MAX_FRAME_BYTES as u64 + 1),
            Err(WsError::FrameTooLarge {
                declared: MAX_FRAME_BYTES as u64 + 1
            })
        );
        assert_eq!(
            WsFrame::validate_declared_length(MAX_FRAME_BYTES as u64),
            Ok(MAX_FRAME_BYTES)
        );
    }

    #[test]
    fn an_oversize_outbound_payload_is_refused_before_encoding() {
        let text = "x".repeat(MAX_FRAME_BYTES + 1);
        assert_eq!(
            encode_server_text(&text),
            Err(WsError::FrameTooLarge {
                declared: MAX_FRAME_BYTES as u64 + 1
            })
        );
    }

    #[test]
    fn a_control_frame_over_125_bytes_is_refused_from_its_header() {
        let header = [0x89, 126, 0, 126];
        assert_eq!(
            WsFrame::parse(&header),
            Err(WsError::ControlFrameTooLarge { declared: 126 })
        );
    }

    #[test]
    fn extended_payload_lengths_must_use_the_minimal_encoding() {
        let cases = [
            (
                vec![0x81, 126, 0, 125],
                WsError::NonMinimalLengthEncoding {
                    marker: 126,
                    declared: 125,
                },
            ),
            (
                {
                    let mut frame = vec![0x81, 127];
                    frame.extend_from_slice(&65_535_u64.to_be_bytes());
                    frame
                },
                WsError::NonMinimalLengthEncoding {
                    marker: 127,
                    declared: 65_535,
                },
            ),
        ];
        for (frame, expected) in cases {
            assert_eq!(WsFrame::parse(&frame), Err(expected));
        }
    }

    #[test]
    fn a_64_bit_payload_length_with_its_high_bit_set_is_refused() {
        let mut header = vec![0x81, 127];
        header.extend_from_slice(&(1_u64 << 63).to_be_bytes());
        assert_eq!(WsFrame::parse(&header), Err(WsError::Invalid64BitLength));
    }

    #[test]
    fn a_masked_server_frame_is_refused() {
        let frame = encode_client_text("server must not mask", [1, 2, 3, 4]).expect("encode");
        assert_eq!(WsFrame::parse(&frame), Err(WsError::MaskedServerFrame));
    }

    #[test]
    fn frames_outside_the_bridge_peer_contract_are_each_refused_specifically() {
        // Fragmented (FIN clear).
        assert_eq!(WsFrame::parse(&[0x01, 0x00]), Err(WsError::Fragmented));
        // A reserved bit set.
        assert_eq!(WsFrame::parse(&[0xc1, 0x00]), Err(WsError::ReservedBitSet));
        // Binary and continuation opcodes.
        assert_eq!(
            WsFrame::parse(&[0x82, 0x00]),
            Err(WsError::UnsupportedOpcode { opcode: 0x2 })
        );
        assert_eq!(
            WsFrame::parse(&[0x80, 0x00]),
            Err(WsError::UnsupportedOpcode { opcode: 0x0 })
        );
        // An unmasked client frame.
        assert_eq!(
            WsFrame::parse_from_client(&[0x81, 0x00]),
            Err(WsError::UnmaskedClientFrame)
        );
        // Invalid UTF-8 in a text frame.
        assert_eq!(WsFrame::parse(&[0x81, 0x01, 0xff]), Err(WsError::NotUtf8));
    }

    #[test]
    fn a_short_read_reports_incomplete_and_nothing_else() {
        assert!(WsFrame::parse(&[]).expect_err("empty").is_incomplete());
        assert!(
            WsFrame::parse(&[0x81])
                .expect_err("one byte")
                .is_incomplete()
        );
        assert!(
            WsFrame::parse(&[0x81, 0x05, b'H'])
                .expect_err("truncated payload")
                .is_incomplete()
        );
        assert!(
            WsFrame::parse(&[0x81, 0x7e, 0x01])
                .expect_err("truncated 16-bit length")
                .is_incomplete()
        );
        assert!(
            WsFrame::parse_from_client(&[0x81, 0x85, 0x37])
                .expect_err("truncated mask")
                .is_incomplete()
        );
    }

    #[test]
    fn coalesced_frames_are_returned_one_at_a_time() {
        let mut buffer = encode_server_text("one").expect("encode");
        buffer.extend_from_slice(&encode_server_text("two").expect("encode"));
        let first = WsFrame::parse(&buffer).expect("first frame");
        assert_eq!(first.payload, b"one");
        let second = WsFrame::parse(&buffer[first.consumed..]).expect("second frame");
        assert_eq!(second.payload, b"two");
        assert_eq!(first.consumed + second.consumed, buffer.len());
    }

    #[test]
    fn close_ping_and_pong_frames_decode() {
        let close = encode_close(1000, "bye").expect("encode");
        let parsed = WsFrame::parse(&close).expect("parse");
        assert_eq!(parsed.opcode, Opcode::Close);
        assert_eq!(&parsed.payload[..2], &1000_u16.to_be_bytes());
        assert_eq!(&parsed.payload[2..], b"bye");

        let ping = super::encode_server_frame(Opcode::Ping, b"p").expect("encode");
        assert_eq!(WsFrame::parse(&ping).expect("parse").opcode, Opcode::Ping);
        let pong = super::encode_server_frame(Opcode::Pong, b"p").expect("encode");
        assert_eq!(WsFrame::parse(&pong).expect("parse").opcode, Opcode::Pong);
    }

    #[test]
    fn invalid_close_payloads_are_refused() {
        let cases = [
            (
                "one-byte payload",
                vec![0x88, 0x01, 0x03],
                WsError::InvalidClosePayloadLength,
            ),
            (
                "reserved status code",
                vec![0x88, 0x02, 0x03, 0xed],
                WsError::InvalidCloseStatusCode { code: 1005 },
            ),
            (
                "non-UTF-8 reason",
                vec![0x88, 0x03, 0x03, 0xe8, 0xff],
                WsError::InvalidCloseReason,
            ),
        ];

        for (name, frame, expected) in cases {
            assert_eq!(WsFrame::parse(&frame), Err(expected), "case {name}");
        }
        assert_eq!(
            super::encode_server_frame(Opcode::Close, &[0x03]),
            Err(WsError::InvalidClosePayloadLength)
        );
    }

    #[test]
    fn the_opening_handshake_round_trips_and_rejects_every_deviation() {
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let request = client_handshake_request(endpoint(), key);
        assert_eq!(
            parse_client_handshake(request.as_bytes()).expect("parse"),
            key
        );
        let response = server_handshake_response(key);
        parse_server_handshake(response.as_bytes(), key).expect("accept");
        assert_eq!(
            parse_server_handshake(response.as_bytes(), "AAAAAAAAAAAAAAAAAAAAAA=="),
            Err(WsError::AcceptKeyMismatch)
        );
        assert_eq!(
            parse_client_handshake(b"POST / HTTP/1.1\r\nUpgrade: websocket\r\n\r\n"),
            Err(WsError::NotAnUpgrade)
        );
        assert_eq!(
            parse_client_handshake(b"GET / HTTP/1.1\r\nUpgrade: h2c\r\n\r\n"),
            Err(WsError::NotAnUpgrade)
        );
        assert_eq!(
            parse_client_handshake(
                b"GET / HTTP/1.1\r\nHost: 127.0.0.1:45871\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\n\r\n"
            ),
            Err(WsError::MissingKey)
        );
        assert!(matches!(
            parse_client_handshake(b"GET / HTTP/1.1\r\nnot-a-header\r\n\r\n"),
            Err(WsError::MalformedHandshake { .. })
        ));
        assert_eq!(
            parse_server_handshake(b"HTTP/1.1 400 Bad Request\r\n\r\n", key),
            Err(WsError::NotAnUpgrade)
        );
    }

    #[test]
    fn a_client_handshake_rejects_a_key_that_is_not_base64_of_16_bytes() {
        for key in ["x", "YWJj"] {
            let request = format!(
                "GET / HTTP/1.1\r\nHost: 127.0.0.1:45871\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: {}\r\n\r\n",
                super::SUBPROTOCOL
            );
            assert_eq!(
                parse_client_handshake(request.as_bytes()),
                Err(WsError::InvalidWebSocketKey),
                "key {key:?}"
            );
        }
    }

    #[test]
    fn a_client_handshake_requires_a_host_header() {
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let request = format!(
            "GET / HTTP/1.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: {}\r\n\r\n",
            super::SUBPROTOCOL
        );
        assert_eq!(
            parse_client_handshake(request.as_bytes()),
            Err(WsError::MissingHost)
        );
    }

    #[test]
    fn a_client_handshake_requires_the_frozen_subprotocol_token() {
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let missing = format!(
            "GET / HTTP/1.1\r\nHost: 127.0.0.1:45871\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        );
        assert_eq!(
            parse_client_handshake(missing.as_bytes()),
            Err(WsError::ClientSubprotocolMismatch)
        );

        let wrong = format!(
            "GET / HTTP/1.1\r\nHost: 127.0.0.1:45871\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: godot-mcp-bridge.v2\r\n\r\n"
        );
        assert_eq!(
            parse_client_handshake(wrong.as_bytes()),
            Err(WsError::ClientSubprotocolMismatch)
        );

        let offered_in_a_list = format!(
            "GET / HTTP/1.1\r\nHost: 127.0.0.1:45871\r\nUpgrade: websocket\r\nConnection: keep-alive, Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: unrelated,  {} \r\n\r\n",
            super::SUBPROTOCOL
        );
        assert_eq!(
            parse_client_handshake(offered_in_a_list.as_bytes()),
            Ok(key.to_owned())
        );
    }

    #[test]
    fn a_server_handshake_requires_the_frozen_subprotocol_token() {
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let accept = accept_key(key);
        let missing = format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
        );
        assert_eq!(
            parse_server_handshake(missing.as_bytes(), key),
            Err(WsError::ServerSubprotocolMismatch)
        );

        let wrong = format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\nSec-WebSocket-Protocol: godot-mcp-bridge.v2\r\n\r\n"
        );
        assert_eq!(
            parse_server_handshake(wrong.as_bytes(), key),
            Err(WsError::ServerSubprotocolMismatch)
        );

        let echoed_in_a_list = format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\nSec-WebSocket-Protocol: unrelated,  {} \r\n\r\n",
            super::SUBPROTOCOL
        );
        assert_eq!(
            parse_server_handshake(echoed_in_a_list.as_bytes(), key),
            Ok(())
        );
    }

    #[test]
    fn both_handshake_parsers_fail_closed_on_required_upgrade_members() {
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        assert_eq!(
            parse_client_handshake(
                format!(
                    "GET / HTTP/1.0\r\nHost: 127.0.0.1:45871\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
                )
                .as_bytes()
            ),
            Err(WsError::WrongHttpVersion)
        );
        assert_eq!(
            parse_client_handshake(
                format!(
                    "GET / HTTP/1.1\r\nHost: 127.0.0.1:45871\r\nUpgrade: websocket\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
                )
                .as_bytes()
            ),
            Err(WsError::MissingConnectionUpgrade)
        );
        assert_eq!(
            parse_client_handshake(
                format!(
                    "GET / HTTP/1.1\r\nHost: 127.0.0.1:45871\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 12\r\n\r\n"
                )
                .as_bytes()
            ),
            Err(WsError::UnsupportedWebSocketVersion)
        );

        let accept = accept_key(key);
        assert_eq!(
            parse_server_handshake(
                format!(
                    "HTTP/1.0 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
                )
                .as_bytes(),
                key
            ),
            Err(WsError::WrongHttpVersion)
        );
        assert_eq!(
            parse_server_handshake(
                format!(
                    "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
                )
                .as_bytes(),
                key
            ),
            Err(WsError::MissingConnectionUpgrade)
        );
        assert_eq!(
            parse_server_handshake(
                format!(
                    "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
                )
                .as_bytes(),
                key
            ),
            Err(WsError::NotAnUpgrade)
        );
    }

    #[test]
    fn the_handshake_head_is_bounded() {
        assert_eq!(handshake_head_len(b"GET /"), Ok(None));
        assert_eq!(handshake_head_len(b"GET /\r\n\r\nrest"), Ok(Some(9)));
        let flood = vec![b'a'; super::MAX_HANDSHAKE_BYTES + 1];
        assert_eq!(handshake_head_len(&flood), Err(WsError::HandshakeTooLarge));
    }

    /// The negotiated subprotocol is one frozen string, present in both
    /// directions of the handshake.
    #[test]
    fn both_handshake_directions_name_the_frozen_subprotocol() {
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        assert_eq!(super::SUBPROTOCOL, "godot-mcp-bridge.v1");
        let request = client_handshake_request(endpoint(), key);
        assert!(request.contains(super::SUBPROTOCOL));
        assert_eq!(
            parse_client_handshake(request.as_bytes()),
            Ok(key.to_owned())
        );
        let response = server_handshake_response(key);
        assert!(response.contains(super::SUBPROTOCOL));
        assert_eq!(parse_server_handshake(response.as_bytes(), key), Ok(()));
    }
}
