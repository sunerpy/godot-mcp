# Bundled canonical-write helper scripts (versioned)

Every file in this directory is **shipped with the server**, committed to the
repository and pinned by SHA-256 in `crates/godot-mcp-engine/src/canonical/helper.rs`.

Rules that hold for this directory:

* **Never generated at runtime, never fetched from the network.** The Rust side
  embeds these bytes with `include_str!` and a unit test asserts the pinned
  digest of each script, so a substituted or edited helper is a build failure
  rather than a silent behaviour change.
* **Versioned by path.** The installed `res://` location carries the version
  (`res://godot_mcp_helpers/<version>/<name>.gd`), so two server versions can
  coexist in one project without one overwriting the other's helper.
* **Pure GDScript.** No gdext, no GDExtension, no compiled artifact. The helper
  runs in a supervised, disposable, exiting Godot subprocess launched through
  the Todo 14 launch protocol.
* **Staging only.** The helper writes exclusively inside
  `res://godot_mcp_helpers/<version>/staging/`. It never writes the caller's
  target path. Publishing the staged artifact onto the target is done by the
  Rust side under the Todo 2 lease, as a compare-and-swap against the recorded
  preimage.

| Script | Purpose |
| --- | --- |
| `canonical_write.gd` | Create/update text scenes and resources, and create/register/query `ResourceUID` values. |

## Why the engine must do this

`ResourceUID.create_id_for_path()` is seeded by the engine with the path *and*
the project name, and `id_to_text()` owns the `uid://` spelling. Reproducing
either in Rust would be reimplementing UID derivation, which this project
forbids. Likewise the canonical `.tscn`/`.tres` serialisation belongs to
`ResourceSaver.save()`; hand-written resource text is not canonical bytes.

## Why exit 0 is not enough

A real Godot build **exits 0 with empty output when handed a flag it does not
support** (Todo 13 observed this directly on an export-template build, and the
official CLI documentation states that unknown command-line arguments have no
effect whatsoever). So the helper's exit status alone can never prove the write
happened. The Rust side requires the conjunction of:

1. its own recorded pre-hash/post-hash rules for the on-disk mutation, and
2. a **clean-terminal** `process_launch` proof on the linked launch transaction
   (confirmed containment, `exited-clean`, exit code 0, no signal, and a
   post-exit empty boundary).

Neither half alone is ever sufficient.
