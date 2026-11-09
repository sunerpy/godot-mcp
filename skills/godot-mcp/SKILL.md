---
name: godot-mcp
description: "Read and change a Godot 4 project through a connected godot-mcp MCP server. Use when scenes, scripts, resources, project settings or assets are inspected or edited with godot-mcp tools, when nodes such as a physics body, lights, particles, a HUD, a TileMapLayer, a Theme or an AnimationTree state machine are added, to connect signals or find script errors, when the Godot editor has the scene open, or when a godot-mcp answer is conflict, uncertain, unavailable, unsupported or permission_denied. Covers reading before writing with SHA-256 preconditions and idempotency ids, static script checks, recipes, connect_signal and call_method, typed values and property paths, and what each outcome asks of the agent. Running and testing the game is the godot-mcp-playtest skill; exporting it is the godot-mcp-export skill; installing, connecting and diagnosing the server is the godot-mcp-setup skill."
license: MIT
compatibility: "Needs a godot-mcp MCP server (godot-mcp serve) connected for one Godot 4.4.0 or later project with the editor or full profile. The editor tools also need the godot_mcp add-on running in an open Godot editor."
metadata:
  contract: "0.8.0"
  profiles: "editor,full"
---

# godot-mcp: reading and changing a Godot project

godot-mcp is an MCP server that runs beside one Godot project. Its tools read the project
without an engine, write files through journaled transactions, run a headless Godot for the work
only the engine can do, and act in the open Godot editor through its add-on. This skill says how
to combine them. The tool schemas in `tools/list` are the reference for every argument; where
this skill and a schema disagree, the schema is right.

This skill describes contract 0.8.0, the `contract` of its metadata. `health.get` reports the
contract the server speaks as `schema_version`. When the two differ, follow the schemas and the
tool descriptions.

## Start here

1. Call `health.get`. It names the server's profile and each component (`project`, `godot`,
   `editor`, `runtime`) with `status`; for each one that is not ready it gives a code and a
   remedy. `readiness` says whether reads and writes are served now, and `journal` counts
   quarantined transactions.
2. A tool you need is not in `tools/list`: `capabilities.get` lists the tools of every profile.
   The profile is fixed when the server starts, so tell the user which profile lists the tool
   instead of working around it.
3. `tools/list` holds only `health.get`, `capabilities.get`, `workspace.projects.list`,
   `project.bind` and perhaps `project.create`: the server was started with `--workspace` and no
   project is bound yet. Call `workspace.projects.list`, then `project.bind` with the project's
   workspace-relative `path`. A server binds once for its lifetime, and its tool list then
   changes to the project's tools. `project.create` makes a new project in an absent or empty
   directory; bind it next.

Send one request at a time and wait for its answer. A burst is refused per caller with
`operational limit subject_concurrency was exceeded`.

## What the server is made of

| Plane | What it does | Needs |
| --- | --- | --- |
| static analysis | `scene.tree.get`, `gdscript.references.get`, `project.search` and the other reads answer from the files, each with how sure it is: `exact`, `inferred` or `unresolved` | nothing |
| journaled writes | the file, script, scene, resource, asset and settings writes, each recorded before it takes effect and checked against the bytes it replaces | nothing for plain files |
| headless engine | a supervised headless Godot applies scene and resource batches and runs `project.import.run` and `project.uid.update` | a Godot 4.4.0 or later editor build |
| editor bridge | the `editor.*` tools act in the open editor, as undoable editor actions | the `godot_mcp` add-on connected |
| managed sessions | games and editors the server starts and stops; see the godot-mcp-playtest skill | the runtime or full profile |

A tool the profile lists answers `unavailable` while its plane cannot be reached, for example an
editor tool with no editor connected. `health.get` says which plane and what to do.

## Finding script errors

- `gdscript.diagnostics.get` lists the parse and analysis errors of the project's scripts, or of
  one `script`; `script.validate` checks one script the same way, and `project.validate` the whole
  project, for parse errors, dangling references and invalid settings. They run no engine.
- These checks are static. The engine's own compile of a script is a tool of the runtime and full
  profiles; see the godot-mcp-playtest skill.
- With an editor connected, `editor.log.get` returns what the editor logged since the add-on
  started, its errors and script errors each with the source location, on Godot 4.5.1 and later.

