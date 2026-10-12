# Editor bridge

The `godot_mcp` editor add-on connects a running Godot editor to the server. With it, an MCP
client reads the scene the editor holds and changes it as ordinary, undoable editor actions,
while the server keeps its journal, policy and recovery rules.

**The editor bridge plug-in is not shipped with the first release.** Version 0.1 shipped only
the frozen bridge protocol and its codec. Contract 0.2 ships the add-on this page describes; the
protocol it speaks is still the frozen one, see [Editor bridge protocol](../protocol/bridge-protocol.md).

## Install and enable the add-on

The `editor` and `full` profiles carry four add-on tools. Each change is one journaled
transaction with a precondition on every path it touches.

| Tool | What it does |
| --- | --- |
| `editor.addon.status` | Compares `addons/godot_mcp/` with the bundled add-on, file by file, and says what an install, update or remove would do. Changes nothing. |
| `editor.addon.install` | Writes the add-on and its ownership manifest, `addons/godot_mcp/install-manifest.json`. Refuses any occupied path. `enable: true` also enables the plugin in `project.godot`. |
| `editor.addon.update` | Replaces files you did not edit, writes missing ones, and keeps a file you edited as `<name>.godot-mcp-backup` beside the new one. Changes nothing when current. |
| `editor.addon.remove` | Removes the files whose content still matches what was installed, with the `.uid` files Godot wrote for them, and disables the plugin. A file you edited stays. |

A file the server never wrote is *foreign*: install and update refuse rather than overwrite it.
The editor loads the plugin when it starts with the plugin enabled, or when you enable it under
**Project Settings → Plugins**.

## How the editor connects

When a project is bound under the `editor` or `full` profile, the server listens on an ephemeral
`127.0.0.1` port and writes `.godot-mcp/editor-bridge.rendezvous` inside the project. That file is
private to your user and holds the endpoint, a fresh 256-bit secret and the project's identity.
The plugin reads it, dials the endpoint over the `godot-mcp-bridge.v1` WebSocket subprotocol and
proves it read the file. A loopback connection
without the secret is refused.

- One editor session is served at a time; a newer authenticated session replaces an older one.
- The server sends a heartbeat every 5 seconds and closes a session that stays silent for 30.
- The plugin dials again every second while no server answers, rereading the rendezvous file,
  so a restarted server is found with its new secret.

`editor.status.get` reports whether an editor is connected, its Godot and plugin versions, the open
scenes, the scene being edited, the selection and that scene's undo history. Every other editor
tool answers `unavailable`, naming the editor plane, while no editor is connected.

## The editor tools

| Tool | Class | What it does |
| --- | --- | --- |
| `editor.status.get` | read | Connection status and editor state, as above. |
| `editor.scene.tree.get` | read | The edited scene as the editor holds it: nodes, scripts, instances, groups, changed stored properties as tagged values, persistent connections, unsaved state and a semantic digest. |
| `editor.types.get` | read | One engine class as this exact Godot build defines it: inheritance, properties with types and defaults, signals, methods and enums. |
| `editor.scene.open` | mutation | Opens a scene and makes it the one being edited. |
| `editor.selection.set` | mutation | Replaces the selection. |
| `editor.undo`, `editor.redo` | mutation | Undo or redo in the edited scene's history through the editor's own Undo and Redo, so the editor's history and unsaved state stay its own; refused, changing nothing, while the editor's global or remote history also has a step to take. `save: true` also saves the scene. Each answers the scene's semantic digest. |
| `editor.addon.reload` | mutation | Disables and re-enables the plugin; the editor reconnects with a new session. |

Reads never change editor state: they open nothing, select nothing, mark nothing unsaved and add
nothing to any undo history.

## Editing a scene that is open in the editor

`scene.batch.apply` takes the same request whichever way it runs. When the target scene is open in
the connected editor, the server sends the batch there instead of to the headless helper:

- the editor's buffer must equal the file (by semantic digest) and the file must still hash to
  `expectedSha256`; otherwise the call is refused with `conflict` and nothing changes;
- every operation is applied to the scene the editor holds; the first one that cannot be applied
  undoes everything before it, and nothing is committed;
- on success the whole batch is one undo action, `godot-mcp: scene.batch.apply`, and the scene is
  saved. The answer is the headless answer plus `backend: "editor"` and an `editor` object with
  the action, the history version and the live digest.

A dry run always uses the headless helper.

Every headless writer — the `file.*`, `script.*`, `asset.*` and settings tools, and the headless
scene and resource tools — refuses with `conflict` a path the connected editor holds: any open
scene, and an open script whose editor buffer differs from its file or cannot be compared. Close
it or save it in the editor, or change a scene through `scene.batch.apply`.

## Three outcomes, and no replay

Every editor mutation ends in exactly one of three states.

- `committed` — the editor applied it; the journal records the editor's receipt.
- `not_started` — nothing changed; the call answers the editor's own error.
- `unresolved` — the request reached the editor and no answer came back: the connection dropped or
  the deadline passed. The call answers `uncertain` and the change is **never sent again**, not on
  retry and not after the editor reconnects.

An unresolved change that may have saved a file (a scene batch, or `editor.undo`/`editor.redo`
with `save`) is quarantined, and writes stay blocked until it is resolved; see
[Recovery](recovery.md). An unresolved change to editor state alone (selection, the open scene,
an unsaved undo) touches no project file, so it blocks nothing.

## Inside the editor

- The plugin polls its socket and runs every request from the editor main thread; work requested
  from any other thread is refused with `not_started`.
- Requests still queued when the connection is lost are dropped, never run later.
- Disabling the plugin removes its dock and its bridge node. Enabling, reloading and disabling it
  leave the editor's node count and orphan-node count exactly as they were before.

## Limits

- `editor.scene.tree.get` and `editor.selection.set` work on the scene being edited; another open
  scene must first be made current with `editor.scene.open`.
- On Godot 4.4.1, 4.5.1 and 4.6.1, `unsaved` for an open scene is known only for scenes the
  plugin saw opened or saved, and for an open script only when its source differs from its file;
  otherwise it is `null`, and a headless write to that path is refused. Godot 4.7.1 reports both
  directly.
- No tool calls arbitrary editor methods or runs arbitrary scripts.

## Verified on

Godot 4.4.1, 4.5.1, 4.6.1 and 4.7.1, official Linux x86_64 builds, as headless editors driven
through an MCP client by `scripts/real_client/m4_smoke.py`.
