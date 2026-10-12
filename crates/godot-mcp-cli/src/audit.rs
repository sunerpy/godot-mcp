//! Release-graph, architecture-boundary and declaration-surface audits.
//!
//! These are deterministic, offline functions that read the committed workspace
//! manifests and sources and prove three frozen invariants:
//!
//! 1. No shipping crate pulls in the native Godot bridge (crate `godot`, gdext,
//!    `gdext-bridge`, `.gdextension`, or a native bridge library).
//! 2. The domain/analysis/engine crates never import a transport/UI layer crate.
//! 3. The `godot-mcp-cli` crate's advertised `[package] description` names only
//!    command tokens the shipped binary actually defines
//!    ([`undefined_subcommands_advertised`]) — a manifest cannot advertise a
//!    subcommand that does not exist.
//!
//! They back the `release_graph_audit` and `architecture_boundaries` tests that
//! are part of Todo 1's Green gate. The full CycloneDX-SBOM / archive scan lands
//! with Todo 34; this Todo owns the manifest-graph portion.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// A forbidden native-bridge artifact discovered in the shipping graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForbiddenArtifact {
    /// Which manifest / file the artifact was found in.
    pub location: String,
    /// The exact forbidden edge or artifact named for the audit output.
    pub artifact: String,
}

/// Crate/package names that must never appear in the shipping dependency graph.
pub const FORBIDDEN_CRATE_NAMES: &[&str] = &["godot", "gdext", "gdext-bridge"];

/// File extensions / suffixes that must never ship.
pub const FORBIDDEN_FILE_MARKERS: &[&str] = &[".gdextension"];

/// Native dynamic-library suffixes. These are forbidden ONLY when they appear
/// as shipping addon/bridge content (see [`audit_release_graph`]); an ordinary
/// dependency/build library elsewhere in the tree is never flagged.
pub const NATIVE_LIBRARY_SUFFIXES: &[&str] = &[".so", ".dylib", ".dll"];

/// Directory names whose subtree is treated as shipping addon/bridge content.
/// A native library found under one of these is a forbidden native bridge
/// library; a native library anywhere else is an ordinary artifact and ignored.
pub const SHIPPING_ADDON_DIRS: &[&str] = &["addons"];

/// The exact set of shipping (release) workspace members. `qualification-tools`
/// is intentionally excluded: it is a build/test-only member.
pub const SHIPPING_MEMBERS: &[&str] = &[
    "godot-mcp-domain",
    "godot-mcp-analysis",
    "godot-mcp-engine",
    "godot-mcp-bridge",
    "godot-mcp-server",
    "godot-mcp-cli",
];

/// Layers that the domain/analysis/engine crates must never import.
pub const TRANSPORT_UI_CRATES: &[&str] = &["godot-mcp-server", "godot-mcp-cli"];

/// Lower crates whose manifests must not name a transport/UI dependency.
pub const LOWER_LAYER_CRATES: &[&str] =
    &["godot-mcp-domain", "godot-mcp-analysis", "godot-mcp-engine"];

/// Locate the workspace root from an arbitrary starting directory by walking
/// upward until a `Cargo.toml` containing a `[workspace]` table is found.
///
/// # Errors
/// Returns an error if no workspace root can be located above `start`.
pub fn find_workspace_root(start: &Path) -> Result<PathBuf, String> {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        let manifest = dir.join("Cargo.toml");
        if manifest.is_file() {
            let text = std::fs::read_to_string(&manifest)
                .map_err(|e| format!("read {}: {e}", manifest.display()))?;
            if text.contains("[workspace]") {
                return Ok(dir.to_path_buf());
            }
        }
        cur = dir.parent();
    }
    Err(format!(
        "no workspace root (Cargo.toml with [workspace]) found above {}",
        start.display()
    ))
}

/// Best-effort workspace root relative to the crate at compile time. Falls back
/// to the current directory.
#[must_use]
pub fn default_workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is `<root>/crates/godot-mcp-cli` for this crate.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Parse a member crate manifest and return the set of dependency names it
/// declares across `[dependencies]`, `[dev-dependencies]`, `[build-dependencies]`
/// and any `[target.*]` variants.
fn declared_dependency_names(manifest_text: &str) -> Result<BTreeSet<String>, String> {
    let value: toml::Value =
        toml::from_str(manifest_text).map_err(|e| format!("parse manifest: {e}"))?;
    let mut names = BTreeSet::new();
    collect_dependency_tables(&value, &mut names);
    Ok(names)
}

