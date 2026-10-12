//! Todo 19 acceptance suite: the frozen Rust↔GDScript editor bridge protocol.
//!
//! The socket exchange runs against production RFC 6455, rendezvous, listener,
//! envelope, compatibility and mutation code, with a Rust fake plugin as the
//! client. Before that exchange, the happy path executes the committed
//! pure-GDScript codec in a real headless Godot process whenever one is
//! configured or discovered. A runner without Godot emits an actionable `SKIP`
//! naming `GODOT_MCP_GODOT_BINARY` directly to the process stderr, and a child
//! libtest probe proves the line remains visible without `--nocapture`; lack of
//! an engine is never reported as runtime verification.
//!
//! # What this suite is really about
//!
//! > **A loopback port is not an identity. A per-launch rendezvous secret plus a
//! > matching project root is.**
//!
//! Anything on the machine can dial `127.0.0.1:<port>`. So the first frame after
//! the WebSocket upgrade must prove three separate things — knowledge of a secret
//! that only this launch wrote to a `0600` file, agreement on the *same* project
//! root object identity and generation, and a compatible protocol major version —
//! before a single request is dispatched. And because a reconnect proves none of
//! that about *previous* work, an `unresolved` mutation is never replayed; it goes
//! to Todo 6's journal for reconciliation/quarantine and stays there.
//!
//! Two tests are named by `qa/tasks/19.json` and are the only entry points the QA
//! descriptor invokes:
//!
//! * `bridge_protocol_happy_path`
//! * `bridge_protocol_failure_paths`

#![allow(
    clippy::too_many_lines,
    reason = "acceptance suites assert one whole operation end to end"
)]

use std::io::Write as _;
use std::net::{IpAddr, Ipv4Addr, TcpStream};
use std::path::{Path, PathBuf};

use godot_mcp_bridge::protocol::compat::{
    CompatibilityDecision, CompatibilityMatrix, PROTOCOL_MAJOR, PROTOCOL_VERSION,
};
use godot_mcp_bridge::protocol::envelope::{
    Deadline, Envelope, EnvelopeError, MessageKind, OperationClass, PROTOCOL_SCHEMA_JSON,
    PluginIdentity, ProjectClaim, ServerIdentity, canonical_bytes, validate_canonical,
};
use godot_mcp_bridge::protocol::handshake::{Handshake, HandshakeError, HandshakeOutcome};
use godot_mcp_bridge::protocol::harness::{GDSCRIPT_HARNESS_SHA256, gdscript_harness_source};
use godot_mcp_bridge::protocol::listener::{BridgeListener, ListenerError};
use godot_mcp_bridge::protocol::mutation::{
    MutationLedger, MutationOutcome, MutationStatus, QuarantineHandoff,
};
use godot_mcp_bridge::protocol::rendezvous::{
    RENDEZVOUS_FILE_MODE, RendezvousError, RendezvousFile, SECRET_BYTES, constant_time_eq,
};
use godot_mcp_bridge::protocol::ws::{
    MAX_FRAME_BYTES, Opcode, WsError, WsFrame, accept_key, client_handshake_request,
    encode_client_text, encode_server_text, parse_client_handshake, parse_server_handshake,
    server_handshake_response,
};
use godot_mcp_domain::ErrorKind;
use serde_json::{Value, json};

#[path = "fixtures/protocol/fake_plugin.rs"]
mod fake;
#[path = "support/gdscript_harness.rs"]
mod gdscript_harness;

use fake::{FakePlugin, PluginScript, TempDir};
use gdscript_harness::{
    GODOT_BINARY_ENV, GdscriptCanonicalVector, GdscriptHarnessOutcome,
    execute_gdscript_harness_if_available,
};

// ---------------------------------------------------------------------------
// Committed fixtures and golden vectors
// ---------------------------------------------------------------------------

fn goldens_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/goldens/protocol-v1")
}

