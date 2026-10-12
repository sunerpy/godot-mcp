//! The ADR 0010 spike: the runtime probe's scene-tree read, implemented with
//! gdext, so the GDScript implementation the shipping probe uses can be
//! measured against a native one on identical workloads.
//!
//! [`NativeTreeSnapshot::describe_tree`] walks breadth first from a start node
//! to a depth and a node limit and describes each node exactly as
//! `runtime_probe.gd`'s `_describe_node` does: its path, name, class, script
//! path, child count, public groups and, for a canvas item or a 3D node,
//! whether it is visible in the tree. The benchmark compares the two outputs
//! byte for byte before it compares their times.

use godot::classes::{CanvasItem, Node, Node3D, RefCounted, Resource};
use godot::prelude::*;

struct GodotMcpSpike;

// SAFETY: the one entry point gdext generates for this library; nothing else
// is exported.
#[gdextension]
unsafe impl ExtensionLibrary for GodotMcpSpike {}

/// The native counterpart of the probe's tree read.
#[derive(GodotClass)]
#[class(base = RefCounted, init)]
struct NativeTreeSnapshot {
    base: Base<RefCounted>,
}

#[godot_api]
impl NativeTreeSnapshot {
    /// Describe `start` and its descendants breadth first, to `depth` levels
    /// and at most `limit` nodes.
    #[func]
    fn describe_tree(&self, start: Gd<Node>, depth: i64, limit: i64) -> VarArray {
        let mut nodes = VarArray::new();
        let mut pending: std::collections::VecDeque<(Gd<Node>, i64)> =
            std::collections::VecDeque::new();
        pending.push_back((start, 0));
        while let Some((node, level)) = pending.pop_front() {
            if i64::try_from(nodes.len()).unwrap_or(i64::MAX) >= limit {
                break;
            }
            nodes.push(&describe(&node).to_variant());
            if level < depth {
                for child in node.get_children().iter_shared() {
                    pending.push_back((child, level + 1));
                }
            }
        }
        nodes
    }
}

fn put(entry: &mut VarDictionary, key: &str, value: Variant) {
    entry.set(&key.to_variant(), &value);
}

/// One node, keyed exactly as `_describe_node` keys it.
fn describe(node: &Gd<Node>) -> VarDictionary {
    let mut entry = VarDictionary::new();
    put(&mut entry, "path", GString::from(&node.get_path()).to_variant());
    put(&mut entry, "name", GString::from(&node.get_name()).to_variant());
    put(&mut entry, "class", node.get_class().to_variant());
    let script = node
        .get_script()
        .map(|script| script.upcast::<Resource>().get_path().to_variant())
        .unwrap_or_default();
    put(&mut entry, "script", script);
    put(&mut entry, "children", node.get_child_count().to_variant());
    let mut groups = VarArray::new();
    for group in node.get_groups().iter_shared() {
        let group = GString::from(&group);
        if !group.to_string().starts_with('_') {
            groups.push(&group.to_variant());
        }
    }
    put(&mut entry, "groups", groups.to_variant());
    if let Ok(item) = node.clone().try_cast::<CanvasItem>() {
        put(&mut entry, "visible", item.is_visible_in_tree().to_variant());
    } else if let Ok(spatial) = node.clone().try_cast::<Node3D>() {
        put(&mut entry, "visible", spatial.is_visible_in_tree().to_variant());
    }
    entry
}
