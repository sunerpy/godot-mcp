//! Public artifact authorization and range contracts.

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};

use super::{
    ArtifactId, ArtifactResourceUri, ContractVersion, ContractViolation, PrincipalId,
    ProjectIdentity, SessionId, require_non_empty,
};
use crate::ErrorKind;

/// Public artifact media family.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Screenshot,
    Recording,
    DiagnosticBundle,
    Report,
    Export,
}

/// Principal/session restrictions attached to an artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArtifactOwner {
    pub principal: Option<PrincipalId>,
    pub session: Option<SessionId>,
}

impl ArtifactOwner {
    fn validate(&self) -> Result<(), ContractViolation> {
        if self.principal.is_none() && self.session.is_none() {
            return Err(ContractViolation::new(
                ErrorKind::InvalidInput,
                "artifact owner must constrain a principal, a session, or both",
            ));
        }
        Ok(())
    }
}

/// Public artifact metadata. No storage locator is representable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArtifactMetadata {
    pub kind: ArtifactKind,
    pub media_type: String,
    pub byte_length: u64,
    pub sha256: String,
    pub created_at_unix_ms: u64,
}

impl ArtifactMetadata {
    fn validate(&self) -> Result<(), ContractViolation> {
        require_non_empty(&self.media_type, "artifact media type")?;
        if self.sha256.len() != 64
            || !self
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(ContractViolation::new(
                ErrorKind::InvalidInput,
                "artifact sha256 must be 64 lowercase hexadecimal characters",
            ));
        }
        Ok(())
    }
}

/// Inclusive byte range normalized for an artifact response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ByteRange {
    pub start: u64,
    pub end_inclusive: u64,
}

impl ByteRange {
    /// Create a valid inclusive range.
    pub fn new(start: u64, end_inclusive: u64) -> Result<Self, ContractViolation> {
        if start > end_inclusive {
            return Err(ContractViolation::new(
                ErrorKind::InvalidInput,
                "byte range start must not exceed its inclusive end",
            ));
        }
        Ok(Self {
            start,
            end_inclusive,
        })
    }
}

/// Limits and supported semantics for artifact reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RangeSemantics {
    total_bytes: u64,
    maximum_response_bytes: u64,
    accepts_byte_ranges: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RangeWire {
    total_bytes: u64,
    maximum_response_bytes: u64,
    accepts_byte_ranges: bool,
}

impl RangeSemantics {
    /// Construct bounded artifact range semantics.
    pub fn new(
        total_bytes: u64,
        maximum_response_bytes: u64,
        accepts_byte_ranges: bool,
    ) -> Result<Self, ContractViolation> {
        if total_bytes == 0 || maximum_response_bytes == 0 {
            return Err(ContractViolation::new(
                ErrorKind::InvalidInput,
                "artifact byte limits must be positive",
            ));
        }
        if maximum_response_bytes > total_bytes {
            return Err(ContractViolation::new(
                ErrorKind::InvalidInput,
                "maximum response bytes must not exceed total bytes",
            ));
        }
        Ok(Self {
            total_bytes,
            maximum_response_bytes,
            accepts_byte_ranges,
        })
    }

    /// Validate and normalize an optional inclusive requested range.
    pub fn resolve(&self, requested: Option<ByteRange>) -> Result<ByteRange, ContractViolation> {
        let requested = requested.unwrap_or(ByteRange {
            start: 0,
            end_inclusive: self.total_bytes - 1,
        });
        if !self.accepts_byte_ranges
            && (requested.start != 0 || requested.end_inclusive != self.total_bytes - 1)
        {
            return Err(ContractViolation::new(
                ErrorKind::Unsupported,
                "artifact does not accept partial byte ranges",
            ));
        }
        if requested.end_inclusive >= self.total_bytes {
            return Err(ContractViolation::new(
                ErrorKind::InvalidInput,
                "byte range exceeds the artifact length",
            ));
        }
        let response_bytes = requested.end_inclusive - requested.start + 1;
        if response_bytes > self.maximum_response_bytes {
            return Err(ContractViolation::new(
                ErrorKind::InvalidInput,
                "byte range exceeds the maximum response size",
            ));
        }
        Ok(requested)
    }
}

