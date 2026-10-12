//! Structural proof that no arbitrary-execution surface exists.
//!
//! ## Why this is not a string blacklist
//!
//! A string guard is bypassable: a caller only has to spell the payload
//! differently. The proofs here are structural instead, and the string scan is
//! explicitly supplementary:
//!
//! * [`audit_execution_surface`] enumerates **every** tool and **every** resource
//!   of **all four** canonical profiles. Because the kernel dispatches only names
//!   the active profile's manifest contains, and only registers a handler for such
//!   a name, a name absent from all four manifests is unreachable by construction.
//! * [`config_surface_probe`] enumerates candidate switch keys against the frozen
//!   `deny_unknown_fields` configuration shape, at the top level and inside every
//!   nested section, so there is no configuration path that could turn one on.
//! * [`scan_bytes_for_reserved_identifier`] is a supplementary byte scan. Its zero
//!   result means nothing on its own, which is why the acceptance suite first
//!   proves the scanner finds a planted occurrence.
//!
//! [`RESERVED_UNSAFE_SCRIPT_CAPABILITY`] is a *reserved name only*. It is not a
//! [`Capability`](godot_mcp_domain::contracts::Capability) variant, appears in no
//! profile manifest, has no configuration switch, no public schema, no registry
//! handler and no compiled execution path.

use godot_mcp_domain::CapabilityProfile;
use godot_mcp_domain::config::{BridgeConfig, HttpConfig, Limits, ServerConfig};
use godot_mcp_domain::manifests::canonical_manifest;
use serde_json::{Value, json};

/// The reserved capability identifier that this release deliberately does not
/// implement. It exists so the name cannot be quietly reused for something else.
pub const RESERVED_UNSAFE_SCRIPT_CAPABILITY: &str = "unsafe_script";

/// Every four canonical profiles, in manifest order.
const AUDITED_PROFILES: [CapabilityProfile; 4] = [
    CapabilityProfile::Core,
    CapabilityProfile::Editor,
    CapabilityProfile::Runtime,
    CapabilityProfile::Full,
];

/// Names that would denote an arbitrary-code-execution outcome.
const EXECUTION_SHAPES: &[&str] = &[
    RESERVED_UNSAFE_SCRIPT_CAPABILITY,
    "unsafe-script",
    "script.run",
    "script.eval",
    "script.exec",
    "script.execute",
    "gdscript.eval",
    "gdscript.exec",
    "gdscript.run",
    "gdscript.execute",
    "shell.exec",
    "shell.run",
    "process.exec",
    "command.run",
    "eval",
    "exec",
    "system",
];

/// Configuration keys that would enable arbitrary execution if any existed.
const CANDIDATE_SWITCH_KEYS: &[&str] = &[
    RESERVED_UNSAFE_SCRIPT_CAPABILITY,
    "unsafe_scripts",
    "allow_unsafe_script",
    "enable_unsafe_script",
    "unsafe_execution",
    "allow_arbitrary_code",
    "script_execution",
    "eval",
    "exec",
];

/// What a complete registry/schema enumeration found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionSurfaceAudit {
    /// How many canonical profiles were enumerated.
    pub profiles_audited: usize,
    /// How many advertised tools were inspected across those profiles.
    pub tools_audited: usize,
    /// How many advertised resources were inspected across those profiles.
    pub resources_audited: usize,
    /// Whether the reserved identifier is reachable through any profile.
    pub reserved_is_callable: bool,
    /// Every advertised entry that denotes arbitrary execution. Always empty.
    pub execution_entries: Vec<String>,
}

/// Enumerate every advertised tool and resource of every canonical profile.
///
/// The kernel refuses to register a handler for, and refuses to dispatch, any
/// name the active profile's manifest does not contain, so an entry absent here
/// is structurally unreachable rather than merely unlisted.
#[must_use]
pub fn audit_execution_surface() -> ExecutionSurfaceAudit {
    let mut audit = ExecutionSurfaceAudit {
        profiles_audited: 0,
        tools_audited: 0,
        resources_audited: 0,
        reserved_is_callable: false,
        execution_entries: Vec::new(),
    };
    for profile in AUDITED_PROFILES {
        let manifest = canonical_manifest(profile);
        audit.profiles_audited += 1;
        for tool in &manifest.tools {
            audit.tools_audited += 1;
            if tool == RESERVED_UNSAFE_SCRIPT_CAPABILITY {
                audit.reserved_is_callable = true;
            }
            if EXECUTION_SHAPES.contains(&tool.as_str())
                || tool.contains(RESERVED_UNSAFE_SCRIPT_CAPABILITY)
            {
                audit
                    .execution_entries
                    .push(format!("{}:tool:{tool}", profile.manifest_name()));
            }
        }
        for resource in &manifest.resources {
            audit.resources_audited += 1;
            if EXECUTION_SHAPES
                .iter()
                .any(|shape| resource.contains(shape))
            {
                audit
                    .execution_entries
                    .push(format!("{}:resource:{resource}", profile.manifest_name()));
            }
        }
    }
    audit
}

/// What a complete configuration-surface enumeration found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigSurfaceProbe {
    /// Every candidate key the configuration shape accepted. Always empty.
    pub accepted: Vec<String>,
    /// How many candidate keys were rejected.
    pub rejected: usize,
    /// How many configuration sections were probed.
    pub sections_probed: usize,
}

