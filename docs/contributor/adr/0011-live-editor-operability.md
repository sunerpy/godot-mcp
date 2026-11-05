# 0011 — The live editor is operated through typed tools and one fixed table of editing calls

**Status:** accepted for contract 0.3.0. It builds on
[ADR 0010](0010-complete-development-bridge-strategy.md) and keeps
[ADR 0003](0003-no-arbitrary-script-execution.md) intact.

## Context

Contract 0.2 connected a running editor: an agent could read the scene the editor holds, apply
a scene batch to it, undo, redo, select and open scenes. It still could not do most of the
editor's everyday work: edit without saving and save later, see what the editor shows, read
what it logged, make it notice files changed on disk, open a script at a line, paint a tile
map, key an animation, shape a curve, or press Play. Each of those sent a person back to the
editor, which is the hand-off this project exists to remove.

The editor plugins this was compared with reach the same ground with one general entry
point: call any method, or evaluate any GDScript, inside the editor. That is the shape ADR
0003 refuses. A request that can call anything cannot be reviewed, journaled with its
effect, or undone, and loading a project already runs its code.

## Decision

### Typed tools, one schema each

Every new capability is its own tool with a strict schema: `editor.node.get`,
`editor.types.search`, `editor.log.get`, `editor.preferences.get`, `editor.shortcuts.get`,
`editor.scene.edit`, `editor.scene.save`, `editor.scene.close`, `editor.main_screen.set`,
`editor.screenshot.capture`, `editor.filesystem.scan`, `editor.script.open`,
`editor.preferences.set`, `editor.shortcuts.set`, `editor.play.start`, `editor.play.stop` and,
for managed sessions, `runtime.performance.get`. Tools keep the dotted naming; there is no operation-multiplexing
tool. The bridge protocol does not change: the add-on lists its methods when it connects, and
a tool whose method an older add-on lacks answers `unsupported` with the way to update it.

### Editing calls come from one fixed table

Edits that have no property to set are made by a batch operation, `call_method`, that may call
only the methods one table lists, by class. The table lives in
`crates/godot-mcp-domain/src/editing_methods.rs`; the server's schema lists its methods, and the
editor plugin and the headless helper each carry a generated copy that their crates' tests hold
to it, as a test holds the operator page's table to it. A method is added only there, through
this record:

- every listed method changes only its receiver's own stored state, so the saved file holds
  the whole change and the call can be undone by putting that state back;
- the receiver is a node the scene owns, a resource the scene embeds, or the resource a
  resource batch edits; a resource saved in its own file is refused, because the call would
  change that file too;
- arguments are typed values checked against the method's signature in the running build,
  and a new embedded resource of a listed class is the only object a caller can construct.

Undo in the editor puts the receiver's stored properties back: the ones the call changed are
written back, or the receiver is emptied and every stored property written back in order. The
table names the second strategy, how to empty the class, and a method that brings a class's
stored properties up to date. On Godot 4.4.1, 4.5.1, 4.6.1 and 4.7.1 a probe and the live suite
measured why each is needed:

- an `AnimationMixer` stores its libraries as one dictionary on 4.4.1 and 4.5.1 and as one
  property per library on 4.6.1 and 4.7.1, so a library call changes which properties it stores;
  it has no `clear()`, so it is emptied by removing every library;
- a `TileMapLayer` keeps an erased cell in its stored data until its next update, and writing
  its cells back over the old ones does not restore their stored order, so it is emptied,
  updated, written back and updated again;
- on 4.4.1 and 4.5.1 setting an animation track's path to an empty one raises an engine error,
  so a write-back skips empty node paths, which emptying recreates as empty anyway.

Two later strategies keep the same rules (contract 0.4.0, 2026-11-05). The graph resources —
AnimationNodeStateMachine, AnimationNodeBlendTree, the two blend spaces, VisualShader, TileSet,
TileSetAtlasSource and MeshLibrary — add, rename and remove stored entries that no write takes
away again, so a call on one is made on a rebuilt copy of the outermost embedded resource it
reached, which the node then holds, and undo gives the node the untouched original back. The
navigation regions' bakes rebuild the navigation resource the region holds, which the scene
must embed. A bake is always made with `on_thread` false, so the batch goes on only once the
engine has finished; in the editor it bakes into a rebuilt copy the region then holds, and the
headless helper bakes in a copy of the scene, without its scripts, added to the helper's running
tree, because the engine parses geometry only for nodes inside a running SceneTree: on 4.4.1
and 4.7.1 a bake outside the tree left a 3D navigation mesh with no polygon and a 2D navigation
polygon holding only its outline.

### A save is proven by the editor's report or by the file

