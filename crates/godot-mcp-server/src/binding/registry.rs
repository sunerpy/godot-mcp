//! The session/task/artifact binding registry, and its single refusal.
//!
//! # What a binding is
//!
//! One record per live MCP session, keyed by the [`OpaqueHandle`] this registry
//! minted and advertised as the wire session identifier. Each record holds the
//! [`BoundSubject`] the opening request's credential resolved to, the expiry that
//! credential carried, the internal transport session the SDK actually keys its
//! own state on, the tasks admitted inside the session, the artifacts readable
//! from it, and how many streams are currently open.
//!
//! # A session identifier is a name, never an authority
//!
//! Every entry point takes the subject the *current* request independently
//! authenticated to, and every one of them compares it to the record's subject in
//! full. Presenting a handle therefore proves only that the caller knows the
//! handle; if the credential behind the request resolves to any other subject — or
//! to none at all, because the proxy hop or the token was missing — the lookup
//! fails exactly as if the handle had never existed. That is what makes a stolen
//! identifier worthless rather than merely inconvenient.
//!
//! # Every refusal is the same bytes
//!
//! Todo 27's asymmetry applies here for the same reason: a cross-context refusal
//! must disclose nothing, or the endpoint becomes an enumeration oracle. So there
//! is exactly one refusal constructor, [`binding_refusal`], it takes no arguments,
//! and an unknown handle, a foreign subject, a revoked record, an expired
//! credential, a changed project generation, an unbound artifact, an invalid range,
//! an exhausted stream ceiling and a replayed destructive task all produce
//! byte-identical output. Nothing names which one fired, and nothing names a task,
//! an artifact or a principal.
//!
//! # Nothing here can disclose a path
//!
//! An authorized read returns an [`ArtifactResourceUri`] and a resolved
//! [`ByteRange`] — the Todo 3 contract types, neither of which can carry a storage
//! locator. This module never opens a file, never names one, and its self-audit
//! asserts as much against its own bytes.
//!
//! # Revocation is explicit, and total
//!
//! [`BindingRegistry::revoke_all`] drops every binding, and
//! [`BindingRegistry::revoke_other_generations`] drops every binding that is not the
//! generation now being served. Either is what an authentication, policy or project
//! change calls: a binding made under superseded facts is not repaired, it is
//! discarded.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use godot_mcp_domain::contracts::{
    ArtifactContract, ArtifactResourceUri, ByteRange, ErrorKind, OutcomeError, RangeSemantics,
};

use super::opaque::{HandleKind, OpaqueHandle};
use super::subject::{BoundSubject, SessionBindingPolicy};
use crate::idempotency::family_of;
use crate::kernel::{canonical_principal_id, canonical_session_id};

/// The fixed message every binding refusal carries.
///
/// Deliberately the same words a genuinely absent resource would produce: a caller
/// must not be able to distinguish "you may not have this" from "this does not
/// exist".
pub const BINDING_REFUSAL_MESSAGE: &str = "resource was not found";

/// The HTTP status every binding refusal returns.
///
/// `404` rather than `403`, because `403` would confirm the identifier exists.
pub const BINDING_REFUSAL_STATUS: u16 = 404;

/// The refusal every binding failure produces, whatever actually failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingRefusal {
    // A private unit field, mirroring Todo 27's `ConflictEnvelope`: it makes
    // `BindingRefusal { .. }` unconstructable outside this module even for sibling
    // modules in this crate, so no caller can mint a refusal that carries a cause.
    seal: (),
}

impl BindingRefusal {
    /// The refusal's canonical bytes. Identical for every cause, always.
    ///
    /// Built from constants only, so no cause can influence a single byte. A
    /// serialization failure would itself be a variation, so it collapses to the
    /// same fixed string rather than to an error that could differ.
    #[must_use]
    pub fn as_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(&self.to_outcome())
            .unwrap_or_else(|_| BINDING_REFUSAL_MESSAGE.as_bytes().to_vec())
    }

    /// The refusal as the public domain error taxonomy sees it.
    #[must_use]
    pub fn to_outcome(&self) -> OutcomeError {
        OutcomeError::new(
            ErrorKind::NotFound,
            BINDING_REFUSAL_MESSAGE,
            false,
            BTreeMap::new(),
        )
    }

    /// The fixed caller-facing message.
    #[must_use]
    pub const fn message(&self) -> &'static str {
        BINDING_REFUSAL_MESSAGE
    }

    /// The fixed HTTP status.
    #[must_use]
    pub const fn status_code(&self) -> u16 {
        BINDING_REFUSAL_STATUS
    }
}

/// The single construction site for a binding refusal.
///
/// It takes no arguments on purpose. An unknown handle, a foreign subject, a
/// revoked or expired binding, a superseded project generation, an unbound
/// artifact, an out-of-bounds range, an exhausted stream ceiling and a replayed
/// destructive task all route here and get the same bytes.
#[must_use]
pub fn binding_refusal() -> BindingRefusal {
    BindingRefusal { seal: () }
}

/// One authorized session, as the caller of a successful lookup sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionLease {
    handle: OpaqueHandle,
    internal_session: String,
    subject: BoundSubject,
    expires_at_unix_ms: u64,
}

impl SessionLease {
    /// The opaque handle this session is known by on the wire.
    #[must_use]
    pub const fn handle(&self) -> &OpaqueHandle {
        &self.handle
    }