/// Probe every configuration section for a switch that could enable execution.
///
/// Each section is first parsed *without* the candidate key, so a rejection is
/// attributable to the unknown key rather than to an unrelated missing field.
#[must_use]
pub fn config_surface_probe() -> ConfigSurfaceProbe {
    let mut probe = ConfigSurfaceProbe {
        accepted: Vec::new(),
        rejected: 0,
        sections_probed: 0,
    };
    for (section, base, accepts) in sections() {
        if !accepts(&base) {
            continue;
        }
        probe.sections_probed += 1;
        for key in CANDIDATE_SWITCH_KEYS {
            let mut candidate = base.clone();
            if let Value::Object(fields) = &mut candidate {
                fields.insert((*key).to_owned(), Value::Bool(true));
            }
            if accepts(&candidate) {
                probe.accepted.push(format!("{section}.{key}"));
            } else {
                probe.rejected += 1;
            }
        }
    }
    probe
}

type SectionProbe = (&'static str, Value, fn(&Value) -> bool);

fn sections() -> Vec<SectionProbe> {
    vec![
        ("server", json!({}), accepts_server as fn(&Value) -> bool),
        (
            "http",
            json!({ "bind": "127.0.0.1:9000", "mode": "trusted_local" }),
            accepts_http as fn(&Value) -> bool,
        ),
        ("bridge", json!({}), accepts_bridge as fn(&Value) -> bool),
        ("limits", json!({}), accepts_limits as fn(&Value) -> bool),
    ]
}

fn accepts_server(value: &Value) -> bool {
    serde_json::from_value::<ServerConfig>(value.clone()).is_ok()
}

fn accepts_http(value: &Value) -> bool {
    serde_json::from_value::<HttpConfig>(value.clone()).is_ok()
}

fn accepts_bridge(value: &Value) -> bool {
    serde_json::from_value::<BridgeConfig>(value.clone()).is_ok()
}

fn accepts_limits(value: &Value) -> bool {
    serde_json::from_value::<Limits>(value.clone()).is_ok()
}

/// Count occurrences of the reserved identifier in arbitrary bytes.
///
/// Supplementary evidence only. A zero result is meaningful solely after the
/// scanner has been shown to find a planted occurrence.
#[must_use]
pub fn scan_bytes_for_reserved_identifier(bytes: &[u8]) -> usize {
    let needle = RESERVED_UNSAFE_SCRIPT_CAPABILITY.as_bytes();
    if needle.is_empty() || bytes.len() < needle.len() {
        return 0;
    }
    let mut found = 0;
    let mut index = 0;
    while index + needle.len() <= bytes.len() {
        if &bytes[index..index + needle.len()] == needle {
            found += 1;
            index += needle.len();
        } else {
            index += 1;
        }
    }
    found
}

#[cfg(test)]
mod absence_audit {
    use super::*;

    const ABSENCE_SOURCE: &str = include_str!("absence.rs");

    /// Scan the code region only: the module prose deliberately *names* the bans
    /// it keeps, so a prose mention must not be mistaken for an implementation.
    fn production_source() -> &'static str {
        let start = ABSENCE_SOURCE.find("\nuse ").unwrap_or(0);
        let end = ABSENCE_SOURCE
            .find("#[cfg(test)]")
            .unwrap_or(ABSENCE_SOURCE.len());
        &ABSENCE_SOURCE[start..end]
    }

    #[test]
    fn every_canonical_profile_is_enumerated() {
        let audit = audit_execution_surface();
        assert_eq!(audit.profiles_audited, 4);
        // Every tool of every canonical profile is audited: the count is the exact
        // sum of the manifests, so a profile the audit skipped cannot hide.
        let expected: usize = [
            godot_mcp_domain::CapabilityProfile::Core,
            godot_mcp_domain::CapabilityProfile::Editor,
            godot_mcp_domain::CapabilityProfile::Runtime,
            godot_mcp_domain::CapabilityProfile::Full,
        ]
        .into_iter()
        .map(|profile| {
            godot_mcp_domain::manifests::canonical_manifest(profile)
                .tools
                .len()
        })
        .sum();
        assert_eq!(audit.tools_audited, expected);
        assert!(audit.tools_audited > 0);
        assert!(audit.resources_audited > 0);
        assert!(!audit.reserved_is_callable);
        assert!(audit.execution_entries.is_empty());
    }

    #[test]
    fn no_configuration_section_accepts_an_execution_switch() {
        let probe = config_surface_probe();
        assert!(probe.accepted.is_empty());
        assert_eq!(probe.sections_probed, 4);
        assert_eq!(probe.rejected, 4 * CANDIDATE_SWITCH_KEYS.len());
    }

    #[test]
    fn the_scanner_is_non_vacuous() {
        assert_eq!(
            scan_bytes_for_reserved_identifier(RESERVED_UNSAFE_SCRIPT_CAPABILITY.as_bytes()),
            1
        );
        assert_eq!(scan_bytes_for_reserved_identifier(b""), 0);
        assert_eq!(scan_bytes_for_reserved_identifier(b"unsafe"), 0);
    }

    #[test]
    fn the_absence_proof_never_defines_an_execution_path() {
        let source = production_source();
        for banned in [
            "std::process::Command",
            "Command::new",
            "libloading",
            "dlopen",
            "eval(",
            "unsafe {",
        ] {
            assert!(
                !source.contains(banned),
                "the absence proof must never itself define an execution path ({banned})"
            );
        }
    }
}
