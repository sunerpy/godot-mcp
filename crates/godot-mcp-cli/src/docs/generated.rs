//! Documentation pages that are rendered from a canonical source rather than
//! written by hand.
//!
//! The checker regenerates each page and compares it byte for byte with what is
//! on disk. A stale tool name, a renamed error code or a bumped Godot version can
//! therefore never survive in these pages: the check fails until the page is
//! regenerated from the owner that changed.

use std::fmt::Write as _;

use super::canonical::Canonical;

/// The marker that identifies a generated page and names its generator.
pub const MARKER_PREFIX: &str = "docs-generated:";

/// One generated page: where it lives and how to render it.
#[derive(Debug, Clone, Copy)]
pub struct GeneratedPage {
    /// Workspace-relative path.
    pub relative: &'static str,
    /// Stable generator identifier, recorded in the page's marker comment.
    pub generator: &'static str,
}

/// Every generated page, in path order.
pub const PAGES: [GeneratedPage; 3] = [
    GeneratedPage {
        relative: "docs/operator/version-matrix.md",
        generator: "version-matrix",
    },
    GeneratedPage {
        relative: "docs/protocol/error-taxonomy.md",
        generator: "error-taxonomy",
    },
    GeneratedPage {
        relative: "docs/protocol/tool-inventory.md",
        generator: "tool-inventory",
    },
];

fn header(generator: &str, title: &str, source: &str) -> String {
    format!(
        "<!-- {MARKER_PREFIX} {generator} -->\n\
         <!-- Regenerate with: cargo run -p godot-mcp-cli --features test-hooks --bin docs-check -- --write -->\n\
         # {title}\n\
         \n\
         This page is generated from {source}. Editing it by hand fails the docs\n\
         check; change the canonical source and regenerate instead.\n"
    )
}

/// Render the tool/profile inventory page.
///
/// # Errors
/// Returns an error only if formatting into the output string fails.
pub fn tool_inventory(canonical: &Canonical) -> Result<String, String> {
    let mut out = header(
        "tool-inventory",
        "Profiles, tools and resources",
        "`godot_mcp_domain::catalog::TOOL_CATALOG`, the one owner of every public\nmethod's name, description, capability, plane, annotations and the handler key\nits input schema and handler come from",
    );
    out.push_str(
        "\n\
         `full` is the union of `core`, `editor` and `runtime`. No profile carries an\n\
         arbitrary-script-execution tool: `unsafe_script` is a reserved identifier with\n\
         no capability variant, no handler and no profile membership.\n\
         \n\
         Discovery reports what this build implements. A profile is an authorization\n\
         ceiling: it lists exactly the catalog methods it authorizes, and the catalog\n\
         contains only methods with a schema and a production handler, so a planned\n\
         method is never listed. A listed method whose plane is temporarily\n\
         unreachable answers `unavailable` and names the plane. See\n\
         [Profiles and tool outcomes](../operator/profiles-and-outcomes.md).\n\
         \n\
         ## Profile sizes\n\
         \n\
         | Profile | Capabilities | Tools | Resources |\n\
         | --- | --- | --- | --- |\n",
    );
    for surface in &canonical.profiles {
        writeln!(
            out,
            "| `{}` | {} | {} | {} |",
            surface.name,
            surface.capabilities.len(),
            surface.tools.len(),
            surface.resources.len()
        )
        .map_err(|error| format!("render profile sizes: {error}"))?;
    }
    out.push_str(
        "\n## Methods\n\n\
         | Method | Capability | Needs | Read-only | Destructive | Idempotent | Open world |\n\
         | --- | --- | --- | --- | --- | --- | --- |\n",
    );
    let mut descriptors: Vec<_> = godot_mcp_domain::catalog::TOOL_CATALOG.iter().collect();
    descriptors.sort_by_key(|descriptor| descriptor.name);
    let yes = |flag: bool| if flag { "yes" } else { "no" };
    for descriptor in &descriptors {
        let capability = serde_json::to_value(descriptor.capability)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .ok_or_else(|| format!("render the capability of {}", descriptor.name))?;
        writeln!(
            out,
            "| `{}` | `{capability}` | `{}` | {} | {} | {} | {} |",
            descriptor.name,
            descriptor.needs.as_str(),
            yes(descriptor.annotations.read_only),
            yes(descriptor.annotations.destructive),
            yes(descriptor.annotations.idempotent),
            yes(descriptor.annotations.open_world),
        )
        .map_err(|error| format!("render method row: {error}"))?;
    }
    for descriptor in &descriptors {
        writeln!(
            out,
            "\n### `{}`\n\n{}",
            descriptor.name, descriptor.description
        )
        .map_err(|error| format!("render method description: {error}"))?;
    }
    for surface in &canonical.profiles {
        writeln!(out, "\n## `{}`\n", surface.name)
            .map_err(|error| format!("render profile heading: {error}"))?;
        out.push_str("Capabilities:\n\n");
        for capability in &surface.capabilities {
            writeln!(out, "* `{capability}`")
                .map_err(|error| format!("render capability: {error}"))?;
        }
        out.push_str("\nTools:\n\n");
        for tool in &surface.tools {
            writeln!(out, "* `{tool}`").map_err(|error| format!("render tool: {error}"))?;
        }
        out.push_str(
            "\nBootstrap tools, listed before a `--workspace` server binds its project:\n\n",
        );
        for tool in &surface.bootstrap_tools {
            writeln!(out, "* `{tool}`").map_err(|error| format!("render tool: {error}"))?;
        }
        out.push_str("\nResources:\n\n");
        for resource in &surface.resources {
            writeln!(out, "* `{resource}`").map_err(|error| format!("render resource: {error}"))?;
        }
    }
    Ok(out)
}

