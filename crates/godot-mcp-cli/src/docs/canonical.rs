//! The canonical inventory every documentation page is checked against.
//!
//! Nothing here is a hand-maintained list of names. Each set is derived from the
//! single owner of that name in this repository:
//!
//! | Inventory | Canonical owner |
//! | --- | --- |
//! | tools, resources, capabilities per profile | [`godot_mcp_domain::manifests::canonical_manifest`] |
//! | capability / plane / error-code / profile vocabularies | the `schemars` schema of the domain enum itself |
//! | recognized `GODOT_MCP_*` environment variables | probed through the real [`godot_mcp_domain::config::load_config`] |
//! | configuration key paths | the accept-annotated TOML examples, after the real loader accepted them |
//! | bridge protocol vocabulary | `schemas/bridge/protocol-v1.json` |
//! | HTTP protocol constants | `godot_mcp_server::http`, bound by the all-features test below |
//! | Godot versions | `tests/fixtures/godot-releases.lock.json` |
//!
//! The two HTTP protocol constants are the one place a canonical value is
//! restated rather than imported, because `godot_mcp_server::http` only exists
//! behind that crate's optional `http` feature while the documentation checker
//! must run under the default feature set too. The restatement is not trusted:
//! `http_protocol_constants_match_the_server` asserts byte equality with the
//! server's own constants, and that test runs under the required
//! `cargo test --workspace --all-features` gate.
//!
//! A stale name therefore cannot be "fixed" by editing a list in this file; the
//! only way to change the inventory is to change the owning code, schema or lock.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use godot_mcp_domain::CapabilityProfile;
use godot_mcp_domain::config::{CliOverrides, ConfigError, ServerConfig, load_config};
use godot_mcp_domain::manifests::canonical_manifest;

use crate::release_gates::lock_matrix::GodotReleasesLock;

/// The four canonical profiles, in manifest order.
pub const PROFILES: [CapabilityProfile; 4] = [
    CapabilityProfile::Core,
    CapabilityProfile::Editor,
    CapabilityProfile::Runtime,
    CapabilityProfile::Full,
];

/// The bridge wire-schema path, relative to the workspace root.
pub const BRIDGE_SCHEMA: &str = "schemas/bridge/protocol-v1.json";

/// The recognized-environment-variable prefix. A documented name outside this
/// prefix is not a `godot-mcp` configuration variable and is not probed.
pub const ENV_PREFIX: &str = "GODOT_MCP_";

/// The MCP request path the HTTP backend serves, bound to
/// `godot_mcp_server::http::MCP_PATH` by an all-features test.
pub const HTTP_MCP_PATH: &str = "/mcp";

/// The authenticated proxy-hop header, bound to
/// `godot_mcp_server::http::PROXY_HOP_HEADER` by an all-features test.
pub const HTTP_PROXY_HOP_HEADER: &str = "x-godot-mcp-proxy-hop";

/// One profile's canonical surface.
#[derive(Debug, Clone)]
pub struct ProfileSurface {
    /// Manifest name (`core`, `editor`, `runtime`, `full`).
    pub name: &'static str,
    /// Capability wire names.
    pub capabilities: BTreeSet<String>,
    /// Tool names.
    pub tools: BTreeSet<String>,
    /// Tool names listed before a workspace server binds its project.
    pub bootstrap_tools: BTreeSet<String>,
    /// Resource URIs.
    pub resources: BTreeSet<String>,
}

