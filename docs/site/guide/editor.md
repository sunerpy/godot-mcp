---
description: Connect a running Godot editor to the server with the godot_mcp add-on, and what an agent can then do in it.
---

# The editor add-on

The `godot_mcp` editor add-on connects a Godot editor you are running to the server. With it, an
MCP client inspects what the editor holds, edits scenes as ordinary undoable editor actions, saves
them, closes them, sees the editor's window, reads its log, changes the editor's own settings,
makes it pick up files changed on disk, opens and closes scripts, moves its 3D cameras, answers
its dialogs, restarts it and runs the game, while the server keeps its journal, policy and
recovery rules. It needs
the `editor` or `full` profile; the `runtime` profile serves only the two tools that run the game.

## Install it

| Tool | Does |
| --- | --- |
| `editor.addon.status` | Compares `addons/godot_mcp/` with the bundled add-on, file by file, and says what an install, update or remove would do |
| `editor.addon.install` | Writes the add-on and its ownership manifest, refusing any occupied path; with `enable` it also enables the plugin |
| `editor.addon.update` | Replaces the files you did not edit, writes missing ones, and keeps a file you edited beside the new one as a backup |
| `editor.addon.remove` | Removes the files that still match what was installed and disables the plugin; a file you edited stays |

The editor loads the plugin when it starts with the plugin enabled, or when you enable it under
**Project Settings → Plugins**. After an update, `editor.addon.reload` loads the new files.

## How the editor connects

While a project is bound under a profile with editor tools, the server listens on an ephemeral
loopback port and writes a rendezvous file inside the project's `.godot-mcp/` directory: the
endpoint, a fresh 256-bit secret and the project's identity, readable only by your user. The
plugin reads it, dials the endpoint and proves it read the file; a loopback connection without the
secret is refused. One editor is served at a time, and the plugin finds a restarted server, with
its new secret, by itself.

`editor.status.get` reports whether an editor is connected, its Godot and plugin versions, whether
the bundled add-on is a different version, the open scenes and scripts, the scene being edited,
the selection, its undo history, the main screen shown and whether a game runs. Every other editor
tool answers `unavailable`, naming the editor, while none is connected. A tool the connected
plugin is too old for answers `unsupported` and says to update it.

## The Godot MCP panel

The add-on puts a small dock in the editor, with the connection status, a **Pause agent edits**
switch, **Reconnect now** and **Details**, which opens the **Godot MCP** bottom panel. The panel
shows the connection and the server's version, profile and session; the newest 500 requests the
editor answered, each with its method, the scene, node or script path it was about and its
result, but never its arguments; the server's tools, grouped by what they need and whether that is available now;
and the bridge methods the add-on serves. **Copy diagnostics** copies the versions, the
connection, the counts and the last errors for a bug report, without the connection's secret.

<ScreenFigure src="/screens/editor-en-light.webp" dark="/screens/editor-en-dark.webp" width="1600" height="1000" alt="The Godot 4.7.1 editor with the demo project's main scene in the 2D view and the Player node selected. The godot_mcp dock reads godot-mcp connected, and the Godot MCP panel at the bottom lists the requests the editor answered, newest first." caption="The Godot MCP panel on its Activity page, in Godot 4.7.1 with the demo project from examples/site-demo." />

