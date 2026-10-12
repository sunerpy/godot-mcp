//! Binding HTTP sessions, tasks and artifacts to the subject that authenticated.
//!
//! # The problem this layer exists for
//!
//! Todo 28 and Todo 29 establish *who* is calling: a per-launch bearer secret in
//! `trusted_local`, and an authenticated same-host proxy hop plus one frozen OIDC
//! `(iss, sub)` principal in `proxied_oidc`. Neither says anything about *what state
//! that caller may resume*. Streamable HTTP is stateful — a client keeps an
//! `Mcp-Session-Id` across requests, reconnects an SSE stream, and reads artifacts
//! by URI — and a session identifier travelling in a header is exactly the kind of
//! value that leaks: into a proxy log, into a crash report, into a shell history.
//!
//! So this layer holds one rule, and derives everything else from it:
//!
//! > **A session identifier is a name, not an authority.**
//!
//! Every request re-proves its identity from its credential ([`RequestGuard`] runs
//! the launch secret, or the hop then the token, before anything else), the result
//! is a [`BoundSubject`], and *that* is compared against the binding. A handle whose
//! subject does not match is refused exactly as if it had never existed.
//!
//! [`RequestGuard`]: crate::http::RequestGuard
//!
//! # What is bound, and to what
//!
//! | Bound thing | Bound to |
//! |---|---|
//! | MCP session | mode, mode-specific principal, resource, profile, capabilities, scopes, project object, project generation, credential expiry |
//! | Task | its session, and its tool family's mutation status |
//! | Artifact | its session, its contract's range semantics and its contract's expiry |
//!
//! The project generation is part of the subject, so a generation bump makes every
//! binding made under the old one unresumable ([`BindingRegistry::revoke_other_generations`]),
//! and an authentication or policy change calls [`BindingRegistry::revoke_all`].
//!
//! # The identifiers are minted, not derived
//!
//! [`OpaqueHandle`] is [`OPAQUE_HANDLE_BYTES`] bytes of OS entropy in the frozen
//! Todo 3 identifier shape — the same width and the same source as Todo 28's launch
//! secret and Todo 29's hop credential. The registry mints the session handle and
//! the transport advertises *that* as the wire `Mcp-Session-Id`, substituting the
//! SDK's internal identifier on the way in; the SDK's own identifier never leaves
//! the process. Comparison is constant time.
//!
//! # One refusal, for everything
//!
//! [`binding_refusal`] is the only construction site, it takes no arguments, and
//! every failure — unknown handle, foreign subject, revoked or expired binding,
//! superseded generation, unbound artifact, out-of-range read, exhausted stream
//! ceiling, replayed destructive task — produces byte-identical `404` output. The
//! endpoint is therefore not an enumeration oracle. This is Todo 27's asymmetry
//! applied to a second surface, for the same reason.
//!
//! # What this layer is not
//!
//! It does not authenticate. It does not decide capability policy (Todo 26 does, and
//! this layer only carries the capabilities a subject was admitted with). It emits no
//! audit events and enforces no rate limits — those are Todo 31. And it never opens,
//! names or returns a filesystem path: an authorized read is an
//! [`ArtifactResourceUri`](godot_mcp_domain::contracts::ArtifactResourceUri) and a
//! [`ByteRange`](godot_mcp_domain::contracts::ByteRange), neither of which can carry
//! a storage locator.

pub mod opaque;
pub mod registry;
pub mod subject;

pub use opaque::{
    EntropyUnavailable, HandleKind, OPAQUE_HANDLE_BYTES, OPAQUE_HANDLE_HEX_CHARS, OpaqueHandle,
};
pub use registry::{
    AuthorizedArtifactRead, BINDING_REFUSAL_MESSAGE, BINDING_REFUSAL_STATUS, BindingRefusal,
    BindingRegistry, SessionLease, StreamTicket, TaskBinding, binding_refusal,
};
pub use subject::{BoundSubject, SessionBindingPolicy, SubjectError};

