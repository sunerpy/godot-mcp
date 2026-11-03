# Editor bridge

The `godot_mcp` editor add-on connects a running Godot editor to the server. With it, an MCP
client inspects what the editor holds, changes scenes as ordinary undoable editor actions, saves
them, captures the editor's window, reads its log, rescans the project, opens scripts and runs
the project, while the server keeps its journal, policy and recovery rules.

The editor bridge plug-in is not shipped with the first release: version 0.1 shipped only the
frozen bridge protocol and its codec. Contract 0.2 ships the add-on, and contract 0.3 adds the
live edits, saves, captures, log, scans, script navigation and running the project that this
page describes. The protocol it speaks is still the frozen one; see
[Editor bridge protocol](../protocol/bridge-protocol.md).

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
**Project Settings → Plugins**. A project served only by the `runtime` profile installs the
add-on the same way, from a server started with `editor` or `full`.

## How the editor connects

A profile that lists any tool the live editor answers serves the editor: `editor` and `full`,
and `runtime`, which lists only `editor.play.start` and `editor.play.stop`. When such a project
is bound, the server listens on an ephemeral `127.0.0.1` port and writes
`.godot-mcp/editor-bridge.rendezvous` inside the project. That file is private to your user and
holds the endpoint, a fresh 256-bit secret and the project's identity. The plugin reads it,
dials the endpoint over the `godot-mcp-bridge.v1` WebSocket subprotocol and proves it read the
file. A loopback connection without the secret is refused.

- One editor session is served at a time; a newer authenticated session replaces an older one.
- The server sends a heartbeat every 5 seconds and closes a session that stays silent for 30,
  except while the plugin runs a request whose deadline has not passed: the editor runs one
  request at a time on its main thread and answers no heartbeat meanwhile.
- The plugin dials again every second while no server answers, rereading the rendezvous file,
  so a restarted server is found with its new secret.
- When it connects, the plugin lists what it implements. A tool an older plugin does not
  implement answers `unsupported`, names the plugin's version and says to run
  `editor.addon.update` and then `editor.addon.reload`.

`editor.status.get` reports whether an editor is connected, its Godot and plugin versions,
whether the bundled add-on differs from the plugin, the open scenes and scripts with their unsaved state, the
scene being edited, the selection, that scene's undo history, the main screen shown, whether the
editor is scanning or importing, which scene it is running and whether its log can be read.
Every other editor tool answers `unavailable`, naming the editor plane, while no editor is
connected.

## The editor tools

