//! `editor_runtime`-kind re-derivation, over Todo 19's frozen bridge protocol.
//!
//! A `SafeRoot` content hash can never substitute here: a `.tscn`/`.tres` file's
//! bytes say nothing about editor-side `EditorUndoRedoManager` state, selection or
//! runtime state. So this kind is proven by exactly two things, both obtained on
//! the wire from a *live* bridge:
//!
//! * an authoritative bridge/runtime transaction receipt — the plugin's own
//!   acknowledgment of a specific action or operation identifier, bound to this
//!   transaction ID; and
//! * a live state probe confirming the claimed state through that same bridge.
//!
//! A null receipt or a null probe **always** fails proof, by design. An
//! unreachable bridge likewise leaves the transaction `quarantined`, regardless of
//! any file-level hash on the underlying resource.
//!
//! # No second protocol
//!
//! Every wire step below is Todo 19's frozen contract, used as-is:
//! [`RendezvousFile`] enforces the mode-0600 file, the per-launch 256-bit secret
//! and the project object identity/generation claim; the opening handshake and the
//! framing are [`protocol::ws`](godot_mcp_bridge::protocol::ws)'s minimal RFC 6455
//! subset; and the messages are canonical
//! [`Envelope`]s. Nothing here relaxes a single one of those checks, and the
//! endpoint is required to be loopback.
//!
//! # Which bridge answers
//!
//! The peer names the transaction it wants a receipt for in the authenticated
//! `hello` nonce, spelled [`REPAIR_HELLO_NONCE_PREFIX`] followed by the
//! transaction ID. Reusing the authenticated nonce for this has a useful
//! consequence: the handshake consumes each nonce exactly once, so a receipt
//! request can never be replayed.

use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use godot_mcp_bridge::protocol::compat::PROTOCOL_VERSION;
use godot_mcp_bridge::protocol::envelope::{
    Envelope, MessageKind, PluginIdentity, canonical_bytes,
};
use godot_mcp_bridge::protocol::mutation::MutationStatus;
use godot_mcp_bridge::protocol::rendezvous::{RendezvousFile, is_loopback};
use godot_mcp_bridge::protocol::ws::{
    Opcode, WsFrame, client_handshake_request, encode_client_text, handshake_head_len,
    parse_server_handshake,
};
use godot_mcp_server::operator::{ObservedProof, OperatorSession, RepairTarget};

use super::supplied::{SuppliedEditor, SuppliedHeader};
use super::{Proven, RepairError, Verdict};

/// The frozen prefix of the authenticated `hello` nonce a repair-time receipt
/// request uses. The transaction ID follows it verbatim.
///
/// This is the one bridge-side addressing convention in this release. The live
/// editor plugin plane was cancelled, so the Task-34 fixture bridge is the only
/// implementation of it, and `editor_kind`'s own test asserts the two spellings
/// are identical rather than leaving them to drift.
pub const REPAIR_HELLO_NONCE_PREFIX: &str = "task34-";

/// The method a bridge presents its authoritative repair receipt under.
pub const RECEIPT_METHOD: &str = "bridge.repair_receipt";

/// The file name a bridge's rendezvous record is discovered under.
pub const RENDEZVOUS_FILE_NAME: &str = "rendezvous.json";

/// The Godot version the repair client presents in its authenticated `hello`.
///
/// The canonical envelope requires a non-empty `godot_version` from a peer that
/// fills the plugin half of the identity block. This client is an operator shell
/// command, not an editor, and it says so rather than impersonating a release.
pub const CLIENT_GODOT_VERSION: &str = "0.0.0-godot-mcp-operator-repair";

/// How deep below the project root the rendezvous discovery walk descends.
const DISCOVERY_MAX_DEPTH: usize = 6;

/// How many directory entries the discovery walk may inspect in total.
const DISCOVERY_MAX_ENTRIES: usize = 4096;

/// How long one bridge round trip may take.
const ROUND_TRIP_TIMEOUT: Duration = Duration::from_secs(15);

/// The masking key the repair client uses. RFC 6455 requires client frames to be
/// masked; the value is not a security property, since the server unmasks it
/// unconditionally, and a fixed key keeps the exchange deterministic.
const CLIENT_MASK: [u8; 4] = [0x51, 0x0d, 0xa3, 0x2e];