/// The complete canonical inventory.
#[derive(Debug, Clone)]
pub struct Canonical {
    /// Per-profile surfaces, in manifest order.
    pub profiles: Vec<ProfileSurface>,
    /// Union of every profile's tools.
    pub tools: BTreeSet<String>,
    /// Every tool the frozen version 0.1 contract listed, for historical reports.
    pub historical_tools: BTreeSet<String>,
    /// Union of every profile's resources.
    pub resources: BTreeSet<String>,
    /// Capability wire vocabulary.
    pub capabilities: BTreeSet<String>,
    /// Operation-plane wire vocabulary.
    pub planes: BTreeSet<String>,
    /// Error-code wire vocabulary.
    pub error_codes: BTreeSet<String>,
    /// Confidence wire vocabulary.
    pub confidences: BTreeSet<String>,
    /// Profile-name wire vocabulary.
    pub profile_names: BTreeSet<String>,
    /// Bridge message kinds.
    pub bridge_kinds: BTreeSet<String>,
    /// Bridge envelope field names.
    pub bridge_fields: BTreeSet<String>,
    /// Bridge mutation-status values.
    pub bridge_mutation_statuses: BTreeSet<String>,
    /// The MCP request path served by the HTTP backend.
    pub http_path: String,
    /// The authenticated proxy-hop header name.
    pub proxy_hop_header: String,
    /// Exact Godot patch versions, in lock order.
    pub godot_versions: Vec<String>,
    /// Every locked release, for the generated version-matrix page.
    pub lock: GodotReleasesLock,
}

impl Canonical {
    /// The surface of one profile.
    ///
    /// # Panics
    /// Panics only if the canonical manifest set were built without a profile,
    /// which [`PROFILES`] makes unrepresentable.
    #[must_use]
    pub fn profile(&self, name: &str) -> &ProfileSurface {
        self.profiles
            .iter()
            .find(|surface| surface.name == name)
            .expect("every canonical profile is present")
    }

    /// Whether `token` is a canonical dotted name of any kind.
    #[must_use]
    pub fn is_known_dotted(&self, token: &str, config_keys: &BTreeSet<String>) -> bool {
        self.tools.contains(token)
            || config_keys.contains(token)
            || self.error_codes.contains(token)
            || self.planes.contains(token)
            || self.capabilities.contains(token)
    }
}

/// Enumerate a `schemars`-derived enum's wire vocabulary from its own schema.
fn enum_vocabulary<T: schemars::JsonSchema>(label: &str) -> Result<BTreeSet<String>, String> {
    let schema = schemars::schema_for!(T);
    let value = serde_json::to_value(&schema)
        .map_err(|error| format!("serialize the {label} schema: {error}"))?;
    // `schemars` renders a plain `enum` for an undocumented unit-variant enum and
    // a `oneOf` of single-`const` branches once any variant carries a doc comment.
    // Both are the same vocabulary, so both shapes are accepted and anything else
    // is an error rather than an empty set.
    let names: BTreeSet<String> =
        if let Some(variants) = value.get("enum").and_then(serde_json::Value::as_array) {
            variants
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_owned)
                .collect()
        } else if let Some(branches) = value.get("oneOf").and_then(serde_json::Value::as_array) {
            branches
                .iter()
                .filter_map(|branch| {
                    branch
                        .get("const")
                        .or_else(|| branch.get("enum").and_then(|inner| inner.get(0)))
                        .and_then(serde_json::Value::as_str)
                })
                .map(str::to_owned)
                .collect()
        } else {
            return Err(format!("the {label} schema is not a string enum"));
        };
    if names.is_empty() {
        return Err(format!("the {label} schema enumerated no variant"));
    }
    Ok(names)
}

/// Whether the real configuration loader recognizes this `GODOT_MCP_*` name.
///
/// The probe deliberately asks the production loader rather than comparing
/// against a copy of its table: any answer other than
/// [`ConfigError::UnknownEnvVar`] means the loader routed the name somewhere, and
/// a value-level complaint about a probe value is still recognition.
#[must_use]
pub fn env_var_is_recognized(name: &str) -> bool {
    if !name.starts_with(ENV_PREFIX) {
        return false;
    }
    let env = BTreeMap::from([(name.to_owned(), "docs-probe".to_owned())]);
    match load_config(None, &env, &CliOverrides::default()) {
        Err(ConfigError::UnknownEnvVar { .. }) => false,
        Ok(_) | Err(_) => true,
    }
}