fn collect_dependency_tables(value: &toml::Value, names: &mut BTreeSet<String>) {
    const DEP_KEYS: &[&str] = &["dependencies", "dev-dependencies", "build-dependencies"];
    if let Some(table) = value.as_table() {
        for key in DEP_KEYS {
            if let Some(deps) = table.get(*key).and_then(toml::Value::as_table) {
                for name in deps.keys() {
                    names.insert(name.clone());
                }
            }
        }
        // Recurse into [target.*] tables which nest their own dependency tables.
        if let Some(target) = table.get("target").and_then(toml::Value::as_table) {
            for sub in target.values() {
                collect_dependency_tables(sub, names);
            }
        }
    }
}

/// Scan every shipping member manifest for a forbidden native-bridge dependency
/// or a `.gdextension` marker.
///
/// Returns every forbidden artifact found (empty when clean).
///
/// # Errors
/// Returns an error if a manifest cannot be read or parsed.
pub fn audit_release_graph(workspace_root: &Path) -> Result<Vec<ForbiddenArtifact>, String> {
    let mut found = Vec::new();
    for member in SHIPPING_MEMBERS {
        let manifest = workspace_root
            .join("crates")
            .join(member)
            .join("Cargo.toml");
        let text = std::fs::read_to_string(&manifest)
            .map_err(|e| format!("read {}: {e}", manifest.display()))?;
        let deps = declared_dependency_names(&text)?;
        for forbidden in FORBIDDEN_CRATE_NAMES {
            if deps.contains(*forbidden) {
                found.push(ForbiddenArtifact {
                    location: format!("crates/{member}/Cargo.toml"),
                    artifact: (*forbidden).to_string(),
                });
            }
        }
    }
    // Scan the whole shipping tree for `.gdextension` descriptors.
    for marker in FORBIDDEN_FILE_MARKERS {
        for hit in find_files_with_suffix(workspace_root, marker) {
            found.push(ForbiddenArtifact {
                location: hit,
                artifact: (*marker).to_string(),
            });
        }
    }
    // Scan ONLY the shipping addon tree(s) for a native bridge library. A
    // native `.so`/`.dylib`/`.dll` under `addons/` is shipping bridge content
    // and forbidden; the same suffix anywhere else in the tree is an ordinary
    // build/dependency artifact and is intentionally NOT flagged, so the audit
    // never produces a false positive on a normal dependency library.
    for hit in find_native_bridge_libraries(workspace_root) {
        found.push(ForbiddenArtifact {
            location: hit,
            artifact: "native-bridge-library".to_string(),
        });
    }
    Ok(found)
}

/// Find native dynamic libraries located inside a shipping addon subtree
/// ([`SHIPPING_ADDON_DIRS`]). The search is deliberately scoped: a native
/// library outside every addon subtree is an ordinary artifact and is skipped.
fn find_native_bridge_libraries(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for addon_dir in SHIPPING_ADDON_DIRS {
        let base = root.join(addon_dir);
        if !base.is_dir() {
            continue;
        }
        let mut stack = vec![base];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                if path.is_dir() {
                    stack.push(path);
                } else if NATIVE_LIBRARY_SUFFIXES.iter().any(|s| name.ends_with(s))
                    && let Ok(rel) = path.strip_prefix(root)
                {
                    out.push(repository_path(rel));
                }
            }
        }
    }
    out.sort();
    out
}

/// A path relative to the workspace root, spelled the way the repository spells
/// it — `/`-separated on every host, so a finding reads the same on Windows.
fn repository_path(relative: &Path) -> String {
    relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Recursively find files ending in `suffix`, skipping `target/`, `.git/`,
/// `.omo/` and the isolated non-shipping gdext reference lane if present.
fn find_files_with_suffix(root: &Path, suffix: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if path.is_dir() {
                if matches!(
                    name.as_str(),
                    "target" | ".git" | ".omo" | ".codegraph" | "gdext-reference"
                ) {
                    continue;
                }
                stack.push(path);
            } else if name.ends_with(suffix)
                && let Ok(rel) = path.strip_prefix(root)
            {
                out.push(repository_path(rel));
            }
        }
    }
    out.sort();
    out
}