    /// The transport-internal session identifier the SDK keys its own state on.
    ///
    /// Never leaves the process: the middleware substitutes it on the way in and
    /// replaces it with the handle on the way out.
    #[must_use]
    pub fn internal_session(&self) -> &str {
        &self.internal_session
    }

    /// The subject this session is bound to.
    #[must_use]
    pub const fn subject(&self) -> &BoundSubject {
        &self.subject
    }

    /// When the credential behind this binding expires.
    #[must_use]
    pub const fn expires_at_unix_ms(&self) -> u64 {
        self.expires_at_unix_ms
    }
}

/// One admitted unit of work inside a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskBinding {
    handle: OpaqueHandle,
    mutating: bool,
}

impl TaskBinding {
    /// The opaque handle this task is known by.
    #[must_use]
    pub const fn handle(&self) -> &OpaqueHandle {
        &self.handle
    }

    /// Whether the tool family behind it mutates state.
    #[must_use]
    pub const fn is_mutating(&self) -> bool {
        self.mutating
    }
}

/// One authorized artifact range read.
///
/// Structurally incapable of naming a storage location: it holds the public
/// artifact URI and the normalized range, and nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizedArtifactRead {
    resource_uri: ArtifactResourceUri,
    range: ByteRange,
}

impl AuthorizedArtifactRead {
    /// The public artifact URI this read is for.
    #[must_use]
    pub const fn resource_uri(&self) -> &ArtifactResourceUri {
        &self.resource_uri
    }

    /// The normalized inclusive range the read may return.
    #[must_use]
    pub const fn range(&self) -> &ByteRange {
        &self.range
    }
}

/// An RAII ticket holding one slot of a session's concurrent-stream ceiling.
///
/// The slot is released when the ticket drops. The transport moves it into the
/// response body's own stream, so a long-lived SSE reply holds its slot for exactly
/// as long as the stream lives and a short JSON reply releases it immediately.
#[derive(Debug)]
pub struct StreamTicket {
    open_streams: std::sync::Arc<AtomicUsize>,
}

impl Drop for StreamTicket {
    fn drop(&mut self) {
        // `fetch_update` rather than `fetch_sub`: a slot that was already released
        // must not wrap the counter around.
        let _ = self
            .open_streams
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                Some(current.saturating_sub(1))
            });
    }
}

#[derive(Debug)]
struct ArtifactRecord {
    resource_uri: ArtifactResourceUri,
    ranges: RangeSemantics,
    expires_at_unix_ms: u64,
}

#[derive(Debug)]
struct TaskRecord {
    handle: OpaqueHandle,
    mutating: bool,
    dispatched: bool,
}

#[derive(Debug)]
struct SessionRecord {
    handle: OpaqueHandle,
    internal_session: String,
    subject: BoundSubject,
    expires_at_unix_ms: u64,
    open_streams: std::sync::Arc<AtomicUsize>,
    tasks: BTreeMap<String, TaskRecord>,
    tasks_by_idempotency: BTreeMap<String, String>,
    artifacts: BTreeMap<String, ArtifactRecord>,
}

/// Every live binding, plus the policy they were all admitted under.
#[derive(Debug)]
pub struct BindingRegistry {
    policy: SessionBindingPolicy,
    seeded_artifacts: Vec<ArtifactContract>,
    live_artifacts: Option<std::sync::Arc<crate::artifacts::ArtifactStore>>,
    sessions: Mutex<BTreeMap<String, SessionRecord>>,
    authorized_reads: AtomicUsize,
    refusals: AtomicUsize,
}

impl BindingRegistry {
    /// A registry serving exactly `policy`.
    #[must_use]
    pub fn new(policy: SessionBindingPolicy) -> Self {
        Self {
            policy,
            seeded_artifacts: Vec::new(),
            live_artifacts: None,
            sessions: Mutex::new(BTreeMap::new()),
            authorized_reads: AtomicUsize::new(0),
            refusals: AtomicUsize::new(0),
        }
    }

    /// Make the kernel's already-registered artifacts readable from new sessions.
    ///
    /// Only the ones whose contract names the served project *and* generation are
    /// seeded, and the filter runs again per session, so an artifact from a
    /// superseded generation is never reachable no matter when it was registered.
    #[must_use]
    pub fn with_artifacts(mut self, artifacts: Vec<ArtifactContract>) -> Self {
        self.seeded_artifacts = artifacts;
        self
    }

    /// Make what `store` registers while the transport serves readable too:
    /// exports, captures and recordings a session produced after it opened.
    ///
    /// Such an artifact is bound to a session the first time that session asks
    /// for it, under exactly the rule seeding applies — its project and
    /// generation, and its owner's principal and session — and only when its
    /// identifier has the opaque handle shape [`Self::bind_artifact`] requires.
    #[must_use]
    pub fn with_artifact_store(
        mut self,
        store: std::sync::Arc<crate::artifacts::ArtifactStore>,
    ) -> Self {
        self.live_artifacts = Some(store);
        self
    }

    /// The policy every binding here was admitted under.
    #[must_use]
    pub const fn policy(&self) -> &SessionBindingPolicy {
        &self.policy
    }

    /// How many artifact range reads this registry has authorized.
    ///
    /// Exists so a test can prove *every* read went through authorization rather
    /// than only the first: the count must advance once per read.
    #[must_use]
    pub fn authorized_reads(&self) -> usize {
        self.authorized_reads.load(Ordering::SeqCst)
    }

