//! The engine methods a batch's `call_method` operation may call.
//!
//! Some edits have no property to set: painting a `TileMapLayer`, keying an
//! `Animation`, adding points to a `Curve`. A batch makes them by calling one
//! of the methods listed here on an object the edit owns — a node of the
//! scene, a resource embedded in it, or the resource being edited. Every
//! listed method changes only its receiver's own stored state, so the file the
//! batch saves holds the whole change, and the editor can undo it by putting
//! the receiver's stored properties back.
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
];

/// The classes a `new_resource` argument may create: a new resource embedded
/// in the edit, handed to a call such as `add_animation`, whose own state
/// later operations then set.
pub const NEW_RESOURCE_CLASSES: &[&str] = &[
    "Animation",
    "AnimationLibrary",
    "Curve",
    "Curve2D",
    "Curve3D",
    "Gradient",
    "SpriteFrames",
    "StyleBoxFlat",
    "Theme",
];

/// The most arguments one call may take.
pub const MAX_CALL_ARGUMENTS: usize = 16;

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
                Restore::WriteBack => None,
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
    text.push_str(&format!(
        "const NEW_RESOURCE_CLASSES := [{}]\n",
        quoted(NEW_RESOURCE_CLASSES)
    ));
    text.push_str(&format!(
        "const MAX_CALL_ARGUMENTS := {MAX_CALL_ARGUMENTS}\n"
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
        assert!(table.ends_with("const MAX_CALL_ARGUMENTS := 16\n"));
    }
}
