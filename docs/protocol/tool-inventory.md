<!-- docs-generated: tool-inventory -->
<!-- Regenerate with: cargo run -p godot-mcp-cli --features test-hooks --bin docs-check -- --write -->
# Profiles, tools and resources

This page is generated from `godot_mcp_domain::catalog::TOOL_CATALOG`, the one owner of every public
method's name, description, capability, plane, annotations and the handler key
its input schema and handler come from. Editing it by hand fails the docs
check; change the canonical source and regenerate instead.

`full` is the union of `core`, `editor` and `runtime`. The three script tools,
`script.run`, `editor.script.run` and `runtime.script.run`, run nothing until the
operator turns script execution on for a launch; see
[Script execution](../operator/profiles-and-outcomes.md#script-execution).
`unsafe_script` is a reserved identifier with no capability variant, no handler
and no profile membership.

Discovery reports what this build implements. A profile is an authorization
ceiling: it lists exactly the catalog methods it authorizes, and the catalog
contains only methods with a schema and a production handler, so a planned
method is never listed. A listed method whose plane is temporarily
unreachable answers `unavailable` and names the plane. See
[Profiles and tool outcomes](../operator/profiles-and-outcomes.md).

## Profile sizes

| Profile | Capabilities | Tools | Resources |
| --- | --- | --- | --- |
| `core` | 1 | 22 | 4 |
| `editor` | 3 | 81 | 5 |
| `runtime` | 3 | 74 | 5 |
| `full` | 5 | 129 | 5 |

## Methods

| Method | Capability | Needs | Read-only | Destructive | Idempotent | Open world |
| --- | --- | --- | --- | --- | --- | --- |
| `android.deploy` | `runtime_execute` | `headless_engine` | no | yes | no | yes |
| `android.devices.list` | `read` | `headless_engine` | yes | no | yes | yes |
| `asset.delete` | `project_write` | `project_files` | no | yes | yes | no |
| `asset.put` | `project_write` | `project_files` | no | yes | yes | no |
| `asset.status` | `read` | `project_files` | yes | no | yes | no |
| `capabilities.get` | `read` | `project_files` | yes | no | yes | no |
| `debug.command` | `runtime_execute` | `editor_session` | no | no | no | no |
| `debug.session.get` | `read` | `editor_session` | yes | no | yes | no |
| `editor.addon.install` | `project_write` | `project_files` | no | no | yes | no |
| `editor.addon.reload` | `editor_mutate` | `live_editor` | no | no | yes | no |
| `editor.addon.remove` | `project_write` | `project_files` | no | yes | yes | no |
| `editor.addon.status` | `read` | `project_files` | yes | no | yes | no |
| `editor.addon.update` | `project_write` | `project_files` | no | yes | yes | no |
| `editor.camera.get` | `read` | `live_editor` | yes | no | yes | no |
| `editor.camera.set` | `editor_mutate` | `live_editor` | no | no | yes | no |
| `editor.dialog.respond` | `editor_mutate` | `live_editor` | no | yes | no | no |
| `editor.dialogs.get` | `read` | `live_editor` | yes | no | yes | no |
| `editor.filesystem.scan` | `editor_mutate` | `live_editor` | no | yes | yes | no |
| `editor.log.get` | `read` | `live_editor` | yes | no | yes | no |
| `editor.main_screen.set` | `editor_mutate` | `live_editor` | no | no | yes | no |
| `editor.node.get` | `read` | `live_editor` | yes | no | yes | no |
| `editor.performance.get` | `read` | `live_editor` | yes | no | yes | no |
| `editor.play.start` | `runtime_execute` | `live_editor` | no | no | no | yes |
| `editor.play.stop` | `runtime_execute` | `live_editor` | no | yes | yes | no |
| `editor.preferences.get` | `read` | `live_editor` | yes | no | yes | no |
| `editor.preferences.set` | `editor_mutate` | `live_editor` | no | no | yes | no |
| `editor.redo` | `editor_mutate` | `live_editor` | no | yes | no | no |
| `editor.resource.preview` | `editor_mutate` | `live_editor` | no | no | no | no |
| `editor.restart` | `editor_mutate` | `live_editor` | no | yes | no | no |
| `editor.scene.close` | `editor_mutate` | `live_editor` | no | no | yes | no |
| `editor.scene.edit` | `editor_mutate` | `live_editor` | no | yes | no | no |
| `editor.scene.open` | `editor_mutate` | `live_editor` | no | no | yes | no |
| `editor.scene.read` | `read` | `live_editor` | yes | no | yes | no |
| `editor.scene.save` | `editor_mutate` | `live_editor` | no | yes | yes | no |
| `editor.scene.tree.get` | `read` | `live_editor` | yes | no | yes | no |
| `editor.screenshot.capture` | `editor_mutate` | `live_editor` | no | no | no | no |
| `editor.script.close` | `editor_mutate` | `live_editor` | no | no | yes | no |
| `editor.script.open` | `editor_mutate` | `live_editor` | no | no | yes | no |
| `editor.script.run` | `editor_mutate` | `live_editor` | no | yes | no | yes |
| `editor.selection.set` | `editor_mutate` | `live_editor` | no | no | yes | no |
| `editor.session.start` | `runtime_execute` | `headless_engine` | no | no | no | yes |
| `editor.session.stop` | `runtime_execute` | `editor_session` | no | yes | yes | no |
| `editor.shortcuts.get` | `read` | `live_editor` | yes | no | yes | no |
| `editor.shortcuts.set` | `editor_mutate` | `live_editor` | no | no | yes | no |
| `editor.status.get` | `read` | `live_editor` | yes | no | yes | no |
| `editor.types.get` | `read` | `live_editor` | yes | no | yes | no |
| `editor.types.search` | `read` | `live_editor` | yes | no | yes | no |
| `editor.undo` | `editor_mutate` | `live_editor` | no | yes | no | no |
| `export.presets.list` | `read` | `headless_engine` | yes | no | yes | no |
| `file.create` | `project_write` | `project_files` | no | no | yes | no |
| `file.delete` | `project_write` | `project_files` | no | yes | yes | no |
| `file.get` | `read` | `project_files` | yes | no | yes | no |
| `file.move` | `project_write` | `project_files` | no | yes | yes | no |
| `file.patch` | `project_write` | `project_files` | no | yes | yes | no |
| `gdscript.diagnostics.get` | `read` | `project_files` | yes | no | yes | no |
| `gdscript.impact.get` | `read` | `project_files` | yes | no | yes | no |
| `gdscript.references.get` | `read` | `project_files` | yes | no | yes | no |
| `gdscript.symbols.get` | `read` | `project_files` | yes | no | yes | no |
| `health.get` | `read` | `project_files` | yes | no | yes | no |
| `input.action.send` | `runtime_execute` | `game_session` | no | no | no | no |
| `input.key.send` | `runtime_execute` | `game_session` | no | no | no | no |
| `input.mouse.send` | `runtime_execute` | `game_session` | no | no | no | no |
| `input.recording.start` | `runtime_execute` | `game_session` | no | no | no | no |
| `input.recording.stop` | `runtime_execute` | `game_session` | no | no | no | no |
| `input.replay` | `runtime_execute` | `game_session` | no | no | no | no |
| `input.sequence.send` | `runtime_execute` | `game_session` | no | no | no | no |
| `project.batch.apply` | `project_write` | `headless_engine` | no | yes | yes | no |
| `project.bind` | `read` | `project_files` | yes | no | yes | no |
| `project.create` | `project_write` | `project_files` | no | no | yes | no |
| `project.dependencies.audit` | `read` | `project_files` | yes | no | yes | no |
| `project.dependencies.get` | `read` | `project_files` | yes | no | yes | no |
| `project.discover` | `read` | `project_files` | yes | no | yes | no |
| `project.export` | `export_destructive` | `headless_engine` | no | no | no | yes |
| `project.export.cancel` | `export_destructive` | `headless_engine` | no | yes | yes | no |
| `project.export.status` | `read` | `headless_engine` | yes | no | yes | no |
| `project.files.list` | `read` | `project_files` | yes | no | yes | no |
| `project.import.run` | `project_write` | `headless_engine` | no | no | yes | no |
| `project.info.get` | `read` | `project_files` | yes | no | yes | no |
| `project.metrics.get` | `read` | `project_files` | yes | no | yes | no |
| `project.run` | `runtime_execute` | `headless_engine` | no | no | no | yes |
| `project.run.logs` | `read` | `headless_engine` | yes | no | yes | no |
| `project.run.start` | `runtime_execute` | `headless_engine` | no | no | no | yes |
| `project.run.status` | `read` | `headless_engine` | yes | no | yes | no |
| `project.run.stop` | `runtime_execute` | `game_session` | no | yes | yes | no |
| `project.search` | `read` | `project_files` | yes | no | yes | no |
| `project.settings.get` | `read` | `project_files` | yes | no | yes | no |
| `project.settings.patch` | `project_write` | `project_files` | no | yes | yes | no |
| `project.signals.get` | `read` | `project_files` | yes | no | yes | no |
| `project.test` | `runtime_execute` | `headless_engine` | no | no | no | yes |
| `project.uid.get` | `read` | `project_files` | yes | no | yes | no |
| `project.uid.update` | `project_write` | `headless_engine` | no | yes | yes | no |
| `project.validate` | `read` | `project_files` | yes | no | yes | no |
| `resource.batch.apply` | `project_write` | `headless_engine` | no | yes | yes | no |
| `resource.create` | `project_write` | `headless_engine` | no | no | yes | no |
| `resource.dependencies.get` | `read` | `project_files` | yes | no | yes | no |
| `resource.read` | `runtime_execute` | `headless_engine` | no | no | yes | no |
| `resource.tree.get` | `read` | `project_files` | yes | no | yes | no |
| `resource.update` | `project_write` | `headless_engine` | no | yes | yes | no |
| `runtime.image.compare` | `runtime_execute` | `game_session` | no | no | no | no |
| `runtime.navigation.path.get` | `read` | `game_session` | yes | no | yes | no |
| `runtime.node.move` | `runtime_execute` | `game_session` | no | yes | no | no |
| `runtime.nodes.find` | `read` | `game_session` | yes | no | yes | no |
| `runtime.performance.get` | `read` | `game_session` | yes | no | yes | no |
| `runtime.properties.batch.get` | `read` | `game_session` | yes | no | yes | no |
| `runtime.properties.get` | `read` | `game_session` | yes | no | yes | no |
| `runtime.properties.set` | `runtime_execute` | `game_session` | no | yes | no | no |
| `runtime.recording.capture` | `runtime_execute` | `game_session` | no | no | no | no |
| `runtime.sample` | `read` | `game_session` | yes | no | yes | no |
| `runtime.screenshot.capture` | `runtime_execute` | `game_session` | no | no | no | no |
| `runtime.script.run` | `runtime_execute` | `game_session` | no | yes | no | yes |
| `runtime.signals.get` | `read` | `game_session` | yes | no | yes | no |
| `runtime.time_scale.set` | `runtime_execute` | `game_session` | no | yes | no | no |
| `runtime.tree.get` | `read` | `game_session` | yes | no | yes | no |
| `runtime.ui.get` | `read` | `game_session` | yes | no | yes | no |
| `runtime.ui.press` | `runtime_execute` | `game_session` | no | no | no | no |
| `runtime.wait` | `read` | `game_session` | yes | no | yes | no |
| `scene.batch.apply` | `project_write` | `headless_engine` | no | yes | yes | no |
| `scene.create` | `project_write` | `headless_engine` | no | no | yes | no |
| `scene.dependencies.get` | `read` | `project_files` | yes | no | yes | no |
| `scene.exports.get` | `read` | `project_files` | yes | no | yes | no |
| `scene.nodes.find` | `read` | `project_files` | yes | no | yes | no |
| `scene.read` | `runtime_execute` | `headless_engine` | no | no | yes | no |
| `scene.save` | `project_write` | `live_editor` | no | yes | yes | no |
| `scene.tree.get` | `read` | `project_files` | yes | no | yes | no |
| `script.check` | `runtime_execute` | `headless_engine` | no | no | yes | no |
| `script.create` | `project_write` | `project_files` | no | no | yes | no |
| `script.patch` | `project_write` | `project_files` | no | yes | yes | no |
| `script.run` | `project_write` | `headless_engine` | no | yes | no | yes |
| `script.validate` | `read` | `project_files` | yes | no | yes | no |
| `shader.check` | `runtime_execute` | `headless_engine` | no | no | yes | no |
| `test.scenario.run` | `runtime_execute` | `game_session` | no | yes | no | no |
| `workspace.projects.list` | `read` | `project_files` | yes | no | yes | no |

### `android.deploy`

Install the APK of one finished Android export (`export`) on one device (`serial`) and prove it runs, phase by phase: the device is listed ready, `adb install -r` reports success, the activity manager starts the launcher the APK's own manifest declares, a process of its package appears within `runningTimeoutMs`, and a bounded logcat excerpt of that process is read. Every phase answers its outcome and bounded adb output; the call succeeds only when every phase did, and a failure names its phase. An install adb gave no verdict for is uncertain. Changes the device.

### `android.devices.list`

List the Android devices `adb devices -l` of the configured Android SDK reports: each one's serial, state (device, unauthorized, offline, ...), whether it is ready, model, product and transport. adb starts its own background server when none runs, as adb always does. Without a configured SDK holding adb it is unavailable; nothing is installed. Changes nothing on any device.

### `asset.delete`

Delete one asset whose SHA-256 is `expectedSha256`, together with its `.import` sidecar, in one journaled transaction. The engine's imported cache is left to the next import. `dryRun` writes nothing. Requires `idempotencyId`; returns the new revision.

### `asset.put`

Write one binary asset at `path` from base64 `contentBase64` whose complete SHA-256 is `sha256`: inline, or in ordered chunks of `totalBytes` bytes, each after the first naming its `upload` id and offset, published once complete. A replacement needs `expectedSha256`. Journaled; requires `idempotencyId`.

### `asset.status`

Report one asset's SHA-256 and byte length and its import state — importer, resource type, UID and the byte length and SHA-256 of every imported artifact — without exposing any engine cache path. Read-only; needs no Godot process.

### `capabilities.get`

Report the capability profiles, their granted capabilities and the exact tool and resource sets each one authorizes in this build. Optional `profile` narrows the report to one profile. Read-only; needs no Godot process.

### `debug.command`

Send one `command`, keyed by its name and carrying its own fields, to a managed editor session through its launch receipt. Language server: diagnostics (the engine's own for a script) or symbols. Debug adapter: breakpoints, launch (run the main scene under the debugger), stopped (wait for a breakpoint or pause), stack, scopes, variables, continue, pause, stop. Lines are one-based; there is no expression evaluation.

### `debug.session.get`

Report a managed editor session's language server and debug adapter — whether each endpoint is still owned by the editor's launch receipt, whether it is connected, the debug launch state, the last stop, the breakpoints set and recent debug events — or, without `session`, every editor session's. Changes nothing.

### `editor.addon.install`

Write the bundled godot_mcp editor add-on into addons/godot_mcp/, refusing any occupied path, and record its content ownership; `enable` also enables the plugin in project.godot. The editor loads it when it next starts or rescans plugins. One journaled transaction; requires `idempotencyId`.

### `editor.addon.reload`

Disable and re-enable the godot_mcp plugin in the connected editor, so it reloads its scripts; the editor reconnects with a new session.

### `editor.addon.remove`

Remove the add-on files whose content still matches what was installed, keep any you edited, and disable the plugin in project.godot. One journaled transaction; requires `idempotencyId`.

### `editor.addon.status`

Compare the bundled godot_mcp editor add-on with addons/godot_mcp/ in the project: which files are installed and unchanged, edited, missing or foreign, whether project.godot enables the plugin, and what an install, update or remove would do. Read-only; needs no Godot process.

### `editor.addon.update`

Bring addons/godot_mcp/ to the bundled add-on: unchanged files are replaced, missing ones written, and a file you edited is kept as `<name>.godot-mcp-backup` beside the new one. Nothing changes when it is current. One journaled transaction; requires `idempotencyId`.

### `editor.camera.get`

Read what the connected editor's viewports show: the 2D editor's view, its zoom and the point at the view's center, and the camera of each of the four 3D viewports, its transform, position and forward direction, projection, field of view or size and near and far planes, with whether each viewport is on screen; `view` and `viewport` select one. Changes nothing.

### `editor.camera.set`

Move the camera of one 3D viewport of the connected editor (`viewport` 0 to 3, 0 by default) to `position`, looking at `target`, which the editor then takes as its view, and answer the camera read back a few frames later; the answer is `uncertain` when the editor did not keep it. The viewport must be on screen: show the 3D main screen with editor.main_screen.set. Needs Godot 4.5.1 or later. Changes editor state only.

### `editor.dialog.respond`

Press one `button` (`ok`, `cancel` or a button's action, as editor.dialogs.get names it) of the dialog the connected editor shows with that `dialog` id, as a click does, and answer whether the dialog closed and the dialogs still shown. The button does whatever the editor does for it, which may save, discard or delete project files, so a lost answer blocks writes until it is resolved. Refused when the dialog is no longer shown.

### `editor.dialogs.get`

List the dialogs the connected editor shows, every visible accept, confirmation and file dialog, up to `limit` (32 by default): each with an `id` that stays the same while the dialog exists, its class, title and text, whether it blocks the rest of the editor, and its buttons, each named `ok`, `cancel` or by its action. Changes nothing.

### `editor.filesystem.scan`

Have the connected editor rescan the project's files, import what changed and register new script classes, as it does when it regains focus; `reimport` also reimports those assets. Then reload every open script tab whose file changed and that holds no unsaved edit, and list the tabs left as they were. Refused when an import could start an external program (Blender, FBX2glTF). Waits up to `timeoutMs` (300000 by default). The editor may write any project file meanwhile, so a lost answer blocks writes until it is resolved.

### `editor.log.get`

Return what the connected editor logged since its godot_mcp plugin started — prints, errors, warnings and script and shader errors, each with its source location and script backtrace — after the `after` cursor, oldest first, up to `limit` (100 by default), optionally only some `kinds`. `next` is the cursor for the next read; a different `epoch` means the plugin restarted and the read starts again from the oldest entry kept. Godot 4.4.1 cannot hand its log to a plugin; 4.5.1 and later can. Changes nothing.

### `editor.main_screen.set`

Show the `screen` — 2D, 3D or Script — as the connected editor's main screen, as its own screen buttons do, and answer once it is shown. Changes editor state only.

### `editor.node.get`

Return one `node` — a path relative to the scene root, the root by default — of the scene the connected editor is editing (or the open `scene`, when current) as the editor holds it: class, script, instanced scene, ownership, groups and children; every property, or the named `properties`, with its type, hint, usage, tagged value and whether it differs from its default; and its signals with their connections. Carries the scene's semantic digest. Reading adds no undo history and marks nothing unsaved.

### `editor.performance.get`

Read the connected editor process's performance monitors (frame rate and times, object, node and resource counts, memory and rendering figures) by the names its Godot build gives them (TIME_FPS, OBJECT_NODE_COUNT, ...), with the project's own custom monitors, or only the named `monitors`. The set differs between builds, so every value carries its name; an unknown name is refused. Changes nothing.

### `editor.play.start`

Run the project in the connected editor as its Play buttons do: the main scene by default, the scene being edited for `current`, or the scene at `scene`. The editor first saves the open scenes, which are listed in `saved`, and runs the game as its own child process, outside every boundary this server holds, so an untrusted project is refused; a managed session (project.run.start) runs the game inside one. Needs an editor with a window. Refused while a game runs: stop it with editor.play.stop.

### `editor.play.stop`

Stop the game the connected editor runs, as its Stop button does, and answer once it ended, with `was_playing`. Changes nothing when no game runs.

### `editor.preferences.get`

Read the connected editor's own settings, the ones its Editor Settings dialog shows: the named `settings`, or those whose names start with `prefix`, `limit` at a time (100 by default) from `offset`. Each names its type, tagged value, default, whether it differs from the default, its hint, whether a change needs the editor restarted and whether editor.preferences.set may change it. The value of a setting that holds a path, a host or a credential is withheld. Changes nothing.

### `editor.preferences.set`

Change the connected editor's own settings: `settings` maps setting names to tagged values of their types. The editor applies them as its Editor Settings dialog does and writes them to the user's editor settings file, which every project shares. All or nothing: a value of another type or outside its range or choices, or a setting that names a program, a path, a host or a credential, refuses the whole change, and so does a value the editor puts back, such as a color its theme preset sets while that preset is not Custom. Answers each setting before and after, anything the editor changed with them, and whether the editor must restart for one to take effect. No project file is written.

### `editor.redo`

Redo the most recently undone action in the undo history of the scene being edited, in the connected editor, as the editor's own Redo does; refused with nothing changed while the editor's global or remote history also has an action to redo. `save` also saves the scene. Returns the scene's semantic digest after the redo. A lost answer is reported as unresolved and never repeated.

### `editor.resource.preview`

Ask the connected editor for its preview of the resource at `path`, the image its FileSystem dock shows, and its small thumbnail; wait up to `timeoutMs` (10000 by default) and answer each as a PNG artifact read back from the editor and checked against its digest. Needs an editor with a window: a headless editor makes no previews, and a resource the editor makes no preview for answers `unavailable`. Changes nothing in the project.

### `editor.restart`

Restart the connected editor as its Reload Current Project does, and answer once the add-on of the restarted editor, a different process, connects within `timeoutMs` (120000 by default); otherwise the answer is `uncertain`. Refused while an open scene or script is unsaved, unless `save` saves them first (scripts from Godot 4.7.1 on); while a game runs; for an editor started with --headless, which Godot does not pass on to the editor it restarts; and for an editor this server runs as a managed session. A question the editor still asks before restarting is cancelled, and nothing is restarted.

### `editor.scene.close`

Close the scene the connected editor is editing, or the open `scene`, as its Close Scene does, and answer the scenes left open and the one the editor then shows. Refused while the scene holds a change its file does not, which closing would discard: save it with editor.scene.save first. Needs Godot 4.5.1 or later. Changes editor state only; no project file is written.

### `editor.scene.edit`

Apply typed `operations` — the scene.batch.apply model, nodes named by path — to the scene the connected editor is editing (or the open `scene`) as the editor holds it, unsaved changes included, when its semantic digest is still `expectedDigest`. The whole batch is one undoable action, named after `label`; a refused operation changes nothing. Nothing is saved: save with editor.scene.save. Returns the new digest.

### `editor.scene.open`

Open the text scene at `path` in the connected editor and make it the scene being edited; `reload` re-reads an open scene from its file, refused while the editor holds changes the file does not. Changes editor state only; no project file is written.

### `editor.scene.read`

Call reading methods (the table scene.read uses: painted TileMapLayer cells, GridMap items, animation tracks and keys, state machines and blend trees, curves, gradients, theme entries, navigation polygons and more) on nodes of the scene the connected editor is editing (or the open `scene`, when current), or on resources they hold, and answer each value as a tagged value, as the editor holds it, unsaved edits included. Each call carries the errors and warnings the editor logged while it ran, which Godot 4.5.1 and later hand to the plugin. Carries the scene's semantic digest. Reading adds no undo history and marks nothing unsaved.

### `editor.scene.save`

Save the scene the connected editor is editing, the open `scene`, or with `all` every open scene, when each file still holds the bytes it held when the call was checked. Succeeds only when every file then holds its scene as the editor does; otherwise the answer is uncertain. Lists every file saved, including the scripts and resources the editor wrote with them. An uncertain or lost answer leaves every project file in doubt and blocks writes until it is resolved.

### `editor.scene.tree.get`

Return the scene the connected editor is editing (or the open `scene`, when current) as the editor holds it: nodes with class, script, instanced scene, groups and changed stored properties as tagged values, persistent signal connections, unsaved state and a semantic digest. Reading adds no undo history and marks nothing unsaved.

### `editor.screenshot.capture`

Capture what the connected editor shows — its whole window, or the 2D or one 3D viewport (`viewport` 0 to 3) of the main screen shown, which editor.main_screen.set switches — after its next drawn frame, optionally scaled to `maxWidth`, as a PNG artifact read back from the editor and checked against its digest, and answer its opaque artifact URI, size and SHA-256. Needs an editor with a window; a headless editor draws nothing. Changes nothing in the project.

### `editor.script.close`

Close the connected editor's script tab of the GDScript file at `path` and answer the scripts left open, read back. Refused while the tab holds edits its file does not, which closing would discard: save them in the editor first. Needs Godot 4.7.1 or later. Changes editor state only; no project file is written.

### `editor.script.open`

Open the GDScript file at `path` in the connected editor's built-in script editor with the caret at `line` and `column` (both from 1, the start by default). Refused when the editor is set to open scripts in an external program. Changes editor state only.

### `editor.script.run`

Run caller-written GDScript `source`, which defines `func run()`, in the connected editor on its main thread, as a tool script that reaches the editor through EditorInterface, and answer what `run()` returned as a tagged value, what the editor logged meanwhile (Godot 4.5.1 and later) and every project file that differs from before the run. Nothing is undone, and nothing enters an undo history unless the code puts it there. Godot cannot stop a script part way: when `deadline_ms` passes first the answer is `uncertain`, the code may still be running, and writes stay blocked until it is resolved. The code runs with the user's privileges, and nothing confines what it does. Off unless the server was started with --allow-scripts.

### `editor.selection.set`

Select `nodes` — paths relative to the root of the scene being edited — in the connected editor, replacing the selection. Changes editor state only.

### `editor.session.start`

Start a headless Godot editor on the project as a managed session this server owns, hosting the GDScript language server and debug adapter on loopback ports the server allocates, and answer once both listen and are held by that editor. debug.command reaches them only through this session's launch receipt. A game the debugger runs renders into the server's display when it has one. The session ends at `lifetimeSeconds` at the latest. Runs editor plugins and tool scripts; needs a Godot editor build.

### `editor.session.stop`

Stop a managed editor session: its language-server and debug-adapter connections are closed and the editor, with any game it runs under the debugger, is terminated inside its boundary. Stopping a session that already ended answers its record. Answers the exit facts.

### `editor.shortcuts.get`

Read the connected editor's shortcuts, the ones its Editor Settings dialog lists on its Shortcuts tab: every editor shortcut and every built-in input action of its Common section. Answers the named `shortcuts`, or those whose names start with `prefix`, only the changed ones with `changedOnly`, `limit` at a time (100 by default) from `offset`. Each names its kind, label, key events and default events, and whether it differs from them. Needs Godot 4.6.1 or later. Changes nothing.

### `editor.shortcuts.set`

Change the connected editor's shortcuts and built-in actions, as editor.shortcuts.get names them: `shortcuts` maps each to the key events it is triggered by, an empty list for none, or `default` for its default events. The editor applies them as its Shortcuts tab does and writes them to the user's editor settings file, which every project shares. All or nothing: an unknown shortcut or key refuses the whole change. Answers each before and after, and anything the editor changed with them. Needs Godot 4.6.1 or later. No project file is written.

### `editor.status.get`

Report whether a Godot editor running the godot_mcp plugin is connected, with its Godot and plugin versions, and, when it is, the open scenes and which one is being edited, the selection, unsaved state, the edited scene's undo history, whether the file system is being scanned, the main screen shown, the scene being played, whether its log can be read, whether it can close scenes, whether it hands its shortcuts to a plugin and whether the plugin differs from the one this server bundles. Changes nothing in the editor.

### `editor.types.get`

Describe one `class` as the connected editor sees it: an engine class of its exact Godot build, or a project script class by its `class_name` — inheritance, its own properties with types, hints and defaults, signals, methods and enums. Inherited members are listed under the parent class.

### `editor.types.search`

Find the classes the connected editor knows — the engine classes of its exact Godot build and the project's script classes — whose name contains `query` in any case, optionally only those inheriting `base` or only instantiable ones, up to `limit` (100 by default). Each names its kind, parent class, whether it can be instantiated and, for a script class, its script. editor.types.get describes one class. Changes nothing.

### `editor.undo`

Undo the most recent action in the undo history of the scene being edited, in the connected editor, as the editor's own Undo does; refused with nothing changed while the editor's global or remote history also has an action to undo. `save` also saves the scene. Returns the scene's semantic digest after the undo. A lost answer is reported as unresolved and never repeated.

### `export.presets.list`

List the project's export presets: name, platform, whether it is the runnable one, the export template it needs and whether exactly that template of the resolved Godot build is installed, and the names (never the values) of any secret written into a preset. Changes nothing; template facts need a Godot build.

### `file.create`

Create one new UTF-8 text file at `path` with `content`; the path must be absent, and missing directories are created. `dryRun` returns the bounded diff without writing; `expectedRevision` refuses if the workspace moved on. Journaled and published with compare-and-swap; requires `idempotencyId`; returns the new revision.

### `file.delete`

Delete one file whose SHA-256 is `expectedSha256`, together with its engine `.import`/`.uid` sidecars, in one journaled transaction; the removal is proven against the recorded bytes. `dryRun` writes nothing. Requires `idempotencyId`; returns the new revision.

### `file.get`

Return one project file's SHA-256, byte length and contents — UTF-8 text, or base64 for binary data — bounded by `maxBytes` from byte `offset`, with the workspace revision it was read at. Engine cache and server state are not readable. Read-only; needs no Godot process.

### `file.move`

Move one file whose SHA-256 is `expectedSha256` from `from` to an absent `to`, together with its engine `.import`/`.uid` sidecars, in one journaled transaction. References to the old path are not rewritten. `dryRun` writes nothing. Requires `idempotencyId`; returns the new revision.

### `file.patch`

Change one UTF-8 text file whose current SHA-256 is `expectedSha256` by one `change`: a whole replacement (`content`), bounded line `edits` or a strict `unifiedDiff`, and return the bounded diff. `dryRun` writes nothing. Journaled compare-and-swap; requires `idempotencyId`; returns the new revision.

### `gdscript.diagnostics.get`

List static parse and analysis diagnostics for the project or for one `script`, paged with `offset`/`limit`, from the coherent workspace snapshot. This is a static check, not an engine compile; use script.check for that. Read-only; needs no Godot process.

### `gdscript.impact.get`

List the scenes, resources and scripts that depend on one `script` or one of its declared symbols, transitively, from the coherent workspace snapshot. Read-only; needs no Godot process.

### `gdscript.references.get`

List references to and callers of one `script`, or of one function, signal or class name it declares (`symbol`), from the coherent workspace snapshot, with the confidence of each resolved edge. Read-only; needs no Godot process.

### `gdscript.symbols.get`

List the classes, functions, signals, variables and constants one `script` declares, parsed from its current bytes and paged with `offset`/`limit`. Read-only; needs no Godot process.

### `health.get`

Report the health of the held project, the analysis index, the durable journal, the scheduler and each effect plane, with bounded metrics and the current write-block reason, if any. Read-only; needs no Godot process.

### `input.action.send`

Press, release or tap an input `action` in a managed game session. A press is held until released, `holdMs` passes, the session stops or the probe loses the server, whichever is first. Answers what the session holds afterwards.

### `input.key.send`

Press, release or tap one `key` (a Godot key name) with optional `modifiers` in a managed game session, held under the same expiry rules as an action. Answers what the session holds afterwards.

### `input.mouse.send`

Send one mouse `event` to a managed game session: a `move` to a position, or a `button` press, release or tap there, held under the same expiry rules as an action. Answers what the session holds afterwards.

### `input.recording.start`

Start recording every input event the managed game session's window receives — keys, mouse buttons and motion, joypad buttons and axes, actions — with its time from now, at most `maxEvents` (default 3000) for at most `maxDurationMs` (default 30 s). Each event says whether this server injected it or it came from elsewhere. One recording at a time; input.recording.stop answers it.

### `input.recording.stop`

Stop the managed game session's input recording and answer every event it kept, in the form input.replay takes, with how many the server injected and how many came from elsewhere and why the recording ended. The recording is also kept as a JSON artifact, whose opaque URI is answered.

### `input.replay`

Deliver input `events` — as input.recording.stop answers them, or written by hand — to a managed game session again at their times from now, for at most 30 s. Everything the replay pressed and still holds is released when it ends or is cancelled, when the session stops and when the probe loses the server. Other requests to the session wait while it runs; cancelling the call ends it at the next frame. Answers how many events were sent or skipped and how late the latest was.

### `input.sequence.send`

Apply up to 64 input `steps` in order in a managed game session, with `wait` steps between them (at most 10 s in total). Everything the sequence pressed is released when it ends, fails or is cancelled. Answers how many steps ran.

### `project.batch.apply`

Apply a batch of typed `operations` to each of several existing text scenes and resources (`files`, each with its `kind`, `path`, `expectedSha256` and the operations scene.batch.apply or resource.batch.apply takes) in one supervised headless Godot run, and publish every file as one journaled transaction: every file changes or none does. Each file is edited as it is on disk. A file open in the connected editor refuses. `dryRun` returns each file's diff and tree without publishing.

### `project.bind`

Bind this server, once and for the rest of its lifetime, to the Godot project at workspace-relative `path`: acquire its single-writer lock, reconcile its journal and publish workspace revision 1. The tool list then changes to the project tools. Changes only the server's private state; needs no Godot process.

### `project.create`

Create a new Godot project in `path`, a workspace-relative directory that is absent or empty, with application name `name` and an optional `renderer`. It refuses to escape the workspace, follow a symlink or replace an existing project. Available only before a project is bound; call project.bind next. Needs no Godot process; requires `idempotencyId`.

### `project.dependencies.audit`

Audit the project's file reference graph from the coherent workspace snapshot. `view` `unused` lists the scripts, scenes, resources, shaders and assets no root reaches (main scene, autoloads, project settings, enabled plugins, export filters), each with why; loads whose path is computed at run time are listed as `dynamic_loads`, so the used set is a lower bound. `view` `cycles` lists strongly connected file sets, classified as `inheritance` or `resource_load` (Godot fails on these) or `tolerated`. `include` / `exclude` globs narrow what is reported; paged with `offset`/`limit`. Read-only; needs no Godot process.

### `project.dependencies.get`

List every resolved and dangling dependency edge between project scripts, scenes and resources, paged with `offset`/`limit`, from the coherent workspace snapshot. Read-only; needs no Godot process.

### `project.discover`

List the project's scripts, scenes, resources and other files with their kinds, paged with `offset`/`limit`, from the coherent workspace snapshot. Read-only; needs no Godot process.

### `project.export`

Start exporting `preset` (a release build, or `mode` debug) through exactly the installed export template of the resolved Godot build, as a managed export this server owns, and answer at once with its export ID. The output goes to the server's private storage, never into the project; every file a successful export produces becomes an artifact, and `smoke` runs the build headless once where this host can run it: a Linux build on Linux, a Windows build on Windows, a macOS app bundle, exported as one zip, on a Mac. `kind` pack exports the project's data as one PCK or ZIP, and patch only the files that differ from the base packs `patches` names (an earlier export's artifact or a project path); the pack's file table is read back and answered. An Android preset exports an APK through the configured Android SDK and JDK, read back and verified with apksigner. A preset with a secret written into it, a missing template and an unsupported platform are refused before anything runs. Executes project code; needs a Godot editor build.

### `project.export.cancel`

Cancel a running managed export: its process is terminated inside its boundary and its partial output is removed, so nothing of it becomes an artifact. Cancelling an export that already ended answers its record.

### `project.export.status`

Report one managed export — its state, preset, platform and template, progress while it runs, and when it ended the exit facts, every output file with its size, SHA-256 and artifact URI (until the artifact expires) and the smoke run's result — or, without `export`, every export this server owns. Changes nothing.

### `project.files.list`

List the project's files with their kind, byte length and SHA-256, optionally under one `prefix` and matching one `glob`, paged with `offset`/`limit`, from the coherent workspace snapshot. The hashes are the preconditions mutations require. Read-only; needs no Godot process.

### `project.import.run`

Run the Godot importer headlessly over the held project, rewriting its import metadata, and report the classified verdict, parsed engine diagnostics, bounded output, which `expected_artifacts` were produced and, for each of `assets`, its import state and imported artifact hashes. Needs a supervised headless Godot editor process; the workspace revision advances after it commits.

### `project.info.get`

Report the held project's path-free identity, its parsed project settings summary and the workspace revision the answer was taken from. Read-only; needs no Godot process.

### `project.metrics.get`

Report project statistics (`view` `statistics`: files and bytes by kind and extension, GDScript code, comment and blank lines, classes, functions, signals, scene nodes) or rank complexity (`view` `complexity`: GDScript functions by McCabe complexity, length or nesting, or scenes by nodes, depth or instances, with `min_*` thresholds), from the coherent workspace snapshot, paged with `offset`/`limit`. Read-only; needs no Godot process.

### `project.run`

Run the project's main scene, or `main_scene`, in a supervised Godot process until it exits or `deadline_ms` elapses, and report the classified verdict, exit facts, parsed engine diagnostics and bounded output. A game that keeps running ends as a timeout. Executes project code; needs a Godot build.

### `project.run.logs`

Read one bounded page of a managed session's captured `stream` (stdout or stderr) from byte `offset`, while it runs or after it ended, with the next offset, the total length and whether the end was reached. Changes nothing.

### `project.run.start`

Start the project's main scene, or `scene`, as a managed game session this server owns, and answer once its runtime probe is connected rather than when the game exits. `display` window renders into the server's display (captures need it); headless renders nothing. The session ends at `lifetimeSeconds` at the latest. Executes project code; needs a Godot build.

### `project.run.status`

Report one managed session — its state, scene, display, timestamps, containment strength, exit facts, whether its probe is connected and what input it holds — or, without `session`, every session this server owns. Changes nothing.

### `project.run.stop`

Stop a managed game session: its probe releases every held input and quits the game; a game that does not end by itself is terminated inside its boundary. Stopping a session that already ended answers its record. Answers the exit facts.

### `project.search`

Search the project's text files for a literal `query`, or a linear-time regular expression with `regex`, with optional `case_sensitive`, `whole_word`, `include` / `exclude` globs and `context` lines. Matches stop at `max_matches` and are paged with `offset`/`limit`; binary, oversized and unread files are counted, never silently skipped, and files Godot's editor ignores (hidden, or under a `.gdignore`) are left out and counted. Read-only; needs no Godot process.

### `project.settings.get`

Return the typed project settings — application name, main scene, autoloads, input actions and their events, enabled plugins and the allowlisted values — with the SHA-256 of project.godot that project.settings.patch requires. Read-only; needs no Godot process.

### `project.settings.patch`

Apply typed `operations` — set or clear the main scene, set or remove an autoload or an input action, set an allowlisted value, enable or disable an installed plugin — to project.godot whose SHA-256 is `expectedSha256`, preserving unrelated keys and comments. While a live editor is connected it owns project.godot: the editor applies the change and saves the file with its own writer, which keeps every setting but not comments or layout, and the answer describes the bytes it saved. Journaled; requires `idempotencyId`.

### `project.signals.get`

Trace signal flow from the coherent workspace snapshot: every declared signal with its scene connections (resolved through instanced scenes and script inheritance), code connections and emission sites. `view` `problems` lists handlers missing from the receiving scripts, signals emitted but never connected, and connections to signals the emitter's scripts never declare; `view` `undeclared_connections` lists connections to engine or unknown signals. Optional `signal` and `script` narrow it; paged with `offset`/`limit`. Read-only; needs no Godot process.

### `project.test`

Run the project's already-committed GUT or gdUnit4 test suite, optionally narrowed by `selection`, in a supervised headless Godot process and report the parsed summary and verdict. A project without a framework is unsupported; nothing is installed. Executes project code; needs a Godot build.

### `project.uid.get`

Look a `uid` up as a project path, or a project `path` up as its UID, from the scene and resource headers, `.uid` sidecars and `.import` files the project records; Godot's own `.godot/uid_cache.bin` is not read, so an unrecorded UID is `unknown`. With neither, page the whole table with `offset`/`limit`. Conflicts and orphaned sidecars are reported as such. Read-only; needs no Godot process.

### `project.uid.update`

Register or repair the UID of one text `target` resource through a supervised headless Godot canonical re-save, published with compare-and-swap. `dry_run` validates without writing. Needs a headless Godot editor process; the workspace revision advances after it commits.

### `project.validate`

Statically validate the whole project — parse errors, dangling references and invalid settings — from the coherent workspace snapshot, paged with `offset`/`limit`. It does not run the engine and says so. Read-only; needs no Godot process.

### `resource.batch.apply`

Apply one atomic batch of typed `operations` — set or clear properties, create sub-resources — to the text resource at `path` whose SHA-256 is `expectedSha256`, through a supervised headless Godot process. Any refused operation publishes nothing; the resource keeps its UID.

### `resource.create`

Create one new text resource of engine `class` at an absent `path`, with typed `operations` setting its initial properties and sub-resources, saved by a supervised headless Godot process and published with compare-and-swap. Requires `idempotencyId`; returns the diff and the parsed result.

### `resource.dependencies.get`

List the external and sub-resource dependencies of one text `resource` (.tres) and their resolution, from the coherent workspace snapshot. Read-only; needs no Godot process.

### `resource.read`

Make up to 64 `calls` of read-only engine methods from the reading table on the text resource at `path`, or on resources it holds: an animation's tracks and keys, a curve's points, a theme's items, a navigation mesh's polygons, a shader's uniforms. Each call answers its value as a tagged value, or why it was not made, with the engine errors it raised. A supervised headless Godot process makes the calls and saves nothing; it starts the project's autoloads.

### `resource.tree.get`

Return the parsed structure of one text `resource` (.tres): its type, properties, sub-resources and references, paged with `offset`/`limit`. Read-only; needs no Godot process.

### `resource.update`

Canonicalize one existing text `target` resource through a supervised headless Godot re-save and publish the result with compare-and-swap. `dry_run` validates without writing. Needs a headless Godot editor process; the workspace revision advances after it commits.

### `runtime.image.compare`

Capture the managed game session's root viewport after the next drawn frame and compare it with the project PNG `reference`, both decoded by the game's probe with Godot's own Image API: answers both sizes, whether they match, how many and what share of the pixels differ by more than `tolerance` (0 to 255) in a channel, the largest channel difference and the box around the differing pixels, with the capture and a difference image as artifacts. A size mismatch fails the comparison; nothing is resized. Needs a windowed session.

### `runtime.navigation.path.get`

Query a path over the navigation map a running Node2D or Node3D uses (its NavigationAgent's map and layers when it has one, its world's default map otherwise) from `from` (its position by default) to `to`, once the map has synchronized its regions, waiting at most `syncTimeoutMs`. Answers the points, the length and whether the path reaches the target; a map without a navigation region is unavailable. Changes nothing in the game.

### `runtime.node.move`

Move a running Node2D or Node3D to `to`: `teleport` sets its global position at once; `along_path` walks it along its navigation map's path at `speed` units per second, setting its position every physics frame, until it arrives, makes no progress for 30 physics frames or `timeoutMs` passes. The node is moved directly: the game's own movement code and physics collision response do not move it. Other requests to the session wait while it walks; cancelling the call ends it at the next frame. Answers the outcome and the final position.

### `runtime.nodes.find`

Find the running game's nodes under `root` that match every given filter: `class` (an engine or script class, inheriting ones included), `script` (attached or extended), `group`, `name` (a glob with * and ?) and `near` (within `radius` of a point or of another node, in 2D or 3D, nearest first). Answers at most `limit` (default 100) with paths, classes and positions; a scan stops after 20000 nodes and says so. Changes nothing in the game.

### `runtime.performance.get`

Read the running game's performance monitors — frame rate and times, object, node and resource counts, memory, rendering, physics, audio and navigation figures — by the names its Godot build gives them (TIME_FPS, OBJECT_NODE_COUNT, ...), with the project's own custom monitors, or only the named `monitors`. The set differs between builds, so every value carries its name; an unknown name is refused. Answers the probe's revision and frame. Changes nothing in the game.

### `runtime.properties.batch.get`

Read the properties of up to 32 `targets` in one call — running nodes, or the `Engine` singleton's time_scale, physics_ticks_per_second and max_fps — each target's named `properties` (its stored and editor-visible ones by default, at most 64) as tagged values. A target that cannot be read says why while the others are read; once the answer nears 2.5 MB the remaining targets are marked skipped. Changes nothing in the game.

### `runtime.properties.get`

Read the current values of one running node's `properties` (its stored and editor-visible ones by default) as tagged values, with the probe's revision and frame. A value the contract cannot carry is named, not serialized. Changes nothing in the game.

### `runtime.properties.set`

Write `properties` of one running node, or the Engine singleton's time_scale, physics_ticks_per_second and max_fps, from tagged values. A value is converted to the property's declared type only where nothing is lost; an unknown or read-only property, or a value that does not convert, refuses the whole write before anything changes. Each property is read back: answers its value before and after, and whether it holds what was asked, which a setter may change.

### `runtime.recording.capture`

Record `durationMs` (at most 10 s) of the managed game session's root viewport at `fps` (at most 30), optionally scaled to `maxWidth`, as one animated PNG artifact, and answer its opaque artifact URI, frame count, size and SHA-256. Needs a windowed session.

### `runtime.sample`

Sample named properties of running nodes (or the Engine singleton's) and named performance monitors every `intervalMs` for `durationMs` (at most 10 s and 600 samples): each series of values, every sample's time, frames and pause and focus state, and the minimum, maximum, mean and last of each numeric series. Large values and strings over 256 characters are not sampled. Changes nothing in the game.

### `runtime.screenshot.capture`

Capture the managed game session's root viewport after the next drawn frame as a PNG artifact, read back from the probe and checked against its digest, and answer its opaque artifact URI, size and SHA-256. Needs a windowed session.

### `runtime.script.run`

Run caller-written GDScript in a managed game session, on the game's main thread through its runtime probe, and answer what its `run()` returned, what it printed and raised, and which project files differ after it. Only a server whose operator turned script execution on runs it, and it runs with the user's privileges: nothing confines it, and nothing it does is undone. Godot cannot stop a script part way through: a run its deadline outlasts, or one withdrawn, is `uncertain` and goes on until it returns or the session stops.

### `runtime.signals.get`

Read one running node's signals with their arguments and current connections; with `watch` the probe also counts every later emission of those signals, which later reads report. Answers the probe's revision and frame. Changes nothing in the game.

### `runtime.time_scale.set`

Set the managed game session's Engine.time_scale to `timeScale` (0 to 100; 1 is real time, 0 stops game time) and read it back: answers the value before, the value asked for and the value read back. It changes the running game only: nothing is written to the project or saved, and the scale holds until it is changed again or the session ends. A read-back that differs from the request is answered as uncertain.

### `runtime.tree.get`

Read the running game's scene tree from `root` (the whole tree by default) to `depth`: each node's path, class, script, groups, child count and visibility, with the probe's `revision` (it advances whenever a node enters, leaves or is renamed) and the engine `frame` observed. Changes nothing in the game.

### `runtime.ui.get`

Read the running game's visible Controls: path, class, global rectangle, text, disabled and pressed state, and which one has focus, with the probe's revision and frame. Changes nothing in the game.

### `runtime.ui.press`

Click one visible, enabled button (any BaseButton) of a managed game session, named by `node` or by the `text` it shows, with a synthetic left mouse press and release at its centre, through the same input path as input.mouse.send. Answers whether its `pressed` signal fired and its state afterwards. A disabled, hidden or ambiguous button, or one outside the root viewport (64x64 pixels in a headless session), is refused.

### `runtime.wait`

Wait in the running game until one `condition`, evaluated every frame, holds or `timeoutMs` (at most 30 s) passes: a node exists or is gone, a property compares (==, !=, <, <=, >, >=, contains, approx within `tolerance`) with a tagged value, a signal was emitted `count` times after the wait began, or a visible Control shows `text`. Answers whether it held, the last observed value and the time waited; cancelling or stopping the session ends it at the next frame. Changes nothing in the game.

### `scene.batch.apply`

Apply one atomic batch of typed `operations` to the text scene at `path` whose SHA-256 is `expectedSha256`: add, remove, rename, move or duplicate nodes, set properties, attach scripts, (re)instance scenes, connect signals, edit groups and sub-resources. Nodes are named by path or by a scene tree `handle`; a handle of older bytes refuses. The engine applies and saves it; any refused operation publishes nothing. A scene open in the connected editor is changed there instead, as one undoable action, and saved; one holding unsaved editor changes refuses.

### `scene.create`

Create one new text scene at an absent `path` with a `rootType` root node and an optional batch of typed `operations` — nodes, instances, typed properties, scripts, signal connections, groups and sub-resources — applied and saved by a supervised headless Godot process, then published with compare-and-swap. Requires `idempotencyId`.

### `scene.dependencies.get`

List the scripts, sub-scenes and resources one text `scene` (.tscn) depends on and their resolution, from the coherent workspace snapshot. Read-only; needs no Godot process.

### `scene.exports.get`

List the `@export` variables of every scripted node of one text `scene` (.tscn), through instanced scenes and script inheritance: each variable's type hint and script default, the value the scene or an instanced scene stores, and which applies. Optional `node` narrows it to one node path; paged with `offset`/`limit`. Read-only; needs no Godot process.

### `scene.nodes.find`

Find nodes across the project's text scenes by `type` (an engine class exactly, or a project class_name through script inheritance), `group`, `name` pattern, `script`, or a stored `property` with an optional exact `value`, resolving instanced scenes. `include` / `exclude` globs pick the scenes; paged with `offset`/`limit`. Read-only; needs no Godot process.

### `scene.read`

Make up to 64 `calls` of read-only engine methods from the reading table on nodes of the text scene at `path`, or on resources they hold: painted TileMapLayer cells, animation tracks and keys, state machine states, navigation polygons, shader uniforms. Each call answers its value as a tagged value, or why it was not made, with the engine errors it raised. A supervised headless Godot process makes the calls and saves nothing; it starts the project's autoloads, and instancing the scene runs its scripts' `_init`.

### `scene.save`

Canonicalize one existing text scene `target` through a supervised headless Godot re-save. A live editor may hold the authoritative buffer, so the editor plane must be consulted first; without a connected editor the call is refused as unavailable rather than risking an unsaved edit.

### `scene.tree.get`

Return the node tree of one text `scene` (.tscn) with node types, attached scripts, groups and signal connections, paged with `offset`/`limit`, plus the file's SHA-256 and a `handle` per node that scene.batch.apply accepts until the file changes. Read-only; needs no Godot process.

### `script.check`

Check one `script` with the engine's own `--check-only` compiler in a supervised headless Godot process, after a calibration canary proves the flag is honoured, and report the engine diagnostics. Needs a Godot editor build.

### `script.create`

Create one new `.gd` script at an absent `path` after a static parse of `content`. A script with parse errors is refused unless `allowParseErrors` is true, and its diagnostics are returned either way; nothing is executed. Journaled compare-and-swap; requires `idempotencyId`.

### `script.patch`

Change one `.gd` script whose SHA-256 is `expectedSha256` by one `change` (`content`, line `edits` or a strict `unifiedDiff`), after a static parse of the result; parse errors refuse unless `allowParseErrors` is true. Nothing is executed; use script.check for the engine verdict. Requires `idempotencyId`.

### `script.run`

Run caller-written GDScript `source`, which defines `func run()`, in a supervised headless Godot process against the held project (`context` `project`, the default) or a disposable copy of it (`copy`), and answer what `run()` returned as a tagged value, the engine diagnostics, bounded output and every project file the run added, changed or removed. Nothing is undone: changes in the project stay, and a copy's are reported and discarded. The code runs with the user's privileges, and nothing confines what it does. Off unless the server was started with --allow-scripts; refused while an editor is connected (use `context` `copy`).

### `script.validate`

Statically validate one `script` with the GDScript parser and report its diagnostics. It does not run the engine; script.check performs the engine check. Read-only; needs no Godot process.

### `shader.check`

Compile the `.gdshader` at `path` with the engine's own shader language front end in a supervised headless Godot process, and answer whether it `compiled`, each error's message, file and line, including errors in the files it includes, its `mode` and its `uniforms` with their types and hints. No rendering backend compiles it, so an error only a GPU driver's compiler finds is not reported. Saves nothing; the process starts the project's autoloads.

### `test.scenario.run`

Run an ordered list of `steps` against one managed game session and answer one report: input (input.sequence.send's steps), waits (runtime.wait's conditions), property writes, button presses, time-scale changes, property and Control-text assertions, screenshots, image comparisons and stress windows that instance a project scene, sample the performance monitors and check the instances are freed. Each step is passed, failed (with expected and actual), error, skipped or uncertain; `stopOnFailure` (default true) skips the steps after the first failure or error, and an uncertain step ends the run. The verdict is passed only when every step passed, the run is bounded by `maxDurationMs`, and the report is also kept as a JSON artifact. A text assertion reads a Control's text property; nothing reads the screen as text.

### `workspace.projects.list`

List the Godot projects (directories holding a project.godot) under the server's authorized workspace, as workspace-relative paths with their configured names, so one can be bound. Available only before a project is bound; read-only; needs no Godot process.

## `core`

Capabilities:

* `read`

Tools:

* `capabilities.get`
* `gdscript.diagnostics.get`
* `gdscript.impact.get`
* `gdscript.references.get`
* `gdscript.symbols.get`
* `health.get`
* `project.dependencies.audit`
* `project.dependencies.get`
* `project.discover`
* `project.info.get`
* `project.metrics.get`
* `project.search`
* `project.signals.get`
* `project.uid.get`
* `project.validate`
* `resource.dependencies.get`
* `resource.tree.get`
* `scene.dependencies.get`
* `scene.exports.get`
* `scene.nodes.find`
* `scene.tree.get`
* `script.validate`

Bootstrap tools, listed before a `--workspace` server binds its project:

* `capabilities.get`
* `health.get`
* `project.bind`
* `workspace.projects.list`

Resources:

* `godot-mcp://resources/capabilities`
* `godot-mcp://resources/health`
* `godot-mcp://resources/project`
* `godot-mcp://resources/scheduler`

## `editor`

Capabilities:

* `editor_mutate`
* `project_write`
* `read`

Tools:

* `asset.delete`
* `asset.put`
* `asset.status`
* `capabilities.get`
* `editor.addon.install`
* `editor.addon.reload`
* `editor.addon.remove`
* `editor.addon.status`
* `editor.addon.update`
* `editor.camera.get`
* `editor.camera.set`
* `editor.dialog.respond`
* `editor.dialogs.get`
* `editor.filesystem.scan`
* `editor.log.get`
* `editor.main_screen.set`
* `editor.node.get`
* `editor.performance.get`
* `editor.preferences.get`
* `editor.preferences.set`
* `editor.redo`
* `editor.resource.preview`
* `editor.restart`
* `editor.scene.close`
* `editor.scene.edit`
* `editor.scene.open`
* `editor.scene.read`
* `editor.scene.save`
* `editor.scene.tree.get`
* `editor.screenshot.capture`
* `editor.script.close`
* `editor.script.open`
* `editor.script.run`
* `editor.selection.set`
* `editor.shortcuts.get`
* `editor.shortcuts.set`
* `editor.status.get`
* `editor.types.get`
* `editor.types.search`
* `editor.undo`
* `file.create`
* `file.delete`
* `file.get`
* `file.move`
* `file.patch`
* `gdscript.diagnostics.get`
* `gdscript.impact.get`
* `gdscript.references.get`
* `gdscript.symbols.get`
* `health.get`
* `project.batch.apply`
* `project.dependencies.audit`
* `project.dependencies.get`
* `project.discover`
* `project.files.list`
* `project.import.run`
* `project.info.get`
* `project.metrics.get`
* `project.search`
* `project.settings.get`
* `project.settings.patch`
* `project.signals.get`
* `project.uid.get`
* `project.uid.update`
* `project.validate`
* `resource.batch.apply`
* `resource.create`
* `resource.dependencies.get`
* `resource.tree.get`
* `resource.update`
* `scene.batch.apply`
* `scene.create`
* `scene.dependencies.get`
* `scene.exports.get`
* `scene.nodes.find`
* `scene.save`
* `scene.tree.get`
* `script.create`
* `script.patch`
* `script.run`
* `script.validate`

Bootstrap tools, listed before a `--workspace` server binds its project:

* `capabilities.get`
* `health.get`
* `project.bind`
* `project.create`
* `workspace.projects.list`

Resources:

* `godot-mcp://artifacts/{artifact_id}`
* `godot-mcp://resources/capabilities`
* `godot-mcp://resources/health`
* `godot-mcp://resources/project`
* `godot-mcp://resources/scheduler`

## `runtime`

Capabilities:

* `export_destructive`
* `read`
* `runtime_execute`

Tools:

* `android.deploy`
* `android.devices.list`
* `asset.status`
* `capabilities.get`
* `debug.command`
* `debug.session.get`
* `editor.play.start`
* `editor.play.stop`
* `editor.session.start`
* `editor.session.stop`
* `export.presets.list`
* `file.get`
* `gdscript.diagnostics.get`
* `gdscript.impact.get`
* `gdscript.references.get`
* `gdscript.symbols.get`
* `health.get`
* `input.action.send`
* `input.key.send`
* `input.mouse.send`
* `input.recording.start`
* `input.recording.stop`
* `input.replay`
* `input.sequence.send`
* `project.dependencies.audit`
* `project.dependencies.get`
* `project.discover`
* `project.export`
* `project.export.cancel`
* `project.export.status`
* `project.files.list`
* `project.info.get`
* `project.metrics.get`
* `project.run`
* `project.run.logs`
* `project.run.start`
* `project.run.status`
* `project.run.stop`
* `project.search`
* `project.settings.get`
* `project.signals.get`
* `project.test`
* `project.uid.get`
* `project.validate`
* `resource.dependencies.get`
* `resource.read`
* `resource.tree.get`
* `runtime.image.compare`
* `runtime.navigation.path.get`
* `runtime.node.move`
* `runtime.nodes.find`
* `runtime.performance.get`
* `runtime.properties.batch.get`
* `runtime.properties.get`
* `runtime.properties.set`
* `runtime.recording.capture`
* `runtime.sample`
* `runtime.screenshot.capture`
* `runtime.script.run`
* `runtime.signals.get`
* `runtime.time_scale.set`
* `runtime.tree.get`
* `runtime.ui.get`
* `runtime.ui.press`
* `runtime.wait`
* `scene.dependencies.get`
* `scene.exports.get`
* `scene.nodes.find`
* `scene.read`
* `scene.tree.get`
* `script.check`
* `script.validate`
* `shader.check`
* `test.scenario.run`

Bootstrap tools, listed before a `--workspace` server binds its project:

* `capabilities.get`
* `health.get`
* `project.bind`
* `workspace.projects.list`

Resources:

* `godot-mcp://artifacts/{artifact_id}`
* `godot-mcp://resources/capabilities`
* `godot-mcp://resources/health`
* `godot-mcp://resources/project`
* `godot-mcp://resources/scheduler`

## `full`

Capabilities:

* `editor_mutate`
* `export_destructive`
* `project_write`
* `read`
* `runtime_execute`

Tools:

* `android.deploy`
* `android.devices.list`
* `asset.delete`
* `asset.put`
* `asset.status`
* `capabilities.get`
* `debug.command`
* `debug.session.get`
* `editor.addon.install`
* `editor.addon.reload`
* `editor.addon.remove`
* `editor.addon.status`
* `editor.addon.update`
* `editor.camera.get`
* `editor.camera.set`
* `editor.dialog.respond`
* `editor.dialogs.get`
* `editor.filesystem.scan`
* `editor.log.get`
* `editor.main_screen.set`
* `editor.node.get`
* `editor.performance.get`
* `editor.play.start`
* `editor.play.stop`
* `editor.preferences.get`
* `editor.preferences.set`
* `editor.redo`
* `editor.resource.preview`
* `editor.restart`
* `editor.scene.close`
* `editor.scene.edit`
* `editor.scene.open`
* `editor.scene.read`
* `editor.scene.save`
* `editor.scene.tree.get`
* `editor.screenshot.capture`
* `editor.script.close`
* `editor.script.open`
* `editor.script.run`
* `editor.selection.set`
* `editor.session.start`
* `editor.session.stop`
* `editor.shortcuts.get`
* `editor.shortcuts.set`
* `editor.status.get`
* `editor.types.get`
* `editor.types.search`
* `editor.undo`
* `export.presets.list`
* `file.create`
* `file.delete`
* `file.get`
* `file.move`
* `file.patch`
* `gdscript.diagnostics.get`
* `gdscript.impact.get`
* `gdscript.references.get`
* `gdscript.symbols.get`
* `health.get`
* `input.action.send`
* `input.key.send`
* `input.mouse.send`
* `input.recording.start`
* `input.recording.stop`
* `input.replay`
* `input.sequence.send`
* `project.batch.apply`
* `project.dependencies.audit`
* `project.dependencies.get`
* `project.discover`
* `project.export`
* `project.export.cancel`
* `project.export.status`
* `project.files.list`
* `project.import.run`
* `project.info.get`
* `project.metrics.get`
* `project.run`
* `project.run.logs`
* `project.run.start`
* `project.run.status`
* `project.run.stop`
* `project.search`
* `project.settings.get`
* `project.settings.patch`
* `project.signals.get`
* `project.test`
* `project.uid.get`
* `project.uid.update`
* `project.validate`
* `resource.batch.apply`
* `resource.create`
* `resource.dependencies.get`
* `resource.read`
* `resource.tree.get`
* `resource.update`
* `runtime.image.compare`
* `runtime.navigation.path.get`
* `runtime.node.move`
* `runtime.nodes.find`
* `runtime.performance.get`
* `runtime.properties.batch.get`
* `runtime.properties.get`
* `runtime.properties.set`
* `runtime.recording.capture`
* `runtime.sample`
* `runtime.screenshot.capture`
* `runtime.script.run`
* `runtime.signals.get`
* `runtime.time_scale.set`
* `runtime.tree.get`
* `runtime.ui.get`
* `runtime.ui.press`
* `runtime.wait`
* `scene.batch.apply`
* `scene.create`
* `scene.dependencies.get`
* `scene.exports.get`
* `scene.nodes.find`
* `scene.read`
* `scene.save`
* `scene.tree.get`
* `script.check`
* `script.create`
* `script.patch`
* `script.run`
* `script.validate`
* `shader.check`
* `test.scenario.run`

Bootstrap tools, listed before a `--workspace` server binds its project:

* `capabilities.get`
* `health.get`
* `project.bind`
* `project.create`
* `workspace.projects.list`

Resources:

* `godot-mcp://artifacts/{artifact_id}`
* `godot-mcp://resources/capabilities`
* `godot-mcp://resources/health`
* `godot-mcp://resources/project`
* `godot-mcp://resources/scheduler`