#[cfg(test)]
mod self_audit {
    //! Source-scanning bans, asserted against this module's own bytes so a future
    //! edit that reintroduces one fails a test rather than passing review.

    const OPAQUE_SOURCE: &str = include_str!("binding/opaque.rs");
    const REGISTRY_SOURCE: &str = include_str!("binding/registry.rs");
    const SUBJECT_SOURCE: &str = include_str!("binding/subject.rs");
    const TRANSPORT_SOURCE: &str = include_str!("http.rs");
    const STAGED_SOURCE: &str = include_str!("http/staged.rs");

    fn production_source(source: &str) -> &str {
        source
            .find("#[cfg(test)]")
            .map_or(source, |index| &source[..index])
    }

    fn all_sources() -> [(&'static str, &'static str); 3] {
        [
            ("binding/opaque.rs", OPAQUE_SOURCE),
            ("binding/registry.rs", REGISTRY_SOURCE),
            ("binding/subject.rs", SUBJECT_SOURCE),
        ]
    }

    /// A handle's entropy comes from the OS CSPRNG, and from nowhere else.
    #[test]
    fn handles_are_minted_only_from_os_entropy() {
        let opaque = production_source(OPAQUE_SOURCE);
        assert_eq!(
            opaque.matches("getrandom::fill").count(),
            1,
            "there must be exactly one entropy site"
        );
        for banned in [
            "SystemTime",
            "elapsed",
            "AtomicUsize",
            "fetch_add",
            "Sha256",
            "hasher",
            "wrapping_mul",
            "seed",
        ] {
            assert!(
                !opaque.contains(banned),
                "a handle must not be derived from {banned}"
            );
        }
        for (name, source) in all_sources() {
            if name == "binding/opaque.rs" {
                continue;
            }
            assert!(
                !production_source(source).contains("getrandom"),
                "{name} must mint through OpaqueHandle, never reach for entropy itself"
            );
        }
    }

    /// Nothing in this layer treats a session identifier as a credential.
    #[test]
    fn a_session_identifier_is_never_accepted_as_authentication() {
        let registry = production_source(REGISTRY_SOURCE);
        // Every request-path entry point takes the independently resolved subject.
        for entry in [
            "pub fn resume(",
            "pub fn open_stream(",
            "pub fn revoke(",
            "pub fn bind_task(",
            "pub fn mark_task_dispatched(",
            "pub fn bind_artifact(",
            "pub fn authorize_artifact_read(",
        ] {
            let signature = registry
                .split_once(entry)
                .unwrap_or_else(|| panic!("{entry} exists"))
                .1
                .split_once(')')
                .expect("the signature closes")
                .0;
            assert!(
                signature.contains("subject: Option<&BoundSubject>"),
                "{entry} must require the subject the request authenticated to"
            );
        }
        // And every one of them reaches the same authorization before doing work.
        for gated in [
            "pub fn open_stream(",
            "pub fn revoke(",
            "pub fn bind_task(",
            "pub fn mark_task_dispatched(",
            "pub fn bind_artifact(",
            "pub fn authorize_artifact_read(",
        ] {
            let body = registry
                .split_once(gated)
                .expect("the entry point exists")
                .1
                .split_once(" {\n")
                .expect("the body opens")
                .1;
            assert!(
                body.trim_start().starts_with("let lease = self.resume("),
                "{gated} must authorize the session before anything else"
            );
        }
        for banned in [
            "if !candidate.is_empty()",
            "trusted_session",
            "session_is_authentication",
        ] {
            assert!(
                !registry.contains(banned),
                "a session identifier must never stand in for a credential ({banned})"
            );
        }
    }

