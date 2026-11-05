//! The engine methods a batch's `call_method` operation may call.
//!
//! Some edits have no property to set: painting a `TileMapLayer`, keying an
//! `Animation`, adding points to a `Curve`. A batch makes them by calling one
//! of the methods listed here on an object the edit owns — a node of the
//! scene, a resource embedded in it, or the resource being edited. Every
//! listed method changes only its receiver's own stored state, so the file the
//! batch saves holds the whole change, and the editor can undo it by putting
//! the receiver's stored properties back, or, for the graph-like resources
//! whose calls rebuild their own stored layout, by putting back a copy of the
//! whole embedded resource the call reached.
//!
//! The table is part of the contract and has one source, this module: the
//! server's schema lists its methods, and the editor plugin and the headless
//! helper each carry the GDScript copy [`gdscript_table`] renders, which their
//! crates' tests hold to it character for character. A method is added only
//! here, through an ADR, never by a caller.

use std::collections::BTreeSet;

/// How the editor undoes a call on objects of one class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restore {
    /// Write back each stored property the call changed.
    WriteBack,
    /// Empty the receiver, then write back every stored property in order:
    /// for a class whose calls add or remove stored properties, as
    /// `Animation`'s tracks, `Theme`'s items and, from Godot 4.6.1, an
    /// `AnimationMixer`'s libraries are, or whose stored order a plain write
    /// does not restore, as a `TileMapLayer`'s cells.
    ClearThenWriteBack(Clearing),
    /// Put back a copy of the outermost embedded resource the call reached
    /// through the node property its `resource` selector starts from: for
    /// graph resources (state machines, blend trees and spaces, visual
    /// shaders, tile sets, mesh libraries) whose calls add, rename and remove
    /// stored entries that a plain write cannot take away again. Undo and
    /// redo then hold a new copy of that resource, so another reference to
    /// the very same embedded instance keeps the changed one.
    Replace,
    /// Bake: the call rebuilds, in place, the resource the named property of
    /// the node holds, as a navigation region's bake rebuilds its navigation
    /// mesh or polygon from the nodes below it. It is always made with
    /// `on_thread` false, so it returns only once the engine has finished,
    /// and the batch answers what the bake produced. The receiver is the node
    /// itself, never a resource a selector picks, and the resource must be
    /// one the edited file embeds. The editor bakes into a copy, which the
    /// node then holds, and undo gives it the untouched original back.
    Bake(&'static str),
}

/// How a [`Restore::ClearThenWriteBack`] class is emptied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Clearing {
    /// Call this method.
    Call(&'static str),
    /// Call `remove` with every name `list` returns, for a class without a
    /// method that empties it.
    RemoveEach {
        list: &'static str,
        remove: &'static str,
    },
}

/// The methods one engine class, and every class inheriting it, may be called
/// with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditingClass {
    /// The engine class, matched with `ClassDB.is_parent_class`.
    pub class: &'static str,
    pub restore: Restore,
    /// A method that brings the receiver's stored properties up to date,
    /// called after each call, after emptying the receiver and after each
    /// write back. A `TileMapLayer` keeps an erased cell in its stored data
    /// until its next update, so without it the saved bytes and the scene
    /// digest would name a cell the layer no longer shows, and cells written
    /// back would keep the stored order of the cells they replace.
    pub settle: Option<&'static str>,
    pub methods: &'static [&'static str],
}

