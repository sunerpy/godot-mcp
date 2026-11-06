//! Captures a bridge peer makes and the server keeps as artifacts.
//!
//! The peer — a managed game's runtime probe or the live editor's plugin —
//! captures, keeps the PNG bytes and reports each frame's length and SHA-256.
//! The server reads every frame back in bounded chunks, checks it against what
//! the peer reported and registers the result in the artifact store, owned by
//! the caller who asked for it. Nothing is registered unless every byte
//! arrived as described.

use std::collections::BTreeMap;
use std::sync::Arc;

use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::contracts::{
    ArtifactContract, ArtifactId, ArtifactKind, ArtifactMetadata, ArtifactOwner,
    ArtifactResourceUri, ContractViolation, OutcomeError, PrincipalId, ProjectIdentity,
    RangeSemantics, SessionId,
};
use serde_json::{Value, json};

use crate::artifacts::{ArtifactStore, sha256_hex};
use crate::effect::boundary::EffectTicket;

/// The most one chunk read asks for: the peers' own chunk size.
pub(crate) const CHUNK_BYTES: u64 = 1024 * 1024;

/// Where captures are kept, and for how long.
#[derive(Clone)]
pub(crate) struct CaptureSink {
    pub(crate) store: Arc<ArtifactStore>,
    pub(crate) project: ProjectIdentity,
    pub(crate) retention_ms: u64,
    pub(crate) max_range_bytes: u64,
}

impl std::fmt::Debug for CaptureSink {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CaptureSink")
            .field("retention_ms", &self.retention_ms)
            .field("max_range_bytes", &self.max_range_bytes)
            .finish_non_exhaustive()
    }
}

fn internal(message: impl Into<String>) -> OutcomeError {
    OutcomeError::new(ErrorKind::Internal, message.into(), false, BTreeMap::new())
}

/// Read every frame of the capture `described` back in bounded chunks, each
/// with `read_chunk`, and check each frame against the length and SHA-256 the
/// peer reported for it.
pub(crate) fn fetch_frames(
    described: &Value,
    mut read_chunk: impl FnMut(&Value) -> Result<Value, OutcomeError>,
) -> Result<Vec<Vec<u8>>, OutcomeError> {
    let malformed = || internal("the peer described a capture it did not complete");
    let capture = described
        .get("capture_id")
        .and_then(Value::as_str)
        .ok_or_else(malformed)?;
    let frames = described
        .get("frames")
        .and_then(Value::as_array)
        .ok_or_else(malformed)?;
    let mut fetched = Vec::with_capacity(frames.len());
    for (index, frame) in frames.iter().enumerate() {
        let length = frame
            .get("byte_length")
            .and_then(Value::as_u64)
            .ok_or_else(malformed)?;
        let expected = frame
            .get("sha256")
            .and_then(Value::as_str)
            .ok_or_else(malformed)?;
        let mut bytes: Vec<u8> = Vec::with_capacity(usize::try_from(length).unwrap_or(0));
        while (bytes.len() as u64) < length {
            let chunk = read_chunk(&json!({
                "capture_id": capture,
                "frame": index,
                "offset": bytes.len(),
                "length": CHUNK_BYTES,
            }))?;
            let data = chunk
                .get("data_base64")
                .and_then(Value::as_str)
                .ok_or_else(malformed)?;
            let decoded = base64_decode(data).ok_or_else(malformed)?;
            if decoded.is_empty() {
                return Err(malformed());
            }
            bytes.extend_from_slice(&decoded);
        }
        if bytes.len() as u64 != length || sha256_hex(&bytes) != expected {
            return Err(internal(format!(
                "frame {index} did not arrive as the peer described it"
            )));
        }
        fetched.push(bytes);
    }
    Ok(fetched)
}

/// Who may read an artifact the caller `principal`, in `session`, produced.
///
/// The principal alone: every session of the principal that produced it reads
/// it, so a client that reconnects keeps what it captured. Every session one
/// deployment admits is already the same principal with the same capabilities,
/// so this grants no session anything it could not produce itself. Only when no
/// principal is recorded does the session still bind it, so an artifact never
/// becomes readable by everyone through a missing principal.
pub(crate) fn artifact_owner(
    principal: Option<&str>,
    session: Option<&str>,
) -> Option<ArtifactOwner> {
    if let Some(principal) = principal.and_then(|principal| PrincipalId::parse(principal).ok()) {
        return Some(ArtifactOwner {
            principal: Some(principal),
            session: None,
        });
    }
    session
        .and_then(|session| SessionId::parse(session).ok())
        .map(|session| ArtifactOwner {
            principal: None,
            session: Some(session),
        })
}

/// The contract of one capture's artifact: its identity is bound to the
/// transaction that made it, and it belongs to the principal of the `owner` the
/// tool handler recorded (see [`artifact_owner`]).
pub(crate) fn contract(
    sink: &CaptureSink,
    ticket: &EffectTicket,
    kind: ArtifactKind,
    media_type: &str,
    bytes: &[u8],
    owner: Option<&Value>,
    created_unix_ms: u64,
) -> Result<ArtifactContract, OutcomeError> {
    contract_salted(
        sink,
        ticket,
        "",
        kind,
        media_type,
        bytes,
        owner,
        created_unix_ms,
    )
}

