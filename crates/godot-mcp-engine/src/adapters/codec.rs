//! The raw-TCP `Content-Length` frame codec both Godot protocols speak.
//!
//! # This is not WebSocket
//!
//! Godot 3.1/3.2.0 hosted the GDScript language server over WebSocket. Commit
//! `24b27043f` ("Migrating language server from Websockets to raw TCP", 2020-02-01)
//! replaced it, and every 4.x build reads and writes **plain TCP bytes framed by an
//! LSP-style `Content-Length` header**. `LSPeer::handle_data` in
//! `modules/gdscript/language_server/gdscript_language_protocol.cpp` scans for
//! `\r\n\r\n`, takes the length out of the header and then reads exactly that many
//! payload bytes; `format_output` writes `Content-Length: <n>\r\n\r\n` followed by
//! the body. `DAPeer` in
//! `editor/debugger/debug_adapter/debug_adapter_protocol.cpp` is the same shape.
//! There is no HTTP upgrade, no WebSocket handshake and no frame masking anywhere
//! in either path, so speaking WebSocket at these ports would simply never parse.
//!
//! # The 4 MiB ceiling is the engine's own, not a preference
//!
//! Both engine-side peers hold a **fixed inline buffer**:
//! `#define LSP_MAX_BUFFER_SIZE 4194304` in `gdscript_language_protocol.h` and
//! `#define DAP_MAX_BUFFER_SIZE 4194304` in `debug_adapter_protocol.h`, each backing
//! a `uint8_t req_buf[4194304]` per peer. A larger message does not stream, it
//! fails with `ERR_OUT_OF_MEMORY` on the engine side. So [`MAX_PAYLOAD_BYTES`] is a
//! hard refusal here: an oversize frame is rejected **before** any allocation and
//! before any effect is dispatched, rather than being sent and lost.
//!
//! # Fragmentation and coalescing are both normal
//!
//! TCP gives no message boundaries. One `read` can return half a header, and one
//! `read` can return three whole frames plus a partial fourth. [`FrameReader`]
//! therefore owns a persistent buffer: it returns exactly one payload per
//! [`FrameReader::read_frame`] call and keeps whatever followed for the next call.
//! Everything decidable about a frame is decided from the buffer, never from how
//! many bytes a particular syscall happened to return.

use std::io::{self, Read, Write};

/// Hard maximum payload bytes for one frame, in either direction.
///
/// Exactly the engine's own `LSP_MAX_BUFFER_SIZE` / `DAP_MAX_BUFFER_SIZE`.
pub const MAX_PAYLOAD_BYTES: usize = 4 * 1024 * 1024;

/// Hard maximum header-section bytes (everything before `\r\n\r\n`).
///
/// The engine parses only the first header line, so a legitimate header is a few
/// dozen bytes. This cap exists so a peer that never sends the terminator cannot
/// make this process grow without bound.
pub const MAX_HEADER_BYTES: usize = 1024;

/// The header the engine's own `format_output` writes, lowercased for matching.
const CONTENT_LENGTH: &str = "content-length";

/// The header terminator both engine peers scan for.
const HEADER_TERMINATOR: &[u8] = b"\r\n\r\n";

/// Why a frame could not be read or written.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CodecError {
    /// The header section exceeded [`MAX_HEADER_BYTES`] with no terminator.
    #[error("frame header exceeded {MAX_HEADER_BYTES} bytes with no CRLFCRLF terminator")]
    HeaderTooLarge,

    /// The header section is not parsable as `Name: value` lines.
    #[error("frame header is malformed: {detail}")]
    MalformedHeader {
        /// Exactly what was wrong, quotable in a diagnostic.
        detail: String,
    },

    /// No `Content-Length` header was present.
    #[error("frame header carries no Content-Length")]
    MissingContentLength,

    /// The declared payload length exceeds [`MAX_PAYLOAD_BYTES`].
    ///
    /// Refused without allocating the declared size and without dispatching any
    /// effect.
    #[error(
        "frame declares {declared} payload bytes, over the {MAX_PAYLOAD_BYTES}-byte engine limit"
    )]
    PayloadTooLarge {
        /// The length the peer declared.
        declared: u64,
    },

    /// The peer closed the connection mid-frame (or before one started).
    #[error("the peer closed the connection after {read} of {expected} expected bytes")]
    Eof {
        /// How many bytes of the current frame had arrived.
        read: usize,
        /// How many were expected. Zero means no frame had started.
        expected: usize,
    },

    /// The stream is unusable after a refused frame.
    ///
    /// A rejected `Content-Length` cannot be skipped: doing so would mean reading
    /// and discarding the very bytes that were refused. The reader therefore fails
    /// closed and every later call reports this instead of resynchronising onto
    /// attacker-chosen bytes.
    #[error("the framed stream was abandoned after a refused frame and cannot be resynchronised")]
    Abandoned,

    /// The underlying socket failed.
    #[error("framed stream I/O failure: {detail}")]
    Io {
        /// What the OS reported.
        detail: String,
    },
}