/// Every class a `call_method` operation may call, with its methods.
pub const EDITING_METHODS: &[EditingClass] = &[
    EditingClass {
        class: "TileMapLayer",
        restore: Restore::ClearThenWriteBack(Clearing::Call("clear")),
        settle: Some("update_internals"),
        methods: &[
            "set_cell",
            "erase_cell",
            "clear",
            "set_cells_terrain_connect",
            "set_cells_terrain_path",
        ],
    },
    EditingClass {
        class: "GridMap",
        restore: Restore::WriteBack,
        settle: None,
        methods: &["set_cell_item", "clear"],
    },
    EditingClass {
        class: "Line2D",
        restore: Restore::WriteBack,
        settle: None,
        methods: &[
            "add_point",
            "remove_point",
            "set_point_position",
            "clear_points",
        ],
    },
    EditingClass {
        class: "AnimationMixer",
        restore: Restore::ClearThenWriteBack(Clearing::RemoveEach {
            list: "get_animation_library_list",
            remove: "remove_animation_library",
        }),
        settle: None,
        methods: &[
            "add_animation_library",
            "remove_animation_library",
            "rename_animation_library",
        ],
    },
    EditingClass {
        class: "AnimationLibrary",
        restore: Restore::WriteBack,
        settle: None,
        methods: &["add_animation", "remove_animation", "rename_animation"],
    },
    EditingClass {
        class: "Animation",
        restore: Restore::ClearThenWriteBack(Clearing::Call("clear")),
        settle: None,
        methods: &[
            "add_track",
            "remove_track",
            "track_set_path",
            "track_set_enabled",
            "track_set_interpolation_type",
            "track_set_interpolation_loop_wrap",
            "value_track_set_update_mode",
            "track_insert_key",
            "track_remove_key",
            "track_remove_key_at_time",
            "track_set_key_value",
            "track_set_key_time",
            "track_set_key_transition",
            "set_length",
            "set_loop_mode",
            "set_step",
        ],
    },
    EditingClass {
        class: "SpriteFrames",
        restore: Restore::WriteBack,
        settle: None,
        methods: &[
            "add_animation",
            "remove_animation",
            "rename_animation",
            "add_frame",
            "remove_frame",
            "set_frame",
            "clear",
            "set_animation_speed",
            "set_animation_loop",
        ],
    },
    EditingClass {
        class: "Curve",
        restore: Restore::WriteBack,
        settle: None,
        methods: &[
            "add_point",
            "remove_point",
            "set_point_value",
            "set_point_offset",
            "set_point_left_tangent",
            "set_point_right_tangent",
            "clear_points",
        ],
    },
    EditingClass {
        class: "Curve2D",
        restore: Restore::WriteBack,
        settle: None,
        methods: &[
            "add_point",
            "remove_point",
            "set_point_position",
            "set_point_in",
            "set_point_out",
            "clear_points",
        ],
    },
    EditingClass {
        class: "Curve3D",
        restore: Restore::WriteBack,
        settle: None,
        methods: &[
            "add_point",
            "remove_point",
            "set_point_position",
            "set_point_in",
            "set_point_out",
            "set_point_tilt",
            "clear_points",
        ],
    },
    EditingClass {
        class: "Gradient",
        restore: Restore::WriteBack,
        settle: None,
        methods: &["add_point", "remove_point", "set_offset", "set_color"],
    },
    EditingClass {
        class: "Theme",
        restore: Restore::ClearThenWriteBack(Clearing::Call("clear")),
        settle: None,
        methods: &[
            "set_color",
            "set_constant",
            "set_font_size",
            "set_font",
            "set_icon",
            "set_stylebox",
            "clear_color",
            "clear_constant",
            "clear_font_size",
            "clear_font",
            "clear_icon",
            "clear_stylebox",
        ],
    },
    EditingClass {
        class: "Control",
        restore: Restore::WriteBack,
        settle: None,
        methods: &[
            "set_anchors_preset",
            "set_anchors_and_offsets_preset",
            "set_offsets_preset",
            "set_anchor",
            "set_anchor_and_offset",
        ],
    },
    EditingClass {
        class: "AnimationNodeStateMachine",
        restore: Restore::Replace,
        settle: None,
        methods: &[
            "add_node",
            "remove_node",
            "rename_node",
            "replace_node",
            "set_node_position",
            "add_transition",
            "remove_transition",
            "remove_transition_by_index",
            "set_graph_offset",
        ],
    },
    EditingClass {
        class: "AnimationNodeBlendTree",
        restore: Restore::Replace,
        settle: None,
        methods: &[
            "add_node",
            "remove_node",
            "rename_node",
            "connect_node",
            "disconnect_node",
            "set_node_position",
            "set_graph_offset",
        ],
    },
    EditingClass {
        class: "AnimationNodeBlendSpace1D",
        restore: Restore::Replace,
        settle: None,
        methods: &[
            "add_blend_point",
            "remove_blend_point",
            "set_blend_point_position",
            "set_blend_point_node",
        ],
    },
    EditingClass {
        class: "AnimationNodeBlendSpace2D",
        restore: Restore::Replace,
        settle: None,
        methods: &[
            "add_blend_point",
            "remove_blend_point",
            "set_blend_point_position",
            "set_blend_point_node",
            "add_triangle",
            "remove_triangle",
        ],
    },
    EditingClass {
        class: "VisualShader",
        restore: Restore::Replace,
        settle: None,
        methods: &[
            "add_node",
            "remove_node",
            "connect_nodes",
            "disconnect_nodes",
            "set_node_position",
        ],
    },
    EditingClass {
        class: "TileSet",
        restore: Restore::Replace,
        settle: None,
        methods: &[
            "add_source",
            "remove_source",
            "add_physics_layer",
            "remove_physics_layer",
            "set_physics_layer_collision_layer",
            "set_physics_layer_collision_mask",
            "add_terrain_set",
            "remove_terrain_set",
            "set_terrain_set_mode",
            "add_terrain",
            "remove_terrain",
            "set_terrain_name",
            "set_terrain_color",
            "add_navigation_layer",
            "remove_navigation_layer",
            "add_custom_data_layer",
            "remove_custom_data_layer",
            "set_custom_data_layer_name",
            "set_custom_data_layer_type",
        ],
    },
    EditingClass {
        class: "TileSetAtlasSource",
        restore: Restore::Replace,
        settle: None,
        methods: &[
            "create_tile",
            "remove_tile",
            "create_alternative_tile",
            "remove_alternative_tile",
            "clear_tiles_outside_texture",
        ],
    },
    EditingClass {
        class: "NavigationPolygon",
        restore: Restore::WriteBack,
        settle: None,
        methods: &[
            "add_outline",
            "add_outline_at_index",
            "set_outline",
            "remove_outline",
            "clear_outlines",
            "add_polygon",
            "clear_polygons",
            "clear",
        ],
    },
    EditingClass {
        class: "NavigationMesh",
        restore: Restore::WriteBack,
        settle: None,
        methods: &["add_polygon", "clear_polygons", "create_from_mesh", "clear"],
    },
    EditingClass {
        class: "MeshLibrary",
        restore: Restore::Replace,
        settle: None,
        methods: &[
            "create_item",
            "set_item_mesh",
            "set_item_name",
            "set_item_mesh_transform",
            "set_item_shapes",
            "set_item_navigation_mesh",
            "remove_item",
            "clear",
        ],
    },
    EditingClass {
        class: "NavigationRegion2D",
        restore: Restore::Bake("navigation_polygon"),
        settle: None,
        methods: &["bake_navigation_polygon"],
    },
    EditingClass {
        class: "NavigationRegion3D",
        restore: Restore::Bake("navigation_mesh"),
        settle: None,
        methods: &["bake_navigation_mesh"],
    },
];