impl<'de> Deserialize<'de> for RangeSemantics {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = RangeWire::deserialize(deserializer)?;
        Self::new(
            wire.total_bytes,
            wire.maximum_response_bytes,
            wire.accepts_byte_ranges,
        )
        .map_err(D::Error::custom)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactWire {
    schema_version: ContractVersion,
    artifact_id: ArtifactId,
    resource_uri: ArtifactResourceUri,
    project: ProjectIdentity,
    owner: Option<ArtifactOwner>,
    metadata: ArtifactMetadata,
    expires_at_unix_ms: u64,
    ranges: RangeSemantics,
}

/// Complete public artifact contract, intentionally incapable of carrying a path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArtifactContract {
    schema_version: ContractVersion,
    artifact_id: ArtifactId,
    resource_uri: ArtifactResourceUri,
    project: ProjectIdentity,
    owner: Option<ArtifactOwner>,
    metadata: ArtifactMetadata,
    expires_at_unix_ms: u64,
    ranges: RangeSemantics,
}

impl ArtifactContract {
    /// Construct and validate an artifact contract.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        artifact_id: ArtifactId,
        resource_uri: ArtifactResourceUri,
        project: ProjectIdentity,
        owner: Option<ArtifactOwner>,
        metadata: ArtifactMetadata,
        expires_at_unix_ms: u64,
        ranges: RangeSemantics,
    ) -> Result<Self, ContractViolation> {
        if resource_uri != ArtifactResourceUri::for_artifact(&artifact_id) {
            return Err(ContractViolation::new(
                ErrorKind::InvalidInput,
                "artifact ID and resource URI do not identify the same artifact",
            ));
        }
        if let Some(owner) = &owner {
            owner.validate()?;
        }
        metadata.validate()?;
        if metadata.byte_length != ranges.total_bytes {
            return Err(ContractViolation::new(
                ErrorKind::InvalidInput,
                "artifact metadata length must equal range total bytes",
            ));
        }
        if expires_at_unix_ms <= metadata.created_at_unix_ms {
            return Err(ContractViolation::new(
                ErrorKind::InvalidInput,
                "artifact expiry must follow creation",
            ));
        }
        Ok(Self {
            schema_version: ContractVersion::current(),
            artifact_id,
            resource_uri,
            project,
            owner,
            metadata,
            expires_at_unix_ms,
            ranges,
        })
    }

    #[must_use]
    pub fn artifact_id(&self) -> &ArtifactId {
        &self.artifact_id
    }

    #[must_use]
    pub fn resource_uri(&self) -> &ArtifactResourceUri {
        &self.resource_uri
    }

    /// Project object and generation this artifact belongs to.
    #[must_use]
    pub const fn project(&self) -> &ProjectIdentity {
        &self.project
    }

    /// Optional principal/session restrictions for reads.
    #[must_use]
    pub const fn owner(&self) -> Option<&ArtifactOwner> {
        self.owner.as_ref()
    }

    /// Public media metadata for the artifact.
    #[must_use]
    pub const fn metadata(&self) -> &ArtifactMetadata {
        &self.metadata
    }

    /// Artifact expiry timestamp in Unix milliseconds.
    #[must_use]
    pub const fn expires_at_unix_ms(&self) -> u64 {
        self.expires_at_unix_ms
    }

    /// Bounded byte-range policy applied to every read.
    #[must_use]
    pub const fn ranges(&self) -> &RangeSemantics {
        &self.ranges
    }
}

impl<'de> Deserialize<'de> for ArtifactContract {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ArtifactWire::deserialize(deserializer)?;
        if wire.schema_version != ContractVersion::current() {
            return Err(D::Error::custom("unsupported artifact schema version"));
        }
        Self::new(
            wire.artifact_id,
            wire.resource_uri,
            wire.project,
            wire.owner,
            wire.metadata,
            wire.expires_at_unix_ms,
            wire.ranges,
        )
        .map_err(D::Error::custom)
    }
}