| Tool | Class | What it does |
| --- | --- | --- |
| `editor.status.get` | read | Connection status and editor state, as above. |
| `editor.scene.tree.get` | read | The edited scene as the editor holds it: nodes, scripts, instances, groups, changed stored properties as tagged values, persistent connections, unsaved state and a semantic digest. |
| `editor.node.get` | read | One node as the editor holds it: class, script, instanced scene, ownership, groups, children, every property (or the named ones) with type, hint, usage, tagged value and whether it differs from its default, and its signals with their connections. |
| `editor.types.get` | read | One engine class of this exact Godot build, or a project script class: inheritance, properties with types and defaults, signals, methods and enums. |
| `editor.types.search` | read | The classes whose name contains `query`, optionally only those inheriting `base` or only instantiable ones. |
| `editor.log.get` | read | What the editor logged since the plugin started, from a cursor; see [The editor's log](#the-editors-log). |
| `editor.preferences.get` | read | The editor's own settings, the ones its Editor Settings dialog shows, by name or page by page; see [The editor's settings](#the-editors-settings). |
| `editor.shortcuts.get` | read | The editor's shortcuts and built-in actions, the ones its Shortcuts tab lists, by name or page by page; see [The editor's shortcuts](#the-editors-shortcuts). |
| `editor.scene.open` | mutation | Opens a scene and makes it the one being edited; `reload` re-reads an open scene from its file. |
| `editor.selection.set` | mutation | Replaces the selection. |
| `editor.scene.edit` | mutation | Applies typed operations to the scene the editor holds, as one undoable action; see [Editing what the editor holds](#editing-what-the-editor-holds). |
| `editor.scene.save` | mutation | Saves the edited scene, an open `scene`, or with `all` every open scene. |
| `editor.scene.close` | mutation | Closes the edited scene or an open `scene`; refused while it holds a change its file does not. See [Closing a scene](#closing-a-scene). |
| `editor.undo`, `editor.redo` | mutation | Undo or redo in the edited scene's history through the editor's own Undo and Redo; refused, changing nothing, while the editor's global or remote history also has a step to take. `save: true` also saves the scene. Each answers the scene's semantic digest. |
| `editor.main_screen.set` | mutation | Shows the 2D, 3D or Script main screen. |
| `editor.screenshot.capture` | mutation | Captures the editor's window, or its 2D or one 3D viewport, as a PNG artifact. |
| `editor.filesystem.scan` | mutation | Rescans and imports what changed, then reloads the open script tabs whose files changed. |
| `editor.script.open` | mutation | Opens a GDScript file in the built-in script editor at a line and column. |
| `editor.preferences.set` | mutation | Changes editor settings, all or nothing, and writes them to your editor settings file; see [The editor's settings](#the-editors-settings). |
| `editor.shortcuts.set` | mutation | Binds shortcuts and built-in actions to key events, all or nothing, and writes them to your editor settings file; see [The editor's shortcuts](#the-editors-shortcuts). |
| `editor.play.start`, `editor.play.stop` | runtime | Run the project in the editor, as its Play buttons do, and stop it. |
| `editor.addon.reload` | mutation | Disables and re-enables the plugin; the editor reconnects with a new session. |

Reads never change editor state: they open nothing, select nothing, mark nothing unsaved and add
nothing to any undo history.

## Editing what the editor holds

`editor.scene.edit` applies typed operations, the `scene.batch.apply` model with every node named
by its path, to the scene the editor holds, unsaved changes included:

- `expectedDigest` must be the semantic digest `editor.scene.tree.get` or `editor.node.get`
  answered for the scene as the editor holds it; any other digest is a `conflict` and nothing
  changes;
- the first operation that cannot be applied undoes everything before it, and nothing is
  committed;
- the whole edit, up to 128 operations, is one undo action named `godot-mcp: <label>`;
- nothing is saved; `editor.scene.save` saves. The answer carries the new digest.

The digest covers the whole scene: every node the scene owns, every stored property, every
sub-resource the scene embeds, however deep, and every persistent signal connection. The node
list an answer carries is capped and says so; the digest is not.

`scene.batch.apply` takes the same request whichever way it runs. When the target scene is open in
the connected editor, the server sends the batch there instead of to the headless helper:

- the editor's buffer must equal the file (by semantic digest) and the file must still hash to
  `expectedSha256`; otherwise the call is refused with `conflict` and nothing changes;
- every operation is applied to the scene the editor holds; the first one that cannot be applied
  undoes everything before it, and nothing is committed;
- on success the whole batch is one undo action, `godot-mcp: scene.batch.apply`, and the scene is
  saved. The answer is the headless answer plus `backend: "editor"` and an `editor` object with
  the action, the history version and the live digest;
- a save that is not proven, as [Saving](#saving) describes, undoes the batch: the answer is
  `unavailable` once the file is proven to hold the scene as before, and `uncertain` otherwise.

A dry run always uses the headless helper.

Every headless writer — the `file.*`, `script.*`, `asset.*` and settings tools, and the headless
scene and resource tools — refuses with `conflict` a path the connected editor holds: any open
scene, `project.godot`, and an open script whose editor buffer differs from its file or cannot
be compared. Close or save a scene in the editor, or change it through `editor.scene.edit` or
`scene.batch.apply`; after a script was changed on disk, run `editor.filesystem.scan`.

## Saving

`editor.scene.save` saves the scene being edited, an open `scene`, or with `all` every open
scene. The editor announces a scene saved, and its save call answers success, even when it could
not write the file, so every scene is proven instead: it counts as saved when the editor reports
it holds no unsaved change, which Godot 4.7.1 does, or when its file holds the scene as the editor
does, by semantic digest. Godot 4.4.1 names the root of no open scene but the edited one, so there
the plugin shows each other open scene in turn to read it, and then the edited one again. A scene
that is not proven makes the answer `uncertain`, and writes stop until it is resolved.

- Saving a scene also saves every script tab that holds unsaved edits, and lists those scripts in
  `also_saved`. Such a tab holds its file against headless writes; once it is saved, a fix written
  headless is loaded back into the tab by `editor.filesystem.scan`.
- Before Godot 4.7.1 a save is proven only by the file, so a scene the editor saves differently
  from how it holds it answers `uncertain` too: an AnimationMixer with `reset_on_save` saves the
  properties its RESET animation sets at their RESET values.
- `scene.batch.apply` and `editor.play.start` prove the scenes they save the same way.

## Closing a scene

`editor.scene.close` closes the scene being edited, or an open `scene`, as the editor's Close
Scene does, and answers the scenes left open and the one the editor shows next. Godot closes a
scene that holds unsaved changes without asking and discards them, so a close is refused with
`conflict`, before anything changes, while the scene may hold a change its file does not. Godot
4.7.1 reports every unsaved scene; on Godot 4.5.1 and 4.6.1 the plugin goes by the edited scene's
undo history, and for any other scene by whether its file holds the scene as the editor does, and
refuses when neither tells. Save the scene with `editor.scene.save`, or undo the change, then
close it. A scene that is open but not the edited one is shown, closed, and the scene that was
edited is shown again; that needs the edited scene to have a file. Godot 4.4.1 cannot close a
scene from a plugin, so there the tool answers `unsupported`, and `editor.status.get` says
`scene_close: false`.

## The project file and caches belong to the editor

While an editor is connected, it owns `project.godot` and the `.godot/` caches:

- `project.settings.patch` is carried out by the editor, which writes the file with Godot's own
  writer, so comments in it are not kept. The editor's in-memory value of every changed key must
  still be the file's, or nothing changes. The answer is computed from the file the editor
  wrote, with `changed_keys` and `unrequested_keys`, the keys the editor wrote besides the ones
  asked for. Without an editor connected, the file keeps unrelated keys and comments as before.
- Every other headless write to `project.godot` is refused with `conflict`.
- `project.import.run` and `project.uid.update` are refused with `conflict`; import through the
  editor with `editor.filesystem.scan`, whose `reimport` lists assets to import again. While
  either runs, the server admits no editor connection; the plugin connects once it has ended.

An editor that does not run this plugin cannot be detected. With the editor already open, enable
the plugin in it before writing through the server.

## The editor's log

`editor.log.get` reads what the editor logged since the plugin started: prints, errors, warnings,
and script and shader errors, each with its source location and script backtrace. The plugin
keeps the last 2000 entries. A read returns the entries after the `after` cursor, oldest first,
at most `limit` (100 by default, 500 at most), optionally only some `kinds`; `next` is the cursor
of the next read. `dropped` counts entries the ring no longer holds, and a different `epoch`
means the plugin restarted: a read naming the old epoch starts again from the oldest entry kept
and says `reset`.

Godot 4.4.1 cannot hand its log to a plugin, so there `editor.log.get` answers `unsupported`;
4.5.1, 4.6.1 and 4.7.1 can. The plugin compiles its logger from a source constant it carries;
it takes no code from a caller.

## Captures

`editor.screenshot.capture` captures the editor's whole window, or the 2D viewport or one of the
four 3D viewports of the main screen shown, which `editor.main_screen.set` switches, after the
editor's next drawn frame, optionally scaled down to `maxWidth`. The server reads the PNG back
from the editor in bounded chunks, checks its length and SHA-256, and answers an opaque
`godot-mcp://artifacts/{artifact_id}` resource, as `runtime.screenshot.capture` does; see
[Artifacts](artifacts.md). A capture needs an editor with a window: a headless editor draws
nothing and answers `unavailable`.

## Rescanning the project

`editor.filesystem.scan` has the editor rescan the project's files, import what changed and
register new script classes, as it does when it regains focus; `reimport` also imports up to 256
named assets again. It then reloads every open script tab whose file changed, but only a tab it
can prove is that script's, that holds no unsaved edit and whose buffer still matches the script
it was loaded from. Every other changed tab is left as it is and listed in `stale_scripts` with
the reason. It never reloads every open file, which can open a dialog nobody answers. When the
editor sends scripts to an external program, no tab is touched and `script_reload` says
`skipped_external_editor`.

It waits up to `timeoutMs`: 300000 by default, from 1000 to 600000. On 4.5.1 and later the
answer lists the errors, warnings and script and shader errors logged meanwhile. A scan is
refused when importing could start an external program: a `.blend` or `.fbx` file in the
project while the project enables that importer and the editor has a Blender or FBX2glTF path
set.

## Opening a script

`editor.script.open` opens a GDScript file in the built-in script editor with the caret at `line`
and `column`, both counted from 1, and answers where the caret is. It is refused while the editor
is set to open scripts in an external program (`text_editor/external/use_external_editor`), so it
never starts one.

## Running the project in the editor

`editor.play.start` runs the main scene, or the scene being edited for `scene: "current"`, or the
scene at `scene`, as the editor's Play buttons do; `editor.play.stop` stops it, waiting up to 5
seconds, and answers `was_playing`.

- **Trust.** The game is the editor's own child process, outside every boundary this server
  holds, so it is refused with `permission_denied` when `process.trust` is `untrusted`. A managed
  session, `project.run.start`, runs the game inside one; see
  [Managed sessions](managed-sessions.md).
- **No dialogs.** A start is refused when the editor would ask something first: the project has
  no main scene, or the scene being edited was never saved.
- **It saves.** The editor saves the open scenes before it runs one, so a start names every open
  scene with the bytes the server saw, and the answer lists in `saved` the scenes the editor
  saved, each proven as [Saving](#saving) describes, and in `also_saved` the other files it
  reports saved. Godot 4.4.1 saved every open scene before each run, changed or not, keeping the
  bytes of an unchanged one. When a scene it saved is not proven, the game is stopped and the
  answer is `uncertain`.
- It needs an editor with a window, and is refused while a game runs. A game that ends as soon
  as it starts is reported as `unavailable` with mutation status `committed`, since the editor
  saved first; `editor.log.get` shows why.

`editor.play.start` and `editor.play.stop` are `runtime_execute` tools, in the `runtime` and
`full` profiles. Reading the game the editor runs — its tree, its screen, its input — is not
offered; a managed session offers all three.

## The editor's settings

`editor.preferences.get` reads the settings the editor's Editor Settings dialog shows: 446 on
Godot 4.4.1, 460 on 4.5.1, 480 on 4.6.1 and 499 on 4.7.1, counted in a fresh user profile. It
reads the named `settings`, or every setting in name order, under a `prefix`, `limit` at a time
(100 by default, at most 500) from `offset`; `next_offset` is the next page's, or `null` after
the last. Each entry gives the setting's type, tagged value, default, whether it differs from
the default, its hint and hint string, whether a change needs the editor restarted, and whether
`editor.preferences.set` may change it. A named setting the editor does not have is listed in
`unknown`.

`editor.preferences.set` changes up to 64 settings, all or nothing: each value must be of the
setting's type and inside its range, choices or flags, or nothing changes. The editor applies
them as its Editor Settings dialog does, by telling its parts that its settings changed, and
writes them to its settings file, `editor_settings-4.X.tres` in your Godot configuration
directory. That file is yours, not the project's: every project the editor opens uses it. The
answer gives each setting before and after, and `restart_required` for one that takes effect
only once you restart the editor, which is never restarted for you. Godot 4.5.1's dialog applies
and saves a change only while it is open, so the plugin applies and saves each change itself, on
every version.

The editor may change more settings in response, or put a value back, and a change is held to
all of it. The plugin tells the editor of the change in a frame of its own, as the editor
reconsiders its settings once a frame; every setting named must then hold its new value, and
the saved file every setting and shortcut that changed, named or not. The answer lists the ones
not named under `also_changed`, each before and after, a withheld one by name only. When a value
is not kept or the file cannot be written, everything is put back and nothing is changed: a
value not kept is a `conflict`, a file not written `unavailable`.

- Godot 4.7.1 switches a theme preset to Custom when a color it sets changes, and with the accent
  color it changes the text editor's selection color too; both are listed under `also_changed`.
  Choosing `editors/3d/navigation/navigation_scheme` there also sets that scheme's mouse buttons
  and navigation modifier shortcuts, listed the same way.
- Godot 4.4.1, 4.5.1 and 4.6.1 put a color their theme preset or text editor color theme sets
  back to the preset's, so changing one alone is a `conflict`. Set the preset to `Custom` in the
  same change, as the dialog does: `interface/theme/preset` on 4.4.1 and 4.5.1,
  `interface/theme/color_preset` on 4.6.1, or `text_editor/theme/color_theme`. There, choosing
  a navigation scheme changes that setting alone; set its mouse buttons with it, and its
  modifier shortcuts with `editor.shortcuts.set` on 4.6.1.

Some settings are never changed from here, and the ones that hold paths, hosts or credentials
are not read either: their entries say `withheld: true` and carry no value, default or hint
string.

| Settings | Read | Why |
| --- | --- | --- |
| `export/`, `filesystem/file_server/`, `network/`, `version_control/` | withheld | Paths to export tools and keystores, keystore passwords, the hosts and ports the editor listens on or connects to, the password and port of its file server for remote file systems, and version-control credentials. |
| `asset_library/`, `asset_store/`, `filesystem/external_programs/`, `filesystem/import/`, `filesystem/tools/`, `text_editor/external/` | yes | They choose the programs the editor starts (an external script editor, importers, a denoiser, a terminal) or where it downloads assets from. |
| `docks/filesystem/other_file_extensions`, `docks/filesystem/textfile_extensions`, `interface/editor/behavior/import_resources_when_unfocused`, `interface/editor/import_resources_when_unfocused` | yes | A change makes the editor rescan and import files on its own, which can start an importer program. |

Any other setting whose hint is a file, a directory or a password is withheld and refused too,
whatever its name. A setting that makes the editor save on its own, such as autosave or saving
when it loses focus, can be changed: those saves are the editor's, as when you press Save, and
no transaction records them.

## The editor's shortcuts

`editor.shortcuts.get` lists what the editor's Editor Settings dialog lists on its Shortcuts tab:
every editor shortcut, such as `script_editor/save`, of kind `shortcut`, and every built-in input
action of its Common section, such as `ui_accept`, of kind `action`. That is 564 on Godot 4.6.1
and 581 on 4.7.1, counted in a fresh user profile. It reads the named `shortcuts`, or every one in
name order, under a `prefix`, only those that differ from their defaults with `changedOnly`,
`limit` at a time (100 by default, at most 500) from `offset`; `next_offset` is the next page's,
or `null` after the last. Each entry gives the kind, a label, the key events that trigger it, its
default events, and whether the two differ. A named shortcut the editor does not have is listed in
`unknown`.

A key event names its `key` as Godot names keys, such as `S`, `F9`, `Enter` or `Kp Add`, and its
modifiers as flags: `shift`, `ctrl`, `alt`, `meta`, and `command_or_control`, which is Ctrl, or
Command on macOS, and leaves `ctrl` and `meta` false. `match` says how the editor finds the key:
`keycode`, the default, is the key the user's layout maps; `physical` is the key at that position
on a US QWERTY keyboard, whatever the layout prints on it; `label` is the key the layout prints
it on. An answer adds the editor's own `text` for each event, such as `Ctrl+Alt+S`; a change
takes an answer's events back as they are, `text` included, and does not read it.

`editor.shortcuts.set` binds up to 64 shortcuts and actions, all or nothing: `shortcuts` maps
each name to its key events, at most 16, in order, to an empty list for none, or to `default` for
its default events. An unknown name, or a key Godot does not know, refuses the whole change. The
editor takes the events as its Shortcuts tab gives them and writes them to the same settings file
as its settings, which lists a shortcut only while it differs from its default. As for settings,
the change is held to everything that changed, which the answer gives before and after, and
anything else the editor changed with it under `also_changed`; when the file cannot be written,
everything is put back and the answer is `unavailable`, with nothing changed. A change is added
to no undo history; setting the answer's `before` events back undoes it.

Godot 4.4.1 and 4.5.1 name no shortcut to a plugin, so both tools answer `unsupported` there.
`editor.status.get` reports `shortcuts: true` for an editor that names them.

## Three outcomes, and no replay

Every editor mutation ends in exactly one of three states.

- `committed` — the editor applied it; the journal records the editor's receipt.
- `not_started` — nothing changed; the call answers the editor's own error.
- `unresolved` — the request reached the editor and what it changed is not known: no answer came
  back, because the connection dropped or the deadline passed, or a save it made is not proven.
  The call answers `uncertain` and the change is **never sent again**, not on retry and not after
  the editor reconnects.

An unresolved change that may have saved a file is quarantined, and writes stay blocked until it
is resolved; see [Recovery](recovery.md). `editor.scene.save`, `editor.filesystem.scan` and
`editor.play.start` may also write files nobody can list in advance, so their transactions cover
the whole project. An unresolved change to editor state alone — selection, the open scene, the
main screen, an unsaved undo, a stop, a closed scene, an editor setting, a shortcut — touches no
project file, so it blocks nothing.

## Inside the editor

- The plugin polls its socket and runs every request from the editor main thread, one at a time;
  work requested from any other thread is refused with `not_started`.
- Requests still queued when the connection is lost are dropped, never run later.
- Disabling the plugin removes its dock, its bridge node and its logger. Enabling, reloading and
  disabling it leave the editor's node count and orphan-node count exactly as they were before.

## Limits

- `editor.scene.tree.get`, `editor.node.get` and `editor.selection.set` work on the scene being
  edited; another open scene must first be made current with `editor.scene.open`.
- `editor.node.get` lists at most 512 properties, or any 256 named in `properties`, 256 children,
  256 signals with 64 connections each and 1 MiB of values; past that it says `truncated`, and a
  property it names without a value can be read by naming it.
- On Godot 4.4.1, 4.5.1 and 4.6.1, `unsaved` for an open scene is known only for scenes the
  plugin saw opened or saved, and for an open script only when its source differs from its file
  or the plugin last loaded that tab itself; otherwise it is `null`, and a headless write to
  that path is refused. Godot 4.7.1 reports both directly.
- No tool calls arbitrary editor methods or runs arbitrary scripts.

## What the add-on does not do

Each of these has another way to it, or is left to you on purpose.

| Not offered | The way to it |
| --- | --- |
| Editor settings that start programs, hold paths, hosts or credentials, or rescan on their own | Change them in the editor's Editor Settings; see [The editor's settings](#the-editors-settings). |
| Shortcuts on Godot 4.4.1 and 4.5.1 | Change them in the editor's Editor Settings, under Shortcuts; Godot 4.6.1 and later name them to a plugin. |
| Restarting the editor | Restart it yourself: before it restarts, Godot asks whether to save each scene with unsaved changes, which only you can answer. A setting with `restart_required` takes effect once it has restarted. |
| The Asset Library | Fetch the asset yourself, add it with `asset.put`, and import it with `editor.filesystem.scan` and `reimport`. |
| Version control | Use git, or your version control system, on the project directory. |
| Breakpoints in the editor's debugger | Debug in a managed editor session: `editor.session.start`, then `debug.command`. See [Managed sessions](managed-sessions.md). |
| Reading the game the editor runs | Run the game in a managed session, `project.run.start`, which reads its tree, captures its screen and sends it input. |
| One property of an embedded resource, such as `material:albedo_color` | `editor.node.get` lists an embedded resource's stored properties; write the resource back with the change through a `set_subresource` operation, which replaces it. |
| Closing a scene on Godot 4.4.1 | Close it in the editor; Godot 4.4.1 cannot close a scene from a plugin. |
| Discarding a scene's unsaved changes | Undo them with `editor.undo`, then close the scene; a close never discards. |
| More than one editor | One editor connects to a server; run one server for each project. |
| Running a script or calling an arbitrary editor method | Refused by design; see [ADR 0003](../contributor/adr/0003-no-arbitrary-script-execution.md). |

## Verified on

Godot 4.4.1, 4.5.1, 4.6.1 and 4.7.1, official Linux x86_64 builds, by the live suite
`crates/godot-mcp-server/tests/live_editor_bridge.rs` through the production MCP server: headless
editors, and windowed ones drawn by the OpenGL renderer under Xvfb for saves, captures and
running the project. `make godot-test` runs the suite against each locked Linux editor, and the
display check runs its windowed cases too. Windows and macOS editors: not measured.