    /// How many refusals this registry has produced.
    #[must_use]
    pub fn refusals(&self) -> usize {
        self.refusals.load(Ordering::SeqCst)
    }

    /// How many bindings are live.
    #[must_use]
    pub fn open_sessions(&self) -> usize {
        self.locked().len()
    }

    /// How many streams are open against `handle`, or `None` when it is unknown.
    ///
    /// A test-facing observation only; it takes no subject and therefore is never
    /// reachable from a request path.
    #[must_use]
    pub fn open_streams(&self, handle: &str) -> Option<usize> {
        self.locked()
            .get(handle)
            .map(|record| record.open_streams.load(Ordering::SeqCst))
    }

    /// Bind a newly created transport session to `subject`, minting its handle.
    ///
    /// # Errors
    /// Returns the single [`BindingRefusal`] when entropy is unavailable or the
    /// subject was admitted for a project generation this registry does not serve.
    pub fn bind_session(
        &self,
        internal_session: &str,
        subject: &BoundSubject,
        expires_at_unix_ms: u64,
    ) -> Result<SessionLease, BindingRefusal> {
        if internal_session.is_empty() || !subject.matches_project(self.policy.project()) {
            return Err(self.refuse());
        }
        let Ok(handle) = OpaqueHandle::mint(HandleKind::Session) else {
            return Err(self.refuse());
        };
        let artifacts = self
            .seeded_artifacts
            .iter()
            .filter(|contract| artifact_matches_subject(contract, subject, handle.as_str()))
            .map(|contract| {
                (
                    contract.resource_uri().as_str().to_owned(),
                    record_of(contract),
                )
            })
            .collect();
        let record = SessionRecord {
            handle: handle.clone(),
            internal_session: internal_session.to_owned(),
            subject: subject.clone(),
            expires_at_unix_ms,
            open_streams: std::sync::Arc::new(AtomicUsize::new(0)),
            tasks: BTreeMap::new(),
            tasks_by_idempotency: BTreeMap::new(),
            artifacts,
        };
        let lease = lease_of(&record);
        self.locked().insert(handle.as_str().to_owned(), record);
        Ok(lease)
    }

    /// Re-authorize an existing binding for the request now in flight.
    ///
    /// `subject` is `None` when the request presented no acceptable credential —
    /// a missing proxy hop, a missing token, a missing launch secret. That case is
    /// refused with the same bytes as an unknown handle, so "the identifier is
    /// real but you are nobody" and "the identifier is not real" are the same
    /// answer.
    ///
    /// # Errors
    /// Returns the single [`BindingRefusal`].
    pub fn resume(
        &self,
        candidate: &str,
        subject: Option<&BoundSubject>,
        now_unix_ms: u64,
    ) -> Result<SessionLease, BindingRefusal> {
        self.revoke_expired(now_unix_ms);
        self.revoke_other_generations(self.policy.project().generation());
        let Some(subject) = subject else {
            return Err(self.refuse());
        };
        if !subject.matches_project(self.policy.project()) {
            return Err(self.refuse());
        }
        let Some(handle) = OpaqueHandle::parse(HandleKind::Session, candidate) else {
            return Err(self.refuse());
        };
        let sessions = self.locked();
        let Some(record) = sessions.get(handle.as_str()) else {
            return Err(self.refuse());
        };
        // Constant time even though the map lookup above already matched: the
        // comparison that decides admission must never be the short-circuiting one.
        if !record.handle.matches(candidate) {
            return Err(self.refuse());
        }
        if !record.subject.matches_exactly(subject) {
            return Err(self.refuse());
        }
        if now_unix_ms >= record.expires_at_unix_ms {
            return Err(self.refuse());
        }
        Ok(lease_of(record))
    }

