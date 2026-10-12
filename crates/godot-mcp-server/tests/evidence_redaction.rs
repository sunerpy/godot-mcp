use std::fs;
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};
use std::panic::AssertUnwindSafe;

use godot_mcp_server::evidence::{EvidenceRecord, EvidenceWriter, TestPhase};
use godot_mcp_server::observability::{
    CorrelationId, RequestId, SafeField, ScreenshotBytes, SecretValue, SessionId, SourceText,
    TokenValue, isolate_panic,
};
use godot_mcp_server::test_support::{CapturedProtocolHarness, DeterministicFixture};

const TOKEN: &str = "Bearer todo4-token-must-not-leak";
const SOURCE: &str = "extends Node\nvar password = 'source-must-not-leak'";

#[test]
fn stdio_stderr_separation_smoke() {
    let harness = CapturedProtocolHarness::new();
    harness.run(|| {
        tracing::info!(
            correlation_id = %CorrelationId::new("corr-0001").unwrap(),
            request_id = %RequestId::new("req-0001").unwrap(),
            session_id = %SessionId::new("session-0001").unwrap(),
            event = "fixture_started",
            "protocol-safe diagnostic"
        );
        harness.protocol_frame(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#);
    });

    assert_eq!(
        harness.stdout(),
        b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n"
    );
    let stderr = String::from_utf8(harness.stderr()).unwrap();
    assert!(stderr.contains("fixture_started"));
    assert!(stderr.contains("corr-0001"));
    assert!(!stderr.contains("jsonrpc"));
}

#[test]
fn secret_and_source_logging_is_redacted() {
    let harness = CapturedProtocolHarness::new();
    harness.run(|| {
        tracing::warn!(
            token = %TokenValue::new(TOKEN),
            secret = ?SecretValue::new("secret-must-not-leak"),
            source = ?SourceText::new(SOURCE),
            screenshot = ?ScreenshotBytes::new(b"screenshot-bytes-must-not-leak".to_vec()),
            bounded = %SafeField::new("safe-field").unwrap(),
            "redaction fixture"
        );
    });

    let stderr = String::from_utf8(harness.stderr()).unwrap();
    assert!(stderr.contains("[REDACTED]"));
    for forbidden in [
        TOKEN,
        "secret-must-not-leak",
        "source-must-not-leak",
        "screenshot-bytes-must-not-leak",
    ] {
        assert!(
            !stderr.contains(forbidden),
            "raw fixture leaked: {forbidden}"
        );
    }
    assert!(harness.stdout().is_empty());
}

#[test]
fn deterministic_fixture_and_evidence_are_bounded_and_replayable() {
    let sandbox = tempfile::tempdir().unwrap();
    let fixture = DeterministicFixture::new(sandbox.path(), "task-4", 7, 41_000).unwrap();
    assert_eq!(fixture.clock().now_millis(), 7);
    assert_eq!(fixture.clock().now_millis(), 8);
    let first_port = fixture.ports().allocate().unwrap();
    let second_port = fixture.ports().allocate().unwrap();
    assert!(
        first_port >= 41_000,
        "allocation starts at or after the seeded cursor, got {first_port}"
    );
    assert!(
        second_port > first_port,
        "fixture ports increase strictly, got {second_port} after {first_port}"
    );
    for reserved in [first_port, second_port] {
        assert!(
            TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, reserved)).is_err(),
            "allocated port {reserved} must stay reserved for the fixture lifetime"
        );
    }
    assert!(
        fixture
            .temp_root("redaction")
            .unwrap()
            .ends_with("task-4-7-redaction-0000")
    );

    let writer = EvidenceWriter::new(sandbox.path().join("evidence"), 4096).unwrap();
    let record = EvidenceRecord::new(
        TestPhase::Green,
        "stdio-stderr-separation",
        "cargo test -p godot-mcp-server --test evidence_redaction",
        0,
        b"bounded output",
        ["task-4", "stdio-fixture"],
    )
    .unwrap();
    let path = writer.write("task-4-green", &record).unwrap();
    let bytes = fs::read(path).unwrap();
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.contains("\"test_phase\":\"green\""));
    assert!(text.contains("\"exit_code\":0"));
    assert!(text.contains("\"output_sha256\":"));
    assert!(!text.contains("bounded output"));
}

#[test]
fn panic_boundary_redacts_payload_and_preserves_unwind() {
    let harness = CapturedProtocolHarness::new();
    let result = harness.run(|| {
        isolate_panic(
            "fixture-boundary",
            CorrelationId::new("corr-panic").unwrap(),
            AssertUnwindSafe(|| panic!("{TOKEN}: {SOURCE}")),
        )
    });
    assert!(result.is_err());
    let stderr = String::from_utf8(harness.stderr()).unwrap();
    assert!(stderr.contains("panic_isolated"));
    assert!(stderr.contains("[REDACTED]"));
    assert!(!stderr.contains(TOKEN));
    assert!(!stderr.contains(SOURCE));
    assert!(harness.stdout().is_empty());
}