/// The `Sec-WebSocket-Key` the repair client sends. Also not a security property:
/// it only proves the server computed `Sec-WebSocket-Accept` correctly.
const CLIENT_WS_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";

/// The `hello` nonce that addresses one transaction's receipt.
#[must_use]
pub fn repair_nonce(transaction_id: &str) -> String {
    format!("{REPAIR_HELLO_NONCE_PREFIX}{transaction_id}")
}

/// One authoritative receipt, exactly as a live bridge reported it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LiveReceipt {
    /// Whether the bridge answered at all.
    pub reachable: bool,
    /// The transaction the receipt is bound to.
    pub receipt_transaction_id: Option<String>,
    /// The plugin-acknowledged action or operation identifier.
    pub receipt_action_id: Option<String>,
    /// The live state-probe answer.
    pub live_state_probe: Option<String>,
    /// Why the bridge could not be used, when it could not be.
    pub unreachable_reason: Option<String>,
}

/// Re-derive and compare an `editor_runtime`-kind repair.
///
/// # Errors
/// Returns [`RepairError::Observation`] only when the project tree itself cannot
/// be walked. An unreachable or refusing bridge is a [`Verdict::Refused`], not an
/// error, because it is a perfectly ordinary observation: the transaction stays
/// `quarantined`.
pub fn derive(
    session: &OperatorSession,
    project: &Path,
    target: &RepairTarget,
    header: &SuppliedHeader,
    supplied: &SuppliedEditor,
) -> Result<Verdict, RepairError> {
    let Some(ObservedProof::EditorRuntime {
        receipt_action_id: recorded_action_id,
        live_state_probe: recorded_probe,
        ..
    }) = target.recorded_proof.clone()
    else {
        return Ok(Verdict::Refused(format!(
            "transaction {} records no observed editor_runtime receipt, so there is nothing a live \
             probe could confirm",
            target.transaction_id
        )));
    };
    let (Some(recorded_action_id), Some(recorded_probe)) = (recorded_action_id, recorded_probe)
    else {
        return Ok(Verdict::Refused(format!(
            "transaction {} records a null receipt or a null live-state probe, which always fails \
             editor_runtime proof",
            target.transaction_id
        )));
    };

    let live = probe(session, project, &target.transaction_id)?;
    if !live.reachable {
        return Ok(Verdict::Refused(format!(
            "the editor bridge is unreachable, so no receipt or live state could be observed and \
             no file hash can substitute: {}",
            live.unreachable_reason
                .unwrap_or_else(|| "no bridge answered".to_owned())
        )));
    }
    let (Some(observed_transaction), Some(observed_action_id), Some(observed_probe)) = (
        live.receipt_transaction_id.clone(),
        live.receipt_action_id.clone(),
        live.live_state_probe.clone(),
    ) else {
        return Ok(Verdict::Refused(
            "the bridge answered with a null receipt or a null live-state probe, which always \
             fails editor_runtime proof"
                .to_owned(),
        ));
    };
    if observed_transaction != target.transaction_id {
        return Ok(Verdict::Refused(format!(
            "the bridge's receipt is bound to transaction {observed_transaction}, not {}",
            target.transaction_id
        )));
    }
    if observed_action_id != recorded_action_id {
        return Ok(Verdict::Refused(format!(
            "the bridge acknowledges action {observed_action_id} and the record holds \
             {recorded_action_id}"
        )));
    }
    if observed_probe != recorded_probe {
        return Ok(Verdict::Refused(format!(
            "the live state probe reports {observed_probe} and the record holds {recorded_probe}"
        )));
    }
    if !supplied.bridge_reachable {
        return Ok(Verdict::Refused(
            "the proof file claims the bridge is unreachable, and the command reached it"
                .to_owned(),
        ));
    }
    if supplied.bridge_receipt != observed_action_id {
        return Ok(Verdict::Refused(format!(
            "the proof file claims receipt {} and the bridge acknowledges {observed_action_id}",
            supplied.bridge_receipt
        )));
    }
    if supplied.live_state_probe != observed_probe {
        return Ok(Verdict::Refused(format!(
            "the proof file claims live state {} and the bridge reports {observed_probe}",
            supplied.live_state_probe
        )));
    }

    // An `editor_runtime` record has no separate pre/post hash sides to
    // distinguish: the recorded receipt *is* the one recorded state, and what a
    // live match proves is that the recorded state is the state the editor holds
    // now. The operator's `claimed_disposition` therefore selects which side that
    // confirmed state is claimed as, and the requested operation must agree with
    // it — nothing is ever committed without this live receipt match.
    let disposition = header.claimed_disposition;
    Ok(Verdict::Proven(Proven {
        disposition,
        observed: ObservedProof::EditorRuntime {
            bridge_reachable: true,
            receipt_transaction_id: Some(observed_transaction),
            receipt_action_id: Some(observed_action_id.clone()),
            live_state_probe: Some(observed_probe.clone()),
        },
        detail: format!(
            "the live bridge acknowledges action {observed_action_id} bound to this transaction \
             and its state probe reports {observed_probe}, matching the record; claimed as the {} \
             state",
            disposition.as_str()
        ),
        live_survivor: None,
    }))
}