## Read, then write

Every write is a compare-and-swap against the bytes you read:

- Read first. `project.files.list` gives each file's SHA-256, `file.get` one file's bytes and
  hash, and `scene.tree.get` a scene's nodes, its SHA-256 and a `handle` per node.
- A write that replaces, moves or deletes a file takes that hash as `expectedSha256`. A write
  that creates a file needs the path to be absent.
- Every write takes an `idempotencyId`, a new one for each change you mean. Repeating the exact
  call with the same id answers the recorded result without applying it twice; the same id with
  other arguments is a `conflict`.
- `dryRun: true` checks the write and answers its diff without writing; `resource.update`,
  `project.uid.update` and `scene.save` take `dry_run: true` instead.
- A batch is all or nothing: when one operation is refused, nothing is written.
  `project.batch.apply` changes several scenes and resources in one transaction.
- A `connect_signal` operation of a scene batch connects the `signal` of the node `from` to the
  `method` of the node `to`, and the scene saves the connection. The method must already exist on
  `to` or its script, or the batch is refused as `unknown_method`: add it with `script.patch`
  first, then call `editor.filesystem.scan` when the editor has the scene open.
- `project.godot` changes only through `project.settings.patch`.
- `script.create` and `script.patch` parse the result and refuse a parse error unless
  `allowParseErrors` is set. The parse is static; it does not run the script.

A `conflict` means something changed after you read it, and nothing was written. Read again, look
at what changed, and send the change against the new hash. Never resend the call with only the
hash replaced: that is how one change silently overwrites another.

## Scenes the editor has open

With the `godot_mcp` add-on connected, the editor owns what it has open. `editor.status.get`
lists the open scenes and scripts and which of them hold unsaved changes.

- `scene.batch.apply` on a scene open in the editor is applied there as one undoable action and
  saved. It is refused with `conflict` while the editor holds changes the file does not.
- To change what the editor holds, unsaved changes included, read the scene with
  `editor.scene.tree.get`, which answers its semantic digest, and send `editor.scene.edit` with
  that digest as `expectedDigest`. The edit is one undoable action and saves nothing;
  `editor.scene.save` saves.
- The file, script and asset writes, and the headless scene and resource writes, refuse with
  `conflict` a path the editor holds: an open scene, `project.godot`, a script whose editor tab
  differs from its file. Save or close it in the editor, or change it with the editor tools.
- `project.settings.patch` is carried out by the connected editor instead: the editor applies the
  change and saves `project.godot` with its own writer, which keeps every setting but not the
  file's comments or layout.
- After a script changed on disk, call `editor.filesystem.scan`: the editor imports what changed
  and reloads the script tabs that hold no unsaved edit.
- `editor.undo` and `editor.redo` step through the edited scene's history. While the editor's
  global or remote history also has a step to take, they are refused with `conflict` and change
  nothing; that step is taken in the editor.
- While **Pause agent edits** is on in the editor's Godot MCP panel, every mutation that goes
  through the editor answers `unavailable` with status `not_started`, and nothing changed. Tell
  the user. Once they turn it off, retry with a new `idempotencyId`; the old one answers the
  recorded refusal.

This edit renames the node `Player` and moves it, in the scene the editor has open:

```json tool=editor.scene.edit
{
  "expectedDigest": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
  "idempotencyId": "rename-player-1",
  "label": "Rename the player",
  "operations": [
    { "op": "rename_node", "node": "Player", "name": "Hero" },
    { "op": "set_property", "node": "Hero", "property": "position",
      "value": { "type": "vector2", "value": { "x": 64, "y": 128 } } }
  ]
}
```

## Common setups: look for a recipe first

godot-mcp knows common setups by name. Before building one of these by hand, use its recipe:

- `prompts/list` holds one prompt per recipe, `godot-mcp.recipe.<name>`: `body-2d`, `body-3d`,
  `light-3d`, `particles-2d`, `particles-3d`, `navigation-2d`, `navigation-3d`, `audio-2d`,
  `audio-3d`, `shader-2d`, `hud`, `gridmap-3d`, `joystick`, `animation-state-machine`,
  `blend-space-1d`, `theme` and `material-3d`. A prompt's text holds the recipe's parameters, a
  call template and the operations the recipe becomes.