impl CodecError {
    /// Whether this failure is a protocol violation by the peer rather than a
    /// transport fault.
    #[must_use]
    pub const fn is_protocol_violation(&self) -> bool {
        matches!(
            self,
            Self::HeaderTooLarge
                | Self::MalformedHeader { .. }
                | Self::MissingContentLength
                | Self::PayloadTooLarge { .. }
        )
    }

    fn io(error: &io::Error) -> Self {
        Self::Io {
            detail: error.to_string(),
        }
    }
}

/// Encode one payload as a framed message.
///
/// # Errors
/// Returns [`CodecError::PayloadTooLarge`] when the payload exceeds the engine's
/// own fixed buffer, so an unsendable frame is refused here instead of being
/// written into a socket the engine will fail on.
pub fn encode_frame(payload: &[u8]) -> Result<Vec<u8>, CodecError> {
    if payload.len() > MAX_PAYLOAD_BYTES {
        return Err(CodecError::PayloadTooLarge {
            declared: payload.len() as u64,
        });
    }
    let header = format!("Content-Length: {}\r\n\r\n", payload.len());
    let mut frame = Vec::with_capacity(header.len() + payload.len());
    frame.extend_from_slice(header.as_bytes());
    frame.extend_from_slice(payload);
    Ok(frame)
}

/// Write one framed message to `sink`.
///
/// # Errors
/// Returns [`CodecError::PayloadTooLarge`] for an oversize payload (nothing is
/// written) or [`CodecError::Io`] when the write itself fails.
pub fn write_frame(sink: &mut dyn Write, payload: &[u8]) -> Result<(), CodecError> {
    let frame = encode_frame(payload)?;
    sink.write_all(&frame)
        .map_err(|error| CodecError::io(&error))?;
    sink.flush().map_err(|error| CodecError::io(&error))
}

/// The parsed header section of one frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameHeader {
    /// The declared payload length.
    pub content_length: usize,
    /// Every header name/value pair, names lowercased.
    pub fields: Vec<(String, String)>,
}

/// Parse a complete header section (the bytes before `\r\n\r\n`).
///
/// # Errors
/// Returns [`CodecError::MalformedHeader`] for a non-UTF-8 or non-`Name: value`
/// header, [`CodecError::MissingContentLength`] when the length is absent, and
/// [`CodecError::PayloadTooLarge`] when the declared length is over the engine's
/// ceiling.
pub fn parse_header(bytes: &[u8]) -> Result<FrameHeader, CodecError> {
    let text = std::str::from_utf8(bytes).map_err(|error| CodecError::MalformedHeader {
        detail: format!("header bytes are not UTF-8: {error}"),
    })?;
    let mut fields = Vec::new();
    let mut content_length = None;
    for line in text.split("\r\n") {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(CodecError::MalformedHeader {
                detail: format!("header line {line:?} has no `Name: value` separator"),
            });
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim().to_owned();
        if name == CONTENT_LENGTH {
            let declared: u64 = value
                .parse()
                .map_err(|_ignored| CodecError::MalformedHeader {
                    detail: format!("Content-Length {value:?} is not a non-negative integer"),
                })?;
            if declared > MAX_PAYLOAD_BYTES as u64 {
                return Err(CodecError::PayloadTooLarge { declared });
            }
            // The cast is lossless: `declared` is already bounded by
            // MAX_PAYLOAD_BYTES, which is a `usize` constant.
            content_length = Some(declared as usize);
        }
        fields.push((name, value));
    }
    match content_length {
        Some(content_length) => Ok(FrameHeader {
            content_length,
            fields,
        }),
        None => Err(CodecError::MissingContentLength),
    }
}

/// A framed reader over one byte stream.
///
/// Owns a persistent buffer, so fragmented and coalesced frames are both handled
/// without the caller knowing how the bytes were delivered.
pub struct FrameReader<R> {
    inner: R,
    buffer: Vec<u8>,
    abandoned: bool,
    frames_read: u64,
}

impl<R: Read> FrameReader<R> {
    /// Wrap a byte stream.
    #[must_use]
    pub const fn new(inner: R) -> Self {
        Self {
            inner,
            buffer: Vec::new(),
            abandoned: false,
            frames_read: 0,
        }
    }

    /// How many complete frames this reader has returned.
    #[must_use]
    pub const fn frames_read(&self) -> u64 {
        self.frames_read
    }