/// Ask a live bridge for one transaction's authoritative receipt and live probe.
///
/// Every field of the answer comes off the wire. A bridge that cannot be found,
/// authenticated or spoken to yields an unreachable observation with a
/// diagnosable reason, never a fabricated receipt.
///
/// # Errors
/// Returns [`RepairError::Observation`] when the project tree cannot be walked at
/// all.
pub fn probe(
    session: &OperatorSession,
    project: &Path,
    transaction_id: &str,
) -> Result<LiveReceipt, RepairError> {
    let candidates = discover_rendezvous(project)?;
    let mut reasons = Vec::new();
    for candidate in candidates {
        match round_trip(session, &candidate, transaction_id) {
            Ok(receipt) if receipt.reachable => return Ok(receipt),
            Ok(receipt) => reasons.push(
                receipt
                    .unreachable_reason
                    .unwrap_or_else(|| format!("{} answered nothing usable", candidate.display())),
            ),
            Err(reason) => reasons.push(reason),
        }
    }
    Ok(LiveReceipt {
        unreachable_reason: Some(if reasons.is_empty() {
            format!(
                "no {RENDEZVOUS_FILE_NAME} bound to this project object identity and generation \
                 was found under {}",
                project.display()
            )
        } else {
            reasons.join("; ")
        }),
        ..LiveReceipt::default()
    })
}

/// Every rendezvous file under the project root, in a stable order.
///
/// The walk is depth- and entry-bounded and refuses to descend a symlink, so a
/// hostile tree can neither divert it outside the project nor make it run
/// unbounded.
fn discover_rendezvous(project: &Path) -> Result<Vec<PathBuf>, RepairError> {
    let mut found = Vec::new();
    let mut budget = DISCOVERY_MAX_ENTRIES;
    let mut frontier = vec![(project.to_path_buf(), 0_usize)];
    while let Some((directory, depth)) = frontier.pop() {
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => continue,
            Err(error) => {
                return Err(RepairError::Observation(format!(
                    "{} could not be listed while looking for a bridge rendezvous: {error}",
                    directory.display()
                )));
            }
        };
        for entry in entries {
            if budget == 0 {
                break;
            }
            budget -= 1;
            let Ok(entry) = entry else { continue };
            let path = entry.path();
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                if depth < DISCOVERY_MAX_DEPTH {
                    frontier.push((path, depth + 1));
                }
            } else if metadata.is_file() && entry.file_name() == RENDEZVOUS_FILE_NAME {
                found.push(path);
            }
        }
    }
    found.sort();
    Ok(found)
}