    /// Acquire one slot of the concurrent-stream ceiling for an authorized session.
    ///
    /// # Errors
    /// Returns the single [`BindingRefusal`] when the session does not authorize or
    /// the ceiling is already reached.
    pub fn open_stream(
        &self,
        candidate: &str,
        subject: Option<&BoundSubject>,
        now_unix_ms: u64,
    ) -> Result<(SessionLease, StreamTicket), BindingRefusal> {
        let lease = self.resume(candidate, subject, now_unix_ms)?;
        let sessions = self.locked();
        let Some(record) = sessions.get(lease.handle.as_str()) else {
            return Err(self.refuse());
        };
        let ceiling = self.policy.max_concurrent_streams();
        let acquired =
            record
                .open_streams
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                    (current < ceiling).then_some(current + 1)
                });
        if acquired.is_err() {
            return Err(self.refuse());
        }
        Ok((
            lease,
            StreamTicket {
                open_streams: std::sync::Arc::clone(&record.open_streams),
            },
        ))
    }

    /// Revoke one binding, after proving the caller owns it.
    ///
    /// # Errors
    /// Returns the single [`BindingRefusal`] when the session does not authorize,
    /// so a caller cannot terminate a session it could not otherwise touch.
    pub fn revoke(
        &self,
        candidate: &str,
        subject: Option<&BoundSubject>,
        now_unix_ms: u64,
    ) -> Result<SessionLease, BindingRefusal> {
        let lease = self.resume(candidate, subject, now_unix_ms)?;
        self.locked().remove(lease.handle.as_str());
        Ok(lease)
    }

    /// Drop every binding. What an authentication or policy change calls.
    pub fn revoke_all(&self) -> usize {
        let mut sessions = self.locked();
        let dropped = sessions.len();
        sessions.clear();
        dropped
    }

    /// Drop every binding whose project generation is not `generation`.
    ///
    /// What a project-generation bump calls: a session opened against a superseded
    /// generation is never resumable, and never repaired.
    pub fn revoke_other_generations(&self, generation: u64) -> usize {
        let mut sessions = self.locked();
        let before = sessions.len();
        sessions.retain(|_, record| record.subject.project_generation() == generation);
        before - sessions.len()
    }

    /// Drop every binding whose credential has expired at `now_unix_ms`.
    pub fn revoke_expired(&self, now_unix_ms: u64) -> usize {
        let mut sessions = self.locked();
        let before = sessions.len();
        sessions.retain(|_, record| now_unix_ms < record.expires_at_unix_ms);
        before - sessions.len()
    }

    /// Admit one unit of work inside an authorized session.
    ///
    /// A mutating family presenting an `idempotency_id` that already reached
    /// dispatch is refused: "the request went out" is never replayable, which is
    /// Todo 27's rule rather than a second copy of it.
    ///
    /// # Errors
    /// Returns the single [`BindingRefusal`] when the session does not authorize,
    /// the tool is not canonical, entropy is unavailable, or the task is a replay.
    pub fn bind_task(
        &self,
        candidate: &str,
        subject: Option<&BoundSubject>,
        now_unix_ms: u64,
        tool: &str,
        idempotency_id: Option<&str>,
    ) -> Result<TaskBinding, BindingRefusal> {
        let lease = self.resume(candidate, subject, now_unix_ms)?;
        let Ok(family) = family_of(tool) else {
            return Err(self.refuse());
        };
        let mutating = family.is_mutating();
        let Ok(handle) = OpaqueHandle::mint(HandleKind::Task) else {
            return Err(self.refuse());
        };
        let mut sessions = self.locked();
        let Some(record) = sessions.get_mut(lease.handle.as_str()) else {
            return Err(self.refuse());
        };
        if let Some(idempotency_id) = idempotency_id {
            if let Some(existing) = record.tasks_by_idempotency.get(idempotency_id) {
                let replayed = record
                    .tasks
                    .get(existing)
                    .is_some_and(|task| task.dispatched || task.mutating);
                if replayed {
                    return Err(self.refuse());
                }
            }
            record
                .tasks_by_idempotency
                .insert(idempotency_id.to_owned(), handle.as_str().to_owned());
        }
        record.tasks.insert(
            handle.as_str().to_owned(),
            TaskRecord {
                handle: handle.clone(),
                mutating,
                dispatched: false,
            },
        );
        Ok(TaskBinding { handle, mutating })
    }

    /// Record that an admitted task reached dispatch, after proving ownership.
    ///
    /// # Errors
    /// Returns the single [`BindingRefusal`] when the session or the task does not
    /// authorize.
    pub fn mark_task_dispatched(
        &self,
        candidate: &str,
        subject: Option<&BoundSubject>,
        now_unix_ms: u64,
        task: &str,
    ) -> Result<(), BindingRefusal> {
        let lease = self.resume(candidate, subject, now_unix_ms)?;
        let mut sessions = self.locked();
        let Some(record) = sessions.get_mut(lease.handle.as_str()) else {
            return Err(self.refuse());
        };
        let Some(entry) = record.tasks.get_mut(task) else {
            return Err(self.refuse());
        };
        if !entry.handle.matches(task) {
            return Err(self.refuse());
        }
        entry.dispatched = true;
        Ok(())
    }

    /// Make one artifact readable from an authorized session.
    ///
    /// The contract's project and generation must be the session's own, and its
    /// identifier must already be an opaque handle in the frozen shape — an artifact
    /// whose identifier a caller could have guessed is never bound.
    ///
    /// # Errors
    /// Returns the single [`BindingRefusal`].
    pub fn bind_artifact(
        &self,
        candidate: &str,
        subject: Option<&BoundSubject>,
        now_unix_ms: u64,
        contract: &ArtifactContract,
    ) -> Result<OpaqueHandle, BindingRefusal> {
        let lease = self.resume(candidate, subject, now_unix_ms)?;
        if !artifact_matches_subject(contract, &lease.subject, lease.handle.as_str()) {
            return Err(self.refuse());
        }
        let Some(handle) =
            OpaqueHandle::parse(HandleKind::Artifact, contract.artifact_id().as_str())
        else {
            return Err(self.refuse());
        };
        let mut sessions = self.locked();
        let Some(record) = sessions.get_mut(lease.handle.as_str()) else {
            return Err(self.refuse());
        };
        record.artifacts.insert(
            contract.resource_uri().as_str().to_owned(),
            record_of(contract),
        );
        Ok(handle)
    }

    /// Authorize one artifact range read. Called for *every* read, not the first.
    ///
    /// # Errors
    /// Returns the single [`BindingRefusal`] when the session does not authorize,
    /// the URI is not a bound artifact of this session, the artifact has expired, or
    /// the requested range is outside what the contract permits.
    pub fn authorize_artifact_read(
        &self,
        candidate: &str,
        subject: Option<&BoundSubject>,
        now_unix_ms: u64,
        resource_uri: &str,
        requested_range: Option<ByteRange>,
    ) -> Result<AuthorizedArtifactRead, BindingRefusal> {
        let lease = self.resume(candidate, subject, now_unix_ms)?;
        if ArtifactResourceUri::parse(resource_uri).is_err() {
            return Err(self.refuse());
        }
        let produced = self
            .live_artifacts
            .as_ref()
            .and_then(|store| store.contract(resource_uri))
            .filter(|contract| {
                artifact_matches_subject(contract, &lease.subject, lease.handle.as_str())
                    && OpaqueHandle::parse(HandleKind::Artifact, contract.artifact_id().as_str())
                        .is_some()
            });
        let mut sessions = self.locked();
        let Some(record) = sessions.get_mut(lease.handle.as_str()) else {
            return Err(self.refuse());
        };
        if let Some(contract) = produced {
            record
                .artifacts
                .entry(resource_uri.to_owned())
                .or_insert_with(|| record_of(&contract));
        }
        let Some(artifact) = record.artifacts.get(resource_uri) else {
            return Err(self.refuse());
        };
        if now_unix_ms >= artifact.expires_at_unix_ms {
            return Err(self.refuse());
        }
        let Ok(range) = artifact.ranges.resolve(requested_range) else {
            return Err(self.refuse());
        };
        let authorized = AuthorizedArtifactRead {
            resource_uri: artifact.resource_uri.clone(),
            range,
        };
        drop(sessions);
        self.authorized_reads.fetch_add(1, Ordering::SeqCst);
        Ok(authorized)
    }

    /// The single refusal site. Counts, then produces the one envelope.
    fn refuse(&self) -> BindingRefusal {
        self.refusals.fetch_add(1, Ordering::SeqCst);
        binding_refusal()
    }

    /// A poisoned lock means a previous holder panicked mid-update. The stored
    /// bindings are plain data, so recovering the guard is safe and strictly better
    /// than turning every later request into a panic.
    fn locked(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, SessionRecord>> {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn record_of(contract: &ArtifactContract) -> ArtifactRecord {
    ArtifactRecord {
        resource_uri: contract.resource_uri().clone(),
        ranges: contract.ranges().clone(),
        expires_at_unix_ms: contract.expires_at_unix_ms(),
    }
}

fn artifact_matches_subject(
    contract: &ArtifactContract,
    subject: &BoundSubject,
    connection_session: &str,
) -> bool {
    subject.matches_project(contract.project())
        && contract.owner().is_none_or(|owner| {
            let principal = canonical_principal_id(subject.mode(), subject.principal());
            let session = canonical_session_id(subject.mode(), connection_session);
            owner
                .principal
                .as_ref()
                .is_none_or(|expected| expected == &principal)
                && owner
                    .session
                    .as_ref()
                    .is_none_or(|expected| expected == &session)
        })
}

fn lease_of(record: &SessionRecord) -> SessionLease {
    SessionLease {
        handle: record.handle.clone(),
        internal_session: record.internal_session.clone(),
        subject: record.subject.clone(),
        expires_at_unix_ms: record.expires_at_unix_ms,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use godot_mcp_domain::CapabilityProfile;
    use godot_mcp_domain::config::Limits;
    use godot_mcp_domain::contracts::{
        ArtifactId, ArtifactKind, ArtifactMetadata, ProjectIdentity,
    };
    use godot_mcp_domain::objid::{LeaseKey, StableObjectId};

    use super::*;
    use crate::idempotency::{ModePrincipal, TransportAuthMode};

    const NOW: u64 = 4_000;
    const EXPIRY: u64 = 9_000;

    fn project(generation: u64) -> ProjectIdentity {
        ProjectIdentity::from_lease_key(
            LeaseKey {
                root: StableObjectId::Unix { dev: 30, ino: 11 },
                project_file: StableObjectId::Unix { dev: 30, ino: 12 },
            },
            generation,
        )
    }

    fn policy(generation: u64) -> SessionBindingPolicy {
        SessionBindingPolicy::new(
            CapabilityProfile::Full,
            "https://mcp.example.com/mcp",
            project(generation),
            &Limits::default(),
        )
        .expect("policy")
    }

    fn principal(subject: &str) -> ModePrincipal {
        ModePrincipal::Oidc {
            issuer: "https://issuer.example".to_owned(),
            subject: subject.to_owned(),
        }
    }

    fn subject_for(policy: &SessionBindingPolicy, name: &str) -> BoundSubject {
        policy
            .admit(
                TransportAuthMode::ProxiedOidc,
                principal(name),
                BTreeSet::new(),
            )
            .expect("admitted")
    }

    fn artifact(generation: u64, letter: char, length: u64) -> ArtifactContract {
        artifact_expiring(generation, letter, length, EXPIRY)
    }

    fn artifact_expiring(
        generation: u64,
        letter: char,
        length: u64,
        expires_at_unix_ms: u64,
    ) -> ArtifactContract {
        let id = ArtifactId::parse(format!("artifact_{}", letter.to_string().repeat(64)))
            .expect("artifact id");
        ArtifactContract::new(
            id.clone(),
            ArtifactResourceUri::for_artifact(&id),
            project(generation),
            None,
            ArtifactMetadata {
                kind: ArtifactKind::Report,
                media_type: "application/octet-stream".to_owned(),
                byte_length: length,
                sha256: "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
                    .to_owned(),
                created_at_unix_ms: 1_000,
            },
            expires_at_unix_ms,
            RangeSemantics::new(length, length, true).expect("range semantics"),
        )
        .expect("artifact contract")
    }

    fn registry(generation: u64) -> BindingRegistry {
        BindingRegistry::new(policy(generation))
    }

    /// An artifact the session `handle` of `subject` produced.
    fn produced_by(
        generation: u64,
        letter: char,
        subject: &BoundSubject,
        handle: &str,
    ) -> ArtifactContract {
        let id = ArtifactId::parse(format!("artifact_{}", letter.to_string().repeat(64)))
            .expect("artifact id");
        ArtifactContract::new(
            id.clone(),
            ArtifactResourceUri::for_artifact(&id),
            project(generation),
            Some(godot_mcp_domain::contracts::ArtifactOwner {
                principal: Some(canonical_principal_id(subject.mode(), subject.principal())),
                session: Some(canonical_session_id(subject.mode(), handle)),
            }),
            ArtifactMetadata {
                kind: ArtifactKind::Export,
                media_type: "application/octet-stream".to_owned(),
                byte_length: 5,
                sha256: "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
                    .to_owned(),
                created_at_unix_ms: 1_000,
            },
            EXPIRY,
            RangeSemantics::new(5, 5, true).expect("range semantics"),
        )
        .expect("artifact contract")
    }

    #[test]
    fn an_artifact_produced_after_its_session_opened_is_readable_from_that_session_only() {
        let store = std::sync::Arc::new(crate::artifacts::ArtifactStore::in_memory());
        let registry = registry(4).with_artifact_store(std::sync::Arc::clone(&store));
        let owner = subject_for(registry.policy(), "subject-1");
        let lease = registry
            .bind_session("internal-1", &owner, EXPIRY)
            .expect("bound");
        let sibling = registry
            .bind_session("internal-2", &owner, EXPIRY)
            .expect("a second session of the same subject");
        // Produced by the first session after both opened.
        let produced = produced_by(4, 'a', &owner, lease.handle().as_str());
        store
            .register_declared(produced.clone(), b"hello".to_vec())
            .expect("registered");
        let uri = produced.resource_uri().as_str();

        let read = registry
            .authorize_artifact_read(lease.handle().as_str(), Some(&owner), NOW, uri, None)
            .expect("the session that produced it reads it");
        assert_eq!(read.resource_uri().as_str(), uri);
        assert_eq!(read.range(), &ByteRange::new(0, 4).expect("range"));

        let refusals = registry.refusals();
        let foreign = registry
            .authorize_artifact_read(sibling.handle().as_str(), Some(&owner), NOW, uri, None)
            .expect_err("another session never reads it");
        assert_eq!(
            registry.refusals(),
            refusals + 1,
            "one refusal, counted once"
        );
        let unknown = registry
            .authorize_artifact_read(
                lease.handle().as_str(),
                Some(&owner),
                NOW,
                artifact(4, 'b', 5).resource_uri().as_str(),
                None,
            )
            .expect_err("an artifact nobody produced is refused");
        assert_eq!(foreign.as_bytes(), unknown.as_bytes());
        assert_eq!(registry.authorized_reads(), 1);
    }

    #[test]
    fn an_authorized_subject_resumes_its_own_binding_and_nobody_elses() {
        let registry = registry(4);
        let owner = subject_for(registry.policy(), "subject-1");
        let stranger = subject_for(registry.policy(), "subject-2");
        let lease = registry
            .bind_session("internal-1", &owner, EXPIRY)
            .expect("bound");
        assert_eq!(registry.open_sessions(), 1);
        assert_eq!(lease.internal_session(), "internal-1");

        let resumed = registry
            .resume(lease.handle().as_str(), Some(&owner), NOW)
            .expect("the owner resumes");
        assert_eq!(resumed.handle(), lease.handle());
        assert_eq!(resumed.internal_session(), "internal-1");

        // A second subject holding a real handle, and an unauthenticated request
        // holding the same handle, are refused identically.
        let stolen = registry
            .resume(lease.handle().as_str(), Some(&stranger), NOW)
            .expect_err("a second subject is refused");
        let anonymous = registry
            .resume(lease.handle().as_str(), None, NOW)
            .expect_err("no credential is refused");
        let fabricated = registry
            .resume(
                OpaqueHandle::mint(HandleKind::Session)
                    .expect("mint")
                    .as_str(),
                Some(&owner),
                NOW,
            )
            .expect_err("an unbound handle is refused");
        assert_eq!(stolen.as_bytes(), anonymous.as_bytes());
        assert_eq!(stolen.as_bytes(), fabricated.as_bytes());
    }

    #[test]
    fn a_binding_stops_resolving_when_its_credential_expires_or_it_is_revoked() {
        let registry = registry(4);
        let owner = subject_for(registry.policy(), "subject-1");
        let lease = registry
            .bind_session("internal-1", &owner, EXPIRY)
            .expect("bound");
        assert!(
            registry
                .resume(lease.handle().as_str(), Some(&owner), EXPIRY - 1)
                .is_ok()
        );
        assert!(
            registry
                .resume(lease.handle().as_str(), Some(&owner), EXPIRY)
                .is_err(),
            "expiry is exclusive of the last valid instant"
        );
        assert_eq!(
            registry.open_sessions(),
            0,
            "resume eagerly removes the expired binding"
        );
        assert_eq!(
            registry.revoke_expired(EXPIRY),
            0,
            "a second cleanup finds no stale binding"
        );

        let lease = registry
            .bind_session("internal-2", &owner, EXPIRY)
            .expect("bound");
        registry
            .revoke(lease.handle().as_str(), Some(&owner), NOW)
            .expect("the owner terminates its own session");
        assert!(
            registry
                .resume(lease.handle().as_str(), Some(&owner), NOW)
                .is_err()
        );
        assert_eq!(registry.open_sessions(), 0);
    }

    #[test]
    fn a_stranger_cannot_terminate_a_session_it_does_not_own() {
        let registry = registry(4);
        let owner = subject_for(registry.policy(), "subject-1");
        let stranger = subject_for(registry.policy(), "subject-2");
        let lease = registry
            .bind_session("internal-1", &owner, EXPIRY)
            .expect("bound");
        assert!(
            registry
                .revoke(lease.handle().as_str(), Some(&stranger), NOW)
                .is_err()
        );
        assert_eq!(registry.open_sessions(), 1, "the binding survives");
        assert!(
            registry
                .resume(lease.handle().as_str(), Some(&owner), NOW)
                .is_ok()
        );
    }

    #[test]
    fn a_changed_project_generation_invalidates_every_binding_made_under_the_old_one() {
        let registry = registry(4);
        let owner = subject_for(registry.policy(), "subject-1");
        let lease = registry
            .bind_session("internal-1", &owner, EXPIRY)
            .expect("bound");
        let next_generation = subject_for(&policy(5), "subject-1");
        assert!(
            registry
                .resume(lease.handle().as_str(), Some(&next_generation), NOW)
                .is_err(),
            "a subject admitted for another generation never resumes"
        );
        assert_eq!(registry.revoke_other_generations(5), 1);
        assert_eq!(registry.open_sessions(), 0);
        assert!(
            registry
                .resume(lease.handle().as_str(), Some(&owner), NOW)
                .is_err()
        );
    }

    #[test]
    fn a_subject_from_another_generation_is_never_bound_in_the_first_place() {
        let registry = registry(4);
        let foreign = subject_for(&policy(5), "subject-1");
        assert!(
            registry
                .bind_session("internal-1", &foreign, EXPIRY)
                .is_err()
        );
        assert_eq!(registry.open_sessions(), 0);
    }

    #[test]
    fn every_artifact_range_read_is_authorized_and_bounded() {
        let registry = registry(4);
        let owner = subject_for(registry.policy(), "subject-1");
        let stranger = subject_for(registry.policy(), "subject-2");
        let lease = registry
            .bind_session("internal-1", &owner, EXPIRY)
            .expect("bound");
        let contract = artifact(4, 'a', 5);
        let handle = registry
            .bind_artifact(lease.handle().as_str(), Some(&owner), NOW, &contract)
            .expect("bound artifact");
        assert_eq!(handle.as_str(), contract.artifact_id().as_str());
        let uri = contract.resource_uri().as_str();

        assert_eq!(registry.authorized_reads(), 0);
        for expected in 1..=3 {
            let read = registry
                .authorize_artifact_read(
                    lease.handle().as_str(),
                    Some(&owner),
                    NOW,
                    uri,
                    Some(ByteRange::new(1, 3).expect("range")),
                )
                .expect("authorized");
            assert_eq!(read.resource_uri().as_str(), uri);
            assert_eq!(read.range(), &ByteRange::new(1, 3).expect("range"));
            assert_eq!(
                registry.authorized_reads(),
                expected,
                "each read must be authorized, not only the first"
            );
        }

        // A range past the artifact, a second subject, an unbound artifact and an
        // expired artifact are all the same refusal.
        let out_of_bounds = registry
            .authorize_artifact_read(
                lease.handle().as_str(),
                Some(&owner),
                NOW,
                uri,
                Some(ByteRange::new(0, 5).expect("range")),
            )
            .expect_err("a range past the end is refused");
        let foreign = registry
            .authorize_artifact_read(lease.handle().as_str(), Some(&stranger), NOW, uri, None)
            .expect_err("a second subject is refused");
        let unbound = registry
            .authorize_artifact_read(
                lease.handle().as_str(),
                Some(&owner),
                NOW,
                artifact(4, 'b', 5).resource_uri().as_str(),
                None,
            )
            .expect_err("an artifact this session never bound is refused");
        // An artifact whose retention ends before its session's does.
        let short_lived = artifact_expiring(4, 'c', 5, 5_000);
        registry
            .bind_artifact(lease.handle().as_str(), Some(&owner), NOW, &short_lived)
            .expect("bound artifact");
        let expired = registry
            .authorize_artifact_read(
                lease.handle().as_str(),
                Some(&owner),
                6_000,
                short_lived.resource_uri().as_str(),
                None,
            )
            .expect_err("an expired artifact is refused");
        for refusal in [&out_of_bounds, &foreign, &unbound, &expired] {
            assert_eq!(refusal.as_bytes(), out_of_bounds.as_bytes());
        }
        assert_eq!(
            registry.authorized_reads(),
            3,
            "a refused read never counts as authorized"
        );
    }

    #[test]
    fn an_artifact_from_another_generation_is_never_bound() {
        let registry = registry(4);
        let owner = subject_for(registry.policy(), "subject-1");
        let lease = registry
            .bind_session("internal-1", &owner, EXPIRY)
            .expect("bound");
        assert!(
            registry
                .bind_artifact(
                    lease.handle().as_str(),
                    Some(&owner),
                    NOW,
                    &artifact(5, 'a', 5)
                )
                .is_err()
        );
    }

    #[test]
    fn the_concurrent_stream_ceiling_is_enforced_and_released_on_drop() {
        let limits = Limits {
            max_queue: 2,
            ..Limits::default()
        };
        let policy = SessionBindingPolicy::new(
            CapabilityProfile::Full,
            "https://mcp.example.com/mcp",
            project(4),
            &limits,
        )
        .expect("policy");
        let registry = BindingRegistry::new(policy);
        let owner = subject_for(registry.policy(), "subject-1");
        let lease = registry
            .bind_session("internal-1", &owner, EXPIRY)
            .expect("bound");
        let handle = lease.handle().as_str().to_owned();

        let first = registry
            .open_stream(&handle, Some(&owner), NOW)
            .expect("first stream");
        let second = registry
            .open_stream(&handle, Some(&owner), NOW)
            .expect("second stream");
        assert_eq!(registry.open_streams(&handle), Some(2));
        assert!(
            registry.open_stream(&handle, Some(&owner), NOW).is_err(),
            "a third stream exceeds the configured ceiling"
        );
        drop(first);
        assert_eq!(registry.open_streams(&handle), Some(1));
        let third = registry
            .open_stream(&handle, Some(&owner), NOW)
            .expect("a released slot is reusable");
        drop(second);
        drop(third);
        assert_eq!(registry.open_streams(&handle), Some(0));
        assert!(registry.open_streams("session_unknown").is_none());
    }

    #[test]
    fn a_replayed_mutating_task_is_refused_while_a_read_may_repeat() {
        let registry = registry(4);
        let owner = subject_for(registry.policy(), "subject-1");
        let lease = registry
            .bind_session("internal-1", &owner, EXPIRY)
            .expect("bound");
        let handle = lease.handle().as_str().to_owned();

        let read = registry
            .bind_task(
                &handle,
                Some(&owner),
                NOW,
                "project.info.get",
                Some("idem-r"),
            )
            .expect("a read is admitted");
        assert!(!read.is_mutating());
        assert!(
            registry
                .bind_task(
                    &handle,
                    Some(&owner),
                    NOW,
                    "project.info.get",
                    Some("idem-r")
                )
                .is_ok(),
            "a non-mutating task may repeat"
        );

        let mutation = registry
            .bind_task(
                &handle,
                Some(&owner),
                NOW,
                "resource.update",
                Some("idem-m"),
            )
            .expect("a mutation is admitted once");
        assert!(mutation.is_mutating());
        registry
            .mark_task_dispatched(&handle, Some(&owner), NOW, mutation.handle().as_str())
            .expect("dispatched");
        assert!(
            registry
                .bind_task(
                    &handle,
                    Some(&owner),
                    NOW,
                    "resource.update",
                    Some("idem-m")
                )
                .is_err(),
            "a replayed destructive task is refused"
        );
        assert!(
            registry
                .bind_task(&handle, Some(&owner), NOW, "not.a.tool", Some("idem-x"))
                .is_err(),
            "a tool outside the canonical manifest has no family and no task"
        );
        assert!(
            registry
                .mark_task_dispatched(&handle, Some(&owner), NOW, "task_unknown")
                .is_err()
        );
    }

    #[test]
    fn a_task_is_never_admitted_without_an_authorized_session() {
        let registry = registry(4);
        let owner = subject_for(registry.policy(), "subject-1");
        let stranger = subject_for(registry.policy(), "subject-2");
        let lease = registry
            .bind_session("internal-1", &owner, EXPIRY)
            .expect("bound");
        let handle = lease.handle().as_str().to_owned();
        assert!(
            registry
                .bind_task(&handle, Some(&stranger), NOW, "resource.update", None)
                .is_err()
        );
        assert!(
            registry
                .bind_task(&handle, None, NOW, "resource.update", None)
                .is_err()
        );
        assert!(
            registry
                .bind_artifact(&handle, None, NOW, &artifact(4, 'a', 5))
                .is_err()
        );
        assert!(
            registry
                .authorize_artifact_read(&handle, None, NOW, "godot-mcp://artifacts/x", None)
                .is_err()
        );
    }

    #[test]
    fn revoking_everything_leaves_no_binding_resolvable() {
        let registry = registry(4);
        let owner = subject_for(registry.policy(), "subject-1");
        let first = registry
            .bind_session("internal-1", &owner, EXPIRY)
            .expect("bound");
        let second = registry
            .bind_session("internal-2", &owner, EXPIRY)
            .expect("bound");
        assert_ne!(first.handle(), second.handle());
        assert_eq!(registry.revoke_all(), 2);
        for lease in [&first, &second] {
            assert!(
                registry
                    .resume(lease.handle().as_str(), Some(&owner), NOW)
                    .is_err()
            );
        }
    }

    #[test]
    fn the_refusal_names_nothing_and_never_varies() {
        let refusal = binding_refusal();
        assert_eq!(refusal.as_bytes(), binding_refusal().as_bytes());
        assert_eq!(refusal.status_code(), 404);
        let text = String::from_utf8(refusal.as_bytes()).expect("the refusal is UTF-8");
        for leak in [
            "principal",
            "session",
            "subject",
            "generation",
            "artifact",
            "task",
            "scope",
            "profile",
            "range",
            "stream",
            "expired",
            "revoked",
            "replay",
            "exists",
            "oidc",
        ] {
            assert!(
                !text.contains(leak),
                "the binding refusal must not mention {leak}: {text}"
            );
        }
        let outcome = refusal.to_outcome();
        assert_eq!(outcome.code(), "not_found");
        assert!(!outcome.retryable());
        assert!(outcome.details().is_empty());
    }
}
