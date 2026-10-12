//! What an authenticated request resolved to, and the policy it had to satisfy.
//!
//! # A subject, not a session
//!
//! [`BoundSubject`] is the *whole* identity a request proved: the transport mode,
//! that mode's own principal shape, the resource the credential was scoped to, the
//! served capability profile, the capabilities that profile grants, the scopes the
//! credential carried, and the project object plus its generation. It is derived
//! from the credential on every request and never read back off a session, which is
//! the property that makes a session identifier useless as authentication.
//!
//! Two subjects are equal only when *every* one of those elements matches, so a
//! changed audience, a changed profile, a changed capability set, a rotated
//! principal or a bumped project generation all produce a different subject — and a
//! binding made under the old one stops resolving.
//!
//! # A mode may only carry its own principal shape
//!
//! The rule is Todo 27's, not a second copy of it:
//! [`ModePrincipal::required_mode`] decides which mode a principal shape belongs
//! to, and [`SessionBindingPolicy::admit`] refuses a pairing it rejects. A
//! `trusted_local` launch-secret principal therefore cannot be presented as an
//! OIDC subject, and vice versa, without inventing a parallel rule that could drift.
//!
//! # Capabilities come from the frozen manifest
//!
//! A policy names the capabilities a deployment requires, and admission checks them
//! against `canonical_manifest(profile).capabilities` — the Todo 3 manifest. A
//! policy that asks for a capability the served profile does not grant is a
//! configuration error caught when the policy is built, not a request-time surprise.
//!
//! # Both bounds come from configuration, not from a number chosen here
//!
//! The concurrent-stream ceiling is `Limits::max_queue`, the operator's
//! already-validated non-zero bound on concurrently outstanding work. The binding
//! lifetime defaults to `Limits::deadline_ms * Limits::max_queue` — the longest a
//! full queue of operations, each running to its configured per-operation deadline,
//! could legitimately keep one session busy. Both factors are validated by the
//! Todo 2 loader; neither is invented here.
//!
//! A deployment whose credentials expire sooner supplies the shorter value with
//! [`SessionBindingPolicy::with_binding_lifetime_ms`], because a binding must never
//! outlive the credential behind it. It may not exceed the derived default: the
//! ceiling is configuration, not a caller's preference.

use std::collections::BTreeSet;

use godot_mcp_domain::CapabilityProfile;
use godot_mcp_domain::config::Limits;
use godot_mcp_domain::contracts::{Capability, ProjectIdentity, ProjectObjectId};
use godot_mcp_domain::manifests::canonical_manifest;

use crate::idempotency::{ModePrincipal, TransportAuthMode};

/// Why a policy could not be built, or a request could not be admitted.
///
/// These are *internal* diagnostics. Nothing here reaches a caller: a refused
/// request receives the single non-enumerating refusal from
/// [`registry`](super::registry), or the frozen Todo 28/29 authentication challenge.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SubjectError {
    /// The resource the credential is scoped to is empty.
    #[error("a bound subject must name the resource its credential was scoped to")]
    MissingResource,
    /// The principal shape does not belong to the presented transport mode.
    #[error("a {mode} request may not carry that principal shape")]
    PrincipalModeMismatch {
        /// The mode that refused it.
        mode: &'static str,
    },
    /// The policy requires a capability the served profile does not grant.
    #[error("the {profile} profile does not grant every required capability")]
    CapabilityNotInProfile {
        /// The profile that refused it.
        profile: &'static str,
    },
    /// The credential did not carry every required scope.
    #[error("the credential does not carry every required scope")]
    InsufficientScope,
    /// The presented project generation is not the one this policy serves.
    #[error("the presented project generation is not the served generation")]
    ProjectGenerationMismatch,
}

/// The deployment policy every request must satisfy before anything is bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionBindingPolicy {
    profile: CapabilityProfile,
    resource: String,
    required_scopes: BTreeSet<String>,
    required_capabilities: BTreeSet<Capability>,
    project: ProjectIdentity,
    max_concurrent_streams: usize,
    binding_lifetime_ms: u64,
}

