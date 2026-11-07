//! The `instructions` a client receives in the `initialize` answer
//! (docs/protocol/mcp-surface.md, Discovery). They are generated from the
//! selected profile's manifest, so they can name only tools that profile
//! lists, and they stay within a fixed byte budget because clients that put
//! them in the model's context pay for every byte on every turn.

use std::collections::BTreeSet;

use godot_mcp_domain::CapabilityProfile;
use godot_mcp_domain::catalog::TOOL_CATALOG;
use godot_mcp_domain::manifests::canonical_manifest;
use godot_mcp_server::instructions::{MAX_INSTRUCTIONS_BYTES, server_instructions};

const PROFILES: [CapabilityProfile; 4] = [
    CapabilityProfile::Core,
    CapabilityProfile::Editor,
    CapabilityProfile::Runtime,
    CapabilityProfile::Full,
];

/// The text between each pair of backticks.
fn backticked(text: &str) -> Vec<&str> {
    text.split('`').skip(1).step_by(2).collect()
}

/// A token shaped like a dotted tool name: lowercase segments of letters,
/// digits and underscores joined by dots.
fn looks_like_tool_name(token: &str) -> bool {
    token.contains('.')
        && token.split('.').all(|segment| {
            !segment.is_empty()
                && segment
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        })
}

fn listed(profile: CapabilityProfile, bootstrap: bool) -> BTreeSet<String> {
    let manifest = canonical_manifest(profile);
    let mut listed = manifest.tools;
    if bootstrap {
        listed.extend(manifest.bootstrap_tools);
    }
    listed
}

#[test]
fn the_instructions_stay_within_their_byte_budget() {
    for profile in PROFILES {
        for bootstrap in [false, true] {
            let text = server_instructions(profile, bootstrap);
            assert!(
                text.len() <= MAX_INSTRUCTIONS_BYTES,
                "{profile:?} bootstrap={bootstrap}: {} bytes > {MAX_INSTRUCTIONS_BYTES}",
                text.len()
            );
        }
    }
}

#[test]
fn the_instructions_name_only_tools_the_profile_lists() {
    let catalog: BTreeSet<&str> = TOOL_CATALOG.iter().map(|tool| tool.name).collect();
    for profile in PROFILES {
        for bootstrap in [false, true] {
            let text = server_instructions(profile, bootstrap);
            let listed = listed(profile, bootstrap);
            for token in backticked(&text) {
                if looks_like_tool_name(token) {
                    assert!(
                        catalog.contains(token),
                        "{profile:?}: `{token}` is not a catalog tool"
                    );
                    assert!(
                        listed.contains(token),
                        "{profile:?} bootstrap={bootstrap}: `{token}` is not listed"
                    );
                }
            }
        }
    }
}

#[test]
fn write_guidance_appears_only_in_profiles_that_write() {
    let core = server_instructions(CapabilityProfile::Core, false);
    assert!(!core.contains("expectedSha256"), "{core}");
    for profile in [CapabilityProfile::Editor, CapabilityProfile::Full] {
        let text = server_instructions(profile, false);
        for needle in [
            "expectedSha256",
            "`scene.batch.apply`",
            "`editor.scene.edit`",
            "`scene.recipe.apply`",
            "`godot-mcp.recipe.*`",
        ] {
            assert!(text.contains(needle), "{profile:?} lacks {needle}: {text}");
        }
    }
}

#[test]
fn script_tools_are_said_to_need_the_operator_switch_where_listed() {
    for profile in PROFILES {
        let text = server_instructions(profile, false);
        let scripts: Vec<&str> = ["script.run", "editor.script.run", "runtime.script.run"]
            .into_iter()
            .filter(|tool| listed(profile, false).contains(*tool))
            .collect();
        assert_eq!(
            text.contains("--allow-scripts"),
            !scripts.is_empty(),
            "{profile:?}: {text}"
        );
        for tool in scripts {
            assert!(text.contains(&format!("`{tool}`")), "{profile:?}: {text}");
        }
    }
}

#[test]
fn only_a_bootstrap_server_is_told_to_bind_a_project_first() {
    for profile in PROFILES {
        let bound = server_instructions(profile, false);
        assert!(!bound.contains("`project.bind`"), "{profile:?}: {bound}");
        let bootstrap = server_instructions(profile, true);
        assert!(
            bootstrap.contains("`workspace.projects.list`") && bootstrap.contains("`project.bind`"),
            "{profile:?}: {bootstrap}"
        );
        assert_eq!(
            bootstrap.contains("`project.create`"),
            canonical_manifest(profile)
                .bootstrap_tools
                .contains("project.create"),
            "{profile:?}: {bootstrap}"
        );
    }
}

#[test]
fn every_profile_keeps_the_storage_rule_and_points_at_the_skills() {
    for profile in PROFILES {
        for bootstrap in [false, true] {
            let text = server_instructions(profile, bootstrap);
            assert!(text.contains("storage paths are never public"), "{text}");
            assert!(text.contains("`godot-mcp skill install`"), "{text}");
            assert!(text.contains("`uncertain`"), "{text}");
            assert!(text.contains("`unavailable`"), "{text}");
        }
    }
}
