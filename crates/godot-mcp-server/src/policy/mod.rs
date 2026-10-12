//! Capability policy: the canonical matrix, the decision surface, and the single
//! authority that mints unforgeable dispatch permits.
//!
//! Nothing in this module is `pub`. The matrix rows, the policy authority and the
//! permit wrapper are all crate-private, and the wrapper holds Todo 6's sealed
//! [`EffectPermit`](crate::effect::EffectPermit) — a type whose only constructor
//! lives inside the permit module and whose seal field is private there. An
//! adapter, a plugin or any external crate therefore cannot construct a permit,
//! cannot name a boundary and cannot bypass this layer.
//!
//! ## Editor plane in this release
//!
//! The canonical matrix maps the editor plane completely: every editor-profile
//! read and every `EditorUndoRedoManager` mutation has a row naming its required
//! capability and its boundary. The editor bridge itself is not shipped in this
//! release, so [`authority::BoundaryAvailability`] reports `editor = false` and every editor
//! row is refused with `unavailable` at evaluation time, before any queueing.
//! The rows are deliberately *not* removed: the frozen Todo 3 manifests still
//! advertise those tools, and a policy layer that silently dropped them would be
//! lying about what the profile means.

// Todo 26 *freezes* this layer. The production probe over Todo 17's launch
// journal and the file-only lane marker are both exercised by this module's own
// unit tests, and the acceptance suite drives every matrix row and every gate
// through the test-hooks harness; their transport call sites arrive with Todos
// 27-31. The relaxation is scoped to this module and carries its reason rather
// than being a blanket crate-wide allow.
#![allow(
    dead_code,
    reason = "Todo 26 freezes the capability policy; its transport call sites land in Todos 27-31, \
              and the unit tests plus the test-hooks acceptance suite exercise every row and gate \
              today"
)]

pub(crate) mod authority;
pub(crate) mod decision;
pub(crate) mod matrix;

// Todo 32's outcome router is the production call site: it evaluates every
// dispatch here before anything is queued, so these are no longer test-face only.
pub(crate) use authority::{
    BoundaryAvailability, DispatchRequest, NoLaunchPlane, PolicyAuthority, PolicyPermit,
    PolicyRefusal, SurvivorProbe,
};
pub(crate) use decision::BoundIdentity;
pub(crate) use matrix::row_for;

#[cfg(any(test, feature = "test-hooks"))]
pub(crate) use authority::PolicyDispatchError;
#[cfg(any(test, feature = "test-hooks"))]
pub(crate) use matrix::all_rows;

#[cfg(test)]
mod self_audit {
    //! Source-scanning self-audit: the bans this layer must keep are asserted
    //! against this module's own bytes, so a future edit that reintroduces one
    //! fails a test rather than passing review.

    const AUTHORITY_SOURCE: &str = include_str!("authority.rs");
    const MATRIX_SOURCE: &str = include_str!("matrix.rs");
    const DECISION_SOURCE: &str = include_str!("decision.rs");

    /// There is exactly one permit-minting call site in the whole policy layer.
    #[test]
    fn the_policy_layer_mints_permits_at_exactly_one_call_site() {
        let mint_sites = AUTHORITY_SOURCE.matches(".permits").count();
        assert_eq!(
            mint_sites, 1,
            "the policy layer must reach the permit authority exactly once; found {mint_sites}"
        );
        assert_eq!(
            AUTHORITY_SOURCE.matches("PermitRequest {").count(),
            1,
            "exactly one PermitRequest may be constructed in the policy layer"
        );
        for source in [MATRIX_SOURCE, DECISION_SOURCE] {
            assert!(
                !source.contains("PermitRequest"),
                "only the authority module may construct a permit request"
            );
        }
    }

    /// The policy layer defines no second gateway and no second scheduler.
    #[test]
    fn the_policy_layer_defines_no_second_gateway_or_scheduler() {
        for source in [AUTHORITY_SOURCE, MATRIX_SOURCE, DECISION_SOURCE] {
            assert!(
                !source.contains("struct EffectGateway"),
                "the policy layer must reuse the Todo 6 gateway, never define another"
            );
            assert!(
                !source.contains("struct Scheduler") && !source.contains("enum Scheduler"),
                "the policy layer must reuse the Todo 5 scheduler, never define another"
            );
        }
    }

    /// No path is ever re-opened from a string inside the policy layer.
    #[test]
    fn the_policy_layer_never_reopens_a_raw_path() {
        for (name, source) in [
            ("authority.rs", AUTHORITY_SOURCE),
            ("matrix.rs", MATRIX_SOURCE),
            ("decision.rs", DECISION_SOURCE),
        ] {
            for banned in [
                "std::fs::",
                "File::open",
                "File::create",
                "Path::new",
                "PathBuf::from",
                "canonicalize",
            ] {
                assert!(
                    !source.contains(banned),
                    "{name} must not reach the filesystem or re-open a raw path ({banned})"
                );
            }
        }
    }

    /// The layer verifies its permits; it never accepts a vague signed context.
    #[test]
    fn the_policy_layer_verifies_rather_than_trusting_a_signed_context() {
        assert!(
            AUTHORITY_SOURCE.contains("fn enforce_at_boundary"),
            "the final-boundary re-check must exist"
        );
        for banned in ["hmac", "Hmac", "signature", "bearer", "Bearer"] {
            assert!(
                !AUTHORITY_SOURCE.contains(banned),
                "the policy layer must not carry an unverifiable signed context ({banned})"
            );
        }
    }

    /// A profile name is never treated as a network identity.
    #[test]
    fn a_profile_name_is_never_an_identity() {
        assert!(
            !AUTHORITY_SOURCE.contains("profile.manifest_name() =="),
            "a profile name must never be compared as an identity"
        );
        assert!(
            AUTHORITY_SOURCE
                .contains("request.principal.as_ref() != self.identity.principal.as_ref()"),
            "identity must be compared against the bound principal"
        );
        assert!(
            AUTHORITY_SOURCE.contains("request.session.as_ref() != self.identity.session.as_ref()"),
            "the session must be compared against the bound session"
        );
    }
}