impl SessionBindingPolicy {
    /// Build a policy for one served profile, resource and project.
    ///
    /// `resource` is the value the presented credential is scoped to: the advertised
    /// external resource URL in `proxied_oidc`, and the single local MCP endpoint in
    /// `trusted_local`. It is part of the subject, so changing it invalidates every
    /// binding made under the old one.
    ///
    /// The concurrent-stream ceiling is `limits.max_queue` and the binding lifetime
    /// is `limits.deadline_ms * limits.max_queue`: both are derived from the
    /// validated configuration rather than chosen here.
    ///
    /// # Errors
    /// Returns [`SubjectError::MissingResource`] when `resource` is empty.
    pub fn new(
        profile: CapabilityProfile,
        resource: &str,
        project: ProjectIdentity,
        limits: &Limits,
    ) -> Result<Self, SubjectError> {
        if resource.trim().is_empty() {
            return Err(SubjectError::MissingResource);
        }
        Ok(Self {
            profile,
            resource: resource.to_owned(),
            required_scopes: BTreeSet::new(),
            required_capabilities: BTreeSet::new(),
            project,
            max_concurrent_streams: usize::try_from(limits.max_queue).unwrap_or(usize::MAX),
            binding_lifetime_ms: limits
                .deadline_ms
                .saturating_mul(u64::from(limits.max_queue)),
        })
    }

    /// Shorten the binding lifetime to `milliseconds`.
    ///
    /// Clamped to the configuration-derived default, and never zero: a binding that
    /// expired the instant it was made would make the mode unusable while looking
    /// like an authentication failure.
    #[must_use]
    pub const fn with_binding_lifetime_ms(mut self, milliseconds: u64) -> Self {
        if milliseconds > 0 && milliseconds < self.binding_lifetime_ms {
            self.binding_lifetime_ms = milliseconds;
        }
        self
    }

    /// Require these scopes on every credential.
    #[must_use]
    pub fn with_required_scopes<S: Into<String>>(
        mut self,
        scopes: impl IntoIterator<Item = S>,
    ) -> Self {
        self.required_scopes = scopes.into_iter().map(Into::into).collect();
        self
    }

    /// Require these capabilities, which the served profile must already grant.
    ///
    /// # Errors
    /// Returns [`SubjectError::CapabilityNotInProfile`] when the frozen manifest for
    /// the served profile does not grant one of them.
    pub fn with_required_capabilities(
        mut self,
        capabilities: impl IntoIterator<Item = Capability>,
    ) -> Result<Self, SubjectError> {
        let requested: BTreeSet<Capability> = capabilities.into_iter().collect();
        let granted = canonical_manifest(self.profile).capabilities;
        if !requested.is_subset(&granted) {
            return Err(SubjectError::CapabilityNotInProfile {
                profile: self.profile.manifest_name(),
            });
        }
        self.required_capabilities = requested;
        Ok(self)
    }

    /// The served capability profile.
    #[must_use]
    pub const fn profile(&self) -> CapabilityProfile {
        self.profile
    }

    /// The resource a credential must be scoped to.
    #[must_use]
    pub fn resource(&self) -> &str {
        &self.resource
    }

    /// The scopes every credential must carry.
    #[must_use]
    pub const fn required_scopes(&self) -> &BTreeSet<String> {
        &self.required_scopes
    }

    /// The capabilities every admitted subject holds.
    #[must_use]
    pub const fn required_capabilities(&self) -> &BTreeSet<Capability> {
        &self.required_capabilities
    }

    /// The project object and generation this policy serves.
    #[must_use]
    pub const fn project(&self) -> &ProjectIdentity {
        &self.project
    }

