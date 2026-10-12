//! A real CycloneDX 1.5 SBOM, derived from the committed lock file.
//!
//! The SBOM describes exactly the crates the shipping binary links — the
//! transitive closure of `godot-mcp-cli` in `Cargo.lock` — and nothing else, so
//! a component that appears here is genuinely inside the artifact and a crate
//! that ships cannot be missing from here.
//!
//! It is generated offline from committed inputs and carries no build clock, so
//! two builds of the same commit produce byte-identical SBOM bytes. The
//! `serialNumber` is a digest of the component set rather than a random UUID for
//! the same reason.

use sha2::{Digest, Sha256};

use super::lock::CargoLock;

/// CycloneDX spec version this generator emits.
pub const SPEC_VERSION: &str = "1.5";

/// A deterministic UUID URN derived from `material`.
fn deterministic_serial_number(material: &str) -> String {
    let digest = Sha256::digest(material.as_bytes());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    // Force the version (4) and variant (8) nibbles so the value is a
    // syntactically valid UUID while staying a pure function of the inputs.
    format!(
        "urn:uuid:{}-{}-4{}-8{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[13..16],
        &hex[17..20],
        &hex[20..32]
    )
}

/// Build the CycloneDX document for the release artifact named `component_name`
/// at `component_version`, describing the linked closure of `root_crate`.
///
/// # Errors
/// Returns an error if the lock does not contain `root_crate` or names a
/// dependency it does not lock.
pub fn from_cargo_lock(
    lock: &CargoLock,
    root_crate: &str,
    component_name: &str,
    component_version: &str,
) -> Result<serde_json::Value, String> {
    let linked = lock.linked_crates(root_crate)?;
    let mut components = Vec::new();
    let mut material = String::new();
    for package in &lock.packages {
        if !linked.contains(&package.name) {
            continue;
        }
        let purl = format!("pkg:cargo/{}@{}", package.name, package.version);
        material.push_str(&purl);
        material.push('\n');
        components.push(serde_json::json!({
            "type": "library",
            "bom-ref": purl,
            "name": package.name,
            "version": package.version,
            "purl": purl,
            "scope": "required",
        }));
    }
    components.sort_by(|left, right| {
        left["bom-ref"]
            .as_str()
            .unwrap_or_default()
            .cmp(right["bom-ref"].as_str().unwrap_or_default())
    });
    Ok(serde_json::json!({
        "bomFormat": "CycloneDX",
        "specVersion": SPEC_VERSION,
        "version": 1,
        "serialNumber": deterministic_serial_number(&material),
        "metadata": {
            "component": {
                "type": "application",
                "bom-ref": format!("pkg:cargo/{component_name}@{component_version}"),
                "name": component_name,
                "version": component_version,
            },
            "properties": [
                {
                    "name": "godot-mcp:sbom-source",
                    "value": "Cargo.lock",
                },
                {
                    "name": "godot-mcp:linked-root-crate",
                    "value": root_crate,
                },
            ],
        },
        "components": components,
    }))
}

/// Serialize an SBOM document to the exact bytes that ship.
///
/// # Errors
/// Returns an error if the document cannot be serialized.
pub fn to_bytes(document: &serde_json::Value) -> Result<Vec<u8>, String> {
    let mut bytes = serde_json::to_vec_pretty(document)
        .map_err(|error| format!("serialize CycloneDX SBOM: {error}"))?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// The component names an SBOM document declares, for the release-graph audit.
#[must_use]
pub fn component_names(document: &serde_json::Value) -> Vec<String> {
    document
        .get("components")
        .and_then(serde_json::Value::as_array)
        .map(|components| {
            components
                .iter()
                .filter_map(|component| component.get("name"))
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}
