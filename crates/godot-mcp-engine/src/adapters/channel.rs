//! The framed byte channel both adapters talk over, and its bounded TCP backing.
//!
//! # Why a trait and not a `TcpStream` directly
//!
//! Godot's two protocol servers are editor plugins, so exercising them for real
//! needs a running editor. The adapters therefore speak to a
//! [`FramedChannel`] — deterministic fake servers in the test suite implement it
//! over a scripted loopback socket, and production uses [`TcpChannel`]. Nothing
//! about the protocol logic differs between the two, so what the fakes prove is the
//! real code path.
//!
//! # What "bounded" means here
//!
//! Every read is bounded three ways: by [`crate::adapters::codec::MAX_PAYLOAD_BYTES`]
//! per frame, by [`crate::adapters::codec::MAX_HEADER_BYTES`] per header, and by the
//! socket's own read timeout, which becomes [`CodecError::Io`] rather than a hang.
//! There is no unbounded `read_to_end` anywhere on this path.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use crate::adapters::codec::{CodecError, FrameReader, write_frame};

/// A bidirectional framed message channel.
pub trait FramedChannel {
    /// Send one payload as a framed message.
    ///
    /// # Errors
    /// Returns [`CodecError::PayloadTooLarge`] for an oversize payload (nothing is
    /// written) or [`CodecError::Io`] when the write fails.
    fn send(&mut self, payload: &[u8]) -> Result<(), CodecError>;

    /// Receive exactly one payload.
    ///
    /// # Errors
    /// Returns the [`CodecError`] for a framing violation, a closed peer or an I/O
    /// failure.
    fn receive(&mut self) -> Result<Vec<u8>, CodecError>;

    /// Whether the channel has been abandoned by a framing violation and cannot be
    /// resynchronised.
    fn is_abandoned(&self) -> bool;

    /// Bound how long one [`Self::receive`] waits for the peer. A channel with
    /// no timeout of its own ignores it.
    ///
    /// # Errors
    /// Returns [`CodecError::Io`] when the timeout cannot be configured.
    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> Result<(), CodecError> {
        let _ = timeout;
        Ok(())
    }
}

/// A framed channel over any paired reader/writer.
///
/// Used by the deterministic fakes and by [`TcpChannel`] alike, so the framing path
/// under test is the production one.
pub struct DuplexChannel<R: Read, W: Write> {
    reader: FrameReader<R>,
    writer: W,
}

impl<R: Read, W: Write> DuplexChannel<R, W> {
    /// Pair a reader and a writer into a framed channel.
    pub const fn new(reader: R, writer: W) -> Self {
        Self {
            reader: FrameReader::new(reader),
            writer,
        }
    }

    /// How many frames have been received.
    pub const fn frames_received(&self) -> u64 {
        self.reader.frames_read()
    }
}

impl<R: Read, W: Write> FramedChannel for DuplexChannel<R, W> {
    fn send(&mut self, payload: &[u8]) -> Result<(), CodecError> {
        write_frame(&mut self.writer, payload)
    }

    fn receive(&mut self) -> Result<Vec<u8>, CodecError> {
        self.reader.read_frame()
    }

    fn is_abandoned(&self) -> bool {
        self.reader.is_abandoned()
    }
}

/// A framed channel over one loopback TCP connection.
pub struct TcpChannel {
    inner: DuplexChannel<TcpStream, TcpStream>,
    /// One more handle on the same socket, through which the read timeout is
    /// changed.
    control: TcpStream,
}

impl TcpChannel {
    /// Dial `address` and wrap the connection.
    ///
    /// Both directions carry `timeout`, so a peer that stops answering surfaces as an
    /// I/O failure instead of blocking the caller forever.
    ///
    /// # Errors
    /// Returns [`CodecError::Io`] when the connection or the timeout configuration
    /// fails. A refused connection is the normal "no editor is hosting this" case and
    /// the caller maps it to `unavailable`.
    pub fn connect(address: SocketAddr, timeout: Duration) -> Result<Self, CodecError> {
        let stream =
            TcpStream::connect_timeout(&address, timeout).map_err(|error| CodecError::Io {
                detail: format!("connect {address}: {error}"),
            })?;
        stream
            .set_read_timeout(Some(timeout))
            .map_err(|error| CodecError::Io {
                detail: format!("set read timeout: {error}"),
            })?;
        stream
            .set_write_timeout(Some(timeout))
            .map_err(|error| CodecError::Io {
                detail: format!("set write timeout: {error}"),
            })?;
        stream.set_nodelay(true).map_err(|error| CodecError::Io {
            detail: format!("set nodelay: {error}"),
        })?;
        let reader = stream.try_clone().map_err(|error| CodecError::Io {
            detail: format!("clone stream: {error}"),
        })?;
        let control = stream.try_clone().map_err(|error| CodecError::Io {
            detail: format!("clone stream: {error}"),
        })?;
        Ok(Self {
            inner: DuplexChannel::new(reader, stream),
            control,
        })
    }
}

impl FramedChannel for TcpChannel {
    fn send(&mut self, payload: &[u8]) -> Result<(), CodecError> {
        self.inner.send(payload)
    }

    fn receive(&mut self) -> Result<Vec<u8>, CodecError> {
        self.inner.receive()
    }

    fn is_abandoned(&self) -> bool {
        self.inner.is_abandoned()
    }

    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> Result<(), CodecError> {
        self.control
            .set_read_timeout(timeout.map(|timeout| timeout.max(Duration::from_millis(1))))
            .map_err(|error| CodecError::Io {
                detail: format!("set read timeout: {error}"),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn a_duplex_channel_round_trips_a_frame_through_the_production_codec() {
        let inbound = crate::adapters::codec::encode_frame(b"{\"id\":1}").expect("frame");
        let mut channel = DuplexChannel::new(Cursor::new(inbound), Vec::new());
        assert_eq!(channel.receive().expect("received"), b"{\"id\":1}".to_vec());
        channel.send(b"{\"id\":2}").expect("sent");
        assert_eq!(channel.frames_received(), 1);
    }

    #[test]
    fn an_oversize_send_writes_nothing() {
        let mut channel = DuplexChannel::new(Cursor::new(Vec::new()), Vec::new());
        let error = channel
            .send(&vec![0_u8; crate::adapters::codec::MAX_PAYLOAD_BYTES + 1])
            .expect_err("refused");
        assert!(
            matches!(error, CodecError::PayloadTooLarge { .. }),
            "{error}"
        );
    }
}
