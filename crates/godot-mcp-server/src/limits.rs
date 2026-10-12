//! Enforced operational limits: per-process, per-request, concurrency, rate and
//! artifact retention/size bounds.
//!
//! ## Every limit is refused, never inferred
//!
//! A zero bound is a configuration error rather than "unlimited", so a limit can
//! never be disabled by omission. The defaults are derived from Todo 2's already
//! validated [`Limits`](godot_mcp_domain::config::Limits) where an equivalent
//! bound exists, so this layer enforces the configured contract instead of
//! inventing a second one.
//!
//! ## No billing or tenant inference
//!
//! Every bound here is a *resource* bound on this single process and its single
//! held project. There is deliberately no notion of an account, a plan, a tenant
//! or a quota period: nothing in this module can be read as a commercial limit.
//!
//! ## Refusal happens before dispatch
//!
//! Every check returns a [`LimitRejection`] carrying a stable
//! [`ErrorKind`](godot_mcp_domain::contracts::ErrorKind), an HTTP status for the
//! optional HTTP transport, and whether a retry may succeed. Callers run the
//! checks *before* they reach a boundary, so a refused request touches nothing.
//! Concurrency and artifact-store budgets are released by RAII guards, which is
//! what makes recovery automatic rather than a cleanup step someone may forget.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use godot_mcp_domain::contracts::{ErrorKind, OutcomeError};

use crate::observability::SafeField;

/// Which bound refused a request. The name is a stable, payload-free label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LimitKind {
    /// Total requests this process will serve.
    ProcessRequests,
    /// Accepted request-body bytes.
    RequestBytes,
    /// Produced response bytes.
    ResponseBytes,
    /// Requested artifact range bytes.
    RangeBytes,
    /// Concurrent in-flight requests for one subject.
    SubjectConcurrency,
    /// Concurrent in-flight requests across every subject.
    TotalConcurrency,
    /// Requests one subject may start inside the rate window.
    RequestRate,
    /// Total retained artifact bytes.
    ArtifactStoreBytes,
    /// How long an artifact remains readable.
    ArtifactRetention,
}

impl LimitKind {
    /// Stable snake_case label for diagnostics and audit metadata.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ProcessRequests => "process_requests",
            Self::RequestBytes => "request_bytes",
            Self::ResponseBytes => "response_bytes",
            Self::RangeBytes => "range_bytes",
            Self::SubjectConcurrency => "subject_concurrency",
            Self::TotalConcurrency => "total_concurrency",
            Self::RequestRate => "request_rate",
            Self::ArtifactStoreBytes => "artifact_store_bytes",
            Self::ArtifactRetention => "artifact_retention",
        }
    }

    const fn error_kind(self) -> ErrorKind {
        match self {
            Self::RequestBytes | Self::ResponseBytes | Self::RangeBytes => ErrorKind::InvalidInput,
            Self::ArtifactRetention => ErrorKind::NotFound,
            Self::ProcessRequests
            | Self::SubjectConcurrency
            | Self::TotalConcurrency
            | Self::RequestRate
            | Self::ArtifactStoreBytes => ErrorKind::Unavailable,
        }
    }

    const fn http_status(self) -> u16 {
        match self {
            Self::RequestBytes | Self::ResponseBytes => 413,
            Self::RangeBytes => 416,
            Self::ArtifactRetention => 404,
            Self::SubjectConcurrency | Self::TotalConcurrency | Self::RequestRate => 429,
            Self::ArtifactStoreBytes => 507,
            Self::ProcessRequests => 503,
        }
    }

    const fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::SubjectConcurrency
                | Self::TotalConcurrency
                | Self::RequestRate
                | Self::ArtifactStoreBytes
                | Self::ProcessRequests
        )
    }
}

/// One refused request, with its stable class and its recovery advice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("operational limit {} was exceeded", self.kind.label())]
pub struct LimitRejection {
    kind: LimitKind,
}

impl LimitRejection {
    const fn of(kind: LimitKind) -> Self {
        Self { kind }
    }

    /// Which bound refused the request.
    #[must_use]
    pub const fn kind(&self) -> LimitKind {
        self.kind
    }

