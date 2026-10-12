<!-- docs-generated: tool-inventory -->
<!-- Regenerate with: cargo run -p godot-mcp-cli --features test-hooks --bin docs-check -- --write -->
# Profiles, tools and resources

This page is generated from `godot_mcp_domain::catalog::TOOL_CATALOG`, the one owner of every public
method's name, description, capability, plane, annotations and the handler key
its input schema and handler come from. Editing it by hand fails the docs
check; change the canonical source and regenerate instead.

`full` is the union of `core`, `editor` and `runtime`. No profile carries an
arbitrary-script-execution tool: `unsafe_script` is a reserved identifier with
no capability variant, no handler and no profile membership.

Discovery reports what this build implements. A profile is an authorization
ceiling: it lists exactly the catalog methods it authorizes, and the catalog
contains only methods with a schema and a production handler, so a planned
method is never listed. A listed method whose plane is temporarily
unreachable answers `unavailable` and names the plane. See
[Profiles and tool outcomes](../operator/profiles-and-outcomes.md).

## Profile sizes

| Profile | Capabilities | Tools | Resources |
| --- | --- | --- | --- |
| `core` | 1 | 15 | 4 |
| `editor` | 3 | 48 | 4 |
| `runtime` | 3 | 44 | 5 |
| `full` | 5 | 73 | 5 |

## Methods

| Method | Capability | Needs | Read-only | Destructive | Idempotent | Open world |
| --- | --- | --- | --- | --- | --- | --- |
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
| `editor.redo` | `editor_mutate` | `live_editor` | no | yes | no | no |
| `editor.scene.open` | `editor_mutate` | `live_editor` | no | no | yes | no |
| `editor.scene.tree.get` | `read` | `live_editor` | yes | no | yes | no |
| `editor.selection.set` | `editor_mutate` | `live_editor` | no | no | yes | no |
| `editor.session.start` | `runtime_execute` | `headless_engine` | no | no | no | yes |
| `editor.session.stop` | `runtime_execute` | `editor_session` | no | yes | yes | no |
| `editor.status.get` | `read` | `live_editor` | yes | no | yes | no |
| `editor.types.get` | `read` | `live_editor` | yes | no | yes | no |
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
| `input.sequence.send` | `runtime_execute` | `game_session` | no | no | no | no |
| `project.bind` | `read` | `project_files` | yes | no | yes | no |
| `project.create` | `project_write` | `project_files` | no | no | yes | no |
| `project.dependencies.get` | `read` | `project_files` | yes | no | yes | no |
| `project.discover` | `read` | `project_files` | yes | no | yes | no |
| `project.export` | `export_destructive` | `headless_engine` | no | no | no | yes |
| `project.export.cancel` | `export_destructive` | `headless_engine` | no | yes | yes | no |
| `project.export.status` | `read` | `headless_engine` | yes | no | yes | no |
| `project.files.list` | `read` | `project_files` | yes | no | yes | no |
| `project.import.run` | `project_write` | `headless_engine` | no | no | yes | no |
| `project.info.get` | `read` | `project_files` | yes | no | yes | no |
| `project.run` | `runtime_execute` | `headless_engine` | no | no | no | yes |
| `project.run.logs` | `read` | `headless_engine` | yes | no | yes | no |
| `project.run.start` | `runtime_execute` | `headless_engine` | no | no | no | yes |
| `project.run.status` | `read` | `headless_engine` | yes | no | yes | no |
| `project.run.stop` | `runtime_execute` | `game_session` | no | yes | yes | no |
| `project.settings.get` | `read` | `project_files` | yes | no | yes | no |
| `project.settings.patch` | `project_write` | `project_files` | no | yes | yes | no |
| `project.test` | `runtime_execute` | `headless_engine` | no | no | no | yes |
| `project.uid.update` | `project_write` | `headless_engine` | no | yes | yes | no |
| `project.validate` | `read` | `project_files` | yes | no | yes | no |
| `resource.batch.apply` | `project_write` | `headless_engine` | no | yes | yes | no |
| `resource.create` | `project_write` | `headless_engine` | no | no | yes | no |
| `resource.dependencies.get` | `read` | `project_files` | yes | no | yes | no |
| `resource.tree.get` | `read` | `project_files` | yes | no | yes | no |
| `resource.update` | `project_write` | `headless_engine` | no | yes | yes | no |
| `runtime.properties.get` | `read` | `game_session` | yes | no | yes | no |
| `runtime.recording.capture` | `runtime_execute` | `game_session` | no | no | no | no |
| `runtime.screenshot.capture` | `runtime_execute` | `game_session` | no | no | no | no |
| `runtime.signals.get` | `read` | `game_session` | yes | no | yes | no |
| `runtime.tree.get` | `read` | `game_session` | yes | no | yes | no |
| `runtime.ui.get` | `read` | `game_session` | yes | no | yes | no |
| `scene.batch.apply` | `project_write` | `headless_engine` | no | yes | yes | no |
| `scene.create` | `project_write` | `headless_engine` | no | no | yes | no |
| `scene.dependencies.get` | `read` | `project_files` | yes | no | yes | no |
| `scene.save` | `project_write` | `live_editor` | no | yes | yes | no |
| `scene.tree.get` | `read` | `project_files` | yes | no | yes | no |
| `script.check` | `runtime_execute` | `headless_engine` | no | no | yes | no |
| `script.create` | `project_write` | `project_files` | no | no | yes | no |
| `script.patch` | `project_write` | `project_files` | no | yes | yes | no |
| `script.validate` | `read` | `project_files` | yes | no | yes | no |
| `workspace.projects.list` | `read` | `project_files` | yes | no | yes | no |

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

