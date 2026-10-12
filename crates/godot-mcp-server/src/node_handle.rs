//! Revision-bound node handles.
//!
//! A handle names one node of one scene *as it was read*: it is derived from the
//! scene's project-relative path, the node's path and the SHA-256 of the exact
//! bytes the read parsed. Any change to the scene's bytes therefore leaves every
//! handle of the older bytes naming nothing, and a stale handle refuses as a
//! conflict instead of targeting whichever node holds that path now. The
//! handle is opaque: neither the scene nor the node path can be read back from
//! it, so it can only be obtained from an answer, never composed by a caller.

use sha2::{Digest, Sha256};

/// The prefix every node handle starts with. `:` cannot occur in a Godot node
/// name, so a handle is never mistaken for a node path.
pub(crate) const PREFIX: &str = "nh1:";
/// Hex digits after the prefix: 128 bits of the domain-separated digest.
const DIGEST_HEX: usize = 32;

/// The handle of the node at `node_path` in the scene at `scene`
/// (project-relative) whose current bytes hash to `scene_sha256`.
pub(crate) fn mint(scene: &str, node_path: &str, scene_sha256: &str) -> String {
    let mut digest = Sha256::new();
    for part in ["godot-mcp node handle 1", scene, node_path, scene_sha256] {
        digest.update((part.len() as u64).to_be_bytes());
        digest.update(part.as_bytes());
    }
    let mut handle = String::with_capacity(PREFIX.len() + DIGEST_HEX);
    handle.push_str(PREFIX);
    for byte in digest.finalize().iter().take(DIGEST_HEX / 2) {
        handle.push_str(&format!("{byte:02x}"));
    }
    handle
}

/// Whether `value` has the shape of a minted handle.
pub(crate) fn well_formed(value: &str) -> bool {
    value.strip_prefix(PREFIX).is_some_and(|digest| {
        digest.len() == DIGEST_HEX
            && digest
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn a_handle_changes_with_the_scene_the_node_and_the_bytes() {
        let handle = mint("scenes/main.tscn", "Player", SHA);
        assert_eq!(handle, mint("scenes/main.tscn", "Player", SHA));
        assert!(well_formed(&handle), "{handle}");
        for other in [
            mint("scenes/other.tscn", "Player", SHA),
            mint("scenes/main.tscn", "Enemy", SHA),
            mint("scenes/main.tscn", "Player", &SHA.replace('0', "1")),
        ] {
            assert_ne!(handle, other);
        }
        // The node path and the scene are not readable from the handle.
        assert!(
            !handle.contains("Player") && !handle.contains("main"),
            "{handle}"
        );
    }

    #[test]
    fn only_the_minted_shape_is_well_formed() {
        assert!(!well_formed("Player"));
        assert!(!well_formed("nh1:"));
        assert!(!well_formed("nh1:xyz"));
        assert!(!well_formed(&format!("nh1:{}", "A".repeat(32))));
        assert!(!well_formed(&format!("nh1:{}", "a".repeat(31))));
        assert!(well_formed(&format!("nh1:{}", "a".repeat(32))));
    }
}