    /// Stable domain error class.
    #[must_use]
    pub const fn error_kind(&self) -> ErrorKind {
        self.kind.error_kind()
    }

    /// HTTP status the optional HTTP transport returns.
    #[must_use]
    pub const fn http_status(&self) -> u16 {
        self.kind.http_status()
    }

    /// Whether retrying the same request may succeed.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        self.kind.is_retryable()
    }

    /// The caller-safe outcome error. It names the bound, never the payload.
    #[must_use]
    pub fn to_outcome(&self) -> OutcomeError {
        OutcomeError::new(
            self.error_kind(),
            self.to_string(),
            self.is_retryable(),
            BTreeMap::new(),
        )
    }
}

/// A bound that was configured as zero, which would mean "unlimited".
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("operational limit {} must be greater than zero", self.field)]
pub struct LimitConfigError {
    field: &'static str,
}

impl LimitConfigError {
    /// Which configured field was zero.
    #[must_use]
    pub const fn field(&self) -> &'static str {
        self.field
    }
}

/// Every enforced bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitPolicy {
    /// Total requests this process will serve before refusing further work.
    pub max_process_requests: u64,
    /// Maximum accepted request-body bytes.
    pub max_body_bytes: u64,
    /// Maximum produced response bytes.
    pub max_response_bytes: u64,
    /// Maximum artifact range bytes one read may request.
    pub max_range_bytes: u64,
    /// Maximum concurrent in-flight requests for one subject.
    pub max_subject_concurrency: u32,
    /// Maximum concurrent in-flight requests across every subject.
    pub max_total_concurrency: u32,
    /// Requests one subject may start inside [`Self::rate_window_ms`].
    pub requests_per_window: u32,
    /// Width of the sliding rate window.
    pub rate_window_ms: u64,
    /// How long an artifact remains readable after it was created.
    pub artifact_retention_ms: u64,
    /// Maximum total retained artifact bytes.
    pub max_artifact_total_bytes: u64,
}

impl Default for LimitPolicy {
    fn default() -> Self {
        let configured = godot_mcp_domain::config::Limits::default();
        Self {
            max_process_requests: 1_000_000,
            max_body_bytes: configured.max_body_bytes,
            max_response_bytes: configured.max_output_bytes,
            max_range_bytes: configured.max_output_bytes,
            max_subject_concurrency: 8,
            max_total_concurrency: configured.max_queue,
            requests_per_window: 256,
            rate_window_ms: 60_000,
            artifact_retention_ms: 15 * 60 * 1_000,
            max_artifact_total_bytes: 256 * 1024 * 1024,
        }
    }
}

impl LimitPolicy {
    pub(crate) fn from_config(config: &godot_mcp_domain::config::Limits) -> Self {
        Self {
            max_body_bytes: config.max_body_bytes,
            max_response_bytes: config.max_output_bytes,
            max_range_bytes: config.max_output_bytes,
            max_total_concurrency: config.max_queue,
            ..Self::default()
        }
    }