### `editor.redo`

Redo the most recently undone action in the undo history of the scene being edited, in the connected editor, as the editor's own Redo does; refused with nothing changed while the editor's global or remote history also has an action to redo. `save` also saves the scene. Returns the scene's semantic digest after the redo. A lost answer is reported as unresolved and never repeated.

### `editor.scene.open`

Open the text scene at `path` in the connected editor and make it the scene being edited; `reload` re-reads an open scene from its file, refused while the editor holds changes the file does not. Changes editor state only; no project file is written.

### `editor.scene.tree.get`

Return the scene the connected editor is editing (or the open `scene`, when current) as the editor holds it: nodes with class, script, instanced scene, groups and changed stored properties as tagged values, persistent signal connections, unsaved state and a semantic digest. Reading adds no undo history and marks nothing unsaved.

### `editor.selection.set`

Select `nodes` — paths relative to the root of the scene being edited — in the connected editor, replacing the selection. Changes editor state only.

### `editor.session.start`

Start a headless Godot editor on the project as a managed session this server owns, hosting the GDScript language server and debug adapter on loopback ports the server allocates, and answer once both listen and are held by that editor. debug.command reaches them only through this session's launch receipt. A game the debugger runs renders into the server's display when it has one. The session ends at `lifetimeSeconds` at the latest. Runs editor plugins and tool scripts; needs a Godot editor build.

### `editor.session.stop`

Stop a managed editor session: its language-server and debug-adapter connections are closed and the editor, with any game it runs under the debugger, is terminated inside its boundary. Stopping a session that already ended answers its record. Answers the exit facts.

### `editor.status.get`

Report whether a Godot editor running the godot_mcp plugin is connected, with its Godot and plugin versions, and, when it is, the open scenes and which one is being edited, the selection, unsaved state and the edited scene's undo history. Changes nothing in the editor.

### `editor.types.get`

Describe one `class` as the connected editor sees it: an engine class of its exact Godot build, or a project script class by its `class_name` — inheritance, its own properties with types, hints and defaults, signals, methods and enums. Inherited members are listed under the parent class.

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

### `input.sequence.send`

Apply up to 64 input `steps` in order in a managed game session, with `wait` steps between them (at most 10 s in total). Everything the sequence pressed is released when it ends, fails or is cancelled. Answers how many steps ran.

### `project.bind`

Bind this server, once and for the rest of its lifetime, to the Godot project at workspace-relative `path`: acquire its single-writer lock, reconcile its journal and publish workspace revision 1. The tool list then changes to the project tools. Changes only the server's private state; needs no Godot process.

### `project.create`

Create a new Godot project in `path`, a workspace-relative directory that is absent or empty, with application name `name` and an optional `renderer`. It refuses to escape the workspace, follow a symlink or replace an existing project. Available only before a project is bound; call project.bind next. Needs no Godot process; requires `idempotencyId`.

### `project.dependencies.get`