/// Render the outcome-taxonomy page.
///
/// # Errors
/// Returns an error only if formatting into the output string fails.
pub fn error_taxonomy(canonical: &Canonical) -> Result<String, String> {
    let mut out = header(
        "error-taxonomy",
        "Outcome taxonomy",
        "the `schemars` schemas of `godot_mcp_domain`'s own enums, so a variant\nadded or removed in code shows up here and nowhere else",
    );
    for (heading, note, values) in [
        (
            "Error codes",
            "Every failed operation carries exactly one of these codes. `uncertain` is\nnot a synonym for `internal`: it is the honest answer when a static analysis\ncannot prove a result, and it is reported instead of a confident guess.",
            &canonical.error_codes,
        ),
        (
            "Operation planes",
            "The plane an operation acts on, expressed by outcome rather than by\nimplementation.",
            &canonical.planes,
        ),
        (
            "Capabilities",
            "The capability an operation consumes. There is no capability variant for\narbitrary script execution.",
            &canonical.capabilities,
        ),
        (
            "Static confidence",
            "Static answers are never silently presented as certain. Each carries\nprovenance and one of these levels.",
            &canonical.confidences,
        ),
        (
            "Profiles",
            "The selectable capability profiles.",
            &canonical.profile_names,
        ),
    ] {
        writeln!(out, "\n## {heading}\n\n{note}\n")
            .map_err(|error| format!("render taxonomy heading: {error}"))?;
        for value in values {
            writeln!(out, "* `{value}`")
                .map_err(|error| format!("render taxonomy value: {error}"))?;
        }
    }
    Ok(out)
}