    fn validate(&self) -> Result<(), LimitConfigError> {
        let zero_fields: [(&'static str, bool); 10] = [
            ("max_process_requests", self.max_process_requests == 0),
            ("max_body_bytes", self.max_body_bytes == 0),
            ("max_response_bytes", self.max_response_bytes == 0),
            ("max_range_bytes", self.max_range_bytes == 0),
            ("max_subject_concurrency", self.max_subject_concurrency == 0),
            ("max_total_concurrency", self.max_total_concurrency == 0),
            ("requests_per_window", self.requests_per_window == 0),
            ("rate_window_ms", self.rate_window_ms == 0),
            ("artifact_retention_ms", self.artifact_retention_ms == 0),
            (
                "max_artifact_total_bytes",
                self.max_artifact_total_bytes == 0,
            ),
        ];
        for (field, is_zero) in zero_fields {
            if is_zero {
                return Err(LimitConfigError { field });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Default)]
struct SubjectState {
    in_flight: u32,
    admitted_at_ms: Vec<u64>,
}

#[derive(Debug, Default)]
struct LimitState {
    subjects: BTreeMap<String, SubjectState>,
    total_in_flight: u32,
    artifact_bytes: u64,
}

/// The single enforcer every transport and every read path consults.
#[derive(Debug)]
pub struct LimitEnforcer {
    policy: LimitPolicy,
    state: Mutex<LimitState>,
    served: AtomicU64,
}

impl LimitEnforcer {
    /// Build an enforcer, refusing any bound configured as zero.
    ///
    /// # Errors
    /// Returns [`LimitConfigError`] when any bound is zero, because an unbounded
    /// limit is never an acceptable default.
    pub fn new(policy: LimitPolicy) -> Result<Self, LimitConfigError> {
        policy.validate()?;
        Ok(Self {
            policy,
            state: Mutex::new(LimitState::default()),
            served: AtomicU64::new(0),
        })
    }

    /// The enforced bounds.
    #[must_use]
    pub const fn policy(&self) -> &LimitPolicy {
        &self.policy
    }

    /// Count one served request against the per-process bound.
    ///
    /// # Errors
    /// Returns [`LimitRejection`] once this process has served its bound.
    pub fn note_request(&self) -> Result<(), LimitRejection> {
        let served = self.served.fetch_add(1, Ordering::SeqCst);
        if served >= self.policy.max_process_requests {
            return Err(LimitRejection::of(LimitKind::ProcessRequests));
        }
        Ok(())
    }

    /// Check one request body against its bound.
    ///
    /// # Errors
    /// Returns [`LimitRejection`] when the body exceeds the configured bound.
    pub const fn check_body_bytes(&self, bytes: u64) -> Result<(), LimitRejection> {
        if bytes > self.policy.max_body_bytes {
            return Err(LimitRejection::of(LimitKind::RequestBytes));
        }
        Ok(())
    }

    /// Check one response against its bound.
    ///
    /// # Errors
    /// Returns [`LimitRejection`] when the response exceeds the configured bound.
    pub const fn check_response_bytes(&self, bytes: u64) -> Result<(), LimitRejection> {
        if bytes > self.policy.max_response_bytes {
            return Err(LimitRejection::of(LimitKind::ResponseBytes));
        }
        Ok(())
    }

    /// Check one requested artifact range against its bound.
    ///
    /// # Errors
    /// Returns [`LimitRejection`] when the range exceeds the configured bound.
    pub const fn check_range_bytes(&self, bytes: u64) -> Result<(), LimitRejection> {
        if bytes > self.policy.max_range_bytes {
            return Err(LimitRejection::of(LimitKind::RangeBytes));
        }
        Ok(())
    }

    /// Check whether an artifact created at `created_at_unix_ms` is still inside
    /// its retention window at `now_unix_ms`.
    ///
    /// # Errors
    /// Returns [`LimitRejection`] with a `not_found` class once retention has
    /// elapsed, so an expired artifact is indistinguishable from one that never
    /// existed.
    pub const fn check_retention(
        &self,
        created_at_unix_ms: u64,
        now_unix_ms: u64,
    ) -> Result<(), LimitRejection> {
        let age = now_unix_ms.saturating_sub(created_at_unix_ms);
        if age > self.policy.artifact_retention_ms {
            return Err(LimitRejection::of(LimitKind::ArtifactRetention));
        }
        Ok(())
    }

    /// Consume one slot of the sliding rate window for one subject.
    ///
    /// # Errors
    /// Returns [`LimitRejection`] when the subject has already started
    /// [`LimitPolicy::requests_per_window`] requests inside the window.
    pub fn check_rate(&self, subject: &SafeField, now_unix_ms: u64) -> Result<(), LimitRejection> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let window_start = now_unix_ms.saturating_sub(self.policy.rate_window_ms);
        let entry = state
            .subjects
            .entry(subject.as_str().to_owned())
            .or_default();
        entry.admitted_at_ms.retain(|at| *at > window_start);
        if entry.admitted_at_ms.len() as u64 >= u64::from(self.policy.requests_per_window) {
            return Err(LimitRejection::of(LimitKind::RequestRate));
        }
        entry.admitted_at_ms.push(now_unix_ms);
        Ok(())
    }

    /// Reserve one concurrency slot for one subject.
    ///
    /// The returned guard releases the slot when it is dropped, so recovery needs
    /// no explicit cleanup call.
    ///
    /// # Errors
    /// Returns [`LimitRejection`] when either the per-subject or the total
    /// concurrency bound is already reached.
    pub fn admit(
        self: Arc<Self>,
        subject: &SafeField,
        _now_unix_ms: u64,
    ) -> Result<ConcurrencyGuard, LimitRejection> {
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if state.total_in_flight >= self.policy.max_total_concurrency {
                return Err(LimitRejection::of(LimitKind::TotalConcurrency));
            }
            let entry = state
                .subjects
                .entry(subject.as_str().to_owned())
                .or_default();
            if entry.in_flight >= self.policy.max_subject_concurrency {
                return Err(LimitRejection::of(LimitKind::SubjectConcurrency));
            }
            entry.in_flight += 1;
            state.total_in_flight += 1;
        }
        Ok(ConcurrencyGuard {
            enforcer: self,
            subject: subject.as_str().to_owned(),
        })
    }

    /// Reserve artifact-store bytes.
    ///
    /// # Errors
    /// Returns [`LimitRejection`] when the reservation would exceed the total
    /// retained-bytes bound.
    pub fn admit_artifact(
        self: Arc<Self>,
        bytes: u64,
    ) -> Result<ArtifactReservation, LimitRejection> {
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let projected = state.artifact_bytes.saturating_add(bytes);
            if projected > self.policy.max_artifact_total_bytes {
                return Err(LimitRejection::of(LimitKind::ArtifactStoreBytes));
            }
            state.artifact_bytes = projected;
        }
        Ok(ArtifactReservation {
            enforcer: self,
            bytes,
        })
    }