The editor emits `scene_saved` right after it tries to write a scene, whether the write
succeeded or not, and `EditorInterface.save_scene()` answers OK without looking; a probe with a
read-only scene directory observed both on Godot 4.4.1 and 4.7.1. So a scene that a save, an
editor-run batch or a run saves counts as saved only when the editor reports it holds no unsaved
change, which Godot 4.7.1 can, or when its file holds the scene as the editor does, by semantic
digest; any other save is unresolved. Godot 4.4.1 names the root of no open scene but the edited
one, so there the plugin shows each open scene in turn to read it. The live suite checks a
read-only scene on 4.4.1, 4.5.1, 4.6.1 and 4.7.1.

### A scene is closed only when closing loses nothing

`EditorInterface.close_scene()`, which Godot 4.5.1 has and 4.4.1 lacks, closes the edited scene
without asking, and a probe on Godot 4.5.1, 4.6.1 and 4.7.1 saw it discard a scene's unsaved
change. So `editor.scene.close` is refused, before anything changes, while the scene may hold a
change its file does not. Godot 4.7.1 reports every unsaved scene. On Godot 4.5.1 and 4.6.1 the
editor names a scene's undo history, which tells whether it is unsaved, only while that scene is
the edited one, so for any other scene the plugin decides by whether its file holds the scene as
the editor does, by semantic digest; when nothing tells, the close is refused. A scene that is
not the edited one is then shown and closed, and the scene that was edited shown again; a close
that did not happen after the editor showed another scene is unresolved unless the editor shows
the scene it showed before. On Godot 4.4.1 the tool answers `unsupported`.

### The editor's own settings are changed as its dialog changes them, and some never

An editor setting is not project content: the editor keeps it in the user's settings file, which
every project it opens shares. Changing one still sent a person to the editor's dialog, so
`editor.preferences.set` changes them, one of the two changes that reach past the project with
`editor.shortcuts.set`, within these limits.

- **Some settings never change from here.** Those that name a program the editor starts, a path,
  a host, a port or a credential, and those that make the editor rescan and import on its own,
  which can start an importer program. One table lists them by name, in
  `crates/godot-mcp-domain/src/editor_preferences.rs`; the server refuses them before the editor
  is asked, and the plugin carries a generated copy, held to the table by a test, and also
  refuses any setting whose property hint is a file, a directory or a password. Those holding
  paths, hosts or credentials are not read either: `network/debug/remote_host`'s hint, for one,
  lists the machine's addresses.
- **It is applied and saved as the dialog does.** A plugin's `set_setting` only changes the value
  in memory. The Editor Settings dialog is what tells the editor's parts that settings changed
  and saves them, 1.5 seconds after a change; on Godot 4.5.1 it does that only while it is open,
  so a probe saw a plugin's change on 4.5.1 never applied and never saved. The plugin therefore
  sends the editor the same settings-changed notification and saves the settings itself; a probe
  on Godot 4.4.1, 4.5.1, 4.6.1 and 4.7.1 saw the change applied at once and the saved file holding
  it, after 8 to 10 milliseconds in headless editors on a Linux x86_64 host.
- **It is proven by the file, or put back.** A change counts once the saved file holds every
  value, or leaves one out at its default. When the file cannot be written, each setting is put
  back and the editor told again, so the answer is that nothing changed; a probe with a read-only
  settings directory saw the save fail and the file kept on all four versions.
- **It is held to the whole configuration.** The editor changes settings of its own in response,
  and puts some back. Probes saw Godot 4.4.1, 4.5.1 and 4.6.1 put a theme color back to the one
  its preset sets, and Godot 4.7.1, whose dialog follows a plugin's change too, switch the preset
  to Custom and change the text editor's selection color with the accent color, and set a
  navigation scheme's mouse buttons and modifier shortcuts with the scheme. The editor decides
  once a frame whether to rebuild its theme, so on 4.6.1 a color changed in the frame of an
  earlier change was put back only a frame later, after it had been saved. The plugin therefore
  captures every setting, shortcut and built-in action before a change, tells the editor of the
  change in a frame of its own, refuses a change whose values the editor did not keep, putting
  everything back, and holds the saved file to everything that changed. What changed beyond the
  named settings is answered as `also_changed`, so it can be set back too.

### Shortcuts are changed as the Shortcuts tab changes them, from Godot 4.6.1