fn golden(name: &str) -> String {
    let path = goldens_dir().join(format!("{name}.json"));
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

/// Every golden vector this protocol freezes, in canonical order.
const GOLDEN_NAMES: [&str; 10] = [
    "hello",
    "capabilities",
    "request",
    "result",
    "error",
    "progress",
    "cancel",
    "heartbeat",
    "event",
    "shutdown",
];

fn server_identity() -> ServerIdentity {
    ServerIdentity {
        server_version: "0.1.0".to_owned(),
        protocol_version: PROTOCOL_VERSION.to_owned(),
        session_id: "9f2c1d4e6b8a0357".to_owned(),
    }
}

fn plugin_identity() -> PluginIdentity {
    PluginIdentity {
        plugin_version: "0.1.0".to_owned(),
        protocol_version: PROTOCOL_VERSION.to_owned(),
        godot_version: "4.4.1.stable".to_owned(),
    }
}

fn project_claim() -> ProjectClaim {
    ProjectClaim {
        object_identity: "project_1111111111111111111111111111111111111111111111111111111111111111"
            .to_owned(),
        root_object: "unix:dev=66305,ino=4242".to_owned(),
        file_object: "unix:dev=66305,ino=4243".to_owned(),
        generation: 7,
    }
}

const SKIP_OUTPUT_PROBE_ENV: &str = "GODOT_MCP_BRIDGE_SKIP_OUTPUT_PROBE";
const NO_ENGINE_SKIP_MESSAGE: &str = "SKIP real GDScript codec harness: no Godot binary found; set GODOT_MCP_GODOT_BINARY to execute it";

fn report_visible_no_engine_skip() {
    let mut stderr = std::io::stderr().lock();
    writeln!(stderr, "{NO_ENGINE_SKIP_MESSAGE}").expect("write the visible harness skip reason");
}

fn assert_skip_is_visible_under_captured_libtest() {
    let current_test_binary =
        std::env::current_exe().expect("resolve the bridge_protocol integration-test binary");
    let output = std::process::Command::new(&current_test_binary)
        .args(["--exact", "bridge_protocol_skip_output_probe"])
        .env(SKIP_OUTPUT_PROBE_ENV, "1")
        .output()
        .expect("run the skip-output probe without --nocapture");
    assert!(
        output.status.success(),
        "skip-output probe failed with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(NO_ENGINE_SKIP_MESSAGE),
        "the descriptor command omits --nocapture, so the SKIP must bypass libtest capture; stderr was: {stderr}"
    );
}

#[test]
fn bridge_protocol_skip_output_probe() {
    if std::env::var_os(SKIP_OUTPUT_PROBE_ENV).is_some() {
        report_visible_no_engine_skip();
    }
}

fn verify_real_gdscript_codec_when_available() -> Option<Vec<GdscriptCanonicalVector>> {
    match execute_gdscript_harness_if_available()
        .expect("an explicitly configured Godot path must execute the GDScript codec harness")
    {
        GdscriptHarnessOutcome::Executed { canonical_vectors } => Some(canonical_vectors),
        GdscriptHarnessOutcome::SkippedNoEngine => {
            assert_eq!(GODOT_BINARY_ENV, "GODOT_MCP_GODOT_BINARY");
            assert_skip_is_visible_under_captured_libtest();
            report_visible_no_engine_skip();
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Happy path
// ---------------------------------------------------------------------------

#[test]
fn bridge_protocol_happy_path() {
    let real_gdscript_vectors = verify_real_gdscript_codec_when_available();

    // -- 1. The primitives the RFC 6455 upgrade is built from ---------------
    //
    // RFC 6455 §1.3's worked example: the client key `dGhlIHNhbXBsZSBub25jZQ==`
    // concatenated with the GUID and SHA-1'd must base64 to this exact value.
    assert_eq!(
        accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
        "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=",
        "RFC 6455 section 1.3 worked example"
    );

    // -- 2. A real 0600 rendezvous file with a fresh 256-bit secret ---------
    let temp = TempDir::new("bridge-happy");
    let rendezvous_path = temp.path().join("bridge.rendezvous");

    let listener = BridgeListener::bind_loopback().expect("bind an ephemeral loopback port");
    let endpoint = listener.endpoint();
    assert_eq!(
        endpoint.ip(),
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        "the bridge listener binds loopback only"
    );
    assert_ne!(
        endpoint.port(),
        0,
        "an ephemeral port was actually assigned"
    );

    let written = RendezvousFile::write_new(&rendezvous_path, endpoint, project_claim())
        .expect("write the rendezvous file");
    assert_eq!(written.secret().len(), SECRET_BYTES);
    assert_eq!(SECRET_BYTES, 32, "the per-launch secret is 256 bits");
    assert_eq!(
        RendezvousFile::mode_of(&rendezvous_path).expect("stat the rendezvous file"),
        RENDEZVOUS_FILE_MODE,
        "the rendezvous file is mode 0600"
    );
    assert_eq!(RENDEZVOUS_FILE_MODE, 0o600);

    #[cfg(not(windows))]
    let active = {
        let rotated = written.rotate().expect("rotate the rendezvous secret");
        assert_ne!(
            rotated.secret(),
            written.secret(),
            "rotation issues a fresh per-launch secret"
        );
        rotated
    };
    #[cfg(windows)]
    let active = {
        assert_eq!(
            written.rotate(),
            Err(RendezvousError::WindowsExistingFileReplacementUnsupported {
                path: rendezvous_path.display().to_string(),
            })
        );
        written.clone()
    };
    assert_eq!(
        RendezvousFile::mode_of(&rendezvous_path).expect("stat after rotation"),
        RENDEZVOUS_FILE_MODE
    );

    // The plugin reads it back from disk exactly as the GDScript side would.
    let read_back = RendezvousFile::read(&rendezvous_path).expect("plugin reads the rendezvous");
    assert_eq!(read_back.endpoint(), endpoint);
    assert_eq!(read_back.secret(), active.secret());
    assert_eq!(read_back.project(), &project_claim());

    // -- 3. The full authenticated exchange over a real loopback socket -----
    let handshake = Handshake::new(server_identity(), project_claim(), active.clone());
    let plugin = FakePlugin::spawn(&rendezvous_path, PluginScript::happy(plugin_identity()));
    let mut session = listener
        .accept_authenticated(&handshake)
        .expect("the authenticated handshake completes");
    let outcome = session.outcome();
    assert!(matches!(outcome, HandshakeOutcome::Accepted { .. }));
    assert_eq!(session.plugin().godot_version, "4.4.1.stable");

    // hello → capabilities, then ping (heartbeat), then a read request, then a
    // cancellation. Every frame is a real WebSocket text frame.
    let ping = session
        .exchange(&Envelope::heartbeat(
            &server_identity(),
            "hb-0001",
            1_700_000_000_000,
        ))
        .expect("heartbeat round trip");
    assert_eq!(ping.kind(), MessageKind::Heartbeat);

    let read = Envelope::request(
        &server_identity(),
        "req-0001",
        "idem-0001",
        "editor.describe_selection",
        OperationClass::Read,
        Deadline::from_unix_ms(1_700_000_005_000),
        &json!({"include_children": true}),
    );
    let result = session.exchange(&read).expect("read request round trip");
    assert_eq!(result.kind(), MessageKind::Result);
    assert_eq!(result.correlation_id(), Some("req-0001"));

    let cancel = Envelope::cancel(&server_identity(), "cnl-0001", "req-0002");
    let cancelled = session.exchange(&cancel).expect("cancel round trip");
    assert_eq!(cancelled.kind(), MessageKind::Error);
    assert_eq!(cancelled.error_kind(), Some(ErrorKind::Cancelled));

    session
        .shutdown()
        .expect("the shutdown message closes the session cleanly");
    assert!(
        !rendezvous_path.exists(),
        "the production session removes its rendezvous file during shutdown"
    );
    plugin.join().expect("the fake plugin finished cleanly");

    // Shutdown removes the rendezvous file, so a later process cannot dial a
    // dead port with a live-looking secret.
    assert!(
        !rendezvous_path.exists(),
        "the rendezvous file is deleted on shutdown"
    );

    // -- 4. Canonical golden vectors, byte-for-byte ------------------------
    let vectors = golden_envelopes();
    assert_eq!(vectors.len(), GOLDEN_NAMES.len());
    if let Some(engine_vectors) = &real_gdscript_vectors {
        assert_eq!(
            engine_vectors.len(),
            GOLDEN_NAMES.len(),
            "real Godot must emit every canonical vector"
        );
        for (expected_name, engine_vector) in GOLDEN_NAMES.iter().zip(engine_vectors) {
            assert_eq!(
                engine_vector.name, *expected_name,
                "real Godot must emit canonical vectors in frozen order"
            );
            assert_eq!(
                engine_vector.canonical_json,
                golden(expected_name),
                "real Godot canonical bytes for {expected_name} must match the committed golden"
            );
        }
    }
    for (name, envelope) in &vectors {
        let bytes = canonical_bytes(envelope);
        let text = String::from_utf8(bytes.clone()).expect("canonical bytes are UTF-8");
        assert_eq!(text, golden(name), "golden vector {name} is byte-stable");
        assert!(
            !text.ends_with('\n') && !text.contains("  "),
            "canonical JSON carries no trailing or padding whitespace"
        );
        validate_canonical(&serde_json::from_str::<Value>(&text).expect("golden parses"))
            .unwrap_or_else(|error| panic!("golden {name} violates the canonical schema: {error}"));

        // Round-trip: parse the golden text back into an envelope and re-render.
        let parsed = Envelope::parse(&text).unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(canonical_bytes(&parsed), bytes, "{name} round-trips");
    }

    // The pinned GDScript harness is the byte-identical other half: it embeds
    // every golden vector's exact text, so a Rust-side change that broke byte
    // stability would leave the two halves disagreeing.
    let harness = gdscript_harness_source();
    assert_eq!(
        godot_mcp_bridge::protocol::harness::harness_digest(),
        GDSCRIPT_HARNESS_SHA256,
        "the embedded GDScript harness matches its pinned digest"
    );
    for (name, _) in &vectors {
        assert!(
            harness.contains(&format!("\"{name}\"")),
            "the GDScript harness names golden vector {name}"
        );
    }
    for (name, envelope) in &vectors {
        let escaped = golden(name).replace('\\', "\\\\").replace('"', "\\\"");
        assert!(
            harness.contains(&escaped),
            "the GDScript harness embeds golden vector {name} verbatim"
        );
        let _ = envelope;
    }

    // -- 5. The compatibility matrix admits exactly this pairing ------------
    let matrix = CompatibilityMatrix::new(PROTOCOL_VERSION, project_claim());
    assert_eq!(
        matrix.decide(&plugin_identity(), &project_claim()),
        CompatibilityDecision::Compatible
    );

    // -- 6. A committed mutation is durable and never replayed -------------
    let mut ledger = MutationLedger::new();
    let started = ledger
        .open("mut-0001", "idem-mut-0001", OperationClass::Mutation)
        .expect("open a mutation");
    assert_eq!(started.status(), MutationStatus::NotStarted);
    let committed = ledger
        .settle("mut-0001", MutationStatus::Committed, Some("action-42"))
        .expect("settle the mutation");
    assert_eq!(committed.status(), MutationStatus::Committed);
    assert!(
        ledger.replayable("mut-0001").is_none(),
        "a settled mutation is never offered for replay"
    );
    assert_eq!(MutationStatus::ALL.len(), 3);
}

// ---------------------------------------------------------------------------
// Failure paths
// ---------------------------------------------------------------------------

#[test]
fn bridge_protocol_failure_paths() {
    let temp = TempDir::new("bridge-failure");
    let rendezvous_path = temp.path().join("bridge.rendezvous");

    let listener = BridgeListener::bind_loopback().expect("bind an ephemeral loopback port");
    let endpoint = listener.endpoint();
    let rendezvous = RendezvousFile::write_new(&rendezvous_path, endpoint, project_claim())
        .expect("write the rendezvous file");
    let handshake = Handshake::new(server_identity(), project_claim(), rendezvous.clone());

    // -- 1. A missing secret is refused ------------------------------------
    let mut hello = hello_value(
        &plugin_identity(),
        &project_claim(),
        rendezvous.secret_hex(),
    );
    hello.as_object_mut().expect("object").remove("auth");
    assert_eq!(
        handshake.authenticate_value(&hello),
        Err(HandshakeError::MissingSecret),
        "a hello with no auth block is refused"
    );

    // -- 2. A wrong secret is refused, in constant time --------------------
    let wrong = "0".repeat(64);
    let hello_wrong = hello_value(&plugin_identity(), &project_claim(), wrong.clone());
    assert_eq!(
        handshake.authenticate_value(&hello_wrong),
        Err(HandshakeError::SecretMismatch)
    );
    // The comparison examines every byte regardless of where the first
    // difference is: an early-exit implementation would report a different
    // number of examined bytes for these two inputs.
    let reference = [0x5a_u8; SECRET_BYTES];
    let mut differs_first = reference;
    differs_first[0] ^= 0xff;
    let mut differs_last = reference;
    differs_last[SECRET_BYTES - 1] ^= 0xff;
    assert!(!constant_time_eq(&reference, &differs_first));
    assert!(!constant_time_eq(&reference, &differs_last));
    assert_eq!(
        godot_mcp_bridge::protocol::rendezvous::compare_probe(&reference, &differs_first),
        godot_mcp_bridge::protocol::rendezvous::compare_probe(&reference, &differs_last),
        "the secret comparison touches every byte no matter where inputs diverge"
    );
    assert!(constant_time_eq(&reference, &reference));
    assert!(
        !constant_time_eq(&reference, &reference[..SECRET_BYTES - 1]),
        "a truncated candidate never compares equal"
    );

    #[cfg(not(windows))]
    let current = {
        let stale_secret = rendezvous.secret_hex();
        let rotated = rendezvous.rotate().expect("rotate the secret");
        let rotated_handshake = Handshake::new(server_identity(), project_claim(), rotated.clone());
        let replayed = hello_value(&plugin_identity(), &project_claim(), stale_secret);
        assert_eq!(
            rotated_handshake.authenticate_value(&replayed),
            Err(HandshakeError::SecretMismatch),
            "a secret from a previous launch does not authenticate after rotation"
        );
        rotated
    };
    #[cfg(windows)]
    let current = {
        assert_eq!(
            rendezvous.rotate(),
            Err(RendezvousError::WindowsExistingFileReplacementUnsupported {
                path: rendezvous_path.display().to_string(),
            })
        );
        rendezvous.clone()
    };
    let current_handshake = Handshake::new(server_identity(), project_claim(), current.clone());
    // And the same hello is refused a second time even with the *current*
    // secret, because a hello nonce is single-use.
    let fresh = hello_value(&plugin_identity(), &project_claim(), current.secret_hex());
    assert!(current_handshake.authenticate_value(&fresh).is_ok());
    assert_eq!(
        current_handshake.authenticate_value(&fresh),
        Err(HandshakeError::ReplayedNonce),
        "a replayed hello nonce is refused even with the correct secret"
    );

    // -- 4. A stale project identity is refused ----------------------------
    let mut stale_project = project_claim();
    stale_project.generation += 1;
    let matrix = CompatibilityMatrix::new(PROTOCOL_VERSION, project_claim());
    assert_eq!(
        matrix.decide(&plugin_identity(), &stale_project),
        CompatibilityDecision::ProjectGenerationStale {
            expected: 7,
            observed: 8
        }
    );
    let mut other_root = project_claim();
    other_root.root_object = "unix:dev=66305,ino=9999".to_owned();
    assert_eq!(
        matrix.decide(&plugin_identity(), &other_root),
        CompatibilityDecision::ProjectRootMismatch
    );
    let handshake_stale = Handshake::new(server_identity(), project_claim(), current.clone());
    let hello_stale = hello_value(&plugin_identity(), &stale_project, current.secret_hex());
    assert_eq!(
        handshake_stale.authenticate_value(&hello_stale),
        Err(HandshakeError::Incompatible(
            CompatibilityDecision::ProjectGenerationStale {
                expected: 7,
                observed: 8
            }
        ))
    );

    // -- 5. An unsupported major version is refused ------------------------
    let mut future = plugin_identity();
    future.protocol_version = format!("{}.0.0", PROTOCOL_MAJOR + 1);
    assert_eq!(
        matrix.decide(&future, &project_claim()),
        CompatibilityDecision::MajorVersionUnsupported {
            expected: PROTOCOL_MAJOR,
            observed: PROTOCOL_MAJOR + 1
        }
    );
    let mut past = plugin_identity();
    past.protocol_version = "0.9.0".to_owned();
    assert!(matches!(
        matrix.decide(&past, &project_claim()),
        CompatibilityDecision::MajorVersionUnsupported { .. }
    ));
    let mut garbage = plugin_identity();
    garbage.protocol_version = "not-a-version".to_owned();
    assert_eq!(
        matrix.decide(&garbage, &project_claim()),
        CompatibilityDecision::MalformedVersion
    );
    assert_eq!(
        CompatibilityDecision::MajorVersionUnsupported {
            expected: PROTOCOL_MAJOR,
            observed: PROTOCOL_MAJOR + 1
        }
        .error_kind(),
        Some(ErrorKind::Unsupported)
    );

    // -- 6. A 16 MiB frame is refused without allocating it ----------------
    let oversize = 16 * 1024 * 1024_u64;
    assert!(oversize > MAX_FRAME_BYTES as u64);
    assert_eq!(MAX_FRAME_BYTES, 4 * 1024 * 1024);
    assert_eq!(
        WsFrame::validate_declared_length(oversize),
        Err(WsError::FrameTooLarge { declared: oversize })
    );
    let huge = "x".repeat(MAX_FRAME_BYTES + 1);
    assert_eq!(
        encode_server_text(&huge),
        Err(WsError::FrameTooLarge {
            declared: MAX_FRAME_BYTES as u64 + 1
        }),
        "an oversize outbound frame is refused before it is written"
    );
    // A declared 16 MiB inbound length is refused from the header alone.
    let mut header = vec![0x81_u8, 0x7f];
    header.extend_from_slice(&oversize.to_be_bytes());
    assert_eq!(
        WsFrame::parse(&header),
        Err(WsError::FrameTooLarge { declared: oversize })
    );
    // The listener itself survives the attempt and refuses the peer.
    let refused = listener
        .refuse_oversize_probe(oversize)
        .expect("the listener classifies an oversize declaration");
    assert_eq!(refused, ErrorKind::InvalidInput);

    // -- 7. A duplicate mutation ID is refused -----------------------------
    let mut ledger = MutationLedger::new();
    ledger
        .open("mut-dup", "idem-dup", OperationClass::Mutation)
        .expect("first open succeeds");
    assert_eq!(
        ledger
            .open("mut-dup", "idem-dup", OperationClass::Mutation)
            .expect_err("a duplicate mutation ID is refused"),
        MutationOutcome::DuplicateRequestId("mut-dup".to_owned())
    );
    ledger
        .open("mut-other", "idem-dup", OperationClass::Mutation)
        .expect_err("a duplicate idempotency ID is refused too");

    // -- 8. An unresolved mutation goes to Todo 6 and is never replayed -----
    ledger
        .open(
            "mut-unresolved",
            "idem-unresolved",
            OperationClass::Mutation,
        )
        .expect("open the mutation");
    ledger
        .mark_dispatched("mut-unresolved")
        .expect("the request reached the bridge");
    // Every other mutation in this ledger is dispatched and settled too, so the
    // reconnect assertion below is about unresolved work and not about work that
    // simply never left this process.
    ledger.mark_dispatched("mut-dup").expect("dispatch");
    ledger
        .settle("mut-dup", MutationStatus::Committed, Some("action-1"))
        .expect("settle");
    let unresolved = ledger
        .settle("mut-unresolved", MutationStatus::Unresolved, None)
        .expect("settle as unresolved");
    assert_eq!(unresolved.status(), MutationStatus::Unresolved);
    let handoff: QuarantineHandoff = unresolved
        .quarantine_handoff()
        .expect("an unresolved mutation always produces a handoff");
    let record = handoff
        .to_canonical_value(&transaction_envelope())
        .expect("the handoff renders a canonical Todo 6 record");
    assert_eq!(record["effect_kind"], json!("editor_runtime"));
    assert_eq!(record["phase"], json!("quarantined"));
    assert_eq!(record["proof"]["kind"], json!("editor_runtime"));
    godot_mcp_engine::supervision::journal::validate_canonical(&record)
        .expect("the handoff validates against Todo 6's own canonical schema");

    // A reconnect re-authenticates but never re-offers unresolved work.
    let reconnected = ledger.reconnect();
    assert!(
        reconnected.is_empty(),
        "reconnect surfaces no replayable mutation: {reconnected:?}"
    );
    assert!(ledger.replayable("mut-unresolved").is_none());
    assert!(
        ledger.quarantined().iter().any(|id| id == "mut-unresolved"),
        "the unresolved mutation stays quarantined across the reconnect"
    );

    // -- 9. Structural refusals in the codec and envelope ------------------
    // A server-to-client frame is never masked; a client-to-server frame always
    // is (RFC 6455 §5.1).
    let server_frame = encode_server_text("{}").expect("encode");
    assert_eq!(server_frame[1] & 0x80, 0, "server frames are unmasked");
    let client_frame = encode_client_text("{}", [0x11, 0x22, 0x33, 0x44]).expect("encode");
    assert_eq!(client_frame[1] & 0x80, 0x80, "client frames are masked");
    let parsed = WsFrame::parse_from_client(&client_frame).expect("parse the masked frame");
    assert_eq!(parsed.opcode, Opcode::Text);
    assert_eq!(parsed.payload, b"{}");
    let mut unmasked_from_client = client_frame.clone();
    unmasked_from_client[1] &= 0x7f;
    assert_eq!(
        WsFrame::parse_from_client(&unmasked_from_client[..2]),
        Err(WsError::UnmaskedClientFrame)
    );

    // The upgrade request/response pair is checked, not assumed.
    let request = client_handshake_request(endpoint, "dGhlIHNhbXBsZSBub25jZQ==");
    let key = parse_client_handshake(request.as_bytes()).expect("parse the upgrade request");
    let response = server_handshake_response(&key);
    parse_server_handshake(response.as_bytes(), &key).expect("the response accepts the key");
    assert_eq!(
        parse_server_handshake(response.as_bytes(), "AAAAAAAAAAAAAAAAAAAAAA==")
            .expect_err("a mismatched accept is refused"),
        WsError::AcceptKeyMismatch
    );
    assert_eq!(
        parse_client_handshake(
            b"GET / HTTP/1.1\r\nHost: 127.0.0.1:45871\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\n\r\n"
        )
            .expect_err("an upgrade with no key is refused"),
        WsError::MissingKey
    );

    // An envelope missing a required field, or carrying an unknown kind, is
    // refused by the canonical schema rather than silently accepted.
    let mut bad: Value = serde_json::from_str(&golden("request")).expect("golden parses");
    bad.as_object_mut()
        .expect("object")
        .remove("deadline_unix_ms");
    assert!(validate_canonical(&bad).is_err());
    let mut unknown: Value = serde_json::from_str(&golden("request")).expect("golden parses");
    unknown["kind"] = json!("teleport");
    assert!(validate_canonical(&unknown).is_err());
    assert_eq!(
        Envelope::parse("{not json").expect_err("malformed JSON is refused"),
        EnvelopeError::Malformed
    );
    assert!(PROTOCOL_SCHEMA_JSON.contains("\"kind\""));

    // -- 10. A non-loopback bind is impossible ----------------------------
    assert_eq!(
        BridgeListener::bind_address("0.0.0.0:0".parse().expect("addr"))
            .expect_err("a non-loopback bind is refused"),
        ListenerError::NotLoopback
    );

    // -- 11. A rendezvous file with loose permissions is refused ----------
    let loose = temp.path().join("loose.rendezvous");
    std::fs::write(&loose, b"{}").expect("write");
    RendezvousFile::set_mode(&loose, 0o644).expect("relax the mode");
    // Windows has no mode: a file written the ordinary way inherits its
    // directory's ACL, which is exactly what a private rendezvous must not do.
    // Which check refuses it first depends on the host: a file an elevated
    // administrator creates is owned by the Administrators group, so the owner
    // check fires before the inheritance check. Either way it is insecure.
    let loose_refusal =
        RendezvousFile::read(&loose).expect_err("a world-readable rendezvous is refused");
    if cfg!(windows) {
        assert!(
            matches!(loose_refusal, RendezvousError::InsecureAccess { .. }),
            "{loose_refusal:?}"
        );
    } else {
        assert_eq!(
            loose_refusal,
            RendezvousError::InsecurePermissions { mode: 0o644 }
        );
    }
    let missing = temp.path().join("absent.rendezvous");
    assert!(matches!(
        RendezvousFile::read(&missing),
        Err(RendezvousError::Unreadable { .. })
    ));

    drop(listener);
    let dangling = TcpStream::connect(endpoint);
    if let Ok(stream) = dangling {
        drop(stream);
    }
}

// ---------------------------------------------------------------------------
// Golden vector construction
// ---------------------------------------------------------------------------

fn golden_envelopes() -> Vec<(&'static str, Envelope)> {
    let server = server_identity();
    let plugin = plugin_identity();
    let project = project_claim();
    vec![
        (
            "hello",
            Envelope::hello(&plugin, &project, "aa".repeat(32), "nonce-0001"),
        ),
        (
            "capabilities",
            Envelope::capabilities(
                &server,
                &project,
                &["editor.describe_selection", "editor.set_property"],
            ),
        ),
        (
            "request",
            Envelope::request(
                &server,
                "req-0001",
                "idem-0001",
                "editor.describe_selection",
                OperationClass::Read,
                Deadline::from_unix_ms(1_700_000_005_000),
                &json!({"include_children": true}),
            ),
        ),
        (
            "result",
            Envelope::result(
                &plugin,
                "req-0001",
                MutationStatus::NotStarted,
                &json!({"selected": ["res://main.tscn::Player"]}),
            ),
        ),
        (
            "error",
            Envelope::error(
                &plugin,
                "req-0002",
                ErrorKind::Cancelled,
                "the request was cancelled before it started",
            ),
        ),
        (
            "progress",
            Envelope::progress(&plugin, "req-0003", 3, 7, "importing resources"),
        ),
        ("cancel", Envelope::cancel(&server, "cnl-0001", "req-0002")),
        (
            "heartbeat",
            Envelope::heartbeat(&server, "hb-0001", 1_700_000_000_000),
        ),
        (
            "event",
            Envelope::event(
                &plugin,
                "evt-0001",
                "editor.scene_changed",
                &json!({"scene": "res://main.tscn"}),
            ),
        ),
        (
            "shutdown",
            Envelope::shutdown(&server, "sd-0001", "server is stopping"),
        ),
    ]
}

fn hello_value(plugin: &PluginIdentity, project: &ProjectClaim, secret_hex: String) -> Value {
    let envelope = Envelope::hello(plugin, project, secret_hex, "nonce-0001");
    serde_json::from_slice(&canonical_bytes(&envelope)).expect("hello renders canonical JSON")
}

fn transaction_envelope() -> godot_mcp_engine::supervision::journal::TransactionEnvelope {
    use godot_mcp_engine::supervision::journal::{
        OperationFamily, ProjectIdentityClaim, TransactionEnvelope, TransportMode,
    };
    TransactionEnvelope {
        transaction_id: "3f2504e0-4f89-41d3-9a0c-0305e82c3301".to_owned(),
        project: ProjectIdentityClaim {
            object_identity:
                "project_1111111111111111111111111111111111111111111111111111111111111111"
                    .to_owned(),
            root_object: "unix:dev=66305,ino=4242".to_owned(),
            file_object: "unix:dev=66305,ino=4243".to_owned(),
            generation: 7,
        },
        transport_mode: TransportMode::Stdio,
        principal: None,
        session: None,
        operation_family: OperationFamily::EditorMutation,
        tool_name: "editor.set_property".to_owned(),
        idempotency_id: "idem-unresolved".to_owned(),
        canonical_request_hash: "c".repeat(64),
        canonical_request_bytes: 64,
        opened_at_unix_ms: 1_700_000_000_000,
    }
}
