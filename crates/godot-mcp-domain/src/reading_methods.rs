//! The engine methods `scene.read`, `resource.read` and `editor.scene.read`
//! may call.
//!
//! Some facts a scene holds are not stored as one property a parser can show:
//! which cells of a `TileMapLayer` are painted, the keys of an `Animation`
//! track, the states and transitions of an `AnimationNodeStateMachine`, how
//! many polygons a navigation mesh has, the uniforms a `Shader` declares.
//! The read tools ask the engine itself, by calling one of the methods listed
//! here on a node of a scene or on a resource, and answer what it returned:
//! a supervised headless Godot for a saved file, the connected editor for the
//! scene it holds.
//! Every listed method only reads and returns a plain value, never a live
//! object, and nothing a read does is saved. Godot 4.4.1 to 4.7.1 mark a few of
//! them as not constant — `Gradient`'s `get_offset`, `get_color` and `sample`,
//! which sort the gradient's points first, `get_polygon` of the navigation
//! resources, a blend space's `get_triangle_point` and `Shader`'s
//! `get_shader_uniform_list` — and none of them changes a stored value.
//!
//! The table is part of the contract and has one source, this module: the
//! server's schema lists its methods, and the editor plugin and the headless
//! helper each carry the GDScript copy [`gdscript_table`] renders, which their
//! crates' tests hold to it character for character.

use std::collections::BTreeSet;

/// The read-only methods one engine class, and every class inheriting it, may
/// be called with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadingClass {
    /// The engine class, matched with `ClassDB.is_parent_class`.
    pub class: &'static str,
    pub methods: &'static [&'static str],
}

/// Every class a read may call, with its methods.
pub const READING_METHODS: &[ReadingClass] = &[
    ReadingClass {
        class: "TileMapLayer",
        methods: &[
            "get_cell_source_id",
            "get_cell_atlas_coords",
            "get_cell_alternative_tile",
            "get_used_cells",
            "get_used_cells_by_id",
            "get_used_rect",
        ],
    },
    ReadingClass {
        class: "GridMap",
        methods: &[
            "get_cell_item",
            "get_cell_item_orientation",
            "get_used_cells",
            "get_used_cells_by_item",
        ],
    },
    ReadingClass {
        class: "Line2D",
        methods: &["get_point_count", "get_point_position"],
    },
    ReadingClass {
        class: "Skeleton3D",
        methods: &[
            "get_bone_count",
            "get_bone_name",
            "find_bone",
            "get_bone_parent",
            "get_bone_rest",
        ],
    },
    ReadingClass {
        class: "AnimationMixer",
        methods: &[
            "get_animation_library_list",
            "get_animation_list",
            "has_animation",
            "has_animation_library",
        ],
    },
    ReadingClass {
        class: "AnimationLibrary",
        methods: &["get_animation_list", "has_animation"],
    },
    ReadingClass {
        class: "Animation",
        methods: &[
            "get_track_count",
            "find_track",
            "track_get_path",
            "track_get_type",
            "track_is_enabled",
            "track_get_interpolation_type",
            "track_get_interpolation_loop_wrap",
            "value_track_get_update_mode",
            "track_get_key_count",
            "track_get_key_time",
            "track_get_key_value",
            "track_get_key_transition",
            "track_find_key",
            "get_length",
            "get_loop_mode",
            "get_step",
        ],
    },
    ReadingClass {
        class: "SpriteFrames",
        methods: &[
            "get_animation_names",
            "has_animation",
            "get_frame_count",
            "get_frame_duration",
            "get_animation_speed",
            "get_animation_loop",
        ],
    },
    ReadingClass {
        class: "Curve",
        methods: &[
            "get_point_count",
            "get_point_position",
            "get_point_left_tangent",
            "get_point_right_tangent",
            "sample",
        ],
    },
    ReadingClass {
        class: "Curve2D",
        methods: &[
            "get_point_count",
            "get_point_position",
            "get_point_in",
            "get_point_out",
            "get_baked_length",
        ],
    },
    ReadingClass {
        class: "Curve3D",
        methods: &[
            "get_point_count",
            "get_point_position",
            "get_point_in",
            "get_point_out",
            "get_point_tilt",
            "get_baked_length",
        ],
    },
    ReadingClass {
        class: "Gradient",
        methods: &["get_point_count", "get_offset", "get_color", "sample"],
    },
    ReadingClass {
        class: "Theme",
        methods: &[
            "get_type_list",
            "get_color_list",
            "get_constant_list",
            "get_font_size_list",
            "get_font_list",
            "get_icon_list",
            "get_stylebox_list",
            "get_color",
            "get_constant",
            "get_font_size",
            "has_color",
            "has_constant",
            "has_font_size",
            "has_font",
            "has_icon",
            "has_stylebox",
        ],
    },
    ReadingClass {
        class: "AnimationNodeStateMachine",
        methods: &[
            "get_node_list",
            "has_node",
            "get_node_position",
            "get_transition_count",
            "get_transition_from",
            "get_transition_to",
            "get_graph_offset",
        ],
    },
    ReadingClass {
        class: "AnimationNodeBlendTree",
        methods: &[
            "get_node_list",
            "has_node",
            "get_node_position",
            "get_graph_offset",
        ],
    },
    ReadingClass {
        class: "AnimationNodeBlendSpace1D",
        methods: &["get_blend_point_count", "get_blend_point_position"],
    },
    ReadingClass {
        class: "AnimationNodeBlendSpace2D",
        methods: &[
            "get_blend_point_count",
            "get_blend_point_position",
            "get_triangle_count",
            "get_triangle_point",
        ],
    },
    ReadingClass {
        class: "VisualShader",
        methods: &[
            "get_node_list",
            "get_node_position",
            "get_node_connections",
            "get_valid_node_id",
        ],
    },
    ReadingClass {
        class: "TileSet",
        methods: &[
            "get_source_count",
            "get_source_id",
            "has_source",
            "get_physics_layers_count",
            "get_terrain_sets_count",
            "get_terrains_count",
            "get_terrain_name",
            "get_navigation_layers_count",
            "get_custom_data_layers_count",
            "get_custom_data_layer_name",
        ],
    },
    ReadingClass {
        class: "TileSetAtlasSource",
        methods: &[
            "get_tiles_count",
            "get_tile_id",
            "has_tile",
            "get_atlas_grid_size",
            "get_alternative_tiles_count",
        ],
    },
    ReadingClass {
        class: "NavigationPolygon",
        methods: &[
            "get_polygon_count",
            "get_polygon",
            "get_vertices",
            "get_outline_count",
            "get_outline",
        ],
    },
    ReadingClass {
        class: "NavigationMesh",
        methods: &["get_polygon_count", "get_polygon", "get_vertices"],
    },
    ReadingClass {
        class: "MeshLibrary",
        methods: &["get_item_list", "get_item_name", "get_last_unused_item_id"],
    },
    ReadingClass {
        class: "Mesh",
        methods: &["get_surface_count", "get_aabb"],
    },
    ReadingClass {
        class: "Shader",
        methods: &["get_mode", "get_shader_uniform_list"],
    },
];

