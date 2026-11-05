# Recipes

Common setups — a character body, lights and a sky, particles, navigation, a sound, a shader
material, a theme, a HUD, a grid map, an on-screen joystick — written as the typed operations
every authoring tool takes. There is no setup tool per kind of node: a recipe is an `operations`
array, and the same array works with each of these:

* `scene.create`, with the recipe's root type, makes a new scene with the recipe in it;
* `scene.batch.apply` adds it to an existing scene, against the scene's SHA-256;
* `project.batch.apply` adds several recipes to several files in one transaction;
* `editor.scene.edit` adds it to the scene open in the connected editor, as one undoable action.

A recipe for a resource, the theme here, goes to `resource.create` or `resource.batch.apply`.

Every recipe below is applied, as it is on this page, by the test suite on each locked Godot
build ([Verified on](#verified-on)): created headless, read back, then changed by its tuning
batch in a live editor, undone, redone, saved, closed, reopened and read again. The values are
starting points; change them in the batch, not after it.

The operations, their fields and the tagged values they take are described in
[Editor bridge](editor-bridge.md#property-paths); the methods `call_method` may call are its
[editing calls](editor-bridge.md#editing-calls).

## A 2D character body

A `CharacterBody2D` on collision layer 2 that collides with layers 1 and 3, a capsule shape, and
a ray pointing down for ground checks. The root is a `Node2D`.

<!-- recipe id=body-2d path=scenes/player.tscn root=Node2D -->
```json
[
  { "op": "add_node", "parent": ".", "name": "Player", "type": "CharacterBody2D" },
  { "op": "set_property", "node": "Player", "property": "collision_layer", "value": { "type": "integer", "value": 2 } },
  { "op": "set_property", "node": "Player", "property": "collision_mask", "value": { "type": "integer", "value": 5 } },
  { "op": "add_node", "parent": "Player", "name": "Shape", "type": "CollisionShape2D" },
  { "op": "set_subresource", "node": "Player/Shape", "property": "shape", "class": "CapsuleShape2D",
    "properties": { "radius": { "type": "float", "value": 12.0 }, "height": { "type": "float", "value": 48.0 } } },
  { "op": "add_node", "parent": "Player", "name": "GroundRay", "type": "RayCast2D" },
  { "op": "set_property", "node": "Player/GroundRay", "property": "target_position",
    "value": { "type": "vector2", "value": { "x": 0, "y": 32 } } },
  { "op": "set_property", "node": "Player/GroundRay", "property": "collision_mask", "value": { "type": "integer", "value": 4 } }
]
```

Tuning it later changes the shape through a property path, without replacing it:

<!-- recipe-tune id=body-2d -->
```json
[
  { "op": "set_property", "node": "Player/Shape", "property": "shape:radius", "value": { "type": "float", "value": 14.0 } }
]
```

## A 3D physics body

A `RigidBody3D` with its own physics material, a box collision shape and a box mesh with a
material. The root is a `Node3D`.

<!-- recipe id=body-3d path=scenes/crate.tscn root=Node3D -->
```json
[
  { "op": "add_node", "parent": ".", "name": "Crate", "type": "RigidBody3D" },
  { "op": "set_property", "node": "Crate", "property": "mass", "value": { "type": "float", "value": 2.5 } },
  { "op": "set_subresource", "node": "Crate", "property": "physics_material_override", "class": "PhysicsMaterial",
    "properties": { "friction": { "type": "float", "value": 0.4 }, "bounce": { "type": "float", "value": 0.2 } } },
  { "op": "add_node", "parent": "Crate", "name": "Shape", "type": "CollisionShape3D" },
  { "op": "set_subresource", "node": "Crate/Shape", "property": "shape", "class": "BoxShape3D",
    "properties": { "size": { "type": "vector3", "value": { "x": 1.2, "y": 1.2, "z": 1.2 } } } },
  { "op": "add_node", "parent": "Crate", "name": "Mesh", "type": "MeshInstance3D" },
  { "op": "set_subresource", "node": "Crate/Mesh", "property": "mesh", "class": "BoxMesh",
    "properties": { "size": { "type": "vector3", "value": { "x": 1.2, "y": 1.2, "z": 1.2 } } } },
  { "op": "set_subresource", "node": "Crate/Mesh", "property": "material_override", "class": "StandardMaterial3D",
    "properties": {
      "albedo_color": { "type": "color", "value": { "red": 0.8, "green": 0.5, "blue": 0.2, "alpha": 1 } },
      "roughness": { "type": "float", "value": 0.7 } } }
]
```

<!-- recipe-tune id=body-3d -->
```json
[
  { "op": "set_property", "node": "Crate", "property": "physics_material_override:bounce",
    "value": { "type": "float", "value": 0.5 } }
]
```

## Lights, a sky and a camera

A sun with shadows, a warm point light, a `WorldEnvironment` whose `Environment` draws a
procedural sky, and the current camera. The sky is two levels down the environment; a property
path reaches it. The root is a `Node3D`.

<!-- recipe id=light-3d path=scenes/stage.tscn root=Node3D -->
```json
[
  { "op": "add_node", "parent": ".", "name": "Sun", "type": "DirectionalLight3D" },
  { "op": "set_property", "node": "Sun", "property": "rotation_degrees",
    "value": { "type": "vector3", "value": { "x": -50, "y": 30, "z": 0 } } },
  { "op": "set_property", "node": "Sun", "property": "light_energy", "value": { "type": "float", "value": 1.2 } },
  { "op": "set_property", "node": "Sun", "property": "shadow_enabled", "value": { "type": "bool", "value": true } },
  { "op": "add_node", "parent": ".", "name": "Lamp", "type": "OmniLight3D" },
  { "op": "set_property", "node": "Lamp", "property": "position",
    "value": { "type": "vector3", "value": { "x": 2, "y": 3, "z": 0 } } },
  { "op": "set_property", "node": "Lamp", "property": "omni_range", "value": { "type": "float", "value": 6.0 } },
  { "op": "set_property", "node": "Lamp", "property": "light_color",
    "value": { "type": "color", "value": { "red": 1, "green": 0.85, "blue": 0.6, "alpha": 1 } } },
  { "op": "add_node", "parent": ".", "name": "Environment", "type": "WorldEnvironment" },
  { "op": "set_subresource", "node": "Environment", "property": "environment", "class": "Environment",
    "properties": { "background_mode": { "type": "integer", "value": 2 } } },
  { "op": "set_subresource", "node": "Environment", "property": "environment:sky", "class": "Sky" },
  { "op": "set_subresource", "node": "Environment", "property": "environment:sky:sky_material", "class": "ProceduralSkyMaterial",
    "properties": { "sky_top_color": { "type": "color", "value": { "red": 0.3, "green": 0.5, "blue": 0.9, "alpha": 1 } } } },
  { "op": "add_node", "parent": ".", "name": "Camera", "type": "Camera3D" },
  { "op": "set_property", "node": "Camera", "property": "position",
    "value": { "type": "vector3", "value": { "x": 0, "y": 2, "z": 6 } } },
  { "op": "set_property", "node": "Camera", "property": "fov", "value": { "type": "float", "value": 60.0 } },
  { "op": "set_property", "node": "Camera", "property": "current", "value": { "type": "bool", "value": true } }
]
```

`background_mode` 2 is `Environment.BG_SKY`.

<!-- recipe-tune id=light-3d -->
```json
[
  { "op": "set_property", "node": "Sun", "property": "light_energy", "value": { "type": "float", "value": 0.8 } }
]
```

## Particles: a fire

`GPUParticles2D` with a `ParticleProcessMaterial` that sends particles up in a narrow cone, and a
colour ramp from yellow to transparent grey. It is a set of parameters, not a generated effect:
change the speed, spread and colours for smoke, sparks or rain. The root is a `Node2D`.

<!-- recipe id=particles-2d path=scenes/fire.tscn root=Node2D -->
```json
[
  { "op": "add_node", "parent": ".", "name": "Fire", "type": "GPUParticles2D" },
  { "op": "set_property", "node": "Fire", "property": "amount", "value": { "type": "integer", "value": 64 } },
  { "op": "set_property", "node": "Fire", "property": "lifetime", "value": { "type": "float", "value": 0.8 } },
  { "op": "set_subresource", "node": "Fire", "property": "process_material", "class": "ParticleProcessMaterial",
    "properties": {
      "direction": { "type": "vector3", "value": { "x": 0, "y": -1, "z": 0 } },
      "spread": { "type": "float", "value": 25.0 },
      "initial_velocity_min": { "type": "float", "value": 80.0 },
      "initial_velocity_max": { "type": "float", "value": 140.0 },
      "gravity": { "type": "vector3", "value": { "x": 0, "y": -40, "z": 0 } } } },
  { "op": "set_subresource", "node": "Fire", "property": "process_material:color_ramp", "class": "GradientTexture1D" },
  { "op": "set_subresource", "node": "Fire", "property": "process_material:color_ramp:gradient", "class": "Gradient",
    "properties": {
      "offsets": { "type": "packed_float32_array", "value": [0, 0.6, 1] },
      "colors": { "type": "packed_color_array", "value": [
        { "red": 1, "green": 0.9, "blue": 0.3, "alpha": 1 },
        { "red": 1, "green": 0.3, "blue": 0.1, "alpha": 0.8 },
        { "red": 0.2, "green": 0.2, "blue": 0.2, "alpha": 0 } ] } } }
]
```

<!-- recipe-tune id=particles-2d -->
```json
[
  { "op": "set_property", "node": "Fire", "property": "process_material:spread", "value": { "type": "float", "value": 40.0 } }
]
```

## Navigation: a floor and an agent

A `NavigationRegion2D` whose polygon is outlined and baked, and a character with a
`NavigationAgent2D` on the same navigation layers. The bake runs in the engine and the batch goes
on only once it finished, so the saved polygon holds the baked vertices. The root is a `Node2D`.

<!-- recipe id=navigation-2d path=scenes/arena.tscn root=Node2D -->
```json
[
  { "op": "add_node", "parent": ".", "name": "Floor", "type": "NavigationRegion2D" },
  { "op": "set_property", "node": "Floor", "property": "navigation_layers", "value": { "type": "integer", "value": 3 } },
  { "op": "set_subresource", "node": "Floor", "property": "navigation_polygon", "class": "NavigationPolygon",
    "properties": { "agent_radius": { "type": "float", "value": 8.0 } } },
  { "op": "call_method", "node": "Floor", "resource": { "property": "navigation_polygon" }, "method": "add_outline",
    "arguments": [ { "type": "packed_vector2_array", "value": [
      { "x": 0, "y": 0 }, { "x": 640, "y": 0 }, { "x": 640, "y": 360 }, { "x": 0, "y": 360 } ] } ] },
  { "op": "call_method", "node": "Floor", "method": "bake_navigation_polygon",
    "arguments": [ { "type": "bool", "value": false } ] },
  { "op": "add_node", "parent": ".", "name": "Walker", "type": "CharacterBody2D" },
  { "op": "add_node", "parent": "Walker", "name": "Agent", "type": "NavigationAgent2D" },
  { "op": "set_property", "node": "Walker/Agent", "property": "path_desired_distance", "value": { "type": "float", "value": 4.0 } },
  { "op": "set_property", "node": "Walker/Agent", "property": "target_desired_distance", "value": { "type": "float", "value": 8.0 } },
  { "op": "set_property", "node": "Walker/Agent", "property": "radius", "value": { "type": "float", "value": 12.0 } },
  { "op": "set_property", "node": "Walker/Agent", "property": "avoidance_enabled", "value": { "type": "bool", "value": true } },
  { "op": "set_property", "node": "Walker/Agent", "property": "navigation_layers", "value": { "type": "integer", "value": 3 } }
]
```

<!-- recipe-tune id=navigation-2d -->
```json
[
  { "op": "set_property", "node": "Walker/Agent", "property": "target_desired_distance",
    "value": { "type": "float", "value": 12.0 } }
]
```

## A positional sound

An `AudioStreamPlayer2D` playing a sound file of the project on the `SFX` bus. The file must be
imported (`project.import.run`, or the editor's own import) before a batch can refer to it, and
the bus must exist in the project's bus layout: a player keeps `Master` for a bus the layout
lacks, so the batch is refused as `value_not_kept`. The root is a `Node2D`.

<!-- recipe id=audio-2d path=scenes/footsteps.tscn root=Node2D -->
```json
[
  { "op": "add_node", "parent": ".", "name": "Steps", "type": "AudioStreamPlayer2D" },
  { "op": "set_property", "node": "Steps", "property": "stream",
    "value": { "type": "resource", "value": { "resource": "res://audio/step.wav", "resource_type": "AudioStreamWAV" } } },
  { "op": "set_property", "node": "Steps", "property": "volume_db", "value": { "type": "float", "value": -6.0 } },
  { "op": "set_property", "node": "Steps", "property": "max_distance", "value": { "type": "float", "value": 600.0 } },
  { "op": "set_property", "node": "Steps", "property": "bus", "value": { "type": "string_name", "value": "SFX" } }
]
```

<!-- recipe-tune id=audio-2d -->
```json
[
  { "op": "set_property", "node": "Steps", "property": "volume_db", "value": { "type": "float", "value": -3.0 } }
]
```

The bus itself is a property of the bus layout, `default_bus_layout.tres`, which
`resource.batch.apply` changes like any resource: `bus/1/name` set to `SFX` adds the bus.

## A shader material

A `ColorRect` drawn with a project shader, `res://shaders/tint.gdshader`, through a
`ShaderMaterial`, with two of its uniforms set through property paths. The shader file must exist
before a batch refers to it; `file.create` writes it:

<!-- recipe-file path=shaders/tint.gdshader -->
```glsl
shader_type canvas_item;

uniform float strength : hint_range(0.0, 1.0) = 0.0;
uniform vec4 tint : source_color = vec4(1.0);

void fragment() {
	COLOR = mix(COLOR, tint, strength);
}
```

A uniform is a property of the material named `shader_parameter/<uniform>`, so the path from the
node is `material:shader_parameter/<uniform>`. `shader.check` lists the uniforms a shader
declares. A control under a `Node2D` keeps its size as offsets, so the saved scene holds
`offset_right` and `offset_bottom` rather than `size`. The root is a `Node2D`.

<!-- recipe id=shader-2d path=scenes/tinted.tscn root=Node2D -->
```json
[
  { "op": "add_node", "parent": ".", "name": "Tint", "type": "ColorRect" },
  { "op": "set_property", "node": "Tint", "property": "size",
    "value": { "type": "vector2", "value": { "x": 64.0, "y": 64.0 } } },
  { "op": "set_subresource", "node": "Tint", "property": "material", "class": "ShaderMaterial",
    "properties": { "shader": { "type": "resource",
      "value": { "resource": "res://shaders/tint.gdshader", "resource_type": "Shader" } } } },
  { "op": "set_property", "node": "Tint", "property": "material:shader_parameter/strength",
    "value": { "type": "float", "value": 0.5 } },
  { "op": "set_property", "node": "Tint", "property": "material:shader_parameter/tint",
    "value": { "type": "color", "value": { "red": 1.0, "green": 0.5, "blue": 0.2, "alpha": 1.0 } } }
]
```

<!-- recipe-tune id=shader-2d -->
```json
[
  { "op": "set_property", "node": "Tint", "property": "material:shader_parameter/strength",
    "value": { "type": "float", "value": 0.8 } }
]
```

## A theme

A `Theme` resource with a label colour and size, an outline constant and a flat panel style.
These operations go to `resource.create` with `class` `Theme`.

<!-- recipe id=theme path=ui/theme.tres class=Theme -->
```json
[
  { "op": "call_method", "method": "set_color", "arguments": [
    { "type": "string_name", "value": "font_color" }, { "type": "string_name", "value": "Label" },
    { "type": "color", "value": { "red": 0.95, "green": 0.95, "blue": 0.95, "alpha": 1 } } ] },
  { "op": "call_method", "method": "set_font_size", "arguments": [
    { "type": "string_name", "value": "font_size" }, { "type": "string_name", "value": "Label" },
    { "type": "integer", "value": 20 } ] },
  { "op": "call_method", "method": "set_constant", "arguments": [
    { "type": "string_name", "value": "outline_size" }, { "type": "string_name", "value": "Label" },
    { "type": "integer", "value": 2 } ] },
  { "op": "call_method", "method": "set_stylebox", "arguments": [
    { "type": "string_name", "value": "panel" }, { "type": "string_name", "value": "Panel" },
    { "type": "new_resource", "value": { "class": "StyleBoxFlat", "properties": {
      "bg_color": { "type": "color", "value": { "red": 0.1, "green": 0.12, "blue": 0.16, "alpha": 0.9 } },
      "corner_radius_top_left": { "type": "integer", "value": 8 },
      "corner_radius_top_right": { "type": "integer", "value": 8 },
      "corner_radius_bottom_right": { "type": "integer", "value": 8 },
      "corner_radius_bottom_left": { "type": "integer", "value": 8 } } } } ] }
]
```

<!-- recipe-tune id=theme -->
```json
[
  { "op": "call_method", "method": "set_font_size", "arguments": [
    { "type": "string_name", "value": "font_size" }, { "type": "string_name", "value": "Label" },
    { "type": "integer", "value": 22 } ] }
]
```

## A HUD

A full-screen `Control` using the theme above, with a score label in the top-right corner and a
pause button in the bottom-right one. The label's colour and size override the theme for it
alone. The root is a `Control`.

Each control is anchored the way the editor saves it, in three operations: `layout_mode`, 1
(anchors) for a child and 3 (uncontrolled) for the scene's root; `set_anchors_and_offsets_preset`,
which moves the anchors and the offsets; and `anchors_preset`, which records the preset so the
inspector shows it. Without the first, `anchors_preset` does not take, and the batch is refused
as `value_not_kept`; without the last, the inspector shows the anchors as custom. Presets are the
`Control.LayoutPreset` values: 15 is full rect, 1 top right, 3 bottom right, 2 bottom left.

<!-- recipe id=ui path=ui/hud.tscn root=Control -->
```json
[
  { "op": "set_property", "node": ".", "property": "theme",
    "value": { "type": "resource", "value": { "resource": "res://ui/theme.tres", "resource_type": "Theme" } } },
  { "op": "set_property", "node": ".", "property": "layout_mode", "value": { "type": "integer", "value": 3 } },
  { "op": "call_method", "node": ".", "method": "set_anchors_and_offsets_preset", "arguments": [ { "type": "integer", "value": 15 } ] },
  { "op": "set_property", "node": ".", "property": "anchors_preset", "value": { "type": "integer", "value": 15 } },
  { "op": "add_node", "parent": ".", "name": "Score", "type": "Label" },
  { "op": "set_property", "node": "Score", "property": "text", "value": { "type": "string", "value": "Score: 0" } },
  { "op": "set_property", "node": "Score", "property": "horizontal_alignment", "value": { "type": "integer", "value": 2 } },
  { "op": "set_property", "node": "Score", "property": "theme_override_colors/font_color",
    "value": { "type": "color", "value": { "red": 1, "green": 0.9, "blue": 0.3, "alpha": 1 } } },
  { "op": "set_property", "node": "Score", "property": "theme_override_font_sizes/font_size",
    "value": { "type": "integer", "value": 28 } },
  { "op": "set_property", "node": "Score", "property": "layout_mode", "value": { "type": "integer", "value": 1 } },
  { "op": "call_method", "node": "Score", "method": "set_anchors_and_offsets_preset", "arguments": [ { "type": "integer", "value": 1 } ] },
  { "op": "set_property", "node": "Score", "property": "anchors_preset", "value": { "type": "integer", "value": 1 } },
  { "op": "add_node", "parent": ".", "name": "Pause", "type": "Button" },
  { "op": "set_property", "node": "Pause", "property": "text", "value": { "type": "string", "value": "Pause" } },
  { "op": "set_property", "node": "Pause", "property": "layout_mode", "value": { "type": "integer", "value": 1 } },
  { "op": "call_method", "node": "Pause", "method": "set_anchors_and_offsets_preset", "arguments": [ { "type": "integer", "value": 3 } ] },
  { "op": "set_property", "node": "Pause", "property": "anchors_preset", "value": { "type": "integer", "value": 3 } }
]
```

<!-- recipe-tune id=ui -->
```json
[
  { "op": "set_property", "node": "Score", "property": "theme_override_font_sizes/font_size",
    "value": { "type": "integer", "value": 32 } }
]
```

## A grid map

A `GridMap` with a mesh library of one box, and four cells painted with it. The library is
embedded in the scene; `call_method` with `resource` calls its methods. The root is a `Node3D`.

<!-- recipe id=gridmap-3d path=scenes/blocks.tscn root=Node3D -->
```json
[
  { "op": "add_node", "parent": ".", "name": "Grid", "type": "GridMap" },
  { "op": "set_property", "node": "Grid", "property": "cell_size",
    "value": { "type": "vector3", "value": { "x": 1, "y": 1, "z": 1 } } },
  { "op": "set_subresource", "node": "Grid", "property": "mesh_library", "class": "MeshLibrary" },
  { "op": "call_method", "node": "Grid", "resource": { "property": "mesh_library" }, "method": "create_item",
    "arguments": [ { "type": "integer", "value": 0 } ] },
  { "op": "call_method", "node": "Grid", "resource": { "property": "mesh_library" }, "method": "set_item_name",
    "arguments": [ { "type": "integer", "value": 0 }, { "type": "string", "value": "Block" } ] },
  { "op": "call_method", "node": "Grid", "resource": { "property": "mesh_library" }, "method": "set_item_mesh",
    "arguments": [ { "type": "integer", "value": 0 }, { "type": "new_resource", "value": { "class": "BoxMesh" } } ] },
  { "op": "call_method", "node": "Grid", "method": "set_cell_item", "repeat": [
    [ { "type": "vector3i", "value": { "x": 0, "y": 0, "z": 0 } }, { "type": "integer", "value": 0 } ],
    [ { "type": "vector3i", "value": { "x": 1, "y": 0, "z": 0 } }, { "type": "integer", "value": 0 } ],
    [ { "type": "vector3i", "value": { "x": 2, "y": 0, "z": 0 } }, { "type": "integer", "value": 0 } ],
    [ { "type": "vector3i", "value": { "x": 2, "y": 1, "z": 0 } }, { "type": "integer", "value": 0 } ] ] }
]
```

<!-- recipe-tune id=gridmap-3d -->
```json
[
  { "op": "call_method", "node": "Grid", "method": "set_cell_item", "arguments": [
    { "type": "vector3i", "value": { "x": 0, "y": 1, "z": 0 } }, { "type": "integer", "value": 0 } ] }
]
```

## An on-screen joystick

A `VirtualJoystick` in the bottom-left corner that follows the touch and drives four input
actions. The class exists in Godot 4.7.1 and not in 4.4.1, 4.5.1 or 4.6.1: there `add_node`
refuses the batch, naming the class, and nothing is written. On those builds a
`TouchScreenButton` per action is the nearest built-in node. The root is a `Control`.

<!-- recipe id=joystick path=ui/touch.tscn root=Control since=4.7.1 -->
```json
[
  { "op": "add_node", "parent": ".", "name": "Stick", "type": "VirtualJoystick" },
  { "op": "set_property", "node": "Stick", "property": "joystick_mode", "value": { "type": "integer", "value": 1 } },
  { "op": "set_property", "node": "Stick", "property": "deadzone_ratio", "value": { "type": "float", "value": 0.2 } },
  { "op": "set_property", "node": "Stick", "property": "action_left", "value": { "type": "string_name", "value": "move_left" } },
  { "op": "set_property", "node": "Stick", "property": "action_right", "value": { "type": "string_name", "value": "move_right" } },
  { "op": "set_property", "node": "Stick", "property": "action_up", "value": { "type": "string_name", "value": "move_up" } },
  { "op": "set_property", "node": "Stick", "property": "action_down", "value": { "type": "string_name", "value": "move_down" } },
  { "op": "set_property", "node": "Stick", "property": "layout_mode", "value": { "type": "integer", "value": 1 } },
  { "op": "call_method", "node": "Stick", "method": "set_anchors_and_offsets_preset", "arguments": [ { "type": "integer", "value": 2 } ] },
  { "op": "set_property", "node": "Stick", "property": "anchors_preset", "value": { "type": "integer", "value": 2 } }
]
```

`joystick_mode` 1 is dynamic: the joystick appears where the touch starts.

<!-- recipe-tune id=joystick -->
```json
[
  { "op": "set_property", "node": "Stick", "property": "deadzone_ratio", "value": { "type": "float", "value": 0.3 } }
]
```

## Verified on

Godot 4.4.1, 4.5.1, 4.6.1 and 4.7.1, official Linux x86_64 builds, by
`crates/godot-mcp-server/tests/live_recipes.rs` through the production MCP server, which reads
every recipe and tuning batch from this page. Each recipe is created headless with
`scene.create` or `resource.create` and its saved file and scene tree are checked, and
`scene.read` reads the grid map's cells and the navigation region's baked polygons back from the
engine; each scene's tuning batch is then applied with `editor.scene.edit` in a headless editor
running the add-on, undone, redone, saved with `editor.scene.save` and reloaded with
`editor.scene.open`, whose digest must equal the saved one, with no error in the editor's log;
the theme's tuning batch goes to `resource.batch.apply`. The joystick recipe is created on 4.7.1
and refused on 4.4.1, 4.5.1 and 4.6.1, naming `VirtualJoystick`, with nothing written. The four
builds took 246.5 s together with a debug build of the server. Windows and macOS editors: not
measured.
