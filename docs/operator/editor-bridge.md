# Editor bridge

The `godot_mcp` editor add-on connects a running Godot editor to the server. With it, an MCP
client inspects what the editor holds, changes scenes as ordinary undoable editor actions, saves
them, captures the editor's window, reads its log, rescans the project, opens scripts and runs
the project, while the server keeps its journal, policy and recovery rules.

The editor bridge plug-in is not shipped with the first release: version 0.1 shipped only the
frozen bridge protocol and its codec. Contract 0.2 ships the add-on, and contract 0.3 adds the
live edits, saves, captures, log, scans, script navigation, editing calls and running the
project that this page describes. Contract 0.4 adds the editor's cameras, closing script tabs,
its dialogs, restarting it, its performance monitors and resource previews, and contract 0.6
calling the commands a project declares. The protocol it speaks is still the frozen one; see
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
  request at a time on its main thread and answers no heartbeat meanwhile. The status the
  server sends the panel is not such a request, so an editor that hangs is shown as
  disconnected after 30 seconds.
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

## The Godot MCP panel

The add-on shows what the bridge is doing inside the editor. A small dock, in the lower right
dock slot, holds the connection status, an **Agent edits are paused** banner while edits are
paused, the **Pause agent edits** switch, **Reconnect now**, and **Details**, which opens the
**Godot MCP** bottom panel. The panel shows the same banner above its five tabs.

| Tab | What it shows |
| --- | --- |
| Status | The connection and why the last one ended; the server's version, session, profile and transport; the add-on's and Godot's versions; the project generation; when the connection opened; the last request; and how many requests were served, failed and refused. |
| Activity | One row per request the editor answered, newest first: local time, method, operation class, target, result (`ok` or the error kind), mutation status and milliseconds from arrival to answer. **Clear** empties the list; **Copy** copies it as a tab-separated table, oldest row first. |
| Tools | The tools of the server's profile, grouped by what they need: the project's files, a headless Godot, this editor, a managed game session or a managed editor session. Each group says whether that need is available now, and why not; open a group to list its tools. |
| Add-on | The bridge methods this add-on serves: the session's own, the reads and the mutations. |
| Controls | **Pause agent edits**, **Reconnect now** and **Copy diagnostics**. |

- A row's target is the `res://` path of the scene or script the request names, a node as
  `res://main.tscn::Player`, or a fixed name such as `project.godot`, `editor settings` or
  `main screen`. It is never an argument value or file content, it is at most 160 characters
  long, and control characters show as `?`.
- The list keeps the newest 500 rows and counts the ones it dropped. A mutation whose outcome
  the server may not know is highlighted: `unresolved`, or a change whose answer the editor
  could not deliver because its connection ended first.
- *Served* counts answers from the method's handler; *failed*, handlers that ran and failed;
  *refused*, requests refused before their handler ran: paused, past their deadline, malformed,
  an unsupported method, the wrong operation class, a full queue, or withdrawn. The session's own
  `bridge.*` requests are listed but not counted.