/// Whether `name` is a native Godot-bridge crate that must never ship.
///
/// Exact-name matching alone is not enough: gdext publishes a whole family
/// (`godot`, `godot-core`, `godot-ffi`, `godot-macros`, `gdext-*`), and a release
/// graph is only clean if none of them is present. The workspace's own
/// `godot-mcp-*` crates share a prefix with that family, so they are excluded by
/// name rather than by hoping the prefix never collides.
#[must_use]
pub fn forbidden_crate_name(name: &str) -> bool {
    if name.starts_with("godot-mcp") {
        return false;
    }
    FORBIDDEN_CRATE_NAMES.contains(&name)
        || name.starts_with("gdext")
        || name == "godot"
        || name.starts_with("godot-")
        || name.starts_with("godot_")
}

/// One archive to audit, as its file name plus the entries it carries.
#[derive(Debug, Clone, Copy)]
pub struct ArchiveUnderAudit<'a> {
    pub file_name: &'a str,
    pub entries: &'a [crate::package::archive::ArchiveEntry],
}

/// Everything the first-release native-dependency guard inspects.
#[derive(Debug, Clone, Copy)]
pub struct ReleaseAuditInputs<'a> {
    pub workspace_root: &'a Path,
    pub sbom: Option<&'a serde_json::Value>,
    pub archives: &'a [ArchiveUnderAudit<'a>],
}

/// The complete first-release native-dependency guard.
///
/// It covers, in one pass, every surface a gdext artifact could reach a user
/// through: the workspace's production Cargo metadata (`[workspace.dependencies]`
/// and every shipping member's dependency **and feature** tables), the committed
/// `Cargo.lock`, the release binary's linked-crate list (the lock's transitive
/// closure from the shipping crate), each produced archive's entries and payload
/// manifest, and the CycloneDX SBOM's components. The manifest/tree scan from
/// [`audit_release_graph`] is included, so `.gdextension` descriptors and native
/// bridge libraries in the tree are still caught.
///
/// Scope exclusions, both deliberate and structural:
///
/// * the isolated non-shipping gdext reference lane (`gdext-reference/`) is
///   skipped, because it never enters the release graph;
/// * `test-hooks`-gated binaries cannot exist in a release build at all, so they
///   are excluded by construction — and any archive that nevertheless carries one
///   is reported.
///
/// # Errors
/// Returns an error if a manifest, the lock, or the linked-crate closure cannot be
/// read or resolved. An unreadable input is an audit failure, never a pass.
pub fn audit_full_release_graph(
    inputs: &ReleaseAuditInputs<'_>,
) -> Result<Vec<ForbiddenArtifact>, String> {
    let root = inputs.workspace_root;
    let mut found = audit_release_graph(root)?;
    found.extend(audit_workspace_dependency_table(root)?);
    found.extend(audit_member_features(root)?);

    let locked = crate::package::lock::CargoLock::load(&root.join("Cargo.lock"))?;
    for name in locked.names() {
        if forbidden_crate_name(&name) {
            found.push(ForbiddenArtifact {
                location: "Cargo.lock".to_string(),
                artifact: name,
            });
        }
    }
    for name in locked.linked_crates(crate::package::RELEASE_ROOT_CRATE)? {
        if forbidden_crate_name(&name) {
            found.push(ForbiddenArtifact {
                location: format!(
                    "linked-crate list of {}",
                    crate::package::RELEASE_ROOT_CRATE
                ),
                artifact: name,
            });
        }
    }

    if let Some(document) = inputs.sbom {
        for name in crate::package::sbom::component_names(document) {
            if forbidden_crate_name(&name) {
                found.push(ForbiddenArtifact {
                    location: "CycloneDX SBOM component".to_string(),
                    artifact: name,
                });
            }
        }
    }

    let test_hooks = crate::package::test_hook_binary_names(root)?;
    for archive in inputs.archives {
        found.extend(audit_archive(archive, &test_hooks)?);
    }

    found.sort_by(|left, right| {
        (&left.location, &left.artifact).cmp(&(&right.location, &right.artifact))
    });
    found.dedup();
    Ok(found)
}