The editor's shortcuts are part of the same settings file, and changing one also sent a person to
the editor's dialog. Godot 4.6.1 and 4.7.1 give a plugin `EditorSettings.get_shortcut_list` and
`get_shortcut`; on 4.4.1 and 4.5.1 a plugin can read no editor shortcut, so `editor.shortcuts.get`
and `editor.shortcuts.set` answer `unsupported` there, and `editor.status.get` says which an
editor is. They
list what the Shortcuts tab lists: the shortcuts the editor registered, which carry their default
events, and the built-in actions of its Common section, whose defaults are the engine's own and
are read from the project settings' defaults, with the override for a feature the platform has.
A key event is a key name and modifier flags, which the editor matches by key and modifiers as it
does. A change sets a shortcut's events, or a built-in action's override, as the tab does, and is
held to the whole configuration and the saved file as a settings change is; the file lists a
shortcut only while it differs from its default. As with settings, a change is added to no undo
history; its answer gives the events before and after.

### Restarting the editor is left to the person using it

The project owner decided on 2026-11-03 to leave restarting the editor out of the goal. Godot's
own restart first asks whether to save each scene with unsaved changes, a question only the
person using the editor can answer, and Godot 4.4.1's restart does not pass `--headless` on to the
editor it starts. A setting whose answer says `restart_required` takes effect once the editor has
restarted.

[ADR 0013](0013-editor-restart.md) takes the restart up for the 2026-11-05 parity goal.

### The editor owns the project file and caches while it is connected

A connected editor holds `project.godot` and the `.godot/` caches in memory and writes them
when it likes. A headless write beside it would be overwritten, or would overwrite the
editor's. So while an editor is connected, `project.settings.patch` is carried out by the
editor and checked against its in-memory values, every other headless write to the file is
refused, and `project.import.run` and `project.uid.update` are refused in favour of
`editor.filesystem.scan`; while either of them runs, no editor connection is admitted. The
cost is that the editor's writer keeps no comments in `project.godot`.

### A scan reloads only what it can prove

`editor.filesystem.scan` reloads an open script tab only when the tab is provably that script's,
holds no unsaved edit and still shows the script's source. It never calls the editor's
reload-everything path, which also reloads modified tabs and can open a dialog that nobody in
an automated session answers. A scan that could start an external importer program is refused.

### Running the project in the editor is the runtime capability

`editor.play.start` and `editor.play.stop` press the editor's Play and Stop. The game is the
editor's own child process, outside every boundary this server holds, so running it is
`runtime_execute`, the tools are in the `runtime` and `full` profiles, and an untrusted project is
refused. Because `full` must stay the union of the other profiles, the `runtime` profile serves
the editor bridge whenever it lists a tool that needs the live editor, and lists only these two.
Reading the game the editor runs is not offered: managed sessions already read, drive and capture
a game inside a boundary.

### The log is captured by a logger the plugin compiles from its own constant

`Logger` and `OS.add_logger` exist from Godot 4.5.1 on, and a script extending `Logger` does not
parse on 4.4.1. The plugin therefore compiles its logger at start from a source constant it
carries, only where the class exists, and takes no code from a caller.

### The default bridge stays GDScript

A native gdext backend was reconsidered for this contract. The one slow path measured, the scene
digest, was not GDScript's speed: an empty `HashingContext.update` call raises an engine error,
and the errors flooding the editor's output made the digest take long enough to lose the
connection. Skipping empty updates fixed it. Nothing measured since calls for native code, so
the optional backend of ADR 0010 stays optional.

## Consequences

- The domain contract is 0.3.0, the add-on 0.3.0, the headless helper version 3 and the runtime
  probe 0.3.0. A helper of version 2 already in a project stays where it is; version 3 installs
  beside it.
- `editor.scene.save`, `editor.filesystem.scan` and `editor.play.start` may write files nobody can
  list, so their journal entries cover the whole project and a lost answer blocks writes until it
  is resolved.
- `editor.preferences.set`, `editor.shortcuts.set` and `editor.scene.close` hold no project file
  in doubt: a lost answer is unresolved and blocks nothing. A changed setting or shortcut affects
  every project the editor opens; its answer gives each value before and after, so a change can
  be set back.
- While a request runs, the editor answers no heartbeat; the bridge does not close a session that
  is running a request within its deadline.
- The plugin runs requests from its `_process`, right after the poll that received them, and
  never while deferred calls are flushed: there a windowed editor refuses to show the progress
  dialog it saves and runs scenes with, and prints engine errors instead. The live suite checks
  saving and running in windowed Godot 4.4.1, 4.5.1, 4.6.1 and 4.7.1 editors.

Not decided here, and not offered by contract 0.3.0: reading the game the editor runs, the
settings listed above, shortcuts on Godot 4.4.1 and 4.5.1, the Asset Library, version control,
breakpoints in the editor's debugger (a managed editor session's debug adapter provides
debugging), more than one editor session, indexed property paths, and headless writes to an
external resource the editor has loaded. Windows and macOS editors: not measured.
