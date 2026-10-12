# Editing the project

Under the `editor` and `full` profiles the server changes the project. Every change is a journaled
transaction with the same rules, whatever it changes.

## The rules every write follows

- **It names what it replaces.** Replacing, patching, moving or deleting a file takes the file's
  current SHA-256 as `expectedSha256`; read it with `project.files.list` or `file.get`. Creating a
  file needs the path to be free. A stale hash answers `conflict` and writes nothing, so an edit
  made against an old copy can never overwrite a newer one.
- **It is journaled first.** The intent is durable before the effect starts, and the outcome after
  it ends. A process that dies in between is settled at the next start: the change either provably
  happened, provably did not, or the transaction is quarantined and writes stay blocked until it
  is repaired ([Recovery](../operate/recovery.md)).
- **It can be repeated safely.** Every write carries an `idempotencyId`; repeating the call returns
  the recorded result instead of writing again.
- **It can be previewed.** `dryRun: true` validates the change and returns its diff without
  writing anything.
- **It stays in the project.** A path that leaves the project root, crosses a symbolic link or
  reaches into the server's own `.godot-mcp/` state is refused before anything happens.

## Files and scripts

| Tool | Does |
| --- | --- |
| `file.get` | Reads one file with its SHA-256 |
| `file.create`, `script.create` | Create a file or a GDScript script at a free path |
| `file.patch`, `script.patch` | Change a file by exactly one of: a whole replacement, line edits, or a unified diff |
| `file.move`, `file.delete` | Move or delete a file, each against its SHA-256 |

`script.create` and `script.patch` also parse the result: a script with parse errors is refused
unless the call sets `allowParseErrors`, and the diagnostics come back either way. Nothing is
executed; `script.check` asks the engine.

## Scenes and resources

`scene.create`, `scene.batch.apply`, `resource.create` and `resource.batch.apply` change text
scenes and resources through a headless Godot process, so they need a Godot build; `resource.update`
re-saves a resource the way Godot writes it. A batch is a list of typed operations — add,
instance, remove, rename, move or duplicate a node, set or clear a property, attach a script,
connect or disconnect a signal, add a group, set a sub-resource — applied in order as one
transaction: if any operation is refused, nothing is published. A node is named by its path from the scene root or by
the `handle` that `scene.tree.get` gave it; a handle from older bytes is refused as a conflict.
Property values are tagged `{type, value}` objects, so a `Vector2` or a `Color` is never confused
with a string.

When the scene is open in an editor connected through the add-on, the same call runs in the
editor and becomes one undoable action there ([The editor add-on](editor.md)).

## Settings and assets

`project.settings.patch` changes `project.godot` through typed operations: the main scene,
autoloads, input actions with their events, plugins, and individual settings. `asset.put` writes
a binary asset, in chunks when it is large, against the SHA-256 of the whole file;
`asset.status` and `asset.delete` read and remove one. `project.uid.update` registers or repairs
the UID of one text resource through a Godot re-save.

## When the editor holds the file

A headless write refuses, with `conflict`, a path the connected editor holds: any open scene, and
an open script whose editor buffer differs from its file. Save or close it in the editor first,
or change a scene through `scene.batch.apply`, which goes through the editor.