While **Pause agent edits** is on, the editor refuses every change the server asks it to make
before the change starts: the call answers `unavailable`, nothing changed, and reads still work.
Once you turn it off, the client retries with a new `idempotencyId`. Only what passes through the
editor is paused: the server's own file, script, resource and asset writes go on, and the switch
is not a security boundary, since any program running as you can change the project's files. It
lasts until you turn it off, close the editor or reload the add-on. The
[editor bridge reference](../../operator/editor-bridge.md#the-godot-mcp-panel) has the details.

## What it can do

| Tool | Does |
| --- | --- |
| `editor.scene.tree.get` | The edited scene as the editor holds it, unsaved changes included, with a semantic digest |
| `editor.node.get` | One node in full: every property with its type, value and default, its signals and connections |
| `editor.scene.read` | What the engine answers about the edited scene, unsaved changes included: painted tile cells, animation keys, state machine states, navigation polygons and the other reading calls of `scene.read`, each with the errors the editor logged while it ran |
| `editor.types.get`, `editor.types.search` | One engine or project class as this exact Godot build sees it, or the classes whose name matches |
| `editor.scene.edit` | Changes the scene the editor holds as one undoable action, without saving |
| `editor.scene.save` | Saves the edited scene, one open scene, or all of them |
| `editor.scene.open`, `editor.scene.close` | Opens a scene; closes one that holds no unsaved change |
| `editor.selection.set` | Replaces the selection |
| `editor.undo`, `editor.redo` | Undo or redo in the edited scene's history, through the editor's own Undo and Redo |
| `editor.main_screen.set` | Shows the 2D, 3D or Script screen |
| `editor.screenshot.capture` | Captures the editor's window, or its 2D or a 3D viewport |
| `editor.log.get` | What the editor logged: prints, errors and warnings with their source lines |
| `editor.filesystem.scan` | Picks up files changed on disk, imports them and reloads the open scripts that changed |
| `editor.script.open`, `editor.script.close` | Opens a script at a line and column; closes a script tab that holds no unsaved edit |
| `editor.camera.get`, `editor.camera.set` | Reads what the 2D view and the 3D viewports show; moves a 3D viewport's camera to look from one point at another |
| `editor.dialogs.get`, `editor.dialog.respond` | Lists the dialogs the editor shows; presses one button of one of them |
| `editor.resource.preview` | The editor's preview of a resource and its thumbnail, as images |
| `editor.performance.get` | The editor's own performance monitors: frame times, object and node counts, memory |
| `editor.restart` | Restarts the editor once nothing it holds can be lost, and waits for it to connect again |
| `editor.preferences.get`, `editor.preferences.set` | Reads and changes the editor's own settings, the ones its Editor Settings dialog shows |
| `editor.shortcuts.get`, `editor.shortcuts.set` | Reads and rebinds the editor's shortcuts, the ones its Shortcuts tab lists |
| `editor.play.start`, `editor.play.stop` | Runs the game as the editor's Play button does, and stops it |
| `editor.addon.reload` | Reloads the plugin; the editor reconnects |

Reads never change editor state: they open nothing, select nothing, mark nothing unsaved and add
nothing to an undo history.

## Editing the scene the editor holds

`editor.scene.edit` takes the same typed operations as `scene.batch.apply`, naming nodes by path,
and applies them to the scene the editor holds, unsaved changes included. It carries the digest
`editor.scene.tree.get` or `editor.node.get` answered: when the scene changed since, the edit is
refused and nothing changes. The whole edit is one action in the editor's undo history, named
after your `label`, and nothing is saved until `editor.scene.save`. A save counts only when the
editor reports the scene saved or its file holds the scene as the editor does; any other save
answers `uncertain`. Saving a scene also saves every script holding unsaved edits in the editor,
so a script you left unsaved does not block a fix.

`scene.batch.apply` on a scene open in the editor runs there too: the editor's copy must match the
file, and on success the batch is one undo action and the scene is saved. A headless write to a
file the editor holds is refused with `conflict`, so the editor and the server never overwrite
each other.

## Tiles, animations, curves and themes

Painting a tile map, keying an animation or adding curve points has no property to set, so a batch
makes these edits with a `call_method` operation. It may call only the methods of one fixed table:
cells of a `TileMapLayer` or `GridMap`, the points of a `Line2D`, curves and gradients, the
libraries, animations, tracks and keys of an `AnimationPlayer`, the frames of `SpriteFrames`, and
the items of a `Theme`. The call goes to a node of the scene or to a resource the scene embeds;
a resource saved in its own file is edited with `resource.batch.apply`. Arguments are checked
against the method's signature in your Godot build, a new resource can be passed for a call such
as adding an animation, and every answer lists what each call returned. In the editor the calls
are undone like any other edit. The table and the rules are in the
[editor bridge reference](../../operator/editor-bridge.md#editing-calls).

## The project file belongs to the editor

While an editor is connected it owns `project.godot` and the import caches. `project.settings.patch`
is carried out by the editor, which writes the file in its own format, without comments. Importing
goes through the editor too: `project.import.run` and `project.uid.update` are refused, and
`editor.filesystem.scan` with `reimport` imports the assets you name.

## Seeing the editor and reading its log

`editor.screenshot.capture` needs an editor with a window; the PNG is returned as an artifact.
`editor.log.get` reads the editor's log from a cursor, so a client reads each entry once; it needs
Godot 4.5.1 or later.

## The editor's own settings

`editor.preferences.get` reads the editor's settings by name or page by page, each with its type,
value, default and hint, and whether a change needs the editor restarted. `editor.preferences.set`
changes them, all or nothing: a value of the wrong type, or outside the setting's range or
choices, changes nothing. The editor applies the change as its Editor Settings dialog does and
saves it to your editor settings file. That file is yours, not the project's: every project the
editor opens uses it, and the answer gives each value before and after so a change can be set
back. Settings that start programs, hold paths, hosts or credentials, or make the editor rescan
on its own are never changed from here, and the ones holding paths or credentials are not read
either; the [editor bridge reference](../../operator/editor-bridge.md#the-editors-settings) lists
them.

The editor may change more settings in response, and the answer lists those too, under
`also_changed`. A color that a theme preset sets keeps a new value only while the preset is
`Custom`: Godot 4.7.1 switches the preset itself, while 4.4.1, 4.5.1 and 4.6.1 put the color back,
so the change is refused and nothing changes. Set the preset to `Custom` in the same change, as
the Editor Settings dialog does.

## The editor's shortcuts

`editor.shortcuts.get` lists every editor shortcut, such as `script_editor/save`, and every
built-in action of the Shortcuts tab's Common section, such as `ui_accept`, each with the key
events that trigger it and its defaults; `changedOnly` lists only the ones you changed.
`editor.shortcuts.set` binds them, all or nothing: each name maps to its key events, to an empty
list for none, or to `default`. A key event names a key as Godot does (`S`, `F9`, `Enter`) and its
modifiers as flags, where `command_or_control` is Ctrl, or Command on macOS. The editor applies
the change as its Shortcuts tab does and saves it to your editor settings file, and the answer
gives each binding before and after. Godot 4.4.1 and 4.5.1 do not hand their shortcuts to a
plugin, so there both tools answer `unsupported`.

## Closing a scene

`editor.scene.close` closes the edited scene or another open one. Godot discards a scene's unsaved
changes when it closes it, without asking, so a close is refused while the scene holds a change
its file does not: save it first, or undo the change. Closing needs Godot 4.5.1 or later.

## Closing a script tab

`editor.script.close` closes the tab of one script. Godot discards a tab's unsaved edits when a
plugin closes it, so a tab that holds edits its file does not is refused: save them in the editor,
or undo them, first. Closing a script tab needs Godot 4.7.1.

## Cameras

`editor.camera.get` reads where the editor looks: the 2D view's zoom and center, and for each of
the four 3D viewports its camera's position, direction, projection and lens, and whether the
viewport is on screen. `editor.camera.set` moves the camera of a 3D viewport to `position`,
looking at `target`, and answers the camera as the editor holds it a few frames later; the editor
then keeps that view. The viewport must be on screen, so show the 3D screen with
`editor.main_screen.set` first. Moving a camera needs Godot 4.5.1 or later: Godot 4.4.1 moves
the camera on to a point of its own. The 2D view is read but not moved, since the 2D editor sets
it from its own state.

## Dialogs

`editor.dialogs.get` lists the dialogs the editor shows, such as the alert it opens for a scene it
cannot read, each with an `id`, its title, its text and its buttons: `ok`, `cancel`, and custom
ones by the action they report. `editor.dialog.respond` presses one button of one dialog, as a
click does, and answers whether the dialog closed. The button does whatever the editor does for
it, which can save or discard files, so a lost answer blocks further writes until it is resolved.
Nothing answers dialogs by itself: each answer is one call.

## Previews

`editor.resource.preview` asks the editor for the preview its FileSystem dock shows for a
resource, and the small thumbnail, and returns both as PNG artifacts. The editor draws previews
with its renderer, so this needs an editor with a window; a headless editor answers
`unavailable`. Textures, SVG images, materials, meshes, gradients, scripts and sounds get a
preview; a scene written to disk outside the editor gets none. When there is no preview, the
answer says so, never an empty image.

## Restarting the editor

`editor.restart` restarts the editor as **Project → Reload Current Project** does, and answers
once the restarted editor, a new process, has connected. It never loses anything:

- while a scene or script holds unsaved changes, it is refused unless you pass `save: true`,
  which saves the open scenes first, and on Godot 4.7.1 the open scripts too;
- if the editor still asks a question before restarting, the question is cancelled, so it saves
  and discards nothing, and the call says what the editor asked and what `save` already wrote;
- it is refused while the game runs or a dialog is open.

Godot does not pass `--headless` on to the editor it restarts, so an editor started with
`--headless` is refused; start it with `--display-driver headless` instead, which a restart
keeps. An editor the server runs as a managed session is refused too: stop the session and start
a new one. Until the restarted editor connects, the server makes no other change and writes no
project file. When it does not connect within `timeoutMs`, the answer is `uncertain`.

## Files changed on disk

After a script or an asset changed on disk, `editor.filesystem.scan` makes the editor notice:
it rescans, imports what changed, and reloads every open script whose file changed, unless the
script holds unsaved edits. A script it leaves alone is listed with the reason. A scan that would
start an external importer, such as Blender, is refused.

## Running the game

`editor.play.start` runs the main scene, the scene being edited, or a scene you name, and
`editor.play.stop` stops it. The editor saves the open scenes first, and the answer lists what it
saved. It needs an editor with a window, and it is refused when the editor would ask you
something first, such as which scene is the main one. The game runs as the editor's own process,
outside the server's process boundaries, so a project configured as untrusted is refused; run
its game in a managed session instead ([Sessions](sessions.md)).

## Running your own script

`editor.script.run` runs GDScript an agent writes in the editor, on its main thread, once you
start the server with `--allow-scripts`. The script reaches everything the editor does — the
open scenes, `EditorInterface`, the file system — and the answer gives what its `run()`
returned, what it printed and the errors it raised with their line, and every project file that
differs afterwards. On Godot 4.4.1 the editor's log cannot be read, so errors inside `run()` are
not seen. Nothing it changes is in the editor's undo history.

Godot cannot stop a script part way through. The server waits `deadline_ms` and two seconds more;
a run that has not answered by then is `uncertain`, the editor may still be running it, and the
project's changes stay blocked until the editor answers when the run ends, or the transaction is
resolved ([Recovery](../operate/recovery.md)). Only closing the editor ends the code.

## When an answer is lost

Every editor change ends `committed`, `not_started` or `unresolved`. An unresolved change — the
request reached the editor and no answer came back, or a save it made could not be proven —
answers `uncertain` and is never sent again, not on retry and not after the editor reconnects.
One that may have saved a file blocks further writes until the editor's late answer settles it
or it is resolved ([Recovery](../operate/recovery.md)).

## What it does not do

The editor settings listed above, shortcuts on Godot 4.4.1 and 4.5.1, restarting an editor
started with `--headless`, answering the question an editor asks before it restarts, moving the
2D view, the Asset Library and version control are left to you or to the tools you already use; breakpoints and reading the running game
are in managed sessions ([Sessions](sessions.md)); one property of an embedded resource is changed
by writing the resource back; discarding unsaved changes is not offered, and your own scripts
wait for `--allow-scripts`. The
[editor bridge reference](../../operator/editor-bridge.md#what-the-add-on-does-not-do) gives the
way to each.
