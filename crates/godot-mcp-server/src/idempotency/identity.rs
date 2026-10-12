//! Cross-plane mutation identity: the eight-element idempotency tuple.
//!
//! Todo 27 defines mutation identity as exactly
//! `(root/project OS-object identity, project generation, transport/auth mode,
//! mode-specific principal, MCP connection/session, operation/tool family,
//! idempotency ID, full canonical request hash)`.
//!
//! Two rules make this type load-bearing rather than decorative:
//!
//! 1. **The principal is mode-specific and the pairing is enforced.** `stdio`
//!    carries the local OS principal plus its connection session,
//!    `trusted_local` carries its launch-secret principal plus the MCP session,
//!    and `proxied_oidc` carries the exact `(iss, sub)` pair plus the MCP
//!    session. A tuple that pairs a mode with a foreign principal shape cannot
//!    be constructed at all, so no later comparison has to defend against it.
//! 2. **Comparison is total and answers one bit.** [`MutationIdentity::matches_exactly`]
//!    returns a `bool`. There is deliberately no public API that reports *which*
//!    element differed: a caller that could ask "was it the session or the hash?"
//!    would have an enumeration oracle over other contexts' work, which is the
//!    exact disclosure Todo 27 forbids.
//!
//! ## Identity is not availability
//!
//! [`OperationToolFamily`] enumerates every profile's tool families, including
//! the editor-plane families whose bridge is not shipped in this release. The
//! tuple defines *who asked for what*; whether the plane can execute is a
//! separate question answered by [`PlaneAvailability`](super::PlaneAvailability)
//! and refused honestly with `unavailable`. Dropping editor families from this
//! enum to match runtime availability would silently change what an idempotency
//! key means, so they stay.

use serde_json::Value;

/// Transport/auth mode that established the requesting identity.
///
/// This mirrors the durable journal's transport mode; the scheduler keeps its own
/// public spelling because the journal enum is crate-private.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TransportAuthMode {
    /// Local stdio transport.
    Stdio,
    /// Loopback HTTP authenticated by a per-launch, user-scoped secret.
    TrustedLocal,
    /// Loopback HTTP behind an authenticated proxy hop carrying end-user OIDC.
    ProxiedOidc,
}

impl TransportAuthMode {
    /// Every mode, in a stable order.
    pub const ALL: [Self; 3] = [Self::Stdio, Self::TrustedLocal, Self::ProxiedOidc];

    /// Stable snake_case name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::TrustedLocal => "trusted_local",
            Self::ProxiedOidc => "proxied_oidc",
        }
    }
}

/// The mode-specific principal. Each variant is the *only* principal shape its
/// transport mode may present.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ModePrincipal {
    /// `stdio`: the local OS principal that owns the connection.
    LocalOs {
        /// Opaque local principal identifier.
        principal: String,
    },
    /// `trusted_local`: the principal derived from the per-launch secret.
    LaunchSecret {
        /// Opaque launch-scoped principal identifier.
        principal: String,
    },
    /// `proxied_oidc`: the exact `(iss, sub)` pair, never a display name and
    /// never a loopback peer address.
    Oidc {
        /// Exact `iss` claim.
        issuer: String,
        /// Exact `sub` claim.
        subject: String,
    },
}

impl ModePrincipal {
    /// The transport mode this principal shape belongs to.
    #[must_use]
    pub const fn required_mode(&self) -> TransportAuthMode {
        match self {
            Self::LocalOs { .. } => TransportAuthMode::Stdio,
            Self::LaunchSecret { .. } => TransportAuthMode::TrustedLocal,
            Self::Oidc { .. } => TransportAuthMode::ProxiedOidc,
        }
    }

    /// The spelling the durable journal records for this principal.
    ///
    /// The OIDC form is length-prefixed rather than separator-joined because a
    /// separator is not injective: `(iss, "b\u{1f}c")` and `(iss + "\u{1f}b", "c")`
    /// would encode to the same bytes and two different principals would compare
    /// equal. The prefix makes the encoding unambiguous.
    #[must_use]
    pub fn to_journal_string(&self) -> String {
        match self {
            Self::LocalOs { principal } | Self::LaunchSecret { principal } => principal.clone(),
            Self::Oidc { issuer, subject } => {
                format!("{}:{issuer}{subject}", issuer.len())
            }
        }
    }

