//! The shared tagged-value goldens, Rust side: the domain's `GodotValue`
//! reads every golden vector and writes it back unchanged. The GDScript side
//! (`addon/godot_mcp/variant_codec.gd`) is checked against the same file on
//! real Godot builds by `scripts/real_client/m4_smoke.py`.

use godot_mcp_domain::contracts::GodotValue;
use serde_json::Value;

const GOLDENS: &str = include_str!("goldens/variant-codec/v1.json");

#[test]
fn the_domain_round_trips_every_golden_vector() {
    let document: Value = serde_json::from_str(GOLDENS).expect("the golden file is JSON");
    let vectors = document["vectors"].as_array().expect("vectors");
    assert!(vectors.len() >= 30, "every contract type has a vector");
    for vector in vectors {
        let name = vector["name"].as_str().expect("name");
        let tagged = &vector["tagged"];
        let value: GodotValue = serde_json::from_value(tagged.clone())
            .unwrap_or_else(|error| panic!("{name}: the domain reads the vector: {error}"));
        let written = serde_json::to_value(&value).expect("serialize");
        assert_eq!(
            &written, tagged,
            "{name}: the domain writes the vector back unchanged"
        );
    }
}

#[test]
fn every_tag_the_add_on_writes_has_a_vector() {
    let document: Value = serde_json::from_str(GOLDENS).expect("the golden file is JSON");
    let tags: std::collections::BTreeSet<&str> = document["vectors"]
        .as_array()
        .expect("vectors")
        .iter()
        .filter_map(|vector| vector["tagged"]["type"].as_str())
        .collect();
    let codec = include_str!("../addon/godot_mcp/variant_codec.gd");
    for line in codec.lines() {
        let Some(start) = line.find("{\"type\": \"") else {
            continue;
        };
        let rest = &line[start + "{\"type\": \"".len()..];
        let tag = &rest[..rest.find('"').expect("closing quote")];
        assert!(
            tags.contains(tag),
            "the codec writes `{tag}`, which no golden vector covers"
        );
    }
}