- `scene.recipe.apply` adds a scene recipe below `parent` in an existing scene, against its
  SHA-256. `recipe` is an object with one key, the recipe's name, holding its parameters, and
  `{}` takes every default. The tool's schema lists every parameter with its range; the answer
  names each value it used and the operations it applied. With the scene open in the editor, it
  is one undoable action there.
- A recipe only adds below a parent, and `scene.create` makes a scene whose root is a `Node`
  unless `rootType` names another class. A scene whose root is the body itself, such as a
  `CharacterBody2D` player, is `scene.create` with that `rootType` and the shape as `operations`:
  an `add_node` of a `CollisionShape2D` and a `set_subresource` of its `shape`.
- `hud` lays out its `parent` as a full-screen `Control`, and `joystick` anchors itself to a
  `Control` parent. Apply both at `parent` `.` of a scene whose root is a `Control`: below a
  `CanvasLayer` or a `Node2D`, Godot does not keep the layout and the batch is refused, as
  `value_not_kept` or `unknown_property`, with nothing written. For a 2D level, make the HUD its
  own scene with `scene.create` and `rootType` `Control`, then `instance_scene` it below a
  `CanvasLayer` of the level.
- `theme` and `material-3d` make a resource, not nodes: `resource.create` takes them in its
  `recipe` field, with a new `.tres` `path` and neither `class` nor `operations`, and
  `resource.batch.apply` changes the resource later. A `MeshInstance3D` takes the material in
  `material_override` as a `resource` value naming its path.
- `animation-state-machine` and `blend-space-1d` add an `AnimationTree` and do not read the
  scene: `anim_player` must reach an `AnimationPlayer` that has the animations the states or
  points name. Check them first with `editor.scene.read` calling `get_animation_list` on the
  player, with the scene open in the editor. The tool refuses a call that breaks a rule across
  values the schema cannot state: duplicate state names, a transition that leaves or enters an
  undeclared state or enters the state it leaves, two transitions between the same states the
  same way round, a `start` that is not a state, `min_space` not below `max_space`, a point off
  the axis.
- `joystick` builds a `VirtualJoystick`, which Godot 4.4.1, 4.5.1 and 4.6.1 lack; it applies on
  Godot 4.7.1 and 4.7.2.

```json tool=scene.recipe.apply
{
  "path": "res://scenes/level.tscn",
  "expectedSha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
  "idempotencyId": "add-player-body-1",
  "parent": ".",
  "recipe": { "body-2d": { "name": "Player", "shape": "capsule", "ground_ray": true } }
}
```

## Without a recipe: call_method

Some edits have no property to set: adding a state to a state machine, painting cells, keying an
animation, adding theme items. A `call_method` operation makes them, in `scene.batch.apply`,
`editor.scene.edit`, `resource.batch.apply`, `scene.create` and `resource.create`.

- `method` is one of the editing methods the schema's `method` enum lists, called on a receiver
  whose class has it. Nothing else can be called.
- The receiver is `node`, the root `.` by default, or a resource of that node named by
  `resource`: `{ "property": "tree_root" }`, which also takes a property path through embedded
  resources, `{ "animation": "walk" }` or `{ "library": "" }`. The resource must be one the scene
  embeds; one saved in its own file is refused as `external_resource`, so change that file with
  `resource.batch.apply`.
- `arguments` are tagged values, at most 16, checked against the method's signature in the
  running Godot; trailing arguments that have defaults may be left out. A
  `{ "type": "new_resource", "value": { "class": ..., "properties": { ... } } }` argument
  passes a new resource the scene then embeds.
- `repeat`, a list of argument lists, calls the method once per list, at most 4096 calls, as one
  change and one undo step.
- The answer's `results` holds what each call returned.

### An AnimationTree state machine

This batch is for a scene whose root has a child `AnimationPlayer` with the animations `idle`
and `run`. `set_subresource` gives the new AnimationTree an `AnimationNodeStateMachine`,
`add_node` adds a state that plays each animation, and `add_transition` connects them. A new
state machine already holds the `Start` and `End` states, and an `advance_mode` of 2
(`ADVANCE_MODE_AUTO`) lets the machine enter `Idle` by itself.