    /// Rebuild a principal from the journaled spelling for one mode.
    ///
    /// # Errors
    /// Returns [`IdentityError::MalformedPrincipal`] when the recorded spelling
    /// does not decode for `mode`.
    pub fn from_journal_string(
        mode: TransportAuthMode,
        recorded: &str,
    ) -> Result<Self, IdentityError> {
        match mode {
            TransportAuthMode::Stdio => Ok(Self::LocalOs {
                principal: recorded.to_owned(),
            }),
            TransportAuthMode::TrustedLocal => Ok(Self::LaunchSecret {
                principal: recorded.to_owned(),
            }),
            TransportAuthMode::ProxiedOidc => {
                let (length, remainder) = recorded
                    .split_once(':')
                    .ok_or(IdentityError::MalformedPrincipal)?;
                let length: usize = length
                    .parse()
                    .map_err(|_| IdentityError::MalformedPrincipal)?;
                if length > remainder.len() || !remainder.is_char_boundary(length) {
                    return Err(IdentityError::MalformedPrincipal);
                }
                let (issuer, subject) = remainder.split_at(length);
                Ok(Self::Oidc {
                    issuer: issuer.to_owned(),
                    subject: subject.to_owned(),
                })
            }
        }
    }
}

/// The MCP connection/session the request arrived on.
///
/// `stdio` uses its connection session; both HTTP modes use the MCP session.
/// Either way a session is required: an identity tuple with no session could be
/// reused across connections, which is precisely what the tuple exists to stop.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConnectionSession(String);

impl ConnectionSession {
    /// Bind one non-empty session identifier.
    ///
    /// # Errors
    /// Returns [`IdentityError::EmptySession`] for an empty identifier.
    pub fn new(session: impl Into<String>) -> Result<Self, IdentityError> {
        let session = session.into();
        if session.is_empty() {
            return Err(IdentityError::EmptySession);
        }
        Ok(Self(session))
    }

    /// Opaque session identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Public opaque project object identity (root + `project.godot` OS objects,
/// as adapted by Todo 2's lease key).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProjectObjectIdentity(String);

impl ProjectObjectIdentity {
    /// Bind one non-empty project object identity.
    ///
    /// # Errors
    /// Returns [`IdentityError::EmptyProjectObject`] for an empty identity.
    pub fn new(identity: impl Into<String>) -> Result<Self, IdentityError> {
        let identity = identity.into();
        if identity.is_empty() {
            return Err(IdentityError::EmptyProjectObject);
        }
        Ok(Self(identity))
    }

    /// Opaque identity string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The caller-supplied idempotency identifier.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IdempotencyId(String);

impl IdempotencyId {
    /// Minimum identifier length accepted by the kernel and advertised to clients.
    pub const MIN_LENGTH: usize = 1;

    /// Bind one non-empty idempotency identifier.
    ///
    /// # Errors
    /// Returns [`IdentityError::EmptyIdempotencyId`] for an empty identifier.
    pub fn new(id: impl Into<String>) -> Result<Self, IdentityError> {
        let id = id.into();
        if id.len() < Self::MIN_LENGTH {
            return Err(IdentityError::EmptyIdempotencyId);
        }
        Ok(Self(id))
    }