    /// Neither a loopback peer nor a forwarded header is consulted here.
    #[test]
    fn neither_a_peer_address_nor_a_forwarded_header_is_an_identity_here() {
        for (name, source) in all_sources() {
            let production = production_source(source);
            for banned in [
                "is_loopback",
                "peer_addr",
                "ConnectInfo",
                "x-forwarded",
                "X-Forwarded",
                "forwarded",
                "x-real-ip",
            ] {
                assert!(
                    !production.contains(banned),
                    "{name} must not consult {banned}"
                );
            }
        }
    }

    /// A binding never carries, opens or returns a filesystem path.
    #[test]
    fn no_raw_path_can_travel_through_a_binding() {
        for (name, source) in all_sources() {
            let production = production_source(source);
            for banned in [
                "std::fs::",
                "File::open",
                "File::create",
                "Path::new",
                "PathBuf",
                "canonicalize",
                "display()",
            ] {
                assert!(
                    !production.contains(banned),
                    "{name} must not be able to name a storage location ({banned})"
                );
            }
        }
        let registry = production_source(REGISTRY_SOURCE);
        let authorized = registry
            .split_once("pub struct AuthorizedArtifactRead {")
            .expect("the authorized-read type exists")
            .1
            .split_once('}')
            .expect("the type closes")
            .0;
        for field in ["path", "location", "file", "dir"] {
            assert!(
                !authorized.contains(field),
                "an authorized read must not carry a {field}"
            );
        }
    }

    /// There is exactly one refusal construction site, and it takes no arguments.
    #[test]
    fn the_binding_refusal_has_exactly_one_argument_free_construction_site() {
        let registry = production_source(REGISTRY_SOURCE);
        assert_eq!(
            registry.matches("BindingRefusal { seal: () }").count(),
            1,
            "exactly one binding refusal may ever be constructed"
        );
        assert!(
            registry.contains("pub fn binding_refusal() -> BindingRefusal"),
            "the sole construction site must take no arguments, so no cause can leak"
        );
        assert_eq!(
            registry.matches("binding_refusal()").count(),
            2,
            "the registry must reach the single refusal site exactly once, through refuse()"
        );
        assert_eq!(
            registry.matches("fn refuse(&self)").count(),
            1,
            "every refusal must funnel through one private helper"
        );
        for (name, source) in all_sources() {
            if name == "binding/registry.rs" {
                continue;
            }
            assert!(
                !production_source(source).contains("BindingRefusal {"),
                "{name} must not construct a refusal"
            );
        }
    }

    /// The transport authorizes the session before the MCP service can dispatch.
    #[test]
    fn the_transport_binds_after_authentication_and_before_dispatch() {
        let bound = production_source(TRANSPORT_SOURCE)
            .split_once("async fn serve_bound(")
            .expect("the bound serve path exists")
            .1;
        let authorize = bound
            .find("guard.authorize(")
            .expect("the guard authorizes every request");
        let bind = bound
            .find("bindings.resume(")
            .expect("a presented session identifier is re-authorized");
        let run = bound
            .find("next.run(request)")
            .expect("the request continues to the MCP service");
        assert!(
            authorize < bind,
            "the credential must be validated before a binding is consulted"
        );
        assert!(
            bind < run,
            "a binding must be authorized before the request reaches the MCP service"
        );
        let staged = production_source(STAGED_SOURCE);
        for required in [
            "fn bound_subject",
            "authorize_artifact_read",
            "self.record_dispatch()",
        ] {
            assert!(
                staged.contains(required),
                "the handler shell must {required} so a read is authorized per read"
            );
        }
        let read = staged
            .split_once("async fn read_resource(")
            .expect("the read entry point exists")
            .1
            .split_once("\n    }")
            .expect("it closes")
            .0;
        let authorize = read
            .find("authorize_artifact_read")
            .expect("each read is authorized");
        let dispatch = read
            .find("self.record_dispatch()")
            .expect("a dispatch is recorded");
        assert!(
            authorize < dispatch,
            "an artifact read must be authorized before it counts as a dispatch"
        );
    }
}