fn round_trip(
    session: &OperatorSession,
    rendezvous_path: &Path,
    transaction_id: &str,
) -> Result<LiveReceipt, String> {
    let rendezvous = RendezvousFile::read(rendezvous_path).map_err(|error| {
        format!(
            "the rendezvous file {} is unusable: {error}",
            rendezvous_path.display()
        )
    })?;
    let claim = rendezvous.project();
    if claim.object_identity != session.project_object_identity()
        || claim.generation != session.project_generation()
    {
        return Err(format!(
            "the bridge at {} is bound to project object {}/generation {}, and the held lease \
             designates {}/{}",
            rendezvous_path.display(),
            claim.object_identity,
            claim.generation,
            session.project_object_identity(),
            session.project_generation()
        ));
    }
    let endpoint = rendezvous.endpoint();
    if !is_loopback(endpoint) {
        return Err(format!(
            "the bridge at {} advertises the non-loopback endpoint {endpoint}",
            rendezvous_path.display()
        ));
    }

    let plugin = PluginIdentity {
        plugin_version: env!("CARGO_PKG_VERSION").to_owned(),
        protocol_version: PROTOCOL_VERSION.to_owned(),
        godot_version: CLIENT_GODOT_VERSION.to_owned(),
    };
    let nonce = repair_nonce(transaction_id);
    let hello = Envelope::hello(&plugin, claim, rendezvous.secret_hex(), &nonce);

    let mut stream = connect(endpoint)?;
    write_bytes(
        &mut stream,
        client_handshake_request(endpoint, CLIENT_WS_KEY).as_bytes(),
    )?;
    let mut buffer = Vec::new();
    let head = read_head(&mut stream, &mut buffer)?;
    parse_server_handshake(&head, CLIENT_WS_KEY)
        .map_err(|error| format!("the bridge's opening handshake was rejected: {error}"))?;

    write_envelope(&mut stream, &hello)?;
    let first = read_envelope(&mut stream, &mut buffer)?;
    if first.kind() != MessageKind::Capabilities {
        return Ok(LiveReceipt {
            unreachable_reason: Some(format!(
                "the bridge refused the authenticated hello with a {:?} message",
                first.kind()
            )),
            ..LiveReceipt::default()
        });
    }
    let receipt = read_envelope(&mut stream, &mut buffer)?;
    if receipt.kind() != MessageKind::Request || receipt.method() != Some(RECEIPT_METHOD) {
        return Ok(LiveReceipt {
            unreachable_reason: Some(format!(
                "the bridge presented a {:?}/{:?} message instead of a {RECEIPT_METHOD} request",
                receipt.kind(),
                receipt.method()
            )),
            ..LiveReceipt::default()
        });
    }
    let payload = receipt.payload().cloned().unwrap_or_default();
    let observed = LiveReceipt {
        reachable: true,
        receipt_transaction_id: string_field(&payload, "receipt_transaction_id"),
        receipt_action_id: string_field(&payload, "receipt_action_id"),
        live_state_probe: string_field(&payload, "live_state_probe"),
        unreachable_reason: None,
    };
    let acknowledgement = Envelope::result(
        &plugin,
        receipt.message_id(),
        MutationStatus::NotStarted,
        &serde_json::json!({ "acknowledged": true }),
    );
    let _ = write_envelope(&mut stream, &acknowledgement);
    Ok(observed)
}

fn connect(endpoint: SocketAddr) -> Result<TcpStream, String> {
    let stream = TcpStream::connect_timeout(&endpoint, ROUND_TRIP_TIMEOUT)
        .map_err(|error| format!("the bridge at {endpoint} refused a connection: {error}"))?;
    stream
        .set_read_timeout(Some(ROUND_TRIP_TIMEOUT))
        .and_then(|()| stream.set_write_timeout(Some(ROUND_TRIP_TIMEOUT)))
        .map_err(|error| format!("the bridge connection could not be bounded: {error}"))?;
    Ok(stream)
}

fn write_bytes(stream: &mut TcpStream, bytes: &[u8]) -> Result<(), String> {
    stream
        .write_all(bytes)
        .and_then(|()| stream.flush())
        .map_err(|error| format!("the bridge connection could not be written to: {error}"))
}

fn write_envelope(stream: &mut TcpStream, envelope: &Envelope) -> Result<(), String> {
    let text = String::from_utf8(canonical_bytes(envelope))
        .map_err(|error| format!("a bridge message was not UTF-8: {error}"))?;
    let frame = encode_client_text(&text, CLIENT_MASK)
        .map_err(|error| format!("a bridge message could not be framed: {error}"))?;
    write_bytes(stream, &frame)
}

fn read_head(stream: &mut TcpStream, buffer: &mut Vec<u8>) -> Result<Vec<u8>, String> {
    loop {
        match handshake_head_len(buffer) {
            Ok(Some(length)) => {
                let head = buffer[..length].to_vec();
                buffer.drain(..length);
                return Ok(head);
            }
            Ok(None) => fill(stream, buffer)?,
            Err(error) => {
                return Err(format!(
                    "the bridge's opening handshake was unusable: {error}"
                ));
            }
        }
    }
}