    /// Opaque idempotency identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A byte-stable digest of the *complete* canonical request.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequestHash(String);

impl RequestHash {
    /// Lowercase hexadecimal SHA-256 digest.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Adopt an already-computed lowercase hexadecimal digest.
    ///
    /// # Errors
    /// Returns [`IdentityError::MalformedRequestHash`] unless the value is
    /// exactly 64 lowercase hexadecimal characters.
    pub fn parse(digest: impl Into<String>) -> Result<Self, IdentityError> {
        let digest = digest.into();
        let well_formed = digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        if !well_formed {
            return Err(IdentityError::MalformedRequestHash);
        }
        Ok(Self(digest))
    }
}

/// Byte-stable digest over the canonical encoding of the *whole* request.
///
/// The digest covers the canonical tool name and every argument, never a
/// truncation and never a summary: two requests that differ anywhere in their
/// arguments must not share a hash, or an idempotency key would silently authorize
/// a different mutation. This is the single hashing entry point for the tuple's
/// eighth element and it agrees, byte for byte, with the durable journal's
/// canonical request hash.
///
/// # Errors
/// Returns [`IdentityError::UnencodableRequest`] when the arguments cannot be
/// canonically encoded.
pub fn canonical_request_hash(tool: &str, arguments: &Value) -> Result<RequestHash, IdentityError> {
    let (digest, bytes) = crate::journal::canonical_request(tool, arguments);
    if bytes == 0 {
        return Err(IdentityError::UnencodableRequest);
    }
    RequestHash::parse(digest)
}

/// Every profile's operation/tool family.
///
/// The editor-plane families are present even though this release ships no editor
/// bridge: the frozen manifests still advertise those tools, so an idempotency
/// tuple must be able to name them. Availability is answered separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OperationToolFamily {
    /// `capabilities.get`, `health.get`, `project.discover`, `project.info.get`.
    StaticDiscovery,
    /// GDScript symbol/reference/impact/diagnostic reads and static validation.
    StaticAnalysis,
    /// Scene and resource tree/dependency reads.
    StaticSceneGraph,
    /// Descriptor-relative project file mutations.
    ProjectMutation,
    /// Reimporting project assets through a supervised child process.
    ProjectImport,
    /// Editor-plane scene mutations through the plugin bridge.
    EditorSceneMutation,
    /// Editor-plane reads answered by the live editor.
    EditorInspection,
    /// Launching or stopping a supervised runtime.
    RuntimeExecution,
    /// Debug-adapter sessions against a running instance.
    RuntimeDebug,
    /// Input injection and capture commands against a running instance.
    RuntimeCommand,
    /// Read-only live runtime probes.
    RuntimeInspection,
    /// Language-server exchanges.
    LanguageService,
    /// Exports and other destructive project-wide output.
    DestructiveExport,
    /// Read-only diagnostic probes against a boundary.
    DiagnosticProbe,
}

impl OperationToolFamily {
    /// Every family, in a stable order.
    pub const ALL: [Self; 14] = [
        Self::StaticDiscovery,
        Self::StaticAnalysis,
        Self::StaticSceneGraph,
        Self::ProjectMutation,
        Self::ProjectImport,
        Self::EditorSceneMutation,
        Self::EditorInspection,
        Self::RuntimeExecution,
        Self::RuntimeDebug,
        Self::RuntimeCommand,
        Self::RuntimeInspection,
        Self::LanguageService,
        Self::DestructiveExport,
        Self::DiagnosticProbe,
    ];

    /// Stable snake_case name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StaticDiscovery => "static_discovery",
            Self::StaticAnalysis => "static_analysis",
            Self::StaticSceneGraph => "static_scene_graph",
            Self::ProjectMutation => "project_mutation",
            Self::ProjectImport => "project_import",
            Self::EditorSceneMutation => "editor_scene_mutation",
            Self::EditorInspection => "editor_inspection",
            Self::RuntimeExecution => "runtime_execution",
            Self::RuntimeDebug => "runtime_debug",
            Self::RuntimeCommand => "runtime_command",
            Self::RuntimeInspection => "runtime_inspection",
            Self::LanguageService => "language_service",
            Self::DestructiveExport => "destructive_export",
            Self::DiagnosticProbe => "diagnostic_probe",
        }
    }

    /// Whether the family mutates anything outside this process.
    #[must_use]
    pub const fn is_mutating(self) -> bool {
        matches!(
            self,
            Self::ProjectMutation
                | Self::ProjectImport
                | Self::EditorSceneMutation
                | Self::RuntimeExecution
                | Self::RuntimeCommand
                | Self::DestructiveExport
        )
    }
}

/// The single canonical tool-name to family mapping, read from the tool catalog.
///
/// Exactly one mapping exists so a tool cannot belong to two families depending
/// on which call site asked.
///
/// # Errors
/// Returns [`IdentityError::UnknownTool`] for a name the catalog does not list.
pub fn family_of(tool: &str) -> Result<OperationToolFamily, IdentityError> {
    godot_mcp_domain::catalog::descriptor(tool)
        .map(family_of_descriptor)
        .ok_or(IdentityError::UnknownTool)
}