/// A `new_resource` argument may create a resource of any engine class the
/// running Godot can instantiate that inherits `Resource`, except these and the
/// classes inheriting them: a script is code and a GDExtension loads native
/// code, and neither is a value an edit holds. The class is checked against
/// the engine that applies the edit, so a class a Godot build lacks is refused
/// there with that build's own answer.
pub const NEW_RESOURCE_FORBIDDEN: &[&str] = &["Script", "GDExtension"];

/// The longest engine class name a `new_resource` argument may give.
pub const MAX_CLASS_NAME_BYTES: usize = 128;

/// The most arguments one call may take.
pub const MAX_CALL_ARGUMENTS: usize = 16;

/// The most calls one `call_method` operation may make with `repeat`: the
/// same method once per argument list, recorded as one change.
pub const MAX_REPEATED_CALLS: usize = 4096;

/// Whether `class` is spelled as an engine class name: letters, digits and
/// underscores, not starting with a digit.
#[must_use]
pub fn is_class_name(class: &str) -> bool {
    !class.is_empty()
        && class.len() <= MAX_CLASS_NAME_BYTES
        && class
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && class
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
}

/// Every method name the table lists, for any class.
#[must_use]
pub fn method_names() -> BTreeSet<&'static str> {
    EDITING_METHODS
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
        "# The editing methods, as crates/godot-mcp-domain/src/editing_methods.rs lists\n\
         # them; a test holds this copy to that table. Change the table, not this copy.\n\
         const EDITING_METHODS := {\n",
    );
    for class in EDITING_METHODS {
        text.push_str(&format!(
            "\t\"{}\": [{}],\n",
            class.class,
            quoted(class.methods)
        ));
    }
    text.push_str("}\n");
    let clearing: Vec<String> =
        EDITING_METHODS
            .iter()
            .filter_map(|class| match class.restore {
                Restore::WriteBack | Restore::Replace | Restore::Bake(_) => None,
                Restore::ClearThenWriteBack(Clearing::Call(method)) => {
                    Some(format!("\"{}\": [{}]", class.class, quoted(&[method])))
                }
                Restore::ClearThenWriteBack(Clearing::RemoveEach { list, remove }) => Some(
                    format!("\"{}\": [{}]", class.class, quoted(&[list, remove])),
                ),
            })
            .collect();
    text.push_str(&format!("const CLEARING := {{{}}}\n", clearing.join(", ")));
    let settle: Vec<String> = EDITING_METHODS
        .iter()
        .filter_map(|class| {
            class
                .settle
                .map(|method| format!("\"{}\": \"{method}\"", class.class))
        })
        .collect();
    text.push_str(&format!("const SETTLE := {{{}}}\n", settle.join(", ")));
    let replacing: Vec<&str> = EDITING_METHODS
        .iter()
        .filter(|class| class.restore == Restore::Replace)
        .map(|class| class.class)
        .collect();
    text.push_str(&format!("const REPLACING := [{}]\n", quoted(&replacing)));
    let baking: Vec<String> = EDITING_METHODS
        .iter()
        .filter_map(|class| match class.restore {
            Restore::Bake(property) => Some(format!("\"{}\": \"{property}\"", class.class)),
            _ => None,
        })
        .collect();
    text.push_str(&format!("const BAKING := {{{}}}\n", baking.join(", ")));
    text.push_str(&format!(
        "const NEW_RESOURCE_FORBIDDEN := [{}]\n",
        quoted(NEW_RESOURCE_FORBIDDEN)
    ));
    text.push_str(&format!(
        "const MAX_CALL_ARGUMENTS := {MAX_CALL_ARGUMENTS}\n"
    ));
    text.push_str(&format!(
        "const MAX_REPEATED_CALLS := {MAX_REPEATED_CALLS}\n"
    ));
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_class_lists_distinct_methods_once() {
        let mut classes = BTreeSet::new();
        for class in EDITING_METHODS {
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
    fn the_gdscript_copy_names_every_class_and_method() {
        let table = gdscript_table();
        for class in EDITING_METHODS {
            assert!(table.contains(&format!("\"{}\": [", class.class)));
            for method in class.methods {
                assert!(table.contains(&format!("\"{method}\"")));
            }
        }
        assert!(table.contains(
            "const CLEARING := {\"TileMapLayer\": [\"clear\"], \"AnimationMixer\": \
             [\"get_animation_library_list\", \"remove_animation_library\"], \"Animation\": \
             [\"clear\"], \"Theme\": [\"clear\"]}"
        ));
        assert!(table.contains("const SETTLE := {\"TileMapLayer\": \"update_internals\"}"));
        assert!(table.contains(
            "const REPLACING := [\"AnimationNodeStateMachine\", \"AnimationNodeBlendTree\", \
             \"AnimationNodeBlendSpace1D\", \"AnimationNodeBlendSpace2D\", \"VisualShader\", \
             \"TileSet\", \"TileSetAtlasSource\", \"MeshLibrary\"]"
        ));
        assert!(table.contains(
            "const BAKING := {\"NavigationRegion2D\": \"navigation_polygon\", \
             \"NavigationRegion3D\": \"navigation_mesh\"}\n"
        ));
        assert!(table.contains("const NEW_RESOURCE_FORBIDDEN := [\"Script\", \"GDExtension\"]"));
        assert!(table.contains("const MAX_CALL_ARGUMENTS := 16\n"));
        assert!(table.ends_with("const MAX_REPEATED_CALLS := 4096\n"));
    }

    #[test]
    fn a_class_name_is_an_identifier() {
        for good in [
            "StyleBoxFlat",
            "AnimationNodeStateMachine",
            "_Private",
            "Node3D",
        ] {
            assert!(is_class_name(good), "{good}");
        }
        for bad in ["", "3D", "Style Box", "res://x.gd", "a-b", &"A".repeat(129)] {
            assert!(!is_class_name(bad), "{bad}");
        }
    }
}