    /// How many concurrency slots one subject currently holds.
    #[must_use]
    pub fn in_flight(&self, subject: &SafeField) -> u32 {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .subjects
            .get(subject.as_str())
            .map_or(0, |entry| entry.in_flight)
    }

    /// How many artifact bytes are currently reserved.
    #[must_use]
    pub fn reserved_artifact_bytes(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .artifact_bytes
    }

    fn release_concurrency(&self, subject: &str) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(entry) = state.subjects.get_mut(subject) {
            entry.in_flight = entry.in_flight.saturating_sub(1);
        }
        state.total_in_flight = state.total_in_flight.saturating_sub(1);
    }

    fn release_artifact(&self, bytes: u64) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.artifact_bytes = state.artifact_bytes.saturating_sub(bytes);
    }
}

/// One held concurrency slot. Dropping it restores the budget.
#[derive(Debug)]
pub struct ConcurrencyGuard {
    enforcer: Arc<LimitEnforcer>,
    subject: String,
}

impl Drop for ConcurrencyGuard {
    fn drop(&mut self) {
        self.enforcer.release_concurrency(&self.subject);
    }
}

/// One artifact-store reservation. Dropping it restores the budget.
#[derive(Debug)]
pub struct ArtifactReservation {
    enforcer: Arc<LimitEnforcer>,
    bytes: u64,
}

impl ArtifactReservation {
    /// How many bytes this reservation holds.
    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }
}

impl Drop for ArtifactReservation {
    fn drop(&mut self) {
        self.enforcer.release_artifact(self.bytes);
    }
}

#[cfg(test)]
mod limits_audit {
    use super::*;

    const LIMITS_SOURCE: &str = include_str!("limits.rs");