/// The family one already-resolved catalog descriptor declares.
#[must_use]
pub fn family_of_descriptor(
    descriptor: &godot_mcp_domain::catalog::ToolDescriptor,
) -> OperationToolFamily {
    use godot_mcp_domain::catalog::IdentityFamily;

    match descriptor.identity_family {
        IdentityFamily::StaticDiscovery => OperationToolFamily::StaticDiscovery,
        IdentityFamily::StaticAnalysis => OperationToolFamily::StaticAnalysis,
        IdentityFamily::StaticSceneGraph => OperationToolFamily::StaticSceneGraph,
        IdentityFamily::ProjectMutation => OperationToolFamily::ProjectMutation,
        IdentityFamily::ProjectImport => OperationToolFamily::ProjectImport,
        IdentityFamily::EditorSceneMutation => OperationToolFamily::EditorSceneMutation,
        IdentityFamily::EditorInspection => OperationToolFamily::EditorInspection,
        IdentityFamily::RuntimeExecution => OperationToolFamily::RuntimeExecution,
        IdentityFamily::RuntimeCommand => OperationToolFamily::RuntimeCommand,
        IdentityFamily::RuntimeInspection => OperationToolFamily::RuntimeInspection,
        IdentityFamily::RuntimeDebug => OperationToolFamily::RuntimeDebug,
        IdentityFamily::DiagnosticProbe => OperationToolFamily::DiagnosticProbe,
        IdentityFamily::DestructiveExport => OperationToolFamily::DestructiveExport,
    }
}

/// One element of the identity tuple.
///
/// This exists so a *test* can construct a tuple that differs in exactly one
/// element. It is deliberately never carried in a conflict envelope, never
/// returned by a comparison, and never reachable from a transport response: a
/// caller who could learn which element mismatched would hold an enumeration
/// oracle over another context's recorded work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TupleDimension {
    /// Root/project OS-object identity.
    ProjectObject,
    /// Project generation.
    ProjectGeneration,
    /// Transport/auth mode (and, with it, the required principal shape).
    TransportAuthMode,
    /// Mode-specific principal.
    Principal,
    /// MCP connection/session.
    Session,
    /// Operation/tool family.
    OperationFamily,
    /// Idempotency ID.
    IdempotencyId,
    /// Full canonical request hash.
    RequestHash,
}

impl TupleDimension {
    /// Every dimension of the tuple, in the order the plan names them.
    pub const ALL: [Self; 8] = [
        Self::ProjectObject,
        Self::ProjectGeneration,
        Self::TransportAuthMode,
        Self::Principal,
        Self::Session,
        Self::OperationFamily,
        Self::IdempotencyId,
        Self::RequestHash,
    ];

    /// Stable snake_case name, for test diagnostics only.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProjectObject => "project_object",
            Self::ProjectGeneration => "project_generation",
            Self::TransportAuthMode => "transport_auth_mode",
            Self::Principal => "principal",
            Self::Session => "session",
            Self::OperationFamily => "operation_family",
            Self::IdempotencyId => "idempotency_id",
            Self::RequestHash => "request_hash",
        }
    }
}

/// The first two tuple elements: which project object, at which generation.
///
/// They travel together because neither is meaningful alone: a generation without
/// its object names nothing, and an object without its generation cannot tell a
/// re-created project from the original.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProjectBinding {
    object: ProjectObjectIdentity,
    generation: u64,
}

impl ProjectBinding {
    /// Bind one project object identity to its generation.
    #[must_use]
    pub const fn new(object: ProjectObjectIdentity, generation: u64) -> Self {
        Self { object, generation }
    }

    /// Root/project OS-object identity.
    #[must_use]
    pub const fn object(&self) -> &ProjectObjectIdentity {
        &self.object
    }

    /// Project generation.
    #[must_use]
    pub const fn generation(self_: &Self) -> u64 {
        self_.generation
    }
}