/// Scan `[workspace.dependencies]` in the root manifest.
fn audit_workspace_dependency_table(root: &Path) -> Result<Vec<ForbiddenArtifact>, String> {
    let manifest = root.join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest)
        .map_err(|e| format!("read {}: {e}", manifest.display()))?;
    let value: toml::Value =
        toml::from_str(&text).map_err(|e| format!("parse {}: {e}", manifest.display()))?;
    let mut found = Vec::new();
    if let Some(table) = value
        .get("workspace")
        .and_then(|workspace| workspace.get("dependencies"))
        .and_then(toml::Value::as_table)
    {
        for name in table.keys() {
            if forbidden_crate_name(name) {
                found.push(ForbiddenArtifact {
                    location: "Cargo.toml [workspace.dependencies]".to_string(),
                    artifact: name.clone(),
                });
            }
        }
    }
    Ok(found)
}

/// Scan every shipping member's `[features]` table, keys and values alike, so a
/// native bridge cannot be smuggled in behind a feature name.
fn audit_member_features(root: &Path) -> Result<Vec<ForbiddenArtifact>, String> {
    let mut found = Vec::new();
    for member in SHIPPING_MEMBERS {
        let manifest = root.join("crates").join(member).join("Cargo.toml");
        let Ok(text) = std::fs::read_to_string(&manifest) else {
            continue;
        };
        let value: toml::Value =
            toml::from_str(&text).map_err(|e| format!("parse {}: {e}", manifest.display()))?;
        let Some(features) = value.get("features").and_then(toml::Value::as_table) else {
            continue;
        };
        let location = format!("crates/{member}/Cargo.toml [features]");
        for (name, enables) in features {
            if forbidden_crate_name(name) || name.contains("gdextension") {
                found.push(ForbiddenArtifact {
                    location: location.clone(),
                    artifact: name.clone(),
                });
            }
            let Some(list) = enables.as_array() else {
                continue;
            };
            for enabled in list.iter().filter_map(toml::Value::as_str) {
                let crate_part = enabled.split('/').next().unwrap_or(enabled);
                let crate_part = crate_part.strip_prefix("dep:").unwrap_or(crate_part);
                let crate_part = crate_part.strip_suffix('?').unwrap_or(crate_part);
                if forbidden_crate_name(crate_part) || enabled.contains("gdextension") {
                    found.push(ForbiddenArtifact {
                        location: location.clone(),
                        artifact: enabled.to_string(),
                    });
                }
            }
        }
    }
    Ok(found)
}

/// Scan one archive: its entry paths, its embedded payload manifest, and the
/// binaries it carries.
fn audit_archive(
    archive: &ArchiveUnderAudit<'_>,
    test_hooks: &[String],
) -> Result<Vec<ForbiddenArtifact>, String> {
    let mut found = Vec::new();
    let location = format!("archive {}", archive.file_name);
    for entry in archive.entries {
        let name = entry.path.rsplit('/').next().unwrap_or(entry.path.as_str());
        for marker in FORBIDDEN_FILE_MARKERS {
            if entry.path.ends_with(marker) {
                found.push(ForbiddenArtifact {
                    location: format!("{location}:{}", entry.path),
                    artifact: (*marker).to_string(),
                });
            }
        }
        if NATIVE_LIBRARY_SUFFIXES
            .iter()
            .any(|suffix| entry.path.ends_with(suffix))
        {
            found.push(ForbiddenArtifact {
                location: format!("{location}:{}", entry.path),
                artifact: "native-bridge-library".to_string(),
            });
        }
        let stem = name.strip_suffix(".exe").unwrap_or(name);
        if test_hooks.iter().any(|hook| hook == stem) {
            found.push(ForbiddenArtifact {
                location: format!("{location}:{}", entry.path),
                artifact: format!("test-hooks binary {stem}"),
            });
        }
        if entry.path.ends_with("/MANIFEST.json") || entry.path == "MANIFEST.json" {
            found.extend(audit_payload_manifest(&location, &entry.bytes)?);
        }
    }
    Ok(found)
}

