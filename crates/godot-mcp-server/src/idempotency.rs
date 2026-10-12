//! Cross-plane mutation identity and replay decisions.
//!
//! This module is deliberately a *sibling* of [`scheduler`](crate::scheduler)
//! rather than a child of it. Mutation identity is cross-plane: the stdio
//! transport, the optional HTTP transports, the kernel and the scheduler all have
//! to speak about the same eight-element tuple, and routing that vocabulary
//! through the scheduler would imply the tuple only means something once work is
//! queued. It means something the moment a request arrives.
//!
//! Two rules hold everywhere in here:
//!
//! * **Only an exact same-context tuple may return a recorded result.** All eight
//!   elements must match. Anything else is a conflict.
//! * **A conflict says nothing.** There is exactly one conflict constructor, it
//!   takes no arguments, and its bytes are identical for every mismatch. See
//!   [`replay`] for why that asymmetry against Todo 17's expansive refusal is
//!   intentional.
//!
//! Nothing here owns persistence. The durable record of a mutation is the Todo 6
//! journal, and this module reads its own vocabulary *off* that store; it creates
//! no second journal, no competing schema and no in-memory ledger.

// The gate's transport call sites arrive with Todos 28-31, and the scheduler binds
// it once its admission bodies are complete. The relaxation is scoped to this
// module and carries its reason rather than being a crate-wide allow.
#![allow(
    dead_code,
    reason = "Todo 27 defines the cross-plane idempotency vocabulary; its transport call sites \
              land in Todos 28-31 and the scheduler's own binding completes with its admission \
              bodies"
)]

pub(crate) mod gate;
pub mod identity;
pub mod replay;

pub use identity::{
    ConnectionSession, IdempotencyId, IdentityError, ModePrincipal, MutationIdentity,
    OperationToolFamily, ProjectBinding, ProjectObjectIdentity, RequestHash, TransportAuthMode,
    TransportBinding, TupleDimension, canonical_request_hash, family_of, family_of_descriptor,
};
pub use replay::{
    ConflictEnvelope, RecordedMutation, RecordedStatus, RecordedVerdict, ReplayDecision,
    conflict_envelope,
};

#[cfg(test)]
mod self_audit {
    //! Source-scanning self-audit plus the byte-stability proof for the single
    //! conflict envelope. The bans are asserted against this module's own bytes,
    //! so a future edit that reintroduces one fails a test rather than passing
    //! review.

    use super::*;

    const IDENTITY_SOURCE: &str = include_str!("idempotency/identity.rs");
    const REPLAY_SOURCE: &str = include_str!("idempotency/replay.rs");
    const GATE_SOURCE: &str = include_str!("idempotency/gate.rs");

    fn production_source(source: &str) -> &str {
        source
            .find("#[cfg(test)]")
            .map_or(source, |index| &source[..index])
    }

    fn identity(
        tool: &str,
        idempotency_id: &str,
        arguments: &serde_json::Value,
    ) -> MutationIdentity {
        MutationIdentity::new(
            ProjectBinding::new(
                ProjectObjectIdentity::new(format!("project_{}", "a".repeat(64)))
                    .expect("a non-empty project object"),
                7,
            ),
            TransportBinding::new(
                TransportAuthMode::Stdio,
                ModePrincipal::LocalOs {
                    principal: "principal_0123456789abcdef".to_owned(),
                },
                ConnectionSession::new("session_0123456789abcdef").expect("a non-empty session"),
            )
            .expect("stdio pairs with a local-OS principal"),
            family_of(tool).expect("a canonical tool"),
            IdempotencyId::new(idempotency_id).expect("a non-empty identifier"),
            canonical_request_hash(tool, arguments).expect("an encodable request"),
        )
    }

    /// There is exactly one conflict construction site, and it takes no arguments.
    #[test]
    fn the_conflict_envelope_has_exactly_one_argument_free_construction_site() {
        let replay = production_source(REPLAY_SOURCE);
        assert_eq!(
            replay.matches("ConflictEnvelope { seal: () }").count(),
            1,
            "exactly one conflict envelope may ever be constructed"
        );
        assert!(
            replay.contains("pub fn conflict_envelope() -> ConflictEnvelope"),
            "the sole construction site must take no arguments, so no cause can leak"
        );
        for source in [IDENTITY_SOURCE, GATE_SOURCE] {
            assert!(
                !production_source(source).contains("ConflictEnvelope {"),
                "only the replay module may construct a conflict envelope"
            );
        }
    }

    /// The envelope's bytes never vary, and never name an identity element.
    #[test]
    fn the_conflict_envelope_is_byte_stable_and_names_nothing() {
        let bytes = conflict_envelope().as_bytes();
        assert_eq!(bytes, conflict_envelope().as_bytes());
        let text = String::from_utf8(bytes).expect("the envelope is UTF-8");
        for leak in [
            "principal",
            "session",
            "generation",
            "hash",
            "transaction",
            "family",
            "exists",
            "already",
            "recorded",
            "stdio",
            "oidc",
        ] {
            assert!(
                !text.contains(leak),
                "the conflict envelope must not mention {leak}: {text}"
            );
        }
        let outcome = conflict_envelope().to_outcome();
        assert_eq!(outcome.code(), "conflict");
        assert!(!outcome.retryable());
        assert!(outcome.details().is_empty());
    }