    /// Scan the code region only: the module prose deliberately *names* the bans
    /// it keeps, so a prose mention must not be mistaken for an implementation.
    fn production_source() -> &'static str {
        let start = LIMITS_SOURCE.find("\nuse ").unwrap_or(0);
        let end = LIMITS_SOURCE
            .find("#[cfg(test)]")
            .unwrap_or(LIMITS_SOURCE.len());
        &LIMITS_SOURCE[start..end]
    }

    fn subject() -> SafeField {
        SafeField::new("subject_limits").expect("safe subject")
    }

    #[test]
    fn no_bound_may_be_configured_as_unlimited() {
        let fields: [fn(&mut LimitPolicy); 10] = [
            |policy| policy.max_process_requests = 0,
            |policy| policy.max_body_bytes = 0,
            |policy| policy.max_response_bytes = 0,
            |policy| policy.max_range_bytes = 0,
            |policy| policy.max_subject_concurrency = 0,
            |policy| policy.max_total_concurrency = 0,
            |policy| policy.requests_per_window = 0,
            |policy| policy.rate_window_ms = 0,
            |policy| policy.artifact_retention_ms = 0,
            |policy| policy.max_artifact_total_bytes = 0,
        ];
        for zero in fields {
            let mut policy = LimitPolicy::default();
            zero(&mut policy);
            assert!(
                LimitEnforcer::new(policy).is_err(),
                "a zero bound must be refused"
            );
        }
    }

    #[test]
    fn every_limit_kind_has_a_stable_class_status_and_retry_advice() {
        for kind in [
            LimitKind::ProcessRequests,
            LimitKind::RequestBytes,
            LimitKind::ResponseBytes,
            LimitKind::RangeBytes,
            LimitKind::SubjectConcurrency,
            LimitKind::TotalConcurrency,
            LimitKind::RequestRate,
            LimitKind::ArtifactStoreBytes,
            LimitKind::ArtifactRetention,
        ] {
            let rejection = LimitRejection::of(kind);
            assert!(!kind.label().is_empty());
            assert!(rejection.http_status() >= 400);
            assert_eq!(rejection.error_kind(), kind.error_kind());
            assert_eq!(rejection.to_outcome().kind(), kind.error_kind());
            assert!(!rejection.to_outcome().message().is_empty());
        }
    }

    #[test]
    fn concurrency_and_artifact_budgets_recover_on_drop() {
        let enforcer =
            Arc::new(LimitEnforcer::new(LimitPolicy::default()).expect("enforcer builds"));
        let subject = subject();
        let guard = Arc::clone(&enforcer)
            .admit(&subject, 1_000)
            .expect("first slot admits");
        assert_eq!(enforcer.in_flight(&subject), 1);
        drop(guard);
        assert_eq!(enforcer.in_flight(&subject), 0);

        let reservation = Arc::clone(&enforcer)
            .admit_artifact(4_096)
            .expect("artifact reservation admits");
        assert_eq!(reservation.bytes(), 4_096);
        assert_eq!(enforcer.reserved_artifact_bytes(), 4_096);
        drop(reservation);
        assert_eq!(enforcer.reserved_artifact_bytes(), 0);
    }

    #[test]
    fn the_rate_window_slides_rather_than_resetting_on_a_clock() {
        let enforcer = Arc::new(
            LimitEnforcer::new(LimitPolicy {
                requests_per_window: 2,
                rate_window_ms: 1_000,
                ..LimitPolicy::default()
            })
            .expect("enforcer builds"),
        );
        let subject = subject();
        enforcer.check_rate(&subject, 1_000).expect("first admits");
        enforcer.check_rate(&subject, 1_500).expect("second admits");
        assert!(enforcer.check_rate(&subject, 1_900).is_err());
        enforcer
            .check_rate(&subject, 2_600)
            .expect("the window slid past the first two");
    }

    #[test]
    fn the_limit_layer_never_infers_a_commercial_quota() {
        let source = production_source();
        for banned in [
            "billing",
            "tenant",
            "plan",
            "invoice",
            "subscription",
            "seat",
            "quota_period",
            "customer",
        ] {
            assert!(
                !source.contains(banned),
                "an operational limit must never be a commercial quota ({banned})"
            );
        }
    }

    #[test]
    fn the_limit_layer_never_reaches_the_filesystem_or_a_clock() {
        let source = production_source();
        for banned in [
            "std::fs::",
            "OpenOptions",
            "SystemTime",
            "Instant::now",
            "std::process::Command",
        ] {
            assert!(
                !source.contains(banned),
                "limits are decided from caller-supplied facts only ({banned})"
            );
        }
    }
}
