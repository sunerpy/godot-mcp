//! Honest provenance and confidence contracts.

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};

use super::{ContractVersion, ContractViolation, require_non_empty};
use crate::{Confidence, ErrorKind};

/// Evidence class supporting a public static answer.
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
pub enum ProvenanceSource {
    /// Direct syntax-tree evidence.
    StaticSyntax,
    /// A complete deterministic index lookup.
    StaticIndex,
    /// A project setting or descriptor parsed without ambiguity.
    ProjectMetadata,
    /// Heuristic evidence that may have more than one interpretation.
    Heuristic,
    /// A reference whose target depends on runtime values.
    DynamicReference,
    /// Runtime-observed evidence.
    RuntimeObservation,
    /// Authoritative editor state.
    EditorObservation,
    /// No usable evidence resolved the subject.
    Unresolved,
}

impl ProvenanceSource {
    fn permits(self, confidence: Confidence) -> bool {
        match self {
            Self::StaticSyntax | Self::StaticIndex | Self::ProjectMetadata => {
                !matches!(confidence, Confidence::Unresolved)
            }
            Self::RuntimeObservation | Self::EditorObservation => confidence == Confidence::Exact,
            Self::Heuristic | Self::DynamicReference => confidence == Confidence::Inferred,
            Self::Unresolved => confidence == Confidence::Unresolved,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProvenanceWire {
    schema_version: ContractVersion,
    source: ProvenanceSource,
    confidence: Confidence,
    subject: String,
    project_generation: u64,
}

/// Evidence and honesty classification carried by every public static result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    schema_version: ContractVersion,
    source: ProvenanceSource,
    confidence: Confidence,
    subject: String,
    project_generation: u64,
}

impl Provenance {
    /// Construct provenance while enforcing source/confidence honesty.
    pub fn new(
        source: ProvenanceSource,
        confidence: Confidence,
        subject: impl Into<String>,
        project_generation: u64,
    ) -> Result<Self, ContractViolation> {
        let subject = subject.into();
        require_non_empty(&subject, "provenance subject")?;
        if !source.permits(confidence) {
            return Err(ContractViolation::new(
                ErrorKind::Uncertain,
                format!("{source:?} evidence cannot be labeled {confidence:?}"),
            ));
        }
        Ok(Self {
            schema_version: ContractVersion::current(),
            source,
            confidence,
            subject,
            project_generation,
        })
    }

    /// Evidence class.
    #[must_use]
    pub const fn source(&self) -> ProvenanceSource {
        self.source
    }

    /// Honest confidence level.
    #[must_use]
    pub const fn confidence(&self) -> Confidence {
        self.confidence
    }

    /// Project generation against which this evidence was collected.
    #[must_use]
    pub const fn project_generation(&self) -> u64 {
        self.project_generation
    }
}

impl<'de> Deserialize<'de> for Provenance {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ProvenanceWire::deserialize(deserializer)?;
        if wire.schema_version != ContractVersion::current() {
            return Err(D::Error::custom("unsupported provenance schema version"));
        }
        Self::new(
            wire.source,
            wire.confidence,
            wire.subject,
            wire.project_generation,
        )
        .map_err(D::Error::custom)
    }
}