    /// The ceiling on concurrently open streams per session binding.
    #[must_use]
    pub const fn max_concurrent_streams(&self) -> usize {
        self.max_concurrent_streams
    }

    /// How long a binding may live, in milliseconds.
    #[must_use]
    pub const fn binding_lifetime_ms(&self) -> u64 {
        self.binding_lifetime_ms
    }

    /// When a binding opened at `now_unix_ms` expires.
    #[must_use]
    pub const fn expires_at(&self, now_unix_ms: u64) -> u64 {
        now_unix_ms.saturating_add(self.binding_lifetime_ms)
    }

    /// Admit one authenticated request as a bound subject.
    ///
    /// # Errors
    /// Returns the [`SubjectError`] that stopped it: a principal shape foreign to
    /// the mode, or a credential missing a required scope.
    pub fn admit(
        &self,
        mode: TransportAuthMode,
        principal: ModePrincipal,
        scopes: BTreeSet<String>,
    ) -> Result<BoundSubject, SubjectError> {
        if principal.required_mode() != mode {
            return Err(SubjectError::PrincipalModeMismatch {
                mode: mode.as_str(),
            });
        }
        if !self.required_scopes.is_subset(&scopes) {
            return Err(SubjectError::InsufficientScope);
        }
        Ok(BoundSubject {
            mode,
            principal,
            resource: self.resource.clone(),
            profile: self.profile,
            capabilities: self.required_capabilities.clone(),
            scopes,
            project_object: self.project.object_id().clone(),
            project_generation: self.project.generation(),
        })
    }
}

/// One request's complete, independently re-derived identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundSubject {
    mode: TransportAuthMode,
    principal: ModePrincipal,
    resource: String,
    profile: CapabilityProfile,
    capabilities: BTreeSet<Capability>,
    scopes: BTreeSet<String>,
    project_object: ProjectObjectId,
    project_generation: u64,
}

impl BoundSubject {
    /// The transport mode that authenticated this request.
    #[must_use]
    pub const fn mode(&self) -> TransportAuthMode {
        self.mode
    }

    /// The mode-specific principal.
    #[must_use]
    pub const fn principal(&self) -> &ModePrincipal {
        &self.principal
    }

    /// The resource the credential was scoped to.
    #[must_use]
    pub fn resource(&self) -> &str {
        &self.resource
    }

    /// The served capability profile.
    #[must_use]
    pub const fn profile(&self) -> CapabilityProfile {
        self.profile
    }

    /// The capabilities this subject holds.
    #[must_use]
    pub const fn capabilities(&self) -> &BTreeSet<Capability> {
        &self.capabilities
    }

    /// The scopes the credential carried.
    #[must_use]
    pub const fn scopes(&self) -> &BTreeSet<String> {
        &self.scopes
    }

    /// The project object this subject is scoped to.
    #[must_use]
    pub const fn project_object(&self) -> &ProjectObjectId {
        &self.project_object
    }

    /// The project generation this subject is scoped to.
    #[must_use]
    pub const fn project_generation(&self) -> u64 {
        self.project_generation
    }

    /// Whether every element of the two subjects agrees.
    ///
    /// This is the only comparison the registry performs, and it is deliberately
    /// total: a partial match is a mismatch, so there is no dimension a caller can
    /// vary and still resolve someone else's binding.
    #[must_use]
    pub fn matches_exactly(&self, other: &Self) -> bool {
        self == other
    }

    /// Whether this subject was admitted for `project`, generation included.
    #[must_use]
    pub fn matches_project(&self, project: &ProjectIdentity) -> bool {
        self.project_object == *project.object_id()
            && self.project_generation == project.generation()
    }
}

#[cfg(test)]
mod tests {
    use godot_mcp_domain::objid::{LeaseKey, StableObjectId};

    use super::*;

    fn project(generation: u64) -> ProjectIdentity {
        ProjectIdentity::from_lease_key(
            LeaseKey {
                root: StableObjectId::Unix { dev: 30, ino: 1 },
                project_file: StableObjectId::Unix { dev: 30, ino: 2 },
            },
            generation,
        )
    }

