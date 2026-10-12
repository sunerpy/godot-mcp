//! Opaque identifiers and public resource URI types.

use std::fmt;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

use super::{ContractViolation, require_non_empty};
use crate::ErrorKind;
use crate::objid::LeaseKey;

fn is_lower_hex(value: &str, minimum: usize, maximum: usize) -> bool {
    (minimum..=maximum).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_prefixed_hex(
    value: &str,
    prefix: &'static str,
    minimum: usize,
    maximum: usize,
) -> Result<(), ContractViolation> {
    let Some(suffix) = value.strip_prefix(prefix) else {
        return Err(ContractViolation::new(
            ErrorKind::InvalidInput,
            format!("identifier must start with {prefix}"),
        ));
    };
    if !is_lower_hex(suffix, minimum, maximum) {
        return Err(ContractViolation::new(
            ErrorKind::InvalidInput,
            format!("{prefix} identifier payload must be lowercase hexadecimal"),
        ));
    }
    Ok(())
}

macro_rules! opaque_id {
    ($name:ident, $prefix:literal, $minimum:literal, $maximum:literal) => {
        #[doc = concat!("Validated opaque `", $prefix, "` identifier.")]
        #[derive(
            Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, schemars::JsonSchema,
        )]
        #[serde(transparent)]
        #[schemars(transparent)]
        pub struct $name(String);

        impl $name {
            #[doc = "Parse and validate the opaque identifier."]
            pub fn parse(value: impl Into<String>) -> Result<Self, ContractViolation> {
                let value = value.into();
                validate_prefixed_hex(&value, $prefix, $minimum, $maximum)?;
                Ok(Self(value))
            }

            #[doc = "Return the opaque wire value."]
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::parse(value).map_err(D::Error::custom)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}

opaque_id!(ArtifactId, "artifact_", 64, 64);
opaque_id!(OpaqueObjectId, "object_", 16, 64);
opaque_id!(PrincipalId, "principal_", 16, 64);
opaque_id!(ProjectObjectId, "project_", 64, 64);
opaque_id!(SessionId, "session_", 16, 64);

impl ProjectObjectId {
    pub(crate) fn from_lease_key(key: LeaseKey) -> Self {
        fn bind(hasher: &mut Sha256, label: &[u8], identity: crate::objid::StableObjectId) {
            hasher.update((label.len() as u64).to_le_bytes());
            hasher.update(label);
            identity.encode(|field| {
                hasher.update((field.len() as u64).to_le_bytes());
                hasher.update(field);
            });
        }

        let mut hasher = Sha256::new();
        hasher.update(b"godot-mcp-public-project-object-v1");
        bind(&mut hasher, b"root", key.root);
        bind(&mut hasher, b"project.godot", key.project_file);
        let digest = hasher.finalize();
        let mut value = String::with_capacity(72);
        value.push_str("project_");
        for byte in digest {
            use std::fmt::Write as _;
            write!(&mut value, "{byte:02x}").expect("writing to String cannot fail");
        }
        Self(value)
    }
}

/// Opaque public URI for a generated artifact.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, schemars::JsonSchema)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct ArtifactResourceUri(String);

impl ArtifactResourceUri {
    const PREFIX: &'static str = "godot-mcp://artifacts/";

    /// Derive the only valid public resource URI for an artifact ID.
    #[must_use]
    pub fn for_artifact(id: &ArtifactId) -> Self {
        let suffix = id
            .as_str()
            .strip_prefix("artifact_")
            .expect("ArtifactId always has the artifact prefix");
        Self(format!("{}{suffix}", Self::PREFIX))
    }

    /// Parse a public artifact URI.
    pub fn parse(value: impl Into<String>) -> Result<Self, ContractViolation> {
        let value = value.into();
        let Some(suffix) = value.strip_prefix(Self::PREFIX) else {
            return Err(ContractViolation::new(
                ErrorKind::InvalidInput,
                "artifact resource URI must use godot-mcp://artifacts/",
            ));
        };
        if !is_lower_hex(suffix, 64, 64) {
            return Err(ContractViolation::new(
                ErrorKind::InvalidInput,
                "artifact resource URI payload must be 64 lowercase hexadecimal characters",
            ));
        }
        Ok(Self(value))
    }

    /// Return the validated URI.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for ArtifactResourceUri {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

impl fmt::Display for ArtifactResourceUri {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Opaque URI for a non-artifact public resource.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, schemars::JsonSchema)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct ResourceUri(String);

impl ResourceUri {
    const PREFIX: &'static str = "godot-mcp://resources/";

    /// Parse an opaque resource URI.
    pub fn parse(value: impl Into<String>) -> Result<Self, ContractViolation> {
        let value = value.into();
        let Some(suffix) = value.strip_prefix(Self::PREFIX) else {
            return Err(ContractViolation::new(
                ErrorKind::InvalidInput,
                "resource URI must use godot-mcp://resources/",
            ));
        };
        if !is_lower_hex(suffix, 32, 64) {
            return Err(ContractViolation::new(
                ErrorKind::InvalidInput,
                "resource URI payload must be 32 to 64 lowercase hexadecimal characters",
            ));
        }
        Ok(Self(value))
    }

    /// Return the validated URI.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for ResourceUri {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

/// Project-relative Godot `res://` resource name, never a local path.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, schemars::JsonSchema)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct ProjectResource(String);

impl ProjectResource {
    /// Parse a normalized `res://` resource name.
    pub fn parse(value: impl Into<String>) -> Result<Self, ContractViolation> {
        let value = value.into();
        require_non_empty(&value, "project resource")?;
        let Some(relative) = value.strip_prefix("res://") else {
            return Err(ContractViolation::new(
                ErrorKind::InvalidInput,
                "project resource must use res://",
            ));
        };
        if relative.is_empty()
            || relative.starts_with('/')
            || relative.contains('\\')
            || relative
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
        {
            return Err(ContractViolation::new(
                ErrorKind::InvalidInput,
                "project resource must be normalized and project relative",
            ));
        }
        Ok(Self(value))
    }

    /// Return the validated `res://` name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for ProjectResource {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}