    /// Bytes already buffered but not yet consumed by a frame.
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.buffer.len()
    }

    /// Whether the stream was abandoned by a refused frame.
    #[must_use]
    pub const fn is_abandoned(&self) -> bool {
        self.abandoned
    }

    /// Read exactly one payload.
    ///
    /// # Errors
    /// Returns [`CodecError::Eof`] when the peer closed, the header errors from
    /// [`parse_header`] for a violating peer, [`CodecError::HeaderTooLarge`] for a
    /// header with no terminator, and [`CodecError::Abandoned`] once a refusal has
    /// made resynchronisation impossible.
    pub fn read_frame(&mut self) -> Result<Vec<u8>, CodecError> {
        if self.abandoned {
            return Err(CodecError::Abandoned);
        }
        let header_end = match self.fill_until_header() {
            Ok(end) => end,
            Err(error) => return Err(self.abandon_on_violation(error)),
        };
        let header = match parse_header(&self.buffer[..header_end]) {
            Ok(header) => header,
            Err(error) => return Err(self.abandon_on_violation(error)),
        };
        let body_start = header_end + HEADER_TERMINATOR.len();
        let body_end = body_start + header.content_length;
        while self.buffer.len() < body_end {
            if !self.fill_more()? {
                let read = self.buffer.len().saturating_sub(body_start);
                return Err(CodecError::Eof {
                    read,
                    expected: header.content_length,
                });
            }
        }
        let payload = self.buffer[body_start..body_end].to_vec();
        self.buffer.drain(..body_end);
        self.frames_read += 1;
        Ok(payload)
    }

    /// A refused frame makes the rest of the stream untrustworthy: the bytes that
    /// would have to be skipped are exactly the bytes that were refused. Mark the
    /// reader closed rather than resynchronise.
    fn abandon_on_violation(&mut self, error: CodecError) -> CodecError {
        if error.is_protocol_violation() {
            self.abandoned = true;
            self.buffer.clear();
        }
        error
    }

    /// Buffer until the header terminator is present, returning its offset.
    fn fill_until_header(&mut self) -> Result<usize, CodecError> {
        loop {
            if let Some(offset) = find_subslice(&self.buffer, HEADER_TERMINATOR) {
                if offset > MAX_HEADER_BYTES {
                    return Err(CodecError::HeaderTooLarge);
                }
                return Ok(offset);
            }
            if self.buffer.len() > MAX_HEADER_BYTES {
                return Err(CodecError::HeaderTooLarge);
            }
            if !self.fill_more()? {
                return Err(CodecError::Eof {
                    read: self.buffer.len(),
                    expected: 0,
                });
            }
        }
    }

    /// One read into the buffer. `false` means clean end of stream.
    fn fill_more(&mut self) -> Result<bool, CodecError> {
        let mut chunk = [0_u8; 4096];
        match self.inner.read(&mut chunk) {
            Ok(0) => Ok(false),
            Ok(read) => {
                self.buffer.extend_from_slice(&chunk[..read]);
                Ok(true)
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => Ok(true),
            Err(error) => Err(CodecError::io(&error)),
        }
    }
}