/// Load a configuration example exactly the way `godot-mcp check-config` does.
///
/// # Errors
/// Returns the loader's own sanitized rejection.
pub fn load_config_example(text: &str) -> Result<ServerConfig, ConfigError> {
    load_config(Some(text), &BTreeMap::new(), &CliOverrides::default())
}

/// Flatten a TOML document into dotted key paths, recording every intermediate
/// table path as well so `http` and `http.principal` are both known keys.
pub fn flatten_config_keys(text: &str, into: &mut BTreeSet<String>) -> Result<(), String> {
    let value: toml::Value =
        toml::from_str(text).map_err(|error| format!("parse config example: {error}"))?;
    flatten_value(&value, "", into);
    Ok(())
}

fn flatten_value(value: &toml::Value, prefix: &str, into: &mut BTreeSet<String>) {
    let Some(table) = value.as_table() else {
        return;
    };
    for (key, child) in table {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        into.insert(path.clone());
        flatten_value(child, &path, into);
    }
}

#[derive(Debug, Clone)]
struct BridgeVocabulary {
    kinds: BTreeSet<String>,
    fields: BTreeSet<String>,
    mutation_statuses: BTreeSet<String>,
}

fn bridge_vocabulary(workspace_root: &Path) -> Result<BridgeVocabulary, String> {
    let path = workspace_root.join(BRIDGE_SCHEMA);
    let bytes =
        std::fs::read(&path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let schema: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse {}: {error}", path.display()))?;
    let properties = schema
        .get("properties")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| format!("{} has no properties table", path.display()))?;
    let fields: BTreeSet<String> = properties.keys().cloned().collect();
    let kinds = schema_string_enum(&schema, &["properties", "kind", "enum"], &path)?;
    let statuses = schema_string_enum(&schema, &["properties", "mutation_status", "enum"], &path)?;
    if fields.is_empty() || kinds.is_empty() {
        return Err(format!("{} enumerated no field or kind", path.display()));
    }
    Ok(BridgeVocabulary {
        kinds,
        fields,
        mutation_statuses: statuses,
    })
}

fn schema_string_enum(
    schema: &serde_json::Value,
    pointer: &[&str],
    path: &Path,
) -> Result<BTreeSet<String>, String> {
    let mut cursor = schema;
    for step in pointer {
        cursor = cursor
            .get(*step)
            .ok_or_else(|| format!("{} has no {}", path.display(), pointer.join("/")))?;
    }
    // `mutation_status` is nullable, so its enum carries an explicit null member;
    // only the string members are vocabulary.
    let array = cursor
        .as_array()
        .ok_or_else(|| format!("{} {} is not an array", path.display(), pointer.join("/")))?;
    Ok(array
        .iter()
        .filter_map(serde_json::Value::as_str)
        .map(str::to_owned)
        .collect())
}

