---
description: Which godot-mcp tools reach animation, AnimationTree, physics, 3D, particles, navigation, audio, shaders, themes, TileMap and a running game.
---

# What it covers

This page lists, for each part of a Godot project, the tools that make and change it and the
tools that read it back or watch it in a running game. Every tool is in the `full` profile;
[Profiles and answers](profiles.md) says which smaller profile holds it.

Two words in the table stand for a group of tools:

- **Edit** is a `call_method` operation in `scene.batch.apply`, `editor.scene.edit` or
  `resource.batch.apply`, or in the operations of `scene.create` and `resource.create`. It
  calls one method of a fixed table, such as `add_track` on an `Animation`; the
  [editor bridge reference](../../operator/editor-bridge.md#editing-calls) lists every method
  each class takes.
- **Read** is a call of `scene.read`, `resource.read` or, for the scene your editor holds,
  `editor.scene.read`, which asks the engine for what only it knows, such as the painted cells
  of a tile map.

| Area | Make and change | Read back, run and observe |
| --- | --- | --- |
| Animation | Edit `AnimationPlayer`, `AnimationLibrary` and `Animation`: `add_animation`, `add_track`, `track_insert_key`, `set_loop_mode`; `SpriteFrames` for frame animations | Read `get_animation_list`, `track_get_key_count`, `track_get_key_value`; in a game, `runtime.signals.get` and `runtime.wait` |
| AnimationTree | `scene.recipe.apply` with `animation-state-machine` (states, transitions, a start state) or `blend-space-1d`; edit `AnimationNodeStateMachine` (`add_node`, `add_transition`), `AnimationNodeBlendTree` (`connect_node`) and the blend spaces (`add_blend_point`) | Read `get_node_list`, `get_transition_count`, `get_blend_point_count` |
| Physics | `scene.recipe.apply` with `body-2d` or `body-3d`: a character, rigid, static or area body with its shape; `set_subresource` for any shape; `TileSet` physics layers | `scene.tree.get`; in a game, `runtime.nodes.find` near a point, `runtime.properties.get`, `runtime.sample`, `runtime.time_scale.set`, and `runtime.frames.step` frame by frame |
| 3D | `scene.recipe.apply` with `body-3d`, `light-3d` (sun, lamp, sky, camera) or `gridmap-3d`; `resource.create` with the `material-3d` recipe for a `StandardMaterial3D`; edit `GridMap` (`set_cell_item`) and `MeshLibrary` (`create_item`) | Read `get_used_cells`, a mesh's `get_aabb`, a `Skeleton3D`'s bones; `editor.camera.get`, `editor.camera.set`, `editor.screenshot.capture` of a 3D viewport |
| Particles | `scene.recipe.apply` with `particles-2d` or `particles-3d` and a preset: fire, smoke, sparks, rain, snow or explosion; `set_subresource` for a `ParticleProcessMaterial` | In a game, `runtime.screenshot.capture`, `runtime.recording.capture`, `runtime.image.compare` |
| Navigation | `scene.recipe.apply` with `navigation-2d` or `navigation-3d`, which bake the floor; edit `NavigationPolygon` (`add_outline`) and bake with the regions' `bake_navigation_polygon` and `bake_navigation_mesh` | Read `get_polygon_count`, `get_vertices`; in a game, `runtime.navigation.path.get` and `runtime.node.move` along a path |
| Audio | `asset.put` and `project.import.run` for the sound, `scene.recipe.apply` with `audio-2d` or `audio-3d`, `resource.batch.apply` on the bus layout (`default_bus_layout.tres`) for its buses; `project.settings.patch` chooses another layout file | `asset.status`, `resource.tree.get` on the bus layout; in a game, `runtime.properties.get` on the player |
| Shaders | `file.create` for a `.gdshader`; `scene.recipe.apply` with `shader-2d` for a `ShaderMaterial` and its uniforms; edit `VisualShader` (`add_node`, `connect_nodes`) | `shader.check` for compile errors and uniforms; read `get_shader_uniform_list` |
| Themes | `resource.create` with the `theme` recipe, or editing a `Theme` (`set_color`, `set_font_size`, `set_constant`, `set_stylebox`) with `resource.create` or `resource.batch.apply`; `scene.recipe.apply` with `hud`; edit `Control` anchors | Read `get_color`, `get_type_list`; in a game, `runtime.ui.get` and `runtime.ui.press` |
| TileMap | Edit `TileSet` and `TileSetAtlasSource` (`add_source`, `create_tile`, terrains) and `TileMapLayer` (`set_cell`, `erase_cell`, `set_cells_terrain_connect`) | Read `get_used_cells`, `get_cell_source_id`, `get_cell_atlas_coords` |
| Runtime control | `project.run.start` and `project.run.stop`; `input.action.send`, `input.key.send`, `input.mouse.send`, `input.sequence.send`, `input.replay` | `runtime.tree.get`, `runtime.properties.get`, `runtime.properties.set`, `runtime.wait`, `runtime.ui.get`, `runtime.screenshot.capture`; `test.scenario.run` for a whole scenario |
| Project tooling | `editor.commands.call` runs a command the project declares in `godot_mcp_commands.json`, in the connected editor, with arguments checked against the command's schema | `editor.commands.list` lists the commands with their descriptions and argument schemas and runs nothing; a call answers what the command returned and every file it changed |

## Recipes

`scene.recipe.apply` adds a common setup to a scene by name, with its parameters, and answers
every value it used. It turns the recipe into one batch of the operations above, journaled and
checked against the scene's SHA-256, or, when your editor holds the scene, into one undoable
action. Fifteen recipes go into scenes: `body-2d`, `body-3d`, `light-3d`, `particles-2d`,
`particles-3d`, `navigation-2d`, `navigation-3d`, `audio-2d`, `audio-3d`, `shader-2d`, `hud`,
`gridmap-3d`, `joystick`, `animation-state-machine` and `blend-space-1d`. `joystick` needs
Godot 4.7.1 or 4.7.2, where the `VirtualJoystick` class exists. Two recipes make a resource
instead: `resource.create` takes `theme` or `material-3d` in its `recipe` field, in place of
`class` and `operations`. godot-mcp 0.7.0 and earlier have the first ten scene recipes and the
theme, whose operations go to `resource.create` with `class` `Theme`.

```json
{
  "path": "scenes/fire.tscn",
  "expectedSha256": "<the scene's current SHA-256>",
  "recipe": { "particles-2d": { "preset": "smoke" } },
  "idempotencyId": "<a new unique id>"
}
```

An agent finds the recipes without this page: each one is also an MCP prompt,
`godot-mcp.recipe.<name>`, in the `editor` and `full` profiles, with the recipe's parameters, a
call and the operations it becomes. A prompt takes no tool slot. The
[recipes reference](../../operator/recipes.md) shows every recipe's call, its operations and its
parameters.

## Everything else

The [tools reference](../../protocol/tool-inventory.md) lists all 133 tools with their profiles.
A part of Godot with no edit method or recipe here is still reachable: a batch sets a node's
stored properties with `set_property` and embeds resources in it with `set_subresource`, and,
once you start the server with `--allow-scripts`, `script.run`, `editor.script.run` and
`runtime.script.run` run GDScript an agent writes.