/// Scan an archive's embedded payload manifest, so a forbidden path recorded
/// there is caught even if the entry table were tampered with.
fn audit_payload_manifest(location: &str, bytes: &[u8]) -> Result<Vec<ForbiddenArtifact>, String> {
    let document: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|e| format!("parse payload manifest in {location}: {e}"))?;
    let mut found = Vec::new();
    let files = document
        .get("files")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    for file in files {
        let Some(path) = file.get("path").and_then(serde_json::Value::as_str) else {
            continue;
        };
        for marker in FORBIDDEN_FILE_MARKERS {
            if path.ends_with(marker) {
                found.push(ForbiddenArtifact {
                    location: format!("{location}:MANIFEST.json"),
                    artifact: (*marker).to_string(),
                });
            }
        }
        if NATIVE_LIBRARY_SUFFIXES
            .iter()
            .any(|suffix| path.ends_with(suffix))
        {
            found.push(ForbiddenArtifact {
                location: format!("{location}:MANIFEST.json"),
                artifact: "native-bridge-library".to_string(),
            });
        }
    }
    Ok(found)
}

/// A forbidden layer edge: a lower-layer crate importing a transport/UI crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForbiddenLayerEdge {
    /// The lower-layer crate declaring the edge.
    pub from: String,
    /// The transport/UI crate it must not depend on.
    pub to: String,
}

/// The `godot-mcp` binary's own source, relative to the workspace root. It is
/// the single definition of the shipped command surface, so the declaration
/// surface is checked against it rather than against a hand-kept list.
pub const CLI_BINARY_SOURCE: &str = "crates/godot-mcp-cli/src/bin/godot-mcp.rs";

/// The manifest whose `[package] description` advertises that surface.
pub const CLI_MANIFEST: &str = "crates/godot-mcp-cli/Cargo.toml";

/// The shipped binary's own name, which the description may name without it
/// being a subcommand.
pub const CLI_BINARY_NAME: &str = "godot-mcp";

/// Every clap `Subcommand` enum that makes up the surface, paired with the
/// command path it hangs off — empty for the top level.
pub const CLI_SUBCOMMAND_ENUMS: &[(&str, &str)] = &[
    ("Command", ""),
    ("AssetsCommand", "assets"),
    ("TransactionCommand", "transaction"),
];

/// Read the `godot-mcp-cli` crate's advertised `[package] description`.
///
/// # Errors
/// Returns an error if the manifest cannot be read or parsed, or declares no
/// `[package] description`.
pub fn cli_crate_description(workspace_root: &Path) -> Result<String, String> {
    let manifest = workspace_root.join(CLI_MANIFEST);
    let text = std::fs::read_to_string(&manifest)
        .map_err(|error| format!("read {}: {error}", manifest.display()))?;
    let value: toml::Value =
        toml::from_str(&text).map_err(|error| format!("parse {}: {error}", manifest.display()))?;
    value
        .get("package")
        .and_then(|package| package.get("description"))
        .and_then(toml::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("{CLI_MANIFEST} declares no [package] description"))
}

/// Derive the shipped binary's real command surface from its own source.
///
/// The returned set holds every reachable command path in the exact spelling
/// `clap` derives: a top-level command as one word (`check-config`) and a nested
/// one as its parent and child separated by a single space (`assets install`).
///
/// # Errors
/// Returns an error if the binary source cannot be read, or if any enum named in
/// [`CLI_SUBCOMMAND_ENUMS`] is absent, unterminated or empty — an absent enum
/// would otherwise silently shrink the surface and make every claim pass.
pub fn cli_subcommand_surface(workspace_root: &Path) -> Result<BTreeSet<String>, String> {
    let source_path = workspace_root.join(CLI_BINARY_SOURCE);
    let source = std::fs::read_to_string(&source_path)
        .map_err(|error| format!("read {}: {error}", source_path.display()))?;
    let mut surface = BTreeSet::new();
    for (enum_name, parent) in CLI_SUBCOMMAND_ENUMS {
        for variant in subcommand_enum_variants(&source, enum_name)? {
            let command = kebab_case(&variant);
            if parent.is_empty() {
                surface.insert(command);
            } else {
                surface.insert(format!("{parent} {command}"));
            }
        }
    }
    Ok(surface)
}

