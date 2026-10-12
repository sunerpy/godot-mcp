//! Structured audit records, their append-only sink, and the structural proof
//! that no arbitrary-execution surface exists.
//!
//! The three concerns are one module because they are one guarantee: an operation
//! is either recorded with enough non-sensitive facts to reconstruct who did what
//! to which project under which policy, or — for a high-risk mutation under a
//! fail-closed configuration — it does not happen at all.

pub mod absence;
mod context;
pub mod event;
pub mod sink;

pub use absence::{
    ConfigSurfaceProbe, ExecutionSurfaceAudit, RESERVED_UNSAFE_SCRIPT_CAPABILITY,
    audit_execution_surface, config_surface_probe, scan_bytes_for_reserved_identifier,
};
pub use event::{
    AuditActorFacts, AuditBoundary, AuditCorrelationFacts, AuditDigest, AuditEffectFacts,
    AuditError, AuditErrorFacts, AuditEvent, AuditEventDraft, AuditLane, AuditOutcomeFacts,
    AuditPhase, AuditPolicyFacts, AuditPriority, AuditProjectFacts, AuditQueueFacts, AuditStatus,
    AuditTransport, MAX_AUDIT_EVENT_BYTES, capability_audit_name, reason_audit_name,
    verdict_audit_name,
};
pub use sink::{AuditFailurePolicy, AuditLog, AuditSinkConfig};

pub(crate) use context::{PolicyAuditDecision, RequestAudit, RequestEffectFacts};

#[cfg(test)]
mod audit_layer_audit {
    //! The bans this layer must keep are asserted against its own bytes, so a
    //! future edit that reintroduces one fails a test rather than passing review.

    const EVENT_SOURCE: &str = include_str!("event.rs");
    const SINK_SOURCE: &str = include_str!("sink.rs");
    const ABSENCE_SOURCE: &str = include_str!("absence.rs");

    /// Scan the code region only: each module's prose deliberately *names* the
    /// bans it keeps, so a prose mention must not be mistaken for an
    /// implementation.
    fn production_source(source: &str) -> &str {
        let start = source.find("\nuse ").unwrap_or(0);
        let end = source.find("#[cfg(test)]").unwrap_or(source.len());
        &source[start..end]
    }

    /// An audit record has no field type that could carry raw content. The record
    /// struct admits only `&'static str` enum names, `SafeField`, `AuditDigest`,
    /// integers and booleans, so raw arguments, results and storage locators have
    /// nowhere to live rather than merely being omitted by convention.
    #[test]
    fn the_audit_record_has_no_field_that_could_carry_content() {
        let source = production_source(EVENT_SOURCE);
        let record = source
            .find("pub struct AuditEvent {")
            .map(|start| &source[start..])
            .and_then(|rest| rest.find('}').map(|end| &rest[..end]))
            .expect("the audit record struct is present");
        for banned in [
            ": String",
            ": Vec<u8>",
            ": PathBuf",
            ": Value",
            ": serde_json",
            ": Option<String>",
        ] {
            assert!(
                !record.contains(banned),
                "the audit record must not own a field typed {banned}"
            );
        }
    }

    /// Redaction is Todo 4's, not a second implementation.
    #[test]
    fn the_audit_layer_reuses_the_established_redaction_types() {
        let source = production_source(EVENT_SOURCE);
        assert!(
            source.contains("crate::observability::"),
            "the audit layer must reuse the established redaction wrappers"
        );
        for expected in [
            "pub fn of_secret",
            "pub fn of_token",
            "pub fn of_source",
            "pub fn of_screenshot",
        ] {
            assert!(
                source.contains(expected),
                "sensitive material must reach a record only as a digest ({expected})"
            );
        }
        assert!(
            !source.contains("fn expose_into_record"),
            "no path may move exposed material into a record"
        );
    }

    /// Capability and reason names come from the Todo 26 decision module, so the
    /// audit log and the policy digest can never disagree.
    #[test]
    fn capability_and_reason_names_have_one_definition() {
        let source = production_source(EVENT_SOURCE);
        assert!(source.contains("crate::policy::decision::capability_name"));
        assert!(source.contains("crate::policy::decision::reason_name"));
        assert!(
            !source.contains("Capability::ProjectWrite =>"),
            "the audit layer must not restate the capability name table"
        );
    }

    /// No part of this layer reaches the filesystem except the sink.
    #[test]
    fn only_the_sink_touches_the_filesystem() {
        for (name, source) in [
            ("event.rs", production_source(EVENT_SOURCE)),
            ("absence.rs", production_source(ABSENCE_SOURCE)),
        ] {
            for banned in ["std::fs::", "OpenOptions", "File::create", "File::open"] {
                assert!(
                    !source.contains(banned),
                    "{name} must not reach the filesystem ({banned})"
                );
            }
        }
        assert!(
            production_source(SINK_SOURCE).contains("fs::create_dir_all"),
            "the sink owns the only filesystem access in this layer"
        );
    }

    /// The reserved identifier has exactly one definition and no execution path.
    #[test]
    fn the_reserved_identifier_is_only_ever_a_name() {
        let source = production_source(ABSENCE_SOURCE);
        assert_eq!(
            source
                .matches("pub const RESERVED_UNSAFE_SCRIPT_CAPABILITY")
                .count(),
            1,
            "the reserved identifier must have exactly one definition"
        );
        for banned in [
            "Command::new",
            "std::process::Command",
            "fn run_unsafe_script",
            "fn execute_script",
            "register_tool_handler(RESERVED",
        ] {
            assert!(
                !source.contains(banned),
                "the reserved identifier must never gain an execution path ({banned})"
            );
        }
    }

    /// The audit layer never writes to protocol stdout.
    #[test]
    fn the_audit_layer_never_writes_protocol_stdout() {
        for source in [
            production_source(EVENT_SOURCE),
            production_source(SINK_SOURCE),
            production_source(ABSENCE_SOURCE),
        ] {
            for banned in ["io::stdout", "println!", "print!"] {
                assert!(
                    !source.contains(banned),
                    "protocol stdout belongs solely to MCP framing ({banned})"
                );
            }
        }
    }
}