- The Tools tab is filled from the server's status, a request the server sends when the session
  opens and again whenever what it says changes; see
  [Editor bridge protocol](../protocol/bridge-protocol.md#the-servers-status). Each need's
  availability is the one `health.get` reports: its `project`, `godot`, `editor` and `runtime`
  components, and for a managed editor session the router's `lsp_dap` plane. A server older
  than the add-on sends no status; the tab then says the server did not describe its tools, and
  lists none.
- **Copy diagnostics** copies the versions, the connection, the session, the counts and the last
  20 failed answers, each with its method, error kind, mutation status, target and the editor's
  message cut to 300 characters. A message is the editor's own and can name what its request
  named, such as a node, a setting or a value it refused. The copy never holds the rendezvous
  secret.
- **Reconnect now** closes the connection and dials again at once from a fresh read of the
  rendezvous file. Requests still queued are dropped unrun. A request the editor is running then
  finishes, but its answer is not sent: as with any dropped connection, the server records a
  mutation it never heard back from as unresolved.

### Pausing agent edits

While **Pause agent edits** is on, the editor refuses every mutation the server sends it before
the mutation starts. The call answers `unavailable` with mutation status `not_started`, and the
message says that the user paused agent edits in the Godot MCP panel. Reads and the session's
own requests still run, and a mutation already running finishes.

- A refusal is an ordinary `not_started` outcome: nothing changed, nothing is quarantined and no
  write is blocked. The server keeps that outcome under the call's `idempotencyId`, so once
  edits are allowed again, retry with a new one; the same one answers the recorded refusal.
- Only what passes through the editor is paused. That includes `scene.batch.apply` on a scene
  the editor holds and `project.settings.patch` while an editor is connected, which the editor
  carries out. The server's own writes do not pass through it and go on: the file, script,
  resource and asset tools, and `scene.batch.apply` on a scene the editor does not hold.
- The switch lasts until it is turned off, the editor closes, or the add-on is disabled or
  reloaded; it is kept nowhere on disk.
- It is a convenience, not a security boundary: any program running as your user can still
  change the project's files. See the [threat model](../security/threat-model.md).

## The editor tools

| Tool | Class | What it does |
| --- | --- | --- |
| `editor.status.get` | read | Connection status and editor state, as above. |
| `editor.scene.tree.get` | read | The edited scene as the editor holds it: nodes, scripts, instances, groups, changed stored properties as tagged values, persistent connections, unsaved state and a semantic digest. |
| `editor.node.get` | read | One node as the editor holds it: class, script, instanced scene, ownership, groups, children, every property (or the named ones) with type, hint, usage, tagged value and whether it differs from its default, and its signals with their connections. |
| `editor.scene.read` | read | The values the engine answers for reading calls on the edited scene as the editor holds it, unsaved edits included: the calls of `scene.read`; see [Reading in the editor](#reading-in-the-editor). |
| `editor.types.get` | read | One engine class of this exact Godot build, or a project script class: inheritance, properties with types and defaults, signals, methods and enums. |
| `editor.types.search` | read | The classes whose name contains `query`, optionally only those inheriting `base` or only instantiable ones. |
| `editor.log.get` | read | What the editor logged since the plugin started, from a cursor; see [The editor's log](#the-editors-log). |
| `editor.preferences.get` | read | The editor's own settings, the ones its Editor Settings dialog shows, by name or page by page; see [The editor's settings](#the-editors-settings). |
| `editor.shortcuts.get` | read | The editor's shortcuts and built-in actions, the ones its Shortcuts tab lists, by name or page by page; see [The editor's shortcuts](#the-editors-shortcuts). |
| `editor.camera.get` | read | What the 2D view and the four 3D viewports show: the 2D view's zoom and center, each 3D viewport's camera and whether the viewport is on screen; see [Cameras](#cameras). |
| `editor.dialogs.get` | read | The dialogs the editor shows, each with its buttons; see [Dialogs](#dialogs). |
| `editor.performance.get` | read | The editor process's performance monitors, by the names its build gives them; see [Performance monitors](#performance-monitors). |
| `editor.scene.open` | mutation | Opens a scene and makes it the one being edited; `reload` re-reads an open scene from its file. |
| `editor.selection.set` | mutation | Replaces the selection. |
| `editor.scene.edit` | mutation | Applies typed operations to the scene the editor holds, as one undoable action; see [Editing what the editor holds](#editing-what-the-editor-holds). |
| `editor.scene.save` | mutation | Saves the edited scene, an open `scene`, or with `all` every open scene. |
| `editor.scene.close` | mutation | Closes the edited scene or an open `scene`; refused while it holds a change its file does not. See [Closing a scene](#closing-a-scene). |
| `editor.undo`, `editor.redo` | mutation | Undo or redo in the edited scene's history through the editor's own Undo and Redo; refused, changing nothing, while the editor's global or remote history also has a step to take. `save: true` also saves the scene. Each answers the scene's semantic digest. |
| `editor.main_screen.set` | mutation | Shows the 2D, 3D or Script main screen. |
| `editor.screenshot.capture` | mutation | Captures the editor's window, or its 2D or one 3D viewport, as a PNG artifact. |
| `editor.filesystem.scan` | mutation | Waits for a scan or import the editor is running, rescans and imports what changed, then reloads the open script tabs whose files changed. |
| `editor.script.open` | mutation | Opens a GDScript file in the built-in script editor at a line and column. |
| `editor.script.close` | mutation | Closes a script tab that holds no unsaved edit; Godot 4.7.1. See [Closing a script tab](#closing-a-script-tab). |
| `editor.camera.set` | mutation | Moves the camera of a 3D viewport on screen to look from one point at another, read back; Godot 4.5.1 or later. See [Cameras](#cameras). |
| `editor.dialog.respond` | mutation | Presses one button of one dialog, as a click does; see [Dialogs](#dialogs). |
| `editor.resource.preview` | mutation | The editor's preview of a resource and its thumbnail, as PNG artifacts; see [Resource previews](#resource-previews). |
| `editor.restart` | mutation | Restarts the editor once nothing it holds can be lost, and waits for the restarted editor; see [Restarting the editor](#restarting-the-editor). |
| `editor.preferences.set` | mutation | Changes editor settings, all or nothing, and writes them to your editor settings file; see [The editor's settings](#the-editors-settings). |
| `editor.shortcuts.set` | mutation | Binds shortcuts and built-in actions to key events, all or nothing, and writes them to your editor settings file; see [The editor's shortcuts](#the-editors-shortcuts). |
| `editor.play.start`, `editor.play.stop` | runtime | Run the project in the editor, as its Play buttons do, and stop it. |
| `editor.addon.reload` | mutation | Disables and re-enables the plugin; the editor reconnects with a new session. |
| `editor.script.run` | mutation | Runs caller-written GDScript in the editor, when the operator turned script execution on; see [Running a script in the editor](#running-a-script-in-the-editor). |
| `editor.commands.call` | mutation | Calls a command the project declares in `godot_mcp_commands.json`, when the operator has not turned project commands off; see [Calling a project command](#calling-a-project-command). |

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
- nothing is saved; `editor.scene.save` saves. The answer carries the new digest and, for every
  `call_method`, what it returned.

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

## Property paths

Every operation that writes a property — `set_property`, `clear_property` and `set_subresource`,
in `scene.batch.apply`, `editor.scene.edit`, `resource.batch.apply`, `scene.create` and
`resource.create` — takes a property path as well as a property name. Names separated by `:`
pass through properties that hold resources the file embeds, and what follows the last of them
names a property of that resource, then, optionally, a component of its value:

| Path | Writes |
| --- | --- |
| `position:x` | the `x` of the node's `position` |
| `material:albedo_color` | `albedo_color` of the material the node embeds |
| `environment:sky:sky_material:sky_top_color` | a property three embedded resources deep |
| `theme_override_colors/font_color` | a theme override of a Control |
| `tree_root:states/Idle/node:animation` | the animation a state machine's `Idle` state plays |
| `bus/1/volume_db` | a bus of the `AudioBusLayout` a resource batch edits |

A resource saved in its own file is refused as `external_resource` wherever the path passes
through it, and a script as `script_resource`; a path that names no property, or no component of
the value, is `unknown_property`. `clear_property` clears a whole property, never a component.
Dynamic properties count: a state machine's `states/<name>/node`, a shader material's
`shader_parameter/<uniform>`, a bus layout's `bus/<index>/...`.

Every write is read back from the engine. A value the property holds as another type is refused
as `value_type_mismatch`. A boolean, integer, string or string name the engine replaced is
refused as `value_not_kept`, naming the value it kept: an `AudioStreamPlayer2D` keeps `Master`
for a `bus` the project's bus layout lacks. Floats, and the vectors and colours built of them,
are compared by type only, since Godot stores them rounded.

A resource may take a stored property it does not list yet: writing `tree_root:states/Run/node`
with `set_subresource` adds the state `Run`, and writing `bus/2/name` to a bus layout adds a
third bus, and any bus below it the layout does not have yet, as Godot's own loader does. The
resource must list the property once written, or the write is refused as `unknown_property`. A
metadata entry, `metadata/<name>`, can be added to any node or resource.

A bus layout written this way is changed in its file, the layout a game loads when it starts.
An editor or a game that is already running keeps the buses its AudioServer loaded: measured on
Godot 4.4.1, the bus count stayed what it was after the file changed, until the layout was
loaded again with `AudioServer.set_bus_layout`. The answer of a resource batch says what was
saved, never that a running AudioServer changed.

The `resource` selector of a `call_method` takes the same paths, and a resource batch's
`call_method` takes one too, to call a resource the edited resource embeds.

`add_node` and `set_subresource` also take a global script class, a script's `class_name`: the
node or resource is created as its engine base class with the script attached. `instance_scene`
and `replace_instance` take a 3D model Godot imports as a scene — `.glb`, `.gltf`, `.fbx`,
`.blend`, `.dae`, `.obj` imported as a scene — and instance all of it, every mesh, material and
animation, as one instance of the imported scene. A model that has not been imported yet is
refused as `scene_not_loadable`; import it first with `project.import.run` or
`editor.filesystem.scan`.

[Recipes](recipes.md) has common setups — physics bodies, lights and a sky, particles,
navigation, a sound, a theme, a HUD, a grid map — written as these operations.

## Editing calls

Some edits have no property to set: painting a `TileMapLayer`, keying an `Animation`, adding a
state to a state machine. A batch makes them with a `call_method` operation, in
`scene.batch.apply`, `editor.scene.edit`, `resource.batch.apply` and the operations of
`scene.create` and `resource.create`:

```json
{ "op": "call_method", "node": "Ground", "method": "set_cell",
  "arguments": [{ "type": "vector2i", "value": { "x": 3, "y": 4 } },
                { "type": "integer", "value": 0 },
                { "type": "vector2i", "value": { "x": 1, "y": 0 } }] }
```

**The receiver** is the node, `.` by default. A `resource` selector calls a resource of that
node instead: `{ "property": "curve" }` a resource property, or a property path such as
`{ "property": "tree_root:states/Locomotion/node" }`, `{ "animation": "walk" }` an animation of
an AnimationPlayer or AnimationTree, `{ "library": "" }` one of its animation libraries. The
resource must be one the scene embeds. A resource saved in its own file is refused as
`external_resource`, because the call would change that file too; edit it with
`resource.batch.apply`, whose calls go to the resource itself, or, with a `property` selector,
to a resource it embeds.

**The methods** are this table, matched by class and inheritance. Nothing else can be called.
Every method changes only its receiver's own stored state, or, for the two navigation regions,
the navigation resource the region holds, which the scene must embed, so the saved file holds
the whole change.

| Class | Methods |
| --- | --- |
| TileMapLayer | set_cell, erase_cell, clear, set_cells_terrain_connect, set_cells_terrain_path |
| GridMap | set_cell_item, clear |
| Line2D | add_point, remove_point, set_point_position, clear_points |
| AnimationMixer | add_animation_library, remove_animation_library, rename_animation_library |
| AnimationLibrary | add_animation, remove_animation, rename_animation |
| Animation | add_track, remove_track, track_set_path, track_set_enabled, track_set_interpolation_type, track_set_interpolation_loop_wrap, value_track_set_update_mode, track_insert_key, track_remove_key, track_remove_key_at_time, track_set_key_value, track_set_key_time, track_set_key_transition, set_length, set_loop_mode, set_step |
| SpriteFrames | add_animation, remove_animation, rename_animation, add_frame, remove_frame, set_frame, clear, set_animation_speed, set_animation_loop |
| Curve | add_point, remove_point, set_point_value, set_point_offset, set_point_left_tangent, set_point_right_tangent, clear_points |
| Curve2D | add_point, remove_point, set_point_position, set_point_in, set_point_out, clear_points |
| Curve3D | add_point, remove_point, set_point_position, set_point_in, set_point_out, set_point_tilt, clear_points |
| Gradient | add_point, remove_point, set_offset, set_color |
| Theme | set_color, set_constant, set_font_size, set_font, set_icon, set_stylebox, clear_color, clear_constant, clear_font_size, clear_font, clear_icon, clear_stylebox |
| Control | set_anchors_preset, set_anchors_and_offsets_preset, set_offsets_preset, set_anchor, set_anchor_and_offset |
| AnimationNodeStateMachine | add_node, remove_node, rename_node, replace_node, set_node_position, add_transition, remove_transition, remove_transition_by_index, set_graph_offset |
| AnimationNodeBlendTree | add_node, remove_node, rename_node, connect_node, disconnect_node, set_node_position, set_graph_offset |
| AnimationNodeBlendSpace1D | add_blend_point, remove_blend_point, set_blend_point_position, set_blend_point_node |
| AnimationNodeBlendSpace2D | add_blend_point, remove_blend_point, set_blend_point_position, set_blend_point_node, add_triangle, remove_triangle |
| VisualShader | add_node, remove_node, connect_nodes, disconnect_nodes, set_node_position |
| TileSet | add_source, remove_source, add_physics_layer, remove_physics_layer, set_physics_layer_collision_layer, set_physics_layer_collision_mask, add_terrain_set, remove_terrain_set, set_terrain_set_mode, add_terrain, remove_terrain, set_terrain_name, set_terrain_color, add_navigation_layer, remove_navigation_layer, add_custom_data_layer, remove_custom_data_layer, set_custom_data_layer_name, set_custom_data_layer_type |
| TileSetAtlasSource | create_tile, remove_tile, create_alternative_tile, remove_alternative_tile, clear_tiles_outside_texture |
| NavigationPolygon | add_outline, add_outline_at_index, set_outline, remove_outline, clear_outlines, add_polygon, clear_polygons, clear |
| NavigationMesh | add_polygon, clear_polygons, create_from_mesh, clear |
| MeshLibrary | create_item, set_item_mesh, set_item_name, set_item_mesh_transform, set_item_shapes, set_item_navigation_mesh, remove_item, clear |
| NavigationRegion2D | bake_navigation_polygon |
| NavigationRegion3D | bake_navigation_mesh |

The table's one source is `crates/godot-mcp-domain/src/editing_methods.rs`; the server's
schema, the plugin and the headless helper all follow it, and a test holds this page to it.

**Arguments** are tagged values, at most 16, checked against the method's signature in the
running Godot build — their number and their types — before the call. `{ "type":
"new_resource", "value": { "class": "AnimationNodeAnimation", "properties": { "animation": {
"type": "string_name", "value": "run" } } } }` passes a new resource the scene then embeds, with
initial properties as tagged values. Its class is any resource class the running Godot can
create, an engine class or a global script class, except Script, GDExtension and the classes
that inherit them; the engine that applies the edit refuses a class it does not have. Later
operations reach the new resource through a `resource` selector or a property path. A
`resource` value refers to a resource file, which the call does not change.

**Repeat.** `repeat`, a list of argument lists, calls the method once per list, in order, in
place of `arguments`: at most 4096 calls, recorded as one change and one undo step. A batch is
at most 1 MiB of compact JSON. One `set_cell` argument list of three tagged values is 116
bytes, so a `repeat` of 4096 of them is 479,299 bytes, measured as compact JSON; longer
argument lists reach the byte limit first.

**The answer** carries `results`: one `{index, method, value}` per call, in batch order, with
the call's return value as a tagged value, or `null` for a method that returns nothing; a
`repeat` answers `{index, method, values}`, one value per call. A bake also answers `baked`:
the resource's `class` and the `polygons` and `vertices` the bake left in it, and, for a
NavigationPolygon, its `outlines`.

**Baking.** NavigationRegion2D's `bake_navigation_polygon` and NavigationRegion3D's
`bake_navigation_mesh` rebuild the navigation polygon or mesh the region holds from the nodes
the engine parses for it. The call is always made with `on_thread` false, the only value the
argument may take, so the batch goes on only once the engine has finished baking; a bake on a
thread would return before the result exists. It is called on the region itself, once: a
`resource` selector or `repeat` is refused, and so is a region that holds no navigation
resource, one saved in its own file, which the bake would change too, and, in the editor, one
another property of the scene also holds. The engine parses only a scene inside the running
SceneTree. In the editor that is the scene as the editor holds it. A headless batch bakes in a
copy of the whole scene added to the helper's tree, with groups but without scripts or signal
connections, so none of the scene's scripts runs and a script that builds geometry at run time
adds none; the bake writes into the scene's own resource, and the copy is freed before the
scene is saved. Such a batch runs once the helper's tree is running, after the project's
autoloads have started, as they start in every headless run of the helper.
In the editor the bake is made on a rebuilt copy of the navigation resource, which the region
then holds, and undo gives it the untouched original back. A bake whose `polygons` is 0 found
no geometry to bake from: check the resource's parsed geometry type and the region's children.

**Undo.** In the editor a call is undone by putting its receiver's stored properties back: the
ones the call changed are written back, or, for Animation, Theme, AnimationMixer and
TileMapLayer, the receiver is emptied and every stored property written back in order. Those
four are the classes whose calls add or remove stored properties, or whose stored order a plain
write does not restore: an animation's tracks, a theme's items, an AnimationMixer's libraries
from Godot 4.6.1 on, and a layer's cells. A TileMapLayer keeps an erased cell in its stored
data until its next update, so it is brought up to date after the calls, undo and redo. A
call that changes which properties a written-back receiver stores is refused as
`not_undoable` before anything is committed.

The graph resources — AnimationNodeStateMachine, AnimationNodeBlendTree, the two blend spaces,
VisualShader, TileSet, TileSetAtlasSource and MeshLibrary — add, rename and remove entries that
no write takes away again, so a call on one is not made on it at all. The editor rebuilds the
outermost embedded resource the call reached through the node's property, setting the stored
properties of every resource it embeds in the order loading its file would, makes the call on
the copy, and gives the node the copy; undo gives the node the untouched original back. A write
that adds a stored property, such as `tree_root:states/Run/node`, is made the same way. Since a
copy given to one node property would part a resource the scene holds in two places, such an
edit is refused as `shared_resource` when another property of the scene holds the resource or
one it embeds, and names that property. A headless batch changes the resource itself.

## Reading calls

Some facts a file holds are not one stored property a parser can show: which cells of a
`TileMapLayer` are painted, the keys of an animation track, the states of a state machine, the
polygons a baked navigation mesh holds, the uniforms a shader declares. `scene.read` and
`resource.read` ask the engine. Each call names a method of the table below, and the answer is
what the engine returned:

```json
{ "path": "levels/one.tscn",
  "calls": [{ "node": "Ground", "method": "get_used_cells" },
            { "node": "Player", "resource": { "animation": "walk" },
              "method": "track_get_key_count",
              "arguments": [{ "type": "integer", "value": 0 }] }] }
```

**The receiver** of a `scene.read` call is a node of the `.tscn` scene, `.` by default; of a
`resource.read` call, the `.tres` resource. A `resource` selector calls a resource instead, as
in an editing call: `{ "property": "navigation_mesh" }` or a property path through the
resources each property holds, `{ "animation": "walk" }` an animation of an AnimationPlayer,
an AnimationTree or, in `resource.read`, an AnimationLibrary, and `{ "library": "" }` an
animation library of a player or tree. Since a read changes nothing, the resource may also be
one saved in its own file.

**The methods** are this table, matched by class and inheritance. Each only reads and returns a
plain value; none returns a live object.

| Class | Reading methods |
| --- | --- |
| TileMapLayer | get_cell_source_id, get_cell_atlas_coords, get_cell_alternative_tile, get_used_cells, get_used_cells_by_id, get_used_rect |
| GridMap | get_cell_item, get_cell_item_orientation, get_used_cells, get_used_cells_by_item |
| Line2D | get_point_count, get_point_position |
| Skeleton3D | get_bone_count, get_bone_name, find_bone, get_bone_parent, get_bone_rest |
| AnimationMixer | get_animation_library_list, get_animation_list, has_animation, has_animation_library |
| AnimationLibrary | get_animation_list, has_animation |
| Animation | get_track_count, find_track, track_get_path, track_get_type, track_is_enabled, track_get_interpolation_type, track_get_interpolation_loop_wrap, value_track_get_update_mode, track_get_key_count, track_get_key_time, track_get_key_value, track_get_key_transition, track_find_key, get_length, get_loop_mode, get_step |
| SpriteFrames | get_animation_names, has_animation, get_frame_count, get_frame_duration, get_animation_speed, get_animation_loop |
| Curve | get_point_count, get_point_position, get_point_left_tangent, get_point_right_tangent, sample |
| Curve2D | get_point_count, get_point_position, get_point_in, get_point_out, get_baked_length |
| Curve3D | get_point_count, get_point_position, get_point_in, get_point_out, get_point_tilt, get_baked_length |
| Gradient | get_point_count, get_offset, get_color, sample |
| Theme | get_type_list, get_color_list, get_constant_list, get_font_size_list, get_font_list, get_icon_list, get_stylebox_list, get_color, get_constant, get_font_size, has_color, has_constant, has_font_size, has_font, has_icon, has_stylebox |
| AnimationNodeStateMachine | get_node_list, has_node, get_node_position, get_transition_count, get_transition_from, get_transition_to, get_graph_offset |
| AnimationNodeBlendTree | get_node_list, has_node, get_node_position, get_graph_offset |
| AnimationNodeBlendSpace1D | get_blend_point_count, get_blend_point_position |
| AnimationNodeBlendSpace2D | get_blend_point_count, get_blend_point_position, get_triangle_count, get_triangle_point |
| VisualShader | get_node_list, get_node_position, get_node_connections, get_valid_node_id |
| TileSet | get_source_count, get_source_id, has_source, get_physics_layers_count, get_terrain_sets_count, get_terrains_count, get_terrain_name, get_navigation_layers_count, get_custom_data_layers_count, get_custom_data_layer_name |
| TileSetAtlasSource | get_tiles_count, get_tile_id, has_tile, get_atlas_grid_size, get_alternative_tiles_count |
| NavigationPolygon | get_polygon_count, get_polygon, get_vertices, get_outline_count, get_outline |
| NavigationMesh | get_polygon_count, get_polygon, get_vertices |
| MeshLibrary | get_item_list, get_item_name, get_last_unused_item_id |
| Mesh | get_surface_count, get_aabb |
| Shader | get_mode, get_shader_uniform_list |

The table's one source is `crates/godot-mcp-domain/src/reading_methods.rs`; the server's schema
and the headless helper follow it, and a test holds this page to it.

**Arguments** are tagged values that refer to nothing, at most 16, checked against the method's
signature in the running Godot build before the call. A read takes at most 64 calls, and at
most 256 KiB of compact JSON for all of them.

**The answer** carries `calls`: one `{index, method, value}` per call, in order, with the
returned value as a tagged value. A call that cannot be made answers `value` `null` and an
`error` with a `kind` and a `detail`, and the other calls are still made: `not_found` for a
node, animation or property the file does not have, `invalid_input` for a method the
receiver's class does not list or arguments its signature refuses, `unsupported` for a method
the running Godot build does not have, and `too_large` for a value whose text takes more than
262,144 bytes, with its size in `value_bytes`. A value that holds an object answers its text in
`literal` and why it has no tagged value in `value_reason`. `AnimationNodeStateMachine` and
`AnimationNodeBlendTree` have no `get_node_list` on Godot 4.4.1, measured; the other methods
are present on 4.4.1, 4.5.1, 4.6.1 and 4.7.1.

Each call also answers `diagnostics`, the engine errors and warnings it raised, such as the one
for a track index out of range, and `diagnostics_complete`, which is false when the engine's
captured output was cut off before the call ended, so more may have been raised. What the
engine reported outside every call, while loading the file or starting the autoloads, is the
answer's own `diagnostics`; `output_truncated` says whether the output hit its cap. The answer
also carries the file's `sha256`.

**What runs.** A supervised headless Godot process loads the file and makes the calls; nothing
is staged, published or saved. Like every run of the headless helper it starts the project's
autoloads, and instancing a scene runs its scripts' `_init` and property setters; the scene
never enters the tree, so `_ready` never runs. That is why the two tools belong to the runtime
and full profiles, beside `script.check`, and not to the editor profile, which reads through the
connected editor instead. The file's bytes are
read before and after: a file that changed meanwhile is refused as `conflict`, since the answer
could describe either version, and an answer is given only when the helper's process ended
cleanly and nothing it started outlived it; otherwise the call is `uncertain`.

### Reading in the editor

`editor.scene.read` makes the same calls on the scene the connected editor is editing, or on the
open `scene` when it is the current one, as the editor holds it, unsaved edits included. It takes
`calls`, as above, and an optional `scene` in place of `path`:

```json
{ "calls": [{ "node": "Ground", "method": "get_used_cells" }] }
```

The add-on makes the calls on the editor's main thread and answers the same `calls`, each value
read through the same parser as a headless read's, with the scene's semantic `digest` and
`unsaved` in place of the file's `sha256`, and `backend` `editor`. A read adds no undo history and
marks nothing unsaved. A `Gradient` sorts its points when it is read, so the add-on reads a
`Gradient` through a copy; the scene's digest is taken before and after the calls, and a read
that changed it anyway answers `internal` with no value.

Each call's `diagnostics` are the errors and warnings the editor logged while the call ran, read
from the add-on's log capture and parsed as a headless read parses the engine's output. Godot
4.5.1 and later hand their log to a plugin; an entry another editor thread logged at the same
moment would be counted with the call, and nothing the call raised is left out. Godot 4.4.1 does
not, so each of its calls answers `diagnostics_complete` `false`, and the errors it raises appear
only in the editor's Output panel. A call answers at most 64 log entries and says
`diagnostics_complete` `false` past them.

`editor.scene.read` belongs to the editor and full profiles: it starts no process. An answer from
the add-on that does not answer each call asked, in order, is refused as `internal`, and an add-on
that predates the tool answers `unsupported` and says to update it.

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

## Resource previews

`editor.resource.preview` asks the editor for its preview of the resource at `path`, the image
its FileSystem dock shows, and for the small thumbnail, waits up to `timeoutMs` (10000 by
default, from 100 to 60000), and answers each as a PNG artifact read back and checked as a
capture is: `preview` and `thumbnail`, each with its width and height, or `thumbnail: null` when
the editor made none. Two identical images are one artifact.

- **It needs a window.** The editor makes previews with its renderer, so a headless editor makes
  none: on Godot 4.4.1, 4.5.1, 4.6.1 and 4.7.1 it answered no request within 600 frames, for
  textures, SVG images, materials, meshes, gradients, scenes, scripts or sounds. There the tool
  answers `unavailable` without asking.
- **What gets one.** A windowed editor, measured under Xvfb with the OpenGL renderer on the same
  four builds, previews textures, SVG images, materials, meshes, gradients, scripts and sounds,
  at most 64 by 64 pixels with the editor's default thumbnail size (a 48 by 32 texture got a 48
  by 32 preview). Textures, SVG images, materials and gradients also got a 16 by 16 thumbnail;
  meshes, scripts and sounds got none. Scenes written to disk outside the editor got no preview;
  whether a scene the editor saved itself gets one was not measured.
- **No image is guessed.** When no preview comes, or the editor answers an empty or
  single-colored image, which cannot be told from a placeholder, the answer is `unavailable`,
  never an image. A file that does not exist answers `not_found`.

## Cameras

`editor.camera.get` reads what the editor's viewports show, changing nothing: the main screen
shown; the 2D view, its `zoom`, the point at its `center` and its canvas `transform`; and each of
the four 3D viewports, its `size`, whether it is `shown` on screen, and the `camera` the editor
draws it with, with its global `transform`, its `position`, the unit `forward` direction it looks
in, its `projection` (`perspective`, `orthogonal` or `frustum`), `fov`, `size`, `near` and
`far`. `view` (`2d` or `3d`) reads one of the two, and `viewport` (0 to 3) one 3D viewport.

`editor.camera.set` moves the camera of 3D viewport `viewport` (0 by default) to `position`,
looking at `target` with the view's top up, and answers the camera read back five frames later,
or `uncertain` when the read-back differs. The editor then takes the camera's place as its own
view: on Godot 4.5.1, 4.6.1 and 4.7.1 the camera was still where it was moved 120 frames later
and after switching screens.

- **The viewport must be on screen.** The editor drops a camera moved while its viewport is
  hidden as soon as it shows it again, so a hidden viewport is refused with `conflict`. Show the
  3D main screen with `editor.main_screen.set`, in a layout that shows the viewport; the default
  layout shows viewport 0 only.
- **Only the editor's own camera moves.** A camera of the edited scene is refused with
  `conflict`; the scene changes only through its edits.
- **Godot 4.4.1 moves none.** It keeps its orbit pivot where the camera was and glides the camera
  to 4.0 units behind the point asked for, so there the add-on does not offer the method and the
  tool answers `unsupported`.
- **The 2D view is read, not moved.** The 2D editor writes its view from its own state each time
  it updates it, so a plugin's change lasts only until then, on all four builds.

## Closing a script tab

`editor.script.close` closes the script editor's tab of the GDScript file at `path` and answers
the scripts still open, read back. Godot's `ScriptEditor.close_file` reloads a tab with unsaved
edits from its file before closing it, which discards them, so a tab the editor reports unsaved
is refused with `conflict` and nothing is closed: save the edits in the editor, or undo them,
first. A script that is not open answers `not_found`. Nothing is written. Closing needs
`close_file` and `get_unsaved_files`, which Godot 4.7.1 gives a plugin and 4.4.1, 4.5.1 and 4.6.1
do not, so there the tool answers `unsupported`.

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

## Running a script in the editor

`editor.script.run` compiles caller-written GDScript in the editor and calls its `run()` on the
editor's main thread, awaiting it when it is a coroutine
([ADR 0012](../contributor/adr/0012-declared-script-execution.md)). It runs nothing until the
operator starts the server with `--allow-scripts`; until then it answers `permission_denied`,
and the session's capabilities do not offer the plugin the method, which the plugin also
checks before it compiles anything.

- **It runs as a tool script.** `@tool` is written in front of the source's first line when the
  source does not carry it, so its line numbers stay the same, and the editor's API is in reach:
  `EditorInterface`, the open scenes, the file system. Compiling it runs its static
  initializers. A source extending Node is added under the plugin first and released after.
- **It answers what happened.** What `run()` returned, as a tagged value and as Godot's own
  spelling; what the editor logged while the source compiled and ran, its errors with the line
  of the source, and what it printed; and every project file that differs from before the run.
  On Godot 4.4.1 the plugin cannot read the editor's log, so a run that returned is
  `inconclusive`: an error inside `run()` cannot be seen.
- **Nothing confines it, and nothing is undone.** The code runs with your privileges; what it
  changes in a scene is not in the editor's undo history, and what it writes stays.
- **A deadline ends the wait, not the code.** The server waits `deadline_ms` (60 s by default,
  600 s at most) and a further two seconds. A run not answered by then is `uncertain`; the
  editor may still be running it, and since it may still save files the transaction covers the
  whole project: every change is refused, as after an unanswered
  [rescan](#rescanning-the-project), until the editor's answer settles it when the run ends
  ([Late editor answers](recovery.md#late-editor-answers)) or an operator resolves it. Only
  closing the editor ends the code.
- **It holds the editor.** Requests run one at a time, so the editor's other requests wait
  while a run awaits. Code that holds the main thread past the run's deadline leaves the editor
  silent, which can end the session
  ([Editor bridge protocol](../protocol/bridge-protocol.md)); the run's answer then comes on the
  next session.
- **The panel's pause holds it back** like any other mutation; see
  [Pausing agent edits](#pausing-agent-edits).

## Calling a project command

`editor.commands.call` calls a command the project declares in `godot_mcp_commands.json`
([Project commands](project-commands.md), [ADR 0016](../contributor/adr/0016-project-commands.md)).
The server checks the call against the declaration and the arguments against the command's
schema, and sends the add-on the provider's `res://` script and the arguments as tagged values.
`editor.commands.list` shows what can be called; it reads the file and needs no editor.

- **The add-on checks the request again.** It refuses, with nothing loaded, a request on a session
  whose capabilities did not offer the method, a script path that is not a `res://` path to a
  `.gd` file without empty or dot-leading segments, arguments that are not a tagged dictionary,
  and a script that no longer exists or no longer carries `@tool`. Such an answer is
  `not_started`, and nothing is blocked.
- **It loads the provider fresh.** The script is read from its file, not from the editor's cache,
  so an edited provider takes effect at the next call; loading it runs its static initializers.
  A provider extending Node is added under the plugin while it runs and freed after. Its
  `mcp_call(command, args)` is awaited when it is a coroutine.
- **It answers how far the call got**: the script did not compile, defines no `mcp_call`,
  answered without a boolean `ok`, or answered, with the value in Godot's own spelling or the
  error text; what the editor logged meanwhile; and every project file that differs from before
  the call. On Godot 4.4.1 the plugin cannot read the editor's log, so a command that answered
  `ok: true` is `inconclusive`.
- **The rest is as for a script.** The code runs with your privileges and nothing confines it;
  nothing it does is undone; a call not answered within `deadline_ms` and two seconds more is
  `uncertain` and blocks every change until it is resolved; it holds the editor while it runs;
  and the panel's pause holds it back.
- **An older add-on does not have it.** Add-on 0.5.0 serves the method; with an earlier one the
  tool answers `unsupported` and says to run `editor.addon.update` and then `editor.addon.reload`.

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

## Dialogs

`editor.dialogs.get` lists the dialogs the editor shows, at most `limit` (32 by default, 64 at
most), with their `count` and whether the list is `truncated`: every visible accept,
confirmation or file dialog anywhere in the editor's tree. Each has an `id`, its `class`,
`title` and `text` (the text of the labels it shows when it has no dialog text of its own),
whether it is `exclusive`, and its visible `buttons`, each with a `name`, a `role` (`ok`,
`cancel` or `custom`), its `text` and whether it is `disabled`. A custom button is named by the
action it reports to the dialog: the dialog that asks about a missing dependency, for one, offers
`cancel` (Close) and `ok` (Open Anyway), and on Godot 4.4.1 and 4.5.1 also `fixdeps` (Fix
Dependencies). The `id` is the dialog's object id, the same while the dialog exists and never
given to a later one.

`editor.dialog.respond` presses the button `button` of the dialog with that `dialog` id, as a
click does: the button emits `pressed`, and the dialog does what it does for a click. The answer
gives the dialog's title and text, whether it `closed`, and the dialogs shown afterwards; a custom
action can leave its dialog open, and the answer says so. An id that is no longer a visible
dialog, or a button the dialog does not have, is refused with `not_found`, and a disabled button
with `conflict`, with nothing pressed. Nothing answers a dialog by itself: each press is one call.

- **The button does what the editor does.** It can save, discard or delete project files, or
  quit or restart the editor, so a press is journaled as covering the whole project, and a lost
  answer blocks writes until it is resolved.
- **It is pressed from outside the plugin.** A helper node of the add-on presses it, so a button
  that makes the editor unload its plugins, as Reload Current Project does, finds none of the
  plugin's code running.
- **The editor opens dialogs of its own.** In a headless editor, opening a scene it cannot parse
  shows an alert naming the file, and opening a scene whose dependency is gone asks what to do;
  both are listed and answered like any other.

## Restarting the editor

`editor.restart` restarts the connected editor as its Reload Current Project does
([ADR 0013](../contributor/adr/0013-editor-restart.md)), and answers once the add-on of the
restarted editor, a different process, connects: with `restarted`, `previous_pid`, `pid`,
`waited_ms`, and what it saved first. It waits up to `timeoutMs`: 120000 by default, from 1000 to
600000. When no new editor connects by then, the answer is `uncertain`, naming the process asked
and whether it is still connected. The restarted editor reopens the scene that was being edited.

- **Nothing unsaved is lost.** An open scene or script the editor reports unsaved refuses the
  restart with `conflict`, naming it, unless `save` is set. With `save`, the open scenes are saved
  first, each proven as [Saving](#saving) describes, and on Godot 4.7.1 the script tabs too; on
  4.4.1, 4.5.1 and 4.6.1 a plugin cannot save a script tab, so an unsaved one refuses a saving
  restart with `unsupported`. A scene that was never saved, a running game and a visible dialog
  refuse it as well.
- **A question is cancelled.** What the editor still finds unsaved when it is asked, a script
  tab an older build cannot report or a resource changed in the inspector, makes it ask instead of
  restarting. The add-on cancels that question, so the question saves and discards nothing, and
  the call answers `conflict`, quoting it and naming what `save` wrote before the editor was
  asked; `editor.status.get` counts the declines under `restart`.
- **It must come back.** Godot passes on to the editor it restarts only the options it marks
  forwardable, and `--headless` is not one of them: an editor started with it restarts into one
  that finds no display and exits, on Godot 4.4.1, 4.5.1, 4.6.1 and 4.7.1. So an editor started
  with `--headless` is refused with `unsupported`. `--display-driver headless` is passed on, and
  an editor started with `--display-driver headless --audio-driver Dummy` restarts headless on all
  four; one with a window restarts with a window. Only Linux tells a plugin which options started
  its process, so elsewhere a headless editor is refused too.
- **Not a managed session.** An editor this server runs with `editor.session.start` is refused
  with `conflict`: the restarted editor would be a new process outside the session's process
  boundary. Stop the session with `editor.session.stop` and start a new one.
- **Nothing else meanwhile.** Until the restarted editor connects, every other editor change is
  refused with `unavailable`, and nothing writes the project's files: the editor that exits may
  still write them, and the restarted one is loading them.

A restart is journaled: covering the whole project with `save`, the editor's state alone without
it. The add-on says which process it runs in when it connects; an add-on that does not cannot be
told apart from a managed session, so its restart answers `unavailable`.

## Performance monitors

`editor.performance.get` reads the editor process's own performance monitors, as
`runtime.performance.get` reads a game's: every monitor of the build's `Performance.Monitor` enum
by its name there (`TIME_FPS`, `OBJECT_NODE_COUNT`, ...), and the custom monitors added in the
editor process, or only the named `monitors`. The set differs between builds, 39 monitors on
Godot 4.4.1 and 59 on 4.5.1, 4.6.1 and 4.7.1, so every value is answered under its name and an
unknown name is refused with `invalid_input`; a value that is not a finite number is `null`.

## Three outcomes, and no replay

Every editor mutation ends in exactly one of three states.

- `committed` — the editor applied it; the journal records the editor's receipt.
- `not_started` — nothing changed; the call answers the editor's own error.
- `unresolved` — the request reached the editor and what it changed is not known: no answer came
  back, because the connection dropped or the deadline passed, or a save it made is not proven.
  The call answers `uncertain` and the change is **never sent again**, not on retry and not after
  the editor reconnects.

An unresolved change that may have saved a file is quarantined, and writes stay blocked until it
is resolved; see [Recovery](recovery.md). A change the editor never answered can be settled once
the editor process it was sent to has ended, when the server identified that process; see
[An editor that ended before it answered](recovery.md#an-editor-that-ended-before-it-answered).
`editor.scene.save`, `editor.filesystem.scan`, `editor.play.start`, `editor.dialog.respond`,
`editor.restart` with `save`, `editor.script.run` and `editor.commands.call` may also write files
nobody can list in advance, so their transactions cover the whole project. An unresolved change
to editor state alone — selection, the open scene, the main screen, an unsaved undo, a stop, a
closed scene or script tab, a moved camera, an editor setting, a shortcut, a restart without
`save` — touches no project file, so it blocks nothing.

## Inside the editor

- The plugin polls its socket and runs every request from the editor main thread, one at a time;
  work requested from any other thread is refused with `not_started`.
- Requests still queued when the connection is lost are dropped, never run later.
- Disabling the plugin removes its dock, its bottom panel, its bridge node and its logger.
  Enabling, reloading and disabling it leave the editor's node count and orphan-node count
  exactly as they were before.

## Limits

- `editor.scene.tree.get`, `editor.node.get`, `editor.scene.read` and `editor.selection.set`
  work on the scene being edited; another open scene must first be made current with
  `editor.scene.open`.
- `editor.node.get` lists at most 512 properties, or any 256 named in `properties`, 256 children,
  256 signals with 64 connections each and 1 MiB of values; past that it says `truncated`, and a
  property it names without a value can be read by naming it.
- On Godot 4.4.1, 4.5.1 and 4.6.1, `unsaved` for an open scene is known only for scenes the
  plugin saw opened or saved, and for an open script only when its source differs from its file
  or the plugin last loaded that tab itself, as `editor.script.open` does; otherwise it is
  `null`, and a headless write to that path is refused. A script tab opened another way and then
  edited reads `null` until the editor next validates the tab's text, which applies it to the
  script, and `true` after: those builds give a plugin no way to ask the script editor which tabs
  are unsaved. Godot 4.7.1 reports both directly.
- `editor.dialogs.get` lists at most 64 dialogs, 32 by default, with at most 16 buttons each; a
  title is cut at 512 characters and a text at 4096, which `text_truncated` says.
  `editor.performance.get` names at most 256 monitors.
- No typed tool calls an editor method outside the table above; `editor.script.run` runs what the
  caller writes, only while the operator allows scripts, and `editor.commands.call` runs what the
  project declares, unless the operator turned project commands off.

## What the add-on does not do

Each of these has another way to it, or is left to you on purpose.

| Not offered | The way to it |
| --- | --- |
| Editor settings that start programs, hold paths, hosts or credentials, or rescan on their own | Change them in the editor's Editor Settings; see [The editor's settings](#the-editors-settings). |
| Shortcuts on Godot 4.4.1 and 4.5.1 | Change them in the editor's Editor Settings, under Shortcuts; Godot 4.6.1 and later name them to a plugin. |
| Restarting an editor started with `--headless`, or a managed editor session | Start the editor with `--display-driver headless` instead, which a restart keeps; stop a managed session with `editor.session.stop` and start a new one. See [Restarting the editor](#restarting-the-editor). |
| Answering the question an editor asks before it restarts | Answer it in the editor: `editor.restart` cancels it and restarts nothing. |
| Answering dialogs on their own | Each answer is one `editor.dialog.respond` call, naming the dialog and the button. |
| Moving the 2D view, or a 3D camera on Godot 4.4.1 | Pan and zoom in the editor: the 2D editor writes its view from its own state, and Godot 4.4.1 moves a camera a plugin placed. |
| Closing a script tab on Godot 4.4.1, 4.5.1 and 4.6.1 | Close it in the editor; those builds give a plugin no way to close one. |
| Resource previews in a headless editor | Preview in an editor with a window. |
| The Asset Library | Fetch the asset yourself, add it with `asset.put`, and import it with `editor.filesystem.scan` and `reimport`. |
| Version control | Use git, or your version control system, on the project directory. |
| Breakpoints in the editor's debugger | Debug in a managed editor session: `editor.session.start`, then `debug.command`. See [Managed sessions](managed-sessions.md). |
| Reading the game the editor runs | Run the game in a managed session, `project.run.start`, which reads its tree, captures its screen and sends it input. |
| Closing a scene on Godot 4.4.1 | Close it in the editor; Godot 4.4.1 cannot close a scene from a plugin. |
| Discarding a scene's unsaved changes | Undo them with `editor.undo`, then close the scene; a close never discards. |
| More than one editor | One editor connects to a server; run one server for each project. |
| Running a script while the operator has not allowed scripts | Start the server with `--allow-scripts`; see [Running a script in the editor](#running-a-script-in-the-editor). |
| Calling project code the project does not declare, or any while project commands are off | Declare the command in `godot_mcp_commands.json`, and leave `project_commands` on; see [Project commands](project-commands.md). |

## Verified on

Godot 4.4.1, 4.5.1, 4.6.1, 4.7.1 and 4.7.2, official Linux x86_64 builds, by the live suites
`crates/godot-mcp-server/tests/live_editor_bridge.rs` and, for property paths, graph-resource
calls, `repeat`, global script classes, imported models and reading in the editor,
`crates/godot-mcp-server/tests/live_authoring.rs`, for scripts
`crates/godot-mcp-server/tests/live_editor_script.rs`, for project commands
`crates/godot-mcp-server/tests/live_editor_commands.rs`, and for cameras, script tabs, dialogs,
restarts, performance monitors and previews `crates/godot-mcp-server/tests/live_editor_more.rs`,
through the production MCP server: headless editors, and windowed ones drawn by the OpenGL
renderer under Xvfb for saves, captures, previews and running the project. `make godot-test` runs these suites against each locked Linux editor, and
the display check runs their windowed cases too. Windows and macOS editors: not measured.