/// Report every command token the description advertises that the binary does
/// not define.
///
/// Backticks are the description's reserved marker for a command token: every
/// backtick-delimited token in it must be either [`CLI_BINARY_NAME`] or a member
/// of the surface [`cli_subcommand_surface`] derived from the binary's own
/// source. A non-command name (a file, a crate, a flag) therefore belongs in the
/// description unquoted, which is what keeps this check mechanical instead of
/// requiring prose interpretation.
pub fn undefined_subcommands_advertised(
    description: &str,
    surface: &BTreeSet<String>,
) -> Vec<String> {
    backticked_tokens(description)
        .into_iter()
        .filter(|token| token != CLI_BINARY_NAME && !surface.contains(token))
        .collect()
}

/// Collect the variant names of one `#[derive(Subcommand)]` enum.
///
/// The scan is indentation-anchored: a variant is a line indented exactly four
/// spaces whose first character is an uppercase ASCII letter, so an attribute, a
/// doc comment, a nested field and a variant's own closing brace are all skipped
/// without needing to parse Rust.
fn subcommand_enum_variants(source: &str, enum_name: &str) -> Result<Vec<String>, String> {
    let header = format!("enum {enum_name} {{");
    let start = source
        .find(&header)
        .ok_or_else(|| format!("{CLI_BINARY_SOURCE} defines no `enum {enum_name}`"))?;
    let body = &source[start + header.len()..];
    let end = body
        .find("\n}")
        .ok_or_else(|| format!("{CLI_BINARY_SOURCE}: `enum {enum_name}` is unterminated"))?;
    let mut variants = Vec::new();
    for line in body[..end].lines() {
        let Some(rest) = line.strip_prefix("    ") else {
            continue;
        };
        let name: String = rest
            .chars()
            .take_while(char::is_ascii_alphanumeric)
            .collect();
        if name.starts_with(|first: char| first.is_ascii_uppercase()) {
            variants.push(name);
        }
    }
    if variants.is_empty() {
        return Err(format!(
            "{CLI_BINARY_SOURCE}: `enum {enum_name}` declares no variant"
        ));
    }
    Ok(variants)
}

/// Render one enum variant the way `clap` spells it on the command line.
fn kebab_case(variant: &str) -> String {
    let mut spelled = String::with_capacity(variant.len() + 1);
    for (index, character) in variant.char_indices() {
        if character.is_ascii_uppercase() {
            if index != 0 {
                spelled.push('-');
            }
            spelled.push(character.to_ascii_lowercase());
        } else {
            spelled.push(character);
        }
    }
    spelled
}

/// Every backtick-delimited token in `text`, deduplicated and ordered. An
/// unterminated final backtick yields no token rather than swallowing the tail.
fn backticked_tokens(text: &str) -> BTreeSet<String> {
    let mut tokens = BTreeSet::new();
    let mut rest = text;
    while let Some(open) = rest.find('`') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('`') else {
            break;
        };
        tokens.insert(after[..close].to_owned());
        rest = &after[close + 1..];
    }
    tokens
}

/// Prove the domain/analysis/engine crates import no transport/UI layer.
///
/// Returns every forbidden edge found (empty when clean).
///
/// # Errors
/// Returns an error if a manifest cannot be read or parsed.
pub fn audit_architecture_boundaries(
    workspace_root: &Path,
) -> Result<Vec<ForbiddenLayerEdge>, String> {
    let mut edges = Vec::new();
    for member in LOWER_LAYER_CRATES {
        let manifest = workspace_root
            .join("crates")
            .join(member)
            .join("Cargo.toml");
        let text = std::fs::read_to_string(&manifest)
            .map_err(|e| format!("read {}: {e}", manifest.display()))?;
        let deps = declared_dependency_names(&text)?;
        for ui in TRANSPORT_UI_CRATES {
            if deps.contains(*ui) {
                edges.push(ForbiddenLayerEdge {
                    from: (*member).to_string(),
                    to: (*ui).to_string(),
                });
            }
        }
    }
    Ok(edges)
}