List every resolved and dangling dependency edge between project scripts, scenes and resources, paged with `offset`/`limit`, from the coherent workspace snapshot. Read-only; needs no Godot process.

### `project.discover`

List the project's scripts, scenes, resources and other files with their kinds, paged with `offset`/`limit`, from the coherent workspace snapshot. Read-only; needs no Godot process.

### `project.export`

Start exporting `preset` (a release build, or `mode` debug) through exactly the installed export template of the resolved Godot build, as a managed export this server owns, and answer at once with its export ID. The output goes to the server's private storage, never into the project; every file a successful export produces becomes an artifact, and `smoke` runs the build headless once where this host can run it: a Linux build on Linux, a Windows build on Windows, a macOS app bundle, exported as one zip, on a Mac. A preset with a secret written into it, a missing template and an unsupported platform are refused before anything runs. Executes project code; needs a Godot editor build.

### `project.export.cancel`

Cancel a running managed export: its process is terminated inside its boundary and its partial output is removed, so nothing of it becomes an artifact. Cancelling an export that already ended answers its record.

### `project.export.status`

Report one managed export — its state, preset, platform and template, progress while it runs, and when it ended the exit facts, every output file with its size, SHA-256 and artifact URI (until the artifact expires) and the smoke run's result — or, without `export`, every export this server owns. Changes nothing.

### `project.files.list`

List the project's files with their kind, byte length and SHA-256, optionally under one `prefix`, paged with `offset`/`limit`, from the coherent workspace snapshot. The hashes are the preconditions mutations require. Read-only; needs no Godot process.

### `project.import.run`

Run the Godot importer headlessly over the held project, rewriting its import metadata, and report the classified verdict, parsed engine diagnostics, bounded output, which `expected_artifacts` were produced and, for each of `assets`, its import state and imported artifact hashes. Needs a supervised headless Godot editor process; the workspace revision advances after it commits.

### `project.info.get`

Report the held project's path-free identity, its parsed project settings summary and the workspace revision the answer was taken from. Read-only; needs no Godot process.

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

### `project.settings.get`

Return the typed project settings — application name, main scene, autoloads, input actions and their events, enabled plugins and the allowlisted values — with the SHA-256 of project.godot that project.settings.patch requires. Read-only; needs no Godot process.

### `project.settings.patch`

Apply typed `operations` — set or clear the main scene, set or remove an autoload or an input action, set an allowlisted value, enable or disable an installed plugin — to project.godot whose SHA-256 is `expectedSha256`, preserving unrelated keys and comments. Journaled; requires `idempotencyId`.

### `project.test`

Run the project's already-committed GUT or gdUnit4 test suite, optionally narrowed by `selection`, in a supervised headless Godot process and report the parsed summary and verdict. A project without a framework is unsupported; nothing is installed. Executes project code; needs a Godot build.

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

### `resource.tree.get`

Return the parsed structure of one text `resource` (.tres): its type, properties, sub-resources and references, paged with `offset`/`limit`. Read-only; needs no Godot process.

### `resource.update`

Canonicalize one existing text `target` resource through a supervised headless Godot re-save and publish the result with compare-and-swap. `dry_run` validates without writing. Needs a headless Godot editor process; the workspace revision advances after it commits.

### `runtime.properties.get`

Read the current values of one running node's `properties` (its stored and editor-visible ones by default) as tagged values, with the probe's revision and frame. A value the contract cannot carry is named, not serialized. Changes nothing in the game.

### `runtime.recording.capture`

Record `durationMs` (at most 10 s) of the managed game session's root viewport at `fps` (at most 30), optionally scaled to `maxWidth`, as one animated PNG artifact, and answer its opaque artifact URI, frame count, size and SHA-256. Needs a windowed session.

### `runtime.screenshot.capture`

Capture the managed game session's root viewport after the next drawn frame as a PNG artifact, read back from the probe and checked against its digest, and answer its opaque artifact URI, size and SHA-256. Needs a windowed session.

### `runtime.signals.get`

Read one running node's signals with their arguments and current connections; with `watch` the probe also counts every later emission of those signals, which later reads report. Answers the probe's revision and frame. Changes nothing in the game.

### `runtime.tree.get`

Read the running game's scene tree from `root` (the whole tree by default) to `depth`: each node's path, class, script, groups, child count and visibility, with the probe's `revision` (it advances whenever a node enters, leaves or is renamed) and the engine `frame` observed. Changes nothing in the game.

