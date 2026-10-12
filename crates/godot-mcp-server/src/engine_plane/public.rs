//! Public projections of supervised-process facts.
//!
//! The durable journal records a complete [`TypedProof`] — including the
//! boundary's concrete identity (a locator directory, cgroup path or Job Object
//! name), the child PID and its start time — because reconciliation needs them.
//! None of that is a caller fact. Every engine-plane result therefore renders a
//! process through [`execution_value`] and [`stream_value`] only, and those two
//! functions are structurally unable to emit an identity string, a PID or a
//! filesystem path.

use serde_json::{Value, json};

use crate::effect::ProcessOutputMetadata;
use crate::effect::boundary::{ProcessOutputExcerpt, ProcessStreamOutputMetadata, StreamExcerpt};
use crate::journal::{BoundaryMembership, ReleaseStatus, TypedProof};

/// The redacted execution proof: which containment mechanism held the process
/// and how strongly, whether membership and emptiness were proven, and how the
/// process ended.
pub(super) fn execution_value(proof: &TypedProof, kind: &str, strength: &str) -> Value {
    match proof {
        TypedProof::ProcessLaunch {
            boundary_membership,
            release_status,
            exit_code,
            signal,
            boundary_empty_after_exit,
            ..
        } => json!({
            "boundary": {
                "kind": kind,
                "strength": strength,
                "membership": match boundary_membership {
                    BoundaryMembership::Confirmed => "confirmed",
                    BoundaryMembership::Unconfirmed => "unconfirmed",
                },
                "empty_after_exit": boundary_empty_after_exit,
            },
            "release_status": release_status_str(*release_status),
            "exit_code": exit_code,
            "signal": signal,
        }),
        // A non-process proof is never attached to a process result; rendering
        // it as an explicit absence keeps the projection total without leaking.
        TypedProof::File { .. } | TypedProof::EditorRuntime { .. } => Value::Null,
    }
}

/// One captured stream: its byte accounting plus a bounded text excerpt.
pub(super) fn stream_value(
    metadata: ProcessStreamOutputMetadata,
    excerpt: &StreamExcerpt,
) -> Value {
    json!({
        "retained_bytes": metadata.retained_bytes,
        "total_bytes": metadata.total_bytes,
        "truncated": metadata.truncated,
        "text": excerpt.text,
        "text_truncated": excerpt.shortened,
    })
}

/// Both streams, keyed the way every engine-plane result spells them.
pub(super) fn streams_value(
    output: ProcessOutputMetadata,
    excerpt: &ProcessOutputExcerpt,
) -> (Value, Value) {
    (
        stream_value(output.stdout, &excerpt.stdout),
        stream_value(output.stderr, &excerpt.stderr),
    )
}

const fn release_status_str(status: ReleaseStatus) -> &'static str {
    match status {
        ReleaseStatus::Suspended => "suspended",
        ReleaseStatus::ResumedContained => "resumed-contained",
        ReleaseStatus::ExitedClean => "exited-clean",
        ReleaseStatus::Signaled => "signaled",
        ReleaseStatus::NonzeroExit => "nonzero-exit",
        ReleaseStatus::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_execution_projection_never_carries_an_identity_pid_or_start_time() {
        let proof = TypedProof::ProcessLaunch {
            boundary_identity: "/home/user/game/.godot-mcp/launches/locator-1".to_owned(),
            pid: Some(4242),
            start_time: Some("98765".to_owned()),
            boundary_membership: BoundaryMembership::Confirmed,
            release_status: ReleaseStatus::NonzeroExit,
            exit_code: Some(1),
            signal: None,
            boundary_empty_after_exit: Some(true),
        };

        let rendered = execution_value(&proof, "unix_locator", "weak").to_string();

        for private in [
            "/home/user",
            "locator-1",
            "4242",
            "98765",
            "boundary_identity",
        ] {
            assert!(!rendered.contains(private), "{private} leaked: {rendered}");
        }
        assert!(
            rendered.contains("\"release_status\":\"nonzero-exit\""),
            "{rendered}"
        );
        assert!(rendered.contains("\"strength\":\"weak\""), "{rendered}");
    }
}