/// The next three tuple elements: the transport/auth mode, its mode-specific
/// principal, and the MCP connection/session.
///
/// The pairing invariant lives here rather than in [`MutationIdentity`]: a mode
/// and a foreign principal shape cannot be bound together at all, so no later
/// comparison has to defend against the combination.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TransportBinding {
    mode: TransportAuthMode,
    principal: ModePrincipal,
    session: ConnectionSession,
}

impl TransportBinding {
    /// Bind one mode to the principal shape it requires and its session.
    ///
    /// # Errors
    /// Returns [`IdentityError::PrincipalModeMismatch`] when the principal shape
    /// does not belong to `mode`.
    pub fn new(
        mode: TransportAuthMode,
        principal: ModePrincipal,
        session: ConnectionSession,
    ) -> Result<Self, IdentityError> {
        if principal.required_mode() != mode {
            return Err(IdentityError::PrincipalModeMismatch {
                mode: mode.as_str(),
            });
        }
        Ok(Self {
            mode,
            principal,
            session,
        })
    }

    /// Transport/auth mode.
    #[must_use]
    pub const fn mode(&self) -> TransportAuthMode {
        self.mode
    }

    /// Mode-specific principal.
    #[must_use]
    pub const fn principal(&self) -> &ModePrincipal {
        &self.principal
    }

    /// MCP connection/session.
    #[must_use]
    pub const fn session(&self) -> &ConnectionSession {
        &self.session
    }
}

/// The complete eight-element mutation identity.
///
/// Every field is private and there is no field-wise accessor that leaks a
/// comparison verdict; [`Self::matches_exactly`] is the only comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutationIdentity {
    project: ProjectBinding,
    transport: TransportBinding,
    family: OperationToolFamily,
    idempotency_id: IdempotencyId,
    request_hash: RequestHash,
}

impl MutationIdentity {
    /// Assemble one identity from its two bindings and its three own elements.
    #[must_use]
    pub const fn new(
        project: ProjectBinding,
        transport: TransportBinding,
        family: OperationToolFamily,
        idempotency_id: IdempotencyId,
        request_hash: RequestHash,
    ) -> Self {
        Self {
            project,
            transport,
            family,
            idempotency_id,
            request_hash,
        }
    }

    /// The project object identity and generation this identity is bound to.
    #[must_use]
    pub const fn project(&self) -> &ProjectBinding {
        &self.project
    }

    /// The transport mode, principal and session this identity is bound to.
    #[must_use]
    pub const fn transport(&self) -> &TransportBinding {
        &self.transport
    }

    /// Operation/tool family.
    #[must_use]
    pub const fn family(&self) -> OperationToolFamily {
        self.family
    }

    /// Idempotency ID.
    #[must_use]
    pub const fn idempotency_id(&self) -> &IdempotencyId {
        &self.idempotency_id
    }

    /// Full canonical request hash.
    #[must_use]
    pub const fn request_hash(&self) -> &RequestHash {
        &self.request_hash
    }

    /// Whether every one of the eight elements is identical.
    ///
    /// The answer is one bit. Which element differed is intentionally not
    /// observable, here or anywhere downstream.
    #[must_use]
    pub fn matches_exactly(&self, other: &Self) -> bool {
        self == other
    }