```json tool=scene.batch.apply
{
  "path": "res://scenes/player.tscn",
  "expectedSha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
  "idempotencyId": "player-state-machine-1",
  "operations": [
    { "op": "add_node", "parent": ".", "name": "AnimationTree", "type": "AnimationTree" },
    { "op": "set_property", "node": "AnimationTree", "property": "anim_player",
      "value": { "type": "node_path", "value": "../AnimationPlayer" } },
    { "op": "set_subresource", "node": "AnimationTree", "property": "tree_root",
      "class": "AnimationNodeStateMachine" },
    { "op": "call_method", "node": "AnimationTree", "resource": { "property": "tree_root" },
      "method": "add_node",
      "repeat": [
        [ { "type": "string_name", "value": "Idle" },
          { "type": "new_resource", "value": { "class": "AnimationNodeAnimation",
            "properties": { "animation": { "type": "string_name", "value": "idle" } } } },
          { "type": "vector2", "value": { "x": 300, "y": 100 } } ],
        [ { "type": "string_name", "value": "Run" },
          { "type": "new_resource", "value": { "class": "AnimationNodeAnimation",
            "properties": { "animation": { "type": "string_name", "value": "run" } } } },
          { "type": "vector2", "value": { "x": 500, "y": 100 } } ] ] },
    { "op": "call_method", "node": "AnimationTree", "resource": { "property": "tree_root" },
      "method": "add_transition",
      "repeat": [
        [ { "type": "string_name", "value": "Start" }, { "type": "string_name", "value": "Idle" },
          { "type": "new_resource", "value": { "class": "AnimationNodeStateMachineTransition",
            "properties": { "advance_mode": { "type": "integer", "value": 2 } } } } ],
        [ { "type": "string_name", "value": "Idle" }, { "type": "string_name", "value": "Run" },
          { "type": "new_resource", "value": { "class": "AnimationNodeStateMachineTransition",
            "properties": { "xfade_time": { "type": "float", "value": 0.2 } } } } ] ] }
  ]
}
```

A state can also be written as a property: `set_subresource` on
`tree_root:states/Jump/node` with `class` `AnimationNodeAnimation` adds the state `Jump`.

### Painting a TileMapLayer

`set_cell` takes the cell, the TileSet source id and the tile's atlas coordinates. The layer's
`tile_set` must hold that source. With the scene open in the editor, `editor.scene.read` calling
`get_used_cells` reads the painted cells back.

```json tool=scene.batch.apply
{
  "path": "res://scenes/level.tscn",
  "expectedSha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
  "idempotencyId": "paint-floor-1",
  "operations": [
    { "op": "call_method", "node": "Ground", "method": "set_cell",
      "repeat": [
        [ { "type": "vector2i", "value": { "x": 0, "y": 4 } }, { "type": "integer", "value": 0 },
          { "type": "vector2i", "value": { "x": 1, "y": 0 } } ],
        [ { "type": "vector2i", "value": { "x": 1, "y": 4 } }, { "type": "integer", "value": 0 },
          { "type": "vector2i", "value": { "x": 1, "y": 0 } } ],
        [ { "type": "vector2i", "value": { "x": 2, "y": 4 } }, { "type": "integer", "value": 0 },
          { "type": "vector2i", "value": { "x": 2, "y": 0 } } ] ] }
  ]
}
```

### A Theme

A theme saved in its own file is a resource: its calls go to `resource.batch.apply`, whose
`call_method` calls the theme itself.

```json tool=resource.batch.apply
{
  "path": "res://ui/theme.tres",
  "expectedSha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
  "idempotencyId": "button-theme-1",
  "operations": [
    { "op": "call_method", "method": "set_color",
      "arguments": [ { "type": "string_name", "value": "font_color" },
                     { "type": "string_name", "value": "Button" },
                     { "type": "color",
                       "value": { "red": 1.0, "green": 0.85, "blue": 0.3, "alpha": 1.0 } } ] },
    { "op": "call_method", "method": "set_font_size",
      "arguments": [ { "type": "string_name", "value": "font_size" },
                     { "type": "string_name", "value": "Button" },
                     { "type": "integer", "value": 24 } ] }
  ]
}
```

## Typed values and property paths

Every value is a tagged value, `{"type": ..., "value": ...}`, and answers use the same form:

| Godot type | Tagged value |
| --- | --- |
| bool, int, float | `{"type": "bool", "value": true}`, `{"type": "integer", "value": 3}`, `{"type": "float", "value": 0.5}` |
| String, StringName, NodePath | `string`, `string_name` and `node_path`, each with a string |
| Vector2, Vector2i, Vector3 | `{"type": "vector2", "value": {"x": 1, "y": 2}}`; `vector2i` and `vector3` alike |
| Color | `{"type": "color", "value": {"red": 1, "green": 0.5, "blue": 0, "alpha": 1}}` |
| a resource file | `{"type": "resource", "value": {"resource": "res://ui/theme.tres", "resource_type": "Theme"}}` |
| a new embedded resource | `set_subresource`, or a `new_resource` argument of `call_method` |

A property path passes through embedded resources with `:` and may end in a component of the
value: `position:x`, `material:albedo_color`, `theme_override_colors/font_color`,
`tree_root:states/Idle/node:animation`. Every write is read back from the engine:

- `value_type_mismatch`: the property holds another type; send that type.
- `value_not_kept`: the engine replaced the value, and the answer names the one it kept; an
  `AudioStreamPlayer2D` keeps `Master` for a bus the bus layout lacks. Fix the cause, such as
  the missing bus, rather than resending the value.
- `unknown_property`: the path names no property. `editor.types.get` lists a class's
  properties for the running Godot build, and `editor.node.get` one node's.
- `external_resource`: the path passes through a resource saved in its own file; edit that file.

## What an answer means

| Answer | It means | Do |
| --- | --- | --- |
| `conflict` | something changed after you read it, the editor holds the path, or an `idempotencyId` was reused with other arguments; nothing was written | read again and decide; for an editor-held path, save or close it there, or use the editor tools |
| `unavailable` | the plane cannot be reached now: no Godot build, no editor connected, edits paused | `health.get` names the cause and the remedy; tell the user what is missing |
| `uncertain` | a write's outcome cannot be proven, or an analysis cannot prove its answer | for a write, never resend it, and read the project and `health.get` first; for an analysis, check the candidates it lists |
| `unsupported` | this Godot build or the editor's add-on lacks the feature | for an older add-on, `editor.addon.update` and then `editor.addon.reload`; otherwise tell the user |
| `permission_denied` | the server's policy refuses the call | do not work around it; tell the user |
| `invalid_input` | the request does not fit the schema, or the engine refused an argument | correct the request from the message |
| `not_found` | the file, node or class does not exist | read the tree again |
| `timeout` | the call ran past its deadline, so whether its change landed is not proven | as for `uncertain`: do not resend it; read the project first |
| `cancelled` | the request was cancelled before it started; nothing changed | send it again if it is still wanted |
| `internal` | a fault in the server itself | tell the user, with the message |

An editor change whose answer was lost answers `uncertain` and is never sent again, not even
after the editor reconnects. Read the scene before you decide what to do next.

**A closed write gate.** When the server cannot prove how an interrupted transaction ended, it
quarantines the transaction and refuses every later write with `conflict`, saying that writes
are blocked; `health.get` counts the quarantined transactions under `journal`. Reads still work.
Stop writing, and do not change the files some other way. A change the editor is still running
settles when the editor answers it, and the gate reopens then: check `health.get` again once
the editor's work should be done. When the editor was closed or crashed first, or the gate stays
closed, tell the user that the project's owner resolves it on that machine with
`godot-mcp transaction repair`, starting with its `reconcile` operation.

## Commands the project declares

A project can declare its own editor commands in `godot_mcp_commands.json`: rebuilding a level,
checking assets, exporting a table. Before you write a script or a long batch for such work, call
`editor.commands.list`; it reads the file and runs nothing. When a listed command does the job,
`editor.commands.call` checks its arguments against the command's schema and has the connected
editor run it. Nothing a command does is undone, and a call not answered by its deadline is
`uncertain` and closes the write gate until the editor answers, as an editor change does.

## Scripts are the operator's decision

`script.run` and `editor.script.run` run caller-written GDScript in a headless Godot or in the
editor. They run nothing until the operator starts the server with `--allow-scripts`, and answer
`permission_denied` until then. Prefer the typed tools: a typed batch is checked before it
writes and applied whole or not at all, while a script's changes are not undone. Suggest the
switch only when the user wants a script run.