/// First offset of `needle` in `haystack`.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reader that hands out exactly the chunks it was scripted with, so
    /// fragmentation is deterministic instead of timing-dependent.
    struct ScriptedReader {
        chunks: Vec<Vec<u8>>,
        index: usize,
    }

    impl ScriptedReader {
        fn new(chunks: Vec<Vec<u8>>) -> Self {
            Self { chunks, index: 0 }
        }
    }

    impl Read for ScriptedReader {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            if self.index >= self.chunks.len() {
                return Ok(0);
            }
            let chunk = &mut self.chunks[self.index];
            let take = chunk.len().min(out.len());
            out[..take].copy_from_slice(&chunk[..take]);
            chunk.drain(..take);
            if chunk.is_empty() {
                self.index += 1;
            }
            Ok(take)
        }
    }

    fn frame(payload: &str) -> Vec<u8> {
        encode_frame(payload.as_bytes()).expect("small payload")
    }

    #[test]
    fn a_frame_split_across_reads_is_reassembled() {
        let whole = frame("{\"jsonrpc\":\"2.0\"}");
        let chunks = whole
            .chunks(3)
            .map(<[u8]>::to_vec)
            .collect::<Vec<Vec<u8>>>();
        assert!(chunks.len() > 4, "the frame really is fragmented");
        let mut reader = FrameReader::new(ScriptedReader::new(chunks));
        assert_eq!(
            reader.read_frame().expect("reassembled"),
            b"{\"jsonrpc\":\"2.0\"}".to_vec()
        );
    }

    #[test]
    fn three_frames_coalesced_into_one_read_are_returned_one_at_a_time() {
        let mut blob = Vec::new();
        for payload in ["{\"a\":1}", "{\"b\":2}", "{\"c\":3}"] {
            blob.extend_from_slice(&frame(payload));
        }
        let mut reader = FrameReader::new(ScriptedReader::new(vec![blob]));
        assert_eq!(reader.read_frame().expect("first"), b"{\"a\":1}".to_vec());
        assert_eq!(reader.read_frame().expect("second"), b"{\"b\":2}".to_vec());
        assert_eq!(reader.read_frame().expect("third"), b"{\"c\":3}".to_vec());
        assert_eq!(reader.frames_read(), 3);
        assert_eq!(reader.buffered(), 0);
    }

    #[test]
    fn a_declared_length_over_four_mib_is_refused_without_allocating_it() {
        let declared = MAX_PAYLOAD_BYTES as u64 + 1;
        let header = format!("Content-Length: {declared}\r\n\r\n");
        let mut reader = FrameReader::new(ScriptedReader::new(vec![header.into_bytes()]));
        assert_eq!(
            reader.read_frame(),
            Err(CodecError::PayloadTooLarge { declared })
        );
        // The refusal is terminal: the refused bytes are never skipped over.
        assert!(reader.is_abandoned());
        assert_eq!(reader.read_frame(), Err(CodecError::Abandoned));
    }

    #[test]
    fn a_payload_at_exactly_the_limit_is_accepted() {
        // Only the header is read here; the body is proven by the length accounting
        // rather than by materialising four megabytes in a unit test.
        let header = format!("Content-Length: {MAX_PAYLOAD_BYTES}\r\n\r\n");
        let parsed = parse_header(header.trim_end_matches("\r\n\r\n").as_bytes())
            .expect("the ceiling itself is legal");
        assert_eq!(parsed.content_length, MAX_PAYLOAD_BYTES);
    }

    #[test]
    fn a_header_with_no_separator_is_malformed_and_terminal() {
        let mut reader = FrameReader::new(ScriptedReader::new(vec![
            b"not-a-header-line\r\n\r\n{}".to_vec(),
        ]));
        let error = reader.read_frame().expect_err("malformed");
        assert!(
            matches!(error, CodecError::MalformedHeader { .. }),
            "{error}"
        );
        assert!(reader.is_abandoned());
    }

    #[test]
    fn a_header_without_content_length_is_refused() {
        let mut reader = FrameReader::new(ScriptedReader::new(vec![
            b"Content-Type: application/json\r\n\r\n{}".to_vec(),
        ]));
        assert_eq!(reader.read_frame(), Err(CodecError::MissingContentLength));
    }

    #[test]
    fn a_header_that_never_terminates_is_capped() {
        let noise = vec![b'x'; MAX_HEADER_BYTES + 64];
        let mut reader = FrameReader::new(ScriptedReader::new(vec![noise]));
        assert_eq!(reader.read_frame(), Err(CodecError::HeaderTooLarge));
        assert!(reader.is_abandoned());
    }

    #[test]
    fn a_truncated_body_is_end_of_stream_not_a_short_frame() {
        let mut bytes = frame("{\"long\":\"payload\"}");
        bytes.truncate(bytes.len() - 4);
        let mut reader = FrameReader::new(ScriptedReader::new(vec![bytes]));
        let error = reader.read_frame().expect_err("truncated");
        assert!(matches!(error, CodecError::Eof { .. }), "{error}");
        // A transport fault is not a protocol violation, so the stream is not
        // marked abandoned: a reconnect is the caller's decision.
        assert!(!reader.is_abandoned());
    }

    #[test]
    fn a_clean_close_before_any_frame_is_end_of_stream() {
        let mut reader = FrameReader::new(ScriptedReader::new(Vec::new()));
        assert_eq!(
            reader.read_frame(),
            Err(CodecError::Eof {
                read: 0,
                expected: 0
            })
        );
    }

    #[test]
    fn header_matching_is_case_insensitive_like_the_engines_own_writer() {
        let mut reader = FrameReader::new(ScriptedReader::new(vec![
            b"CONTENT-LENGTH: 2\r\n\r\n{}".to_vec(),
        ]));
        assert_eq!(reader.read_frame().expect("parsed"), b"{}".to_vec());
    }

    #[test]
    fn an_oversize_payload_is_never_written_to_the_socket() {
        let payload = vec![b'x'; 8];
        let mut sink: Vec<u8> = Vec::new();
        write_frame(&mut sink, &payload).expect("small write");
        assert!(sink.starts_with(b"Content-Length: 8\r\n\r\n"));

        // The encoder refuses before any byte is produced.
        let error = encode_frame(&vec![0_u8; MAX_PAYLOAD_BYTES + 1]).expect_err("refused");
        assert!(
            matches!(error, CodecError::PayloadTooLarge { .. }),
            "{error}"
        );
    }
}