    /// The gate never reports which element differed.
    #[test]
    fn the_gate_never_reports_which_element_differed() {
        let gate = production_source(GATE_SOURCE);
        for banned in [
            "TupleDimension",
            "mismatched_dimension",
            "which_element",
            "ConflictEnvelope::new",
        ] {
            assert!(
                !gate.contains(banned),
                "the gate must not carry a per-dimension cause ({banned})"
            );
        }
        assert_eq!(
            gate.matches("conflict_envelope()").count(),
            1,
            "the gate must reach the single conflict site exactly once"
        );
    }

    /// The gate answers from the existing Todo 6 store and defines no second one.
    #[test]
    fn the_gate_defines_no_second_persistence() {
        for (name, source) in [
            ("idempotency/identity.rs", IDENTITY_SOURCE),
            ("idempotency/replay.rs", REPLAY_SOURCE),
            ("idempotency/gate.rs", GATE_SOURCE),
        ] {
            let source = production_source(source);
            for banned in [
                "rusqlite",
                "Connection::open",
                "CREATE TABLE",
                "struct TransactionStore",
                "HashMap<String, RecordedMutation>",
                "BTreeMap<String, RecordedMutation>",
            ] {
                assert!(
                    !source.contains(banned),
                    "{name} must read the Todo 6 journal, never keep its own ledger ({banned})"
                );
            }
        }
        assert!(
            production_source(GATE_SOURCE).contains("transactions_for_idempotency_id"),
            "the gate must answer from the existing durable read"
        );
    }

    /// A mode may only carry its own principal shape.
    #[test]
    fn a_mode_cannot_carry_a_foreign_principal_shape() {
        let result = TransportBinding::new(
            TransportAuthMode::Stdio,
            ModePrincipal::Oidc {
                issuer: "https://issuer.invalid".to_owned(),
                subject: "subject-1".to_owned(),
            },
            ConnectionSession::new("session_0123456789abcdef").expect("session"),
        );
        assert_eq!(
            result.expect_err("a foreign principal shape is unrepresentable"),
            IdentityError::PrincipalModeMismatch { mode: "stdio" }
        );
    }

    /// The OIDC principal encoding is injective, so two different `(iss, sub)`
    /// pairs can never compare equal after a journal round trip.
    #[test]
    fn the_oidc_principal_encoding_is_injective() {
        let first = ModePrincipal::Oidc {
            issuer: "https://a".to_owned(),
            subject: "bc".to_owned(),
        };
        let second = ModePrincipal::Oidc {
            issuer: "https://ab".to_owned(),
            subject: "c".to_owned(),
        };
        assert_ne!(first.to_journal_string(), second.to_journal_string());
        for principal in [&first, &second] {
            assert_eq!(
                &ModePrincipal::from_journal_string(
                    TransportAuthMode::ProxiedOidc,
                    &principal.to_journal_string(),
                )
                .expect("the encoding round-trips"),
                principal
            );
        }
    }

    /// Two requests differing anywhere in their arguments never share a hash.
    #[test]
    fn the_request_hash_covers_every_argument() {
        let one = identity(
            "resource.update",
            "idem-1",
            &serde_json::json!({ "path": "a.tscn" }),
        );
        let two = identity(
            "resource.update",
            "idem-1",
            &serde_json::json!({ "path": "b.tscn" }),
        );
        assert_ne!(one.request_hash(), two.request_hash());
        assert!(!one.matches_exactly(&two));
        assert_eq!(one.request_hash().as_str().len(), 64);
    }

    /// A mismatch in any single dimension breaks the exact match.
    #[test]
    fn every_dimension_breaks_an_exact_match() {
        let base = identity(
            "resource.update",
            "idem-1",
            &serde_json::json!({ "path": "a.tscn" }),
        );
        assert!(base.matches_exactly(&base.clone()));
        for dimension in TupleDimension::ALL {
            let varied = base.with_dimension_changed(dimension);
            assert!(
                !base.matches_exactly(&varied),
                "a {} mismatch must break the exact match",
                dimension.as_str()
            );
        }
    }

    /// Every canonical manifest tool maps to exactly one family, editor tools
    /// included: identity must be able to name work whose plane cannot execute.
    #[test]
    fn every_canonical_tool_maps_to_exactly_one_family() {
        use godot_mcp_domain::CapabilityProfile;
        use godot_mcp_domain::manifests::canonical_manifest;

        for profile in [
            CapabilityProfile::Core,
            CapabilityProfile::Editor,
            CapabilityProfile::Runtime,
            CapabilityProfile::Full,
        ] {
            for tool in &canonical_manifest(profile).tools {
                family_of(tool).unwrap_or_else(|error| {
                    panic!(
                        "{tool} in the {} manifest has no family: {error}",
                        profile.manifest_name()
                    )
                });
            }
        }
        let script_check = family_of("script.check").expect("script.check has a family");
        assert_eq!(script_check, OperationToolFamily::DiagnosticProbe);
        assert!(
            !script_check.is_mutating(),
            "a read-only diagnostic process must not acquire mutation replay semantics"
        );
        assert_eq!(family_of("not.a.tool"), Err(IdentityError::UnknownTool));
    }

    /// The recorded-status replay rules match the bridge's: unresolved is never
    /// replayable and committed needs no replay.
    #[test]
    fn only_a_provably_unstarted_mutation_may_execute_again() {
        for status in RecordedStatus::ALL {
            let may_execute = status.may_execute_again();
            assert_eq!(
                may_execute,
                status == RecordedStatus::NotStarted,
                "{} has the wrong replay rule",
                status.as_str()
            );
            if status == RecordedStatus::Unresolved {
                assert!(status.blocks_further_mutations());
            } else {
                assert!(!status.blocks_further_mutations());
            }
        }
    }
}