/// The most calls one read may make.
pub const MAX_READ_CALLS: usize = 64;

/// The most bytes of Godot's text spelling one call's value may take; a longer
/// value is refused for that call, never cut short.
pub const MAX_READ_VALUE_BYTES: usize = 262_144;

/// Every method name the table lists, for any class.
#[must_use]
pub fn method_names() -> BTreeSet<&'static str> {
    READING_METHODS
        .iter()
        .flat_map(|class| class.methods.iter().copied())
        .collect()
}

/// The GDScript constants the editor plugin and the headless helper carry,
/// exactly as each file must contain them.
#[must_use]
pub fn gdscript_table() -> String {
    let quoted = |names: &[&str]| {
        names
            .iter()
            .map(|name| format!("\"{name}\""))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut text = String::from(
        "# The reading methods, as crates/godot-mcp-domain/src/reading_methods.rs lists\n\
         # them; a test holds this copy to that table. Change the table, not this copy.\n\
         const READING_METHODS := {\n",
    );
    for class in READING_METHODS {
        text.push_str(&format!(
            "\t\"{}\": [{}],\n",
            class.class,
            quoted(class.methods)
        ));
    }
    text.push_str("}\n");
    text.push_str(&format!("const MAX_READ_CALLS := {MAX_READ_CALLS}\n"));
    text.push_str(&format!(
        "const MAX_READ_VALUE_BYTES := {MAX_READ_VALUE_BYTES}\n"
    ));
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_class_lists_distinct_methods_once() {
        let mut classes = BTreeSet::new();
        for class in READING_METHODS {
            assert!(
                classes.insert(class.class),
                "{} is listed twice",
                class.class
            );
            let methods: BTreeSet<&str> = class.methods.iter().copied().collect();
            assert_eq!(
                methods.len(),
                class.methods.len(),
                "{} lists a method twice",
                class.class
            );
            assert!(!class.methods.is_empty(), "{} lists no method", class.class);
        }
    }

    #[test]
    fn no_reading_method_is_an_editing_method() {
        let editing = crate::editing_methods::method_names();
        for class in READING_METHODS {
            for method in class.methods {
                assert!(
                    !method.starts_with("set_")
                        && !method.starts_with("add_")
                        && !method.starts_with("remove_")
                        && !method.starts_with("clear")
                        && !method.starts_with("bake"),
                    "{}.{method} reads like a change",
                    class.class
                );
                for edit in crate::editing_methods::EDITING_METHODS {
                    if edit.class == class.class {
                        assert!(
                            !edit.methods.contains(method),
                            "{}.{method} is an editing method",
                            class.class
                        );
                    }
                }
            }
        }
        assert!(!editing.is_empty());
    }

    #[test]
    fn the_gdscript_copy_names_every_class_and_method() {
        let table = gdscript_table();
        for class in READING_METHODS {
            assert!(table.contains(&format!("\"{}\": [", class.class)));
            for method in class.methods {
                assert!(table.contains(&format!("\"{method}\"")));
            }
        }
        assert!(table.contains("const MAX_READ_CALLS := 64\n"));
        assert!(table.ends_with("const MAX_READ_VALUE_BYTES := 262144\n"));
    }
}