fn read_envelope(stream: &mut TcpStream, buffer: &mut Vec<u8>) -> Result<Envelope, String> {
    loop {
        match WsFrame::parse(buffer) {
            Ok(frame) => {
                buffer.drain(..frame.consumed);
                match frame.opcode {
                    Opcode::Close => return Err("the bridge closed the connection".to_owned()),
                    Opcode::Ping | Opcode::Pong => continue,
                    Opcode::Text => {
                        let text = String::from_utf8(frame.payload)
                            .map_err(|error| format!("a bridge message was not UTF-8: {error}"))?;
                        return Envelope::parse(&text).map_err(|error| {
                            format!("a bridge message was not a canonical envelope: {error}")
                        });
                    }
                }
            }
            Err(error) if error.is_incomplete() => fill(stream, buffer)?,
            Err(error) => return Err(format!("a bridge frame was rejected: {error}")),
        }
    }
}

fn fill(stream: &mut TcpStream, buffer: &mut Vec<u8>) -> Result<(), String> {
    let mut chunk = [0_u8; 8192];
    let read = stream
        .read(&mut chunk)
        .map_err(|error| format!("the bridge connection could not be read: {error}"))?;
    if read == 0 {
        return Err("the bridge closed the connection mid-message".to_owned());
    }
    buffer.extend_from_slice(&chunk[..read]);
    Ok(())
}

fn string_field(payload: &serde_json::Value, name: &str) -> Option<String> {
    payload
        .get(name)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The nonce must name exactly one transaction, or a receipt for a different
    /// transaction could be accepted as this one's.
    #[test]
    fn a_nonce_names_exactly_one_transaction() {
        let transaction = "34000000-0000-4000-8000-000000000301";
        assert_eq!(
            repair_nonce(transaction),
            format!("{REPAIR_HELLO_NONCE_PREFIX}{transaction}")
        );
        assert!(repair_nonce(transaction).ends_with(transaction));
        assert_ne!(
            repair_nonce(transaction),
            repair_nonce("34000000-0000-4000-8000-000000000302")
        );
    }

    /// The discovery walk is what finds a live bridge, so it must find a nested
    /// rendezvous file, ignore every other name, and refuse to descend a symlink.
    #[test]
    fn discovery_finds_nested_rendezvous_files_and_never_follows_a_symlink() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path();
        let nested = root.join("state").join("bridge");
        std::fs::create_dir_all(&nested).expect("nested dir");
        std::fs::write(nested.join(RENDEZVOUS_FILE_NAME), "{}").expect("rendezvous");
        std::fs::write(nested.join("other.json"), "{}").expect("other");

        let outside = dir
            .path()
            .parent()
            .expect("a parent exists")
            .join(format!("task34-outside-{}", std::process::id()));
        std::fs::create_dir_all(&outside).expect("outside dir");
        std::fs::write(outside.join(RENDEZVOUS_FILE_NAME), "{}").expect("outside rendezvous");
        #[cfg(unix)]
        let _ = std::os::unix::fs::symlink(&outside, root.join("escape"));

        let found = discover_rendezvous(root).expect("the project tree is walkable");
        let _ = std::fs::remove_dir_all(&outside);
        assert_eq!(found, vec![nested.join(RENDEZVOUS_FILE_NAME)]);
    }

    /// An absent project tree is not an error: there is simply no bridge, which is
    /// an unreachable observation rather than a failure to observe.
    #[test]
    fn an_absent_tree_yields_no_candidates() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert!(
            discover_rendezvous(&dir.path().join("absent"))
                .expect("an absent tree is walkable")
                .is_empty()
        );
    }

    /// The bridge-side addressing convention has exactly one spelling. The
    /// Task-34 fixture bridge implements it, so the two must be byte-identical or
    /// a repair would address a receipt nobody answers.
    #[cfg(feature = "test-hooks")]
    #[test]
    fn the_fixture_bridge_implements_the_same_addressing_convention() {
        let transaction = "34000000-0000-4000-8000-000000000301";
        assert_eq!(
            repair_nonce(transaction),
            crate::fixture34::bridge::repair_nonce(transaction)
        );
        assert_eq!(
            RECEIPT_METHOD,
            crate::fixture34::bridge::RECEIPT_METHOD,
            "one receipt method, not two"
        );
        assert_eq!(
            crate::fixture34::bridge::transaction_of_nonce(&repair_nonce(transaction)),
            Some(transaction)
        );
    }
}
