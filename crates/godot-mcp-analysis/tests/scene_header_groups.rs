//! Godot 4 writes a node's persistent groups as a `groups=[...]` attribute of
//! its `[node ...]` header. The static model must read that form as group
//! membership, exactly as it reads the `groups = [...]` property line.

use godot_mcp_analysis::scene::parse_document;
use godot_mcp_domain::Confidence;

#[test]
fn header_attribute_groups_are_group_memberships() {
    let text = "[gd_scene format=3]\n\n\
                [node name=\"World\" type=\"Node2D\"]\n\n\
                [node name=\"Player\" type=\"CharacterBody2D\" parent=\".\" groups=[\"players\", \"actors\"]]\n\
                position = Vector2(1, 2)\n";
    let document = parse_document("scenes/world.tscn", text, 1).expect("the scene parses");
    let player = document.node_at("Player").expect("the player node");
    assert_eq!(
        player
            .groups()
            .iter()
            .map(|group| group.name())
            .collect::<Vec<_>>(),
        vec!["players", "actors"]
    );
    assert!(
        player
            .groups()
            .iter()
            .all(|group| group.provenance().confidence() == Confidence::Exact)
    );
    assert!(
        player
            .extra_attributes()
            .iter()
            .all(|(key, _)| key != "groups"),
        "a modelled attribute is not also retained as unknown"
    );
    assert!(
        document.diagnostics().is_empty(),
        "{:?}",
        document.diagnostics()
    );
    assert_eq!(document.group_memberships().len(), 2);
}