    /// Derive a copy that differs in exactly one element.
    ///
    /// Test-only: this is how the acceptance suite proves that a mismatch in
    /// *any* dimension yields the same byte-stable conflict envelope. It is
    /// compiled out of every release build.
    #[cfg(feature = "test-hooks")]
    #[must_use]
    pub fn with_dimension_changed(&self, dimension: TupleDimension) -> Self {
        let mut varied = self.clone();
        match dimension {
            TupleDimension::ProjectObject => {
                varied.project = ProjectBinding::new(
                    ProjectObjectIdentity(format!("{}-other", self.project.object().as_str())),
                    ProjectBinding::generation(&self.project),
                );
            }
            TupleDimension::ProjectGeneration => {
                varied.project = ProjectBinding::new(
                    self.project.object().clone(),
                    ProjectBinding::generation(&self.project).wrapping_add(1),
                );
            }
            // Changing the mode must change the principal shape with it: the
            // pairing invariant makes any other combination unrepresentable, so a
            // mode-only perturbation could not be built at all.
            TupleDimension::TransportAuthMode => {
                let mode = match self.transport.mode() {
                    TransportAuthMode::Stdio => TransportAuthMode::TrustedLocal,
                    TransportAuthMode::TrustedLocal | TransportAuthMode::ProxiedOidc => {
                        TransportAuthMode::Stdio
                    }
                };
                let principal = match mode {
                    TransportAuthMode::Stdio => ModePrincipal::LocalOs {
                        principal: "principal_0123456789abcdef".to_owned(),
                    },
                    TransportAuthMode::TrustedLocal => ModePrincipal::LaunchSecret {
                        principal: "principal_0123456789abcdef".to_owned(),
                    },
                    TransportAuthMode::ProxiedOidc => ModePrincipal::Oidc {
                        issuer: "https://issuer.invalid".to_owned(),
                        subject: "subject-1".to_owned(),
                    },
                };
                varied.transport =
                    TransportBinding::new(mode, principal, self.transport.session().clone())
                        .expect("the perturbed mode is paired with its own principal shape");
            }
            TupleDimension::Principal => {
                let principal = match self.transport.principal() {
                    ModePrincipal::LocalOs { principal } => ModePrincipal::LocalOs {
                        principal: format!("{principal}0"),
                    },
                    ModePrincipal::LaunchSecret { principal } => ModePrincipal::LaunchSecret {
                        principal: format!("{principal}0"),
                    },
                    ModePrincipal::Oidc { issuer, subject } => ModePrincipal::Oidc {
                        issuer: issuer.clone(),
                        subject: format!("{subject}-other"),
                    },
                };
                varied.transport = TransportBinding::new(
                    self.transport.mode(),
                    principal,
                    self.transport.session().clone(),
                )
                .expect("the principal shape is preserved");
            }
            TupleDimension::Session => {
                varied.transport = TransportBinding::new(
                    self.transport.mode(),
                    self.transport.principal().clone(),
                    ConnectionSession(format!("{}-other", self.transport.session().as_str())),
                )
                .expect("the principal shape is preserved");
            }
            TupleDimension::OperationFamily => {
                varied.family = if self.family == OperationToolFamily::ProjectMutation {
                    OperationToolFamily::DestructiveExport
                } else {
                    OperationToolFamily::ProjectMutation
                };
            }
            TupleDimension::IdempotencyId => {
                varied.idempotency_id =
                    IdempotencyId(format!("{}-other", self.idempotency_id.as_str()));
            }
            TupleDimension::RequestHash => {
                let flipped = self
                    .request_hash
                    .as_str()
                    .chars()
                    .enumerate()
                    .map(|(index, digit)| if index == 0 { flip_hex(digit) } else { digit })
                    .collect::<String>();
                varied.request_hash = RequestHash(flipped);
            }
        }
        varied
    }
}

#[cfg(feature = "test-hooks")]
const fn flip_hex(digit: char) -> char {
    if digit == '0' { '1' } else { '0' }
}

/// Why an identity element or tuple could not be represented.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdentityError {
    /// A session identifier was empty.
    #[error("an MCP connection/session identifier is required")]
    EmptySession,
    /// A project object identity was empty.
    #[error("a project object identity is required")]
    EmptyProjectObject,
    /// An idempotency identifier was empty.
    #[error("an idempotency identifier is required")]
    EmptyIdempotencyId,
    /// A request hash was not 64 lowercase hexadecimal characters.
    #[error("a canonical request hash must be 64 lowercase hexadecimal characters")]
    MalformedRequestHash,
    /// A journaled principal spelling did not decode for its recorded mode.
    #[error("the recorded principal does not decode for its transport mode")]
    MalformedPrincipal,
    /// The request could not be canonically encoded, so no honest hash exists.
    #[error("the request could not be canonically encoded")]
    UnencodableRequest,
    /// The principal shape does not belong to the declared transport mode.
    #[error("the presented principal does not belong to transport mode {mode}")]
    PrincipalModeMismatch {
        /// Stable name of the declared transport mode.
        mode: &'static str,
    },
    /// The tool name is absent from every canonical profile manifest.
    #[error("unknown tool")]
    UnknownTool,
}
