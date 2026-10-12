# The editor add-on

The `godot_mcp` editor add-on connects a Godot editor you are running to the server. With it, an
MCP client reads the scene the editor holds and changes it as ordinary, undoable editor actions,
while the server keeps its journal, policy and recovery rules. It needs the `editor` or `full`
profile.

## Install it

| Tool | Does |
| --- | --- |
| `editor.addon.status` | Compares `addons/godot_mcp/` with the bundled add-on, file by file, and says what an install, update or remove would do |
| `editor.addon.install` | Writes the add-on and its ownership manifest, refusing any occupied path; with `enable` it also enables the plugin |
| `editor.addon.update` | Replaces the files you did not edit, writes missing ones, and keeps a file you edited beside the new one as a backup |
| `editor.addon.remove` | Removes the files that still match what was installed and disables the plugin; a file you edited stays |

The editor loads the plugin when it starts with the plugin enabled, or when you enable it under
**Project Settings → Plugins**.

## How the editor connects

While a project is bound under `editor` or `full`, the server listens on an ephemeral loopback port
and writes a rendezvous file inside the project's `.godot-mcp/` directory: the endpoint, a fresh
256-bit secret and the project's identity, readable only by your user. The plugin reads it, dials
the endpoint and proves it read the file; a loopback connection without the secret is refused.
One editor is served at a time, and the plugin finds a restarted server, with its new secret, by
itself.

`editor.status.get` reports whether an editor is connected, its Godot and plugin versions, the
open scenes, the scene being edited, the selection and its undo history. Every other editor tool
answers `unavailable`, naming the editor, while none is connected.

## What it can do

| Tool | Does |
| --- | --- |
| `editor.scene.tree.get` | The edited scene as the editor holds it, unsaved changes included, with a semantic digest |
| `editor.types.get` | One engine class as this exact Godot build defines it, or a project class by its `class_name` |
| `editor.scene.open` | Opens a scene and makes it the one being edited |
| `editor.selection.set` | Replaces the selection |
| `editor.undo`, `editor.redo` | Undo or redo in the edited scene's history, through the editor's own Undo and Redo |
| `editor.addon.reload` | Reloads the plugin; the editor reconnects |

Reads never change editor state: they open nothing, select nothing, mark nothing unsaved and add
nothing to an undo history.

## Changing an open scene

`scene.batch.apply` takes the same request whichever way it runs. When the scene is open in the
connected editor, the batch runs there: the editor's copy must match the file, every operation is
applied to the scene the editor holds, and on success the whole batch is one undo action and the
scene is saved. A headless write to a file the editor holds is refused with `conflict`, so the
editor and the server never overwrite each other.

## When an answer is lost

Every editor change ends `committed`, `not_started` or `unresolved`. An unresolved change — the
request reached the editor and no answer came back — answers `uncertain` and is never sent again,
not on retry and not after the editor reconnects. One that may have saved a file blocks further
writes until it is resolved ([Recovery](../operate/recovery.md)).