    fn policy() -> SessionBindingPolicy {
        SessionBindingPolicy::new(
            CapabilityProfile::Full,
            "https://mcp.example.com/mcp",
            project(4),
            &Limits::default(),
        )
        .expect("a non-empty resource")
    }

    fn oidc() -> ModePrincipal {
        ModePrincipal::Oidc {
            issuer: "https://issuer.example".to_owned(),
            subject: "subject-1".to_owned(),
        }
    }

    fn launch() -> ModePrincipal {
        ModePrincipal::LaunchSecret {
            principal: format!("principal_{}", "a".repeat(32)),
        }
    }

    fn scopes(values: &[&str]) -> BTreeSet<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn a_policy_requires_a_resource_and_derives_its_stream_ceiling_from_limits() {
        assert_eq!(
            SessionBindingPolicy::new(
                CapabilityProfile::Core,
                "   ",
                project(1),
                &Limits::default()
            )
            .expect_err("an empty resource is refused"),
            SubjectError::MissingResource
        );
        let limits = Limits {
            max_queue: 9,
            ..Limits::default()
        };
        let policy =
            SessionBindingPolicy::new(CapabilityProfile::Core, "local", project(1), &limits)
                .expect("policy");
        assert_eq!(policy.max_concurrent_streams(), 9);
        assert_eq!(
            policy.max_concurrent_streams(),
            usize::try_from(limits.max_queue).expect("a configured bound fits usize"),
            "the ceiling must come from configuration, not from a number chosen here"
        );
        assert_eq!(
            policy.binding_lifetime_ms(),
            limits.deadline_ms * u64::from(limits.max_queue),
            "the lifetime must be derived from configuration too"
        );
        assert_eq!(
            policy.expires_at(1_000),
            1_000 + policy.binding_lifetime_ms()
        );
        let shortened = policy.clone().with_binding_lifetime_ms(5);
        assert_eq!(shortened.binding_lifetime_ms(), 5);
        assert_eq!(
            policy
                .clone()
                .with_binding_lifetime_ms(u64::MAX)
                .binding_lifetime_ms(),
            policy.binding_lifetime_ms(),
            "a caller may shorten a lifetime but never extend it past configuration"
        );
        assert_eq!(
            policy
                .clone()
                .with_binding_lifetime_ms(0)
                .binding_lifetime_ms(),
            policy.binding_lifetime_ms(),
            "zero is not a lifetime"
        );
    }

    #[test]
    fn a_mode_may_only_carry_its_own_principal_shape() {
        let policy = policy();
        assert_eq!(
            policy
                .admit(TransportAuthMode::TrustedLocal, oidc(), BTreeSet::new())
                .expect_err("an OIDC principal is not a launch secret"),
            SubjectError::PrincipalModeMismatch {
                mode: "trusted_local"
            }
        );
        assert_eq!(
            policy
                .admit(TransportAuthMode::ProxiedOidc, launch(), BTreeSet::new())
                .expect_err("a launch secret is not an OIDC subject"),
            SubjectError::PrincipalModeMismatch {
                mode: "proxied_oidc"
            }
        );
        assert!(
            policy
                .admit(TransportAuthMode::TrustedLocal, launch(), BTreeSet::new())
                .is_ok()
        );
        assert!(
            policy
                .admit(TransportAuthMode::ProxiedOidc, oidc(), BTreeSet::new())
                .is_ok()
        );
    }

    #[test]
    fn a_missing_required_scope_is_never_admitted() {
        let policy = policy().with_required_scopes(["godot.read", "godot.analyze"]);
        assert_eq!(
            policy
                .admit(
                    TransportAuthMode::ProxiedOidc,
                    oidc(),
                    scopes(&["godot.read"])
                )
                .expect_err("a subset of the required scopes is refused"),
            SubjectError::InsufficientScope
        );
        let admitted = policy
            .admit(
                TransportAuthMode::ProxiedOidc,
                oidc(),
                scopes(&["godot.read", "godot.analyze", "extra"]),
            )
            .expect("a superset satisfies the requirement");
        assert_eq!(admitted.scopes().len(), 3);
    }