/// Render the Godot compatibility-matrix page.
///
/// # Errors
/// Returns an error only if formatting into the output string fails.
pub fn version_matrix(canonical: &Canonical) -> Result<String, String> {
    let lock = &canonical.lock;
    let mut out = header(
        "version-matrix",
        "Godot compatibility matrix",
        "`tests/fixtures/godot-releases.lock.json`, the sole source of every Godot\nversion, download URL and expected digest in this repository",
    );
    writeln!(
        out,
        "\n\
         Every entry is an exact patch version. A major-minor alias is never a lock\n\
         entry and never appears in CI, in configuration or in these pages, because an\n\
         alias silently re-points at a different build.\n\
         \n\
         Minimum supported version: `{}`.\n\
         \n\
         ## What this matrix does and does not claim\n\
         \n\
         The matrix below is **defined**: the CI workflows derive their job list, their\n\
         download URLs and their expected digests from this lock, and a test asserts no\n\
         workflow restates a version by hand. Whether a given row has been **observed\n\
         green on a real runner** is a separate fact, recorded per observation and not\n\
         implied by presence here. For the tree this page was generated from, every\n\
         editor row and every template archive below was exercised by the real-client\n\
         matrix (`scripts/real_client/platform_matrix.py`) on a real runner of its\n\
         platform: Linux x86_64, an Apple silicon Mac and a Windows Server 2022 desktop\n\
         session; see [Process boundaries](process-boundaries.md#what-has-been-executed-where).\n\
         An observation belongs to the tree it was made on, so\n\
         a row is not observed for a later tree until the matrix runs there again.\n",
        lock.minimum_supported_version
    )
    .map_err(|error| format!("render matrix preamble: {error}"))?;
    out.push_str("\n## Releases\n");
    for release in &lock.releases {
        writeln!(
            out,
            "\n### `{}`\n\n\
             * Git tag: `{}`\n\
             * Channel: `{}`\n\
             * Published: `{}`\n\
             * Release page: {}\n\n\
             | Platform | Kind | Artifact | Bytes | SHA-256 | Digest provenance |\n\
             | --- | --- | --- | --- | --- | --- |",
            release.version,
            release.git_tag,
            release.channel,
            release.published_at,
            release.release_page_url
        )
        .map_err(|error| format!("render release heading: {error}"))?;
        for artifact in &release.artifacts {
            writeln!(
                out,
                "| `{}` | `{}` | `{}` | {} | `{}` | `{}` |",
                artifact.platform,
                artifact.artifact_kind,
                artifact.artifact_name,
                artifact.size_bytes,
                artifact.sha256,
                artifact.sha256_provenance
            )
            .map_err(|error| format!("render artifact row: {error}"))?;
        }
    }
    out.push_str("\n## Digest provenance kinds\n\n");
    for (kind, meaning) in &lock.sha256_provenance_kinds {
        writeln!(out, "* `{kind}`: {meaning}")
            .map_err(|error| format!("render provenance kind: {error}"))?;
    }
    out.push_str("\n## Required platforms\n\n");
    for platform in &lock.required_platforms {
        writeln!(out, "* `{platform}`")
            .map_err(|error| format!("render required platform: {error}"))?;
    }
    Ok(out)
}

/// Render one generated page by generator identifier.
///
/// # Errors
/// Returns an error for an unknown generator or a formatting failure.
pub fn render(generator: &str, canonical: &Canonical) -> Result<String, String> {
    match generator {
        "tool-inventory" => tool_inventory(canonical),
        "error-taxonomy" => error_taxonomy(canonical),
        "version-matrix" => version_matrix(canonical),
        other => Err(format!("unknown generated page {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canonical() -> Canonical {
        super::super::canonical::build(&crate::audit::default_workspace_root())
            .expect("the canonical inventory builds")
    }

    #[test]
    fn every_generated_page_renders_and_carries_its_marker() {
        let canonical = canonical();
        for page in PAGES {
            let text = render(page.generator, &canonical).expect("the page renders");
            assert!(
                text.starts_with(&format!("<!-- {MARKER_PREFIX} {} -->\n", page.generator)),
                "{} must open with its generator marker",
                page.relative
            );
            assert!(
                text.ends_with('\n'),
                "{} ends with a newline",
                page.relative
            );
        }
        assert!(render("nope", &canonical).is_err());
    }

    #[test]
    fn the_inventory_page_names_every_canonical_tool() {
        let canonical = canonical();
        let text = tool_inventory(&canonical).expect("the page renders");
        for tool in &canonical.tools {
            assert!(
                text.contains(&format!("* `{tool}`")),
                "{tool} must be listed"
            );
        }
    }

    #[test]
    fn the_matrix_page_uses_exact_patch_versions_only() {
        let canonical = canonical();
        let text = version_matrix(&canonical).expect("the page renders");
        for version in &canonical.godot_versions {
            assert!(text.contains(&format!("### `{version}`")));
        }
    }
}