### `runtime.ui.get`

Read the running game's visible Controls: path, class, global rectangle, text, disabled and pressed state, and which one has focus, with the probe's revision and frame. Changes nothing in the game.

### `scene.batch.apply`

Apply one atomic batch of typed `operations` to the text scene at `path` whose SHA-256 is `expectedSha256`: add, remove, rename, move or duplicate nodes, set properties, attach scripts, (re)instance scenes, connect signals, edit groups and sub-resources. Nodes are named by path or by a scene tree `handle`; a handle of older bytes refuses. The engine applies and saves it; any refused operation publishes nothing. A scene open in the connected editor is changed there instead, as one undoable action, and saved; one holding unsaved editor changes refuses.

### `scene.create`

Create one new text scene at an absent `path` with a `rootType` root node and an optional batch of typed `operations` — nodes, instances, typed properties, scripts, signal connections, groups and sub-resources — applied and saved by a supervised headless Godot process, then published with compare-and-swap. Requires `idempotencyId`.

### `scene.dependencies.get`

List the scripts, sub-scenes and resources one text `scene` (.tscn) depends on and their resolution, from the coherent workspace snapshot. Read-only; needs no Godot process.

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

### `script.validate`

Statically validate one `script` with the GDScript parser and report its diagnostics. It does not run the engine; script.check performs the engine check. Read-only; needs no Godot process.

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
* `project.dependencies.get`
* `project.discover`
* `project.info.get`
* `project.validate`
* `resource.dependencies.get`
* `resource.tree.get`
* `scene.dependencies.get`
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
* `editor.redo`
* `editor.scene.open`
* `editor.scene.tree.get`
* `editor.selection.set`
* `editor.status.get`
* `editor.types.get`
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
* `project.dependencies.get`
* `project.discover`
* `project.files.list`
* `project.import.run`
* `project.info.get`
* `project.settings.get`
* `project.settings.patch`
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
* `scene.save`
* `scene.tree.get`
* `script.create`
* `script.patch`
* `script.validate`

Bootstrap tools, listed before a `--workspace` server binds its project:

* `capabilities.get`
* `health.get`
* `project.bind`
* `project.create`
* `workspace.projects.list`

Resources:

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

* `asset.status`
* `capabilities.get`
* `debug.command`
* `debug.session.get`
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
* `input.sequence.send`
* `project.dependencies.get`
* `project.discover`
* `project.export`
* `project.export.cancel`
* `project.export.status`
* `project.files.list`
* `project.info.get`
* `project.run`
* `project.run.logs`
* `project.run.start`
* `project.run.status`
* `project.run.stop`
* `project.settings.get`
* `project.test`
* `project.validate`
* `resource.dependencies.get`
* `resource.tree.get`
* `runtime.properties.get`
* `runtime.recording.capture`
* `runtime.screenshot.capture`
* `runtime.signals.get`
* `runtime.tree.get`
* `runtime.ui.get`
* `scene.dependencies.get`
* `scene.tree.get`
* `script.check`
* `script.validate`

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
* `editor.redo`
* `editor.scene.open`
* `editor.scene.tree.get`
* `editor.selection.set`
* `editor.session.start`
* `editor.session.stop`
* `editor.status.get`
* `editor.types.get`
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
* `input.sequence.send`
* `project.dependencies.get`
* `project.discover`
* `project.export`
* `project.export.cancel`
* `project.export.status`
* `project.files.list`
* `project.import.run`
* `project.info.get`
* `project.run`
* `project.run.logs`
* `project.run.start`
* `project.run.status`
* `project.run.stop`
* `project.settings.get`
* `project.settings.patch`
* `project.test`
* `project.uid.update`
* `project.validate`
* `resource.batch.apply`
* `resource.create`
* `resource.dependencies.get`
* `resource.tree.get`
* `resource.update`
* `runtime.properties.get`
* `runtime.recording.capture`
* `runtime.screenshot.capture`
* `runtime.signals.get`
* `runtime.tree.get`
* `runtime.ui.get`
* `scene.batch.apply`
* `scene.create`
* `scene.dependencies.get`
* `scene.save`
* `scene.tree.get`
* `script.check`
* `script.create`
* `script.patch`
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