    #[test]
    fn a_capability_outside_the_served_profile_is_a_configuration_error() {
        let refused = SessionBindingPolicy::new(
            CapabilityProfile::Core,
            "local",
            project(1),
            &Limits::default(),
        )
        .expect("policy")
        .with_required_capabilities([Capability::Read, Capability::EditorMutate])
        .expect_err("core does not grant editor mutation");
        assert_eq!(
            refused,
            SubjectError::CapabilityNotInProfile { profile: "core" }
        );
        let accepted = SessionBindingPolicy::new(
            CapabilityProfile::Core,
            "local",
            project(1),
            &Limits::default(),
        )
        .expect("policy")
        .with_required_capabilities([Capability::Read])
        .expect("core grants read");
        assert_eq!(accepted.required_capabilities().len(), 1);
    }

    #[test]
    fn every_element_of_a_subject_participates_in_its_identity() {
        let base = policy()
            .admit(
                TransportAuthMode::ProxiedOidc,
                oidc(),
                scopes(&["godot.read"]),
            )
            .expect("admitted");
        assert!(base.matches_exactly(&base.clone()));

        let other_resource = SessionBindingPolicy::new(
            CapabilityProfile::Full,
            "https://other.example/mcp",
            project(4),
            &Limits::default(),
        )
        .expect("policy")
        .admit(
            TransportAuthMode::ProxiedOidc,
            oidc(),
            scopes(&["godot.read"]),
        )
        .expect("admitted");
        assert!(!base.matches_exactly(&other_resource), "audience matters");

        let other_profile = SessionBindingPolicy::new(
            CapabilityProfile::Core,
            "https://mcp.example.com/mcp",
            project(4),
            &Limits::default(),
        )
        .expect("policy")
        .admit(
            TransportAuthMode::ProxiedOidc,
            oidc(),
            scopes(&["godot.read"]),
        )
        .expect("admitted");
        assert!(!base.matches_exactly(&other_profile), "profile matters");

        let other_capabilities = policy()
            .with_required_capabilities([Capability::Read])
            .expect("full grants read")
            .admit(
                TransportAuthMode::ProxiedOidc,
                oidc(),
                scopes(&["godot.read"]),
            )
            .expect("admitted");
        assert!(
            !base.matches_exactly(&other_capabilities),
            "capabilities matter"
        );

        let other_generation = SessionBindingPolicy::new(
            CapabilityProfile::Full,
            "https://mcp.example.com/mcp",
            project(5),
            &Limits::default(),
        )
        .expect("policy")
        .admit(
            TransportAuthMode::ProxiedOidc,
            oidc(),
            scopes(&["godot.read"]),
        )
        .expect("admitted");
        assert!(
            !base.matches_exactly(&other_generation),
            "project generation matters"
        );
        assert!(base.matches_project(&project(4)));
        assert!(!base.matches_project(&project(5)));

        let other_subject = policy()
            .admit(
                TransportAuthMode::ProxiedOidc,
                ModePrincipal::Oidc {
                    issuer: "https://issuer.example".to_owned(),
                    subject: "subject-2".to_owned(),
                },
                scopes(&["godot.read"]),
            )
            .expect("admitted");
        assert!(!base.matches_exactly(&other_subject), "the subject matters");

        let other_scopes = policy()
            .admit(
                TransportAuthMode::ProxiedOidc,
                oidc(),
                scopes(&["godot.read", "godot.analyze"]),
            )
            .expect("admitted");
        assert!(!base.matches_exactly(&other_scopes), "scopes matter");
    }
}