/// [`contract`], for one of several artifacts a transaction makes: a
/// non-empty `salt` sets it apart from the others, so two artifacts of the
/// same bytes made by one transaction still have identities of their own.
#[allow(clippy::too_many_arguments)]
pub(crate) fn contract_salted(
    sink: &CaptureSink,
    ticket: &EffectTicket,
    salt: &str,
    kind: ArtifactKind,
    media_type: &str,
    bytes: &[u8],
    owner: Option<&Value>,
    created_unix_ms: u64,
) -> Result<ArtifactContract, OutcomeError> {
    let sha256 = sha256_hex(bytes);
    let token = if salt.is_empty() {
        sha256_hex(format!("{}\n{sha256}", ticket.transaction_id()).as_bytes())
    } else {
        sha256_hex(format!("{}\n{salt}\n{sha256}", ticket.transaction_id()).as_bytes())
    };
    let id = ArtifactId::parse(format!("artifact_{token}"))
        .map_err(|error| internal(error.to_string()))?;
    let length = bytes.len() as u64;
    let owner = artifact_owner(
        owner
            .and_then(|owner| owner.get("principal"))
            .and_then(Value::as_str),
        owner
            .and_then(|owner| owner.get("session"))
            .and_then(Value::as_str),
    );
    let violation = |error: ContractViolation| internal(error.to_string());
    ArtifactContract::new(
        id.clone(),
        ArtifactResourceUri::for_artifact(&id),
        sink.project.clone(),
        owner,
        ArtifactMetadata {
            kind,
            media_type: media_type.to_owned(),
            byte_length: length,
            sha256,
            created_at_unix_ms: created_unix_ms,
        },
        created_unix_ms.saturating_add(sink.retention_ms.max(1)),
        RangeSemantics::new(
            length.max(1),
            length.clamp(1, sink.max_range_bytes.max(1)),
            true,
        )
        .map_err(violation)?,
    )
    .map_err(violation)
}

/// Encode standard base64 with padding, as Godot's `Marshalls` reads it.
pub(crate) fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for group in bytes.chunks(3) {
        let word = (u32::from(group[0]) << 16)
            | (u32::from(group.get(1).copied().unwrap_or(0)) << 8)
            | u32::from(group.get(2).copied().unwrap_or(0));
        for index in 0..4 {
            if index <= group.len() {
                out.push(char::from(
                    TABLE[((word >> (18 - 6 * index)) & 63) as usize],
                ));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Decode standard base64 (with padding), as Godot's `Marshalls` writes it.
pub(crate) fn base64_decode(text: &str) -> Option<Vec<u8>> {
    fn value(byte: u8) -> Option<u32> {
        Some(u32::from(match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        }))
    }
    let bytes: Vec<u8> = text
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for quad in bytes.chunks(4) {
        let padding = quad.iter().rev().take_while(|byte| **byte == b'=').count();
        if padding > 2 {
            return None;
        }
        let mut word = 0_u32;
        for (position, byte) in quad.iter().enumerate() {
            let digit = if position >= 4 - padding {
                0
            } else {
                value(*byte)?
            };
            word = (word << 6) | digit;
        }
        let decoded = [(word >> 16) as u8, (word >> 8) as u8, word as u8];
        out.extend_from_slice(&decoded[..3 - padding]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRINCIPAL: &str = "principal_00112233445566778899aabbccddeeff";
    const SESSION: &str = "session_00112233445566778899aabbccddeeff";

    #[test]
    fn an_artifact_belongs_to_its_principal_not_to_one_session() {
        assert_eq!(
            artifact_owner(Some(PRINCIPAL), Some(SESSION)),
            Some(ArtifactOwner {
                principal: Some(PrincipalId::parse(PRINCIPAL).expect("principal")),
                session: None,
            }),
            "every session of the principal that produced it may read it"
        );
    }

    #[test]
    fn an_owner_without_a_principal_is_never_widened() {
        assert_eq!(
            artifact_owner(None, Some(SESSION)),
            Some(ArtifactOwner {
                principal: None,
                session: Some(SessionId::parse(SESSION).expect("session")),
            }),
            "without a principal the session still binds it"
        );
        assert_eq!(
            artifact_owner(Some("not a principal"), Some(SESSION)),
            Some(ArtifactOwner {
                principal: None,
                session: Some(SessionId::parse(SESSION).expect("session")),
            }),
            "an unreadable principal is no principal"
        );
        assert_eq!(artifact_owner(None, None), None);
    }

    #[test]
    fn base64_encodes_what_godot_decodes() {
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
        assert_eq!(base64_encode(b"hi"), "aGk=");
        assert_eq!(base64_encode(b"hey"), "aGV5");
        assert_eq!(base64_encode(b""), "");
        let bytes: Vec<u8> = (0..=255).collect();
        assert_eq!(
            base64_decode(&base64_encode(&bytes)).as_deref(),
            Some(&bytes[..])
        );
    }

    #[test]
    fn base64_decodes_what_godot_encodes() {
        assert_eq!(base64_decode("aGVsbG8=").as_deref(), Some(&b"hello"[..]));
        assert_eq!(base64_decode("aGk=").as_deref(), Some(&b"hi"[..]));
        assert_eq!(base64_decode("aGV5").as_deref(), Some(&b"hey"[..]));
        assert_eq!(base64_decode("").as_deref(), Some(&b""[..]));
        assert!(base64_decode("a$==").is_none());
        assert!(base64_decode("abc").is_none());
    }
}