/// Build the canonical inventory.
///
/// # Errors
/// Returns an error if a canonical source is missing, unreadable, or shaped
/// differently than the inventory requires. A missing canonical source is a hard
/// failure, never an empty inventory that would silently pass every check.
pub fn build(workspace_root: &Path) -> Result<Canonical, String> {
    let mut profiles = Vec::new();
    let mut tools = BTreeSet::new();
    let mut resources = BTreeSet::new();
    for profile in PROFILES {
        let manifest = canonical_manifest(profile);
        let capabilities = manifest
            .capabilities
            .iter()
            .map(|capability| {
                serde_json::to_value(capability)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .ok_or_else(|| "a capability has no wire name".to_string())
            })
            .collect::<Result<BTreeSet<String>, String>>()?;
        tools.extend(manifest.tools.iter().cloned());
        // Bootstrap-state tools are canonical names too, listed before a
        // workspace server binds its project.
        tools.extend(manifest.bootstrap_tools.iter().cloned());
        resources.extend(manifest.resources.iter().cloned());
        profiles.push(ProfileSurface {
            name: profile.manifest_name(),
            capabilities,
            tools: manifest.tools.clone(),
            bootstrap_tools: manifest.bootstrap_tools.clone(),
            resources: manifest.resources.clone(),
        });
    }

    let bridge = bridge_vocabulary(workspace_root)?;
    let lock = GodotReleasesLock::load_from_workspace(workspace_root)?;
    let godot_versions: Vec<String> = lock
        .releases
        .iter()
        .map(|release| release.version.clone())
        .collect();
    if godot_versions.is_empty() {
        return Err("the Godot releases lock enumerated no release".to_string());
    }

    Ok(Canonical {
        profiles,
        tools,
        historical_tools: godot_mcp_domain::manifests::historical_manifest_v0_1(
            CapabilityProfile::Full,
        )
        .tools,
        resources,
        capabilities: enum_vocabulary::<godot_mcp_domain::contracts::Capability>("Capability")?,
        planes: enum_vocabulary::<godot_mcp_domain::contracts::OperationPlane>("OperationPlane")?,
        error_codes: enum_vocabulary::<godot_mcp_domain::ErrorKind>("ErrorKind")?,
        confidences: enum_vocabulary::<godot_mcp_domain::Confidence>("Confidence")?,
        profile_names: enum_vocabulary::<CapabilityProfile>("CapabilityProfile")?,
        bridge_kinds: bridge.kinds,
        bridge_fields: bridge.fields,
        bridge_mutation_statuses: bridge.mutation_statuses,
        http_path: HTTP_MCP_PATH.to_owned(),
        proxy_hop_header: HTTP_PROXY_HOP_HEADER.to_owned(),
        godot_versions,
        lock,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> std::path::PathBuf {
        crate::audit::default_workspace_root()
    }

    #[test]
    fn the_inventory_matches_the_canonical_owners() {
        let canonical = build(&root()).expect("the canonical inventory builds");
        assert_eq!(canonical.profile("core").tools.len(), 15);
        assert!(canonical.tools.contains("project.bind"));
        assert_eq!(canonical.capabilities.len(), 5);
        assert_eq!(canonical.planes.len(), 4);
        assert_eq!(canonical.error_codes.len(), 10);
        assert_eq!(canonical.bridge_kinds.len(), 10);
        assert_eq!(canonical.http_path, "/mcp");
        assert_eq!(canonical.proxy_hop_header, "x-godot-mcp-proxy-hop");
        assert!(canonical.tools.contains("capabilities.get"));
        assert!(!canonical.tools.contains("unsafe_script"));
        assert!(
            canonical
                .godot_versions
                .iter()
                .all(|version| version.split('.').count() == 3),
            "every locked version is an exact patch version: {:?}",
            canonical.godot_versions
        );
    }

    #[cfg(feature = "http")]
    #[test]
    fn http_protocol_constants_match_the_server() {
        assert_eq!(HTTP_MCP_PATH, godot_mcp_server::http::MCP_PATH);
        assert_eq!(
            HTTP_PROXY_HOP_HEADER,
            godot_mcp_server::http::PROXY_HOP_HEADER
        );
    }

    #[test]
    fn env_recognition_is_answered_by_the_real_loader() {
        assert!(env_var_is_recognized("GODOT_MCP_TRANSPORT"));
        assert!(env_var_is_recognized("GODOT_MCP_HTTP_BIND"));
        assert!(!env_var_is_recognized("GODOT_MCP_TRANSPORTT"));
        assert!(!env_var_is_recognized("PATH"));
    }

    #[test]
    fn config_keys_flatten_to_every_intermediate_path() {
        let mut keys = BTreeSet::new();
        flatten_config_keys(
            "transport = \"http\"\n[http.principal]\nissuer = \"x\"\n",
            &mut keys,
        )
        .expect("the example flattens");
        assert!(keys.contains("transport"));
        assert!(keys.contains("http"));
        assert!(keys.contains("http.principal"));
        assert!(keys.contains("http.principal.issuer"));
    }
}
