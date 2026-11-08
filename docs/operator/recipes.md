# Recipes

Common setups by name: ten that `scene.recipe.apply` adds to a scene — a 2D or 3D physics body,
lights and a sky, particles, navigation, a sound, a shader material, a HUD, a grid map and an
on-screen joystick — and a theme, a resource that `resource.create` makes. A recipe is a named
batch of the typed operations every authoring tool takes, with parameters: the call names the
recipe and its parameters, the server expands them into operations and applies them as one
`scene.batch.apply` batch, journaled and against the scene's SHA-256, or in the connected editor
as one undoable action when the scene is open there. The answer names every parameter the
recipe used, defaults included, and carries the operations it applied.

Each recipe below shows a call, the operations that call becomes and the recipe's parameters.
The operations also work on their own:

* `scene.create`, with the recipe's root type, makes a new scene with the recipe in it;
* `scene.batch.apply` adds them to an existing scene, against the scene's SHA-256;
* `project.batch.apply` adds several recipes to several files in one transaction;
* `editor.scene.edit` adds them to the scene open in the connected editor, as one undoable
  action.

An agent finds the recipes without this page: each one is also an MCP prompt,
`godot-mcp.recipe.<name>` such as `godot-mcp.recipe.particles-2d`, listed in the `editor` and
`full` profiles. Its text holds the parameter table, a call template and the operations the
recipe becomes, generated from the same registry as the tool. Prompts take no tool slot.

The theme is a recipe for a resource rather than for nodes, so it goes to `resource.create` or
`resource.batch.apply`; `scene.recipe.apply` refuses it and names `resource.create`.

Every call on this page is applied, as it is written here, by the test suite on each locked
Godot build ([Verified on](#verified-on)), and a unit test checks that each call becomes exactly
the operations shown below it and that each table is the registry's.

## The call

`scene.recipe.apply` takes:

* `path`, the scene the recipe goes into. It must exist; `scene.create` makes an empty one.
* `expectedSha256`, the SHA-256 of the scene's current bytes, as `scene.tree.get` or
  `project.files.list` reports it. A scene that changed since is refused as a conflict.
* `recipe`, an object with one key, the recipe's name, holding its parameters. `{}` takes every
  default.
* `parent`, optional: the node the recipe's nodes go below, as a path relative to the scene
  root. The default is `.`, the root.
* `dryRun`, optional: answer the operations and the diff without writing anything.
* `idempotencyId`: a retry with the same identifier and arguments answers from the journal.

A parameter left out keeps the default its table names. `Godot's` means the recipe writes
nothing for it, so the node keeps Godot's own value. A parameter that does not apply to the
chosen variant, such as `height` for a circle, is refused rather than ignored, and so is a
value out of range or a parameter the recipe does not take. A name, where the recipe takes
one, names its main node; the other nodes have fixed names below it.

## A 2D physics body

A `CharacterBody2D`, `RigidBody2D`, `StaticBody2D` or `Area2D` with a collision shape named
`Shape` and, for ground checks, a `RayCast2D` named `GroundRay` pointing down. This call makes
a character body on collision layer 2 that collides with layers 1 and 3, with a capsule shape
and a ground ray. The scene's root is a `Node2D`.

<!-- recipe-call id=body-2d path=scenes/player.tscn root=Node2D -->
```json
{
  "path": "scenes/player.tscn",
  "expectedSha256": "<the scene's current SHA-256>",
  "recipe": {
    "body-2d": {
      "name": "Player", "shape": "capsule", "radius": 12, "height": 48,
      "collision_layer": 2, "collision_mask": 5,
      "ground_ray": true, "ray_length": 32, "ray_mask": 4
    }
  },
  "idempotencyId": "<a new unique id>"
}
```

It becomes these operations:

<!-- recipe id=body-2d -->
```json
[
  { "op": "add_node", "parent": ".", "name": "Player", "type": "CharacterBody2D" },
  { "op": "set_property", "node": "Player", "property": "collision_layer",
    "value": { "type": "integer", "value": 2 } },
  { "op": "set_property", "node": "Player", "property": "collision_mask",
    "value": { "type": "integer", "value": 5 } },
  { "op": "add_node", "parent": "Player", "name": "Shape", "type": "CollisionShape2D" },
  { "op": "set_subresource", "node": "Player/Shape", "property": "shape", "class": "CapsuleShape2D",
    "properties": { "height": { "type": "float", "value": 48.0 },
      "radius": { "type": "float", "value": 12.0 } } },
  { "op": "add_node", "parent": "Player", "name": "GroundRay", "type": "RayCast2D" },
  { "op": "set_property", "node": "Player/GroundRay", "property": "target_position",
    "value": { "type": "vector2", "value": { "x": 0.0, "y": 32.0 } } },
  { "op": "set_property", "node": "Player/GroundRay", "property": "collision_mask",
    "value": { "type": "integer", "value": 4 } }
]
```

<!-- recipe-parameters id=body-2d -->
| Parameter | Takes | Default | Applies when | Sets |
| --- | --- | --- | --- | --- |
| `name` | node name, 1 to 40 characters | Player, RigidBody, StaticBody or Area, by `body` | always | The body's name. |
| `body` | character, rigid, static, area | character | always | The body: CharacterBody2D, RigidBody2D, StaticBody2D or Area2D. |
| `shape` | capsule, circle, rectangle | capsule | always | The collision shape: CapsuleShape2D, CircleShape2D or RectangleShape2D. |
| `radius` | number above 0, at most 100000 | Godot's | `shape` is capsule or circle | The shape's radius, in pixels. |
| `height` | number above 0, at most 100000 | Godot's | `shape` is capsule | The capsule's height, in pixels. |
| `size` | {x, y} above 0 | Godot's | `shape` is rectangle | The rectangle's size, in pixels. |
| `collision_layer` | integer, 0 to 4294967295 | Godot's | always | The layers the body is on, as a bit field. |
| `collision_mask` | integer, 0 to 4294967295 | Godot's | always | The layers the body collides with, as a bit field. |
| `mass` | number above 0, at most 100000 | Godot's | `body` is rigid | The body's mass, in kilograms. |
| `ground_ray` | boolean | false | always | Add a RayCast2D named GroundRay pointing down, for ground checks. |
| `ray_length` | number above 0, at most 100000 | Godot's | `ground_ray` is true | How far down the ray reaches, in pixels. |
| `ray_mask` | integer, 0 to 4294967295 | Godot's | `ground_ray` is true | The layers the ray detects, as a bit field. |

Tuning it later changes the shape through a property path, without replacing it:

<!-- recipe-tune id=body-2d -->
```json
[
  { "op": "set_property", "node": "Player/Shape", "property": "shape:radius", "value": { "type": "float", "value": 14.0 } }
]
```

## A 3D physics body

A `RigidBody3D`, `StaticBody3D`, `CharacterBody3D` or `Area3D` with a collision shape named
`Shape` and a `MeshInstance3D` named `Mesh` of the same shape and size, drawn with its own
`StandardMaterial3D`. A rigid or static body gets its own `PhysicsMaterial` when `friction` or
`bounce` is given. This call makes a rigid crate. The scene's root is a `Node3D`.

<!-- recipe-call id=body-3d path=scenes/crate.tscn root=Node3D -->
```json
{
  "path": "scenes/crate.tscn",
  "expectedSha256": "<the scene's current SHA-256>",
  "recipe": {
    "body-3d": {
      "name": "Crate", "body": "rigid", "shape": "box",
      "size": { "x": 1.2, "y": 1.2, "z": 1.2 },
      "mass": 2.5, "friction": 0.4, "bounce": 0.2,
      "color": { "red": 0.8, "green": 0.5, "blue": 0.2, "alpha": 1 }, "roughness": 0.7
    }
  },
  "idempotencyId": "<a new unique id>"
}
```

It becomes these operations:

<!-- recipe id=body-3d -->
```json
[
  { "op": "add_node", "parent": ".", "name": "Crate", "type": "RigidBody3D" },
  { "op": "set_property", "node": "Crate", "property": "mass",
    "value": { "type": "float", "value": 2.5 } },
  { "op": "set_subresource", "node": "Crate", "property": "physics_material_override",
    "class": "PhysicsMaterial",
    "properties": { "bounce": { "type": "float", "value": 0.2 },
      "friction": { "type": "float", "value": 0.4 } } },
  { "op": "add_node", "parent": "Crate", "name": "Shape", "type": "CollisionShape3D" },
  { "op": "set_subresource", "node": "Crate/Shape", "property": "shape", "class": "BoxShape3D",
    "properties": { "size": { "type": "vector3", "value": { "x": 1.2, "y": 1.2, "z": 1.2 } } } },
  { "op": "add_node", "parent": "Crate", "name": "Mesh", "type": "MeshInstance3D" },
  { "op": "set_subresource", "node": "Crate/Mesh", "property": "mesh", "class": "BoxMesh",
    "properties": { "size": { "type": "vector3", "value": { "x": 1.2, "y": 1.2, "z": 1.2 } } } },
  { "op": "set_subresource", "node": "Crate/Mesh", "property": "material_override",
    "class": "StandardMaterial3D",
    "properties": { "albedo_color": { "type": "color",
        "value": { "red": 0.8, "green": 0.5, "blue": 0.2, "alpha": 1.0 } },
      "roughness": { "type": "float", "value": 0.7 } } }
]
```

<!-- recipe-parameters id=body-3d -->
| Parameter | Takes | Default | Applies when | Sets |
| --- | --- | --- | --- | --- |
| `name` | node name, 1 to 40 characters | RigidBody, StaticBody, Player or Area, by `body` | always | The body's name. |
| `body` | rigid, static, character, area | rigid | always | The body: RigidBody3D, StaticBody3D, CharacterBody3D or Area3D. |
| `shape` | box, sphere, capsule, cylinder | box | always | The collision shape, and the mesh drawn with it: a box, a sphere, a capsule or a cylinder. |
| `size` | {x, y, z} above 0 | Godot's | `shape` is box | The box's size, in metres, for the shape and the mesh. |
| `radius` | number above 0, at most 10000 | Godot's | `shape` is sphere or capsule or cylinder | The radius, in metres, for the shape and the mesh. |
| `height` | number above 0, at most 10000 | Godot's | `shape` is capsule or cylinder | The height, in metres, for the shape and the mesh. |
| `mass` | number above 0, at most 100000 | Godot's | `body` is rigid | The body's mass, in kilograms. |
| `friction` | number, 0 to 1 | Godot's | `body` is rigid or static | The friction of the body's own PhysicsMaterial. |
| `bounce` | number, 0 to 1 | Godot's | `body` is rigid or static | The bounce of the body's own PhysicsMaterial. |
| `collision_layer` | integer, 0 to 4294967295 | Godot's | always | The layers the body is on, as a bit field. |
| `collision_mask` | integer, 0 to 4294967295 | Godot's | always | The layers the body collides with, as a bit field. |
| `mesh` | boolean | true | always | Add a MeshInstance3D named Mesh drawing the shape. |
| `color` | {red, green, blue, alpha}, 0 to 16 | Godot's | `mesh` is true | The albedo colour of the mesh's StandardMaterial3D. |
| `roughness` | number, 0 to 1 | Godot's | `mesh` is true | The roughness of the mesh's StandardMaterial3D. |

<!-- recipe-tune id=body-3d -->
```json
[
  { "op": "set_property", "node": "Crate", "property": "physics_material_override:bounce",
    "value": { "type": "float", "value": 0.5 } }
]
```

## Lights, a sky and a camera

A sun with shadows, a warm point light, a `WorldEnvironment` whose `Environment` draws a
procedural sky, and the current camera; each part can be left out. The sky is two levels down
the environment, which a property path reaches. The scene's root is a `Node3D`.

<!-- recipe-call id=light-3d path=scenes/stage.tscn root=Node3D -->
```json
{
  "path": "scenes/stage.tscn",
  "expectedSha256": "<the scene's current SHA-256>",
  "recipe": {
    "light-3d": {
      "sun_rotation": { "x": -50, "y": 30, "z": 0 }, "sun_energy": 1.2, "shadows": true,
      "lamp_position": { "x": 2, "y": 3, "z": 0 }, "lamp_range": 6,
      "lamp_color": { "red": 1, "green": 0.85, "blue": 0.6, "alpha": 1 },
      "sky_top_color": { "red": 0.3, "green": 0.5, "blue": 0.9, "alpha": 1 },
      "camera_position": { "x": 0, "y": 2, "z": 6 }, "fov": 60
    }
  },
  "idempotencyId": "<a new unique id>"
}
```

It becomes these operations; `background_mode` 2 is `Environment.BG_SKY`:

<!-- recipe id=light-3d -->
```json
[
  { "op": "add_node", "parent": ".", "name": "Sun", "type": "DirectionalLight3D" },
  { "op": "set_property", "node": "Sun", "property": "rotation_degrees",
    "value": { "type": "vector3", "value": { "x": -50.0, "y": 30.0, "z": 0.0 } } },
  { "op": "set_property", "node": "Sun", "property": "light_energy",
    "value": { "type": "float", "value": 1.2 } },
  { "op": "set_property", "node": "Sun", "property": "shadow_enabled",
    "value": { "type": "bool", "value": true } },
  { "op": "add_node", "parent": ".", "name": "Lamp", "type": "OmniLight3D" },
  { "op": "set_property", "node": "Lamp", "property": "position",
    "value": { "type": "vector3", "value": { "x": 2.0, "y": 3.0, "z": 0.0 } } },
  { "op": "set_property", "node": "Lamp", "property": "omni_range",
    "value": { "type": "float", "value": 6.0 } },
  { "op": "set_property", "node": "Lamp", "property": "light_color",
    "value": { "type": "color", "value": { "red": 1.0, "green": 0.85, "blue": 0.6, "alpha": 1.0 } } },
  { "op": "add_node", "parent": ".", "name": "Environment", "type": "WorldEnvironment" },
  { "op": "set_subresource", "node": "Environment", "property": "environment",
    "class": "Environment", "properties": { "background_mode": { "type": "integer", "value": 2 } } },
  { "op": "set_subresource", "node": "Environment", "property": "environment:sky", "class": "Sky" },
  { "op": "set_subresource", "node": "Environment", "property": "environment:sky:sky_material",
    "class": "ProceduralSkyMaterial",
    "properties": { "sky_top_color": { "type": "color",
        "value": { "red": 0.3, "green": 0.5, "blue": 0.9, "alpha": 1.0 } } } },
  { "op": "add_node", "parent": ".", "name": "Camera", "type": "Camera3D" },
  { "op": "set_property", "node": "Camera", "property": "position",
    "value": { "type": "vector3", "value": { "x": 0.0, "y": 2.0, "z": 6.0 } } },
  { "op": "set_property", "node": "Camera", "property": "fov",
    "value": { "type": "float", "value": 60.0 } },
  { "op": "set_property", "node": "Camera", "property": "current",
    "value": { "type": "bool", "value": true } }
]
```

<!-- recipe-parameters id=light-3d -->
| Parameter | Takes | Default | Applies when | Sets |
| --- | --- | --- | --- | --- |
| `sun` | boolean | true | always | Add a DirectionalLight3D named Sun. |
| `sun_rotation` | {x, y, z} | Godot's | `sun` is true | The sun's rotation, in degrees. |
| `sun_energy` | number, 0 to 1000 | Godot's | `sun` is true | The sun's light energy. |
| `shadows` | boolean | Godot's | `sun` is true | Whether the sun casts shadows. |
| `lamp` | boolean | true | always | Add an OmniLight3D named Lamp. |
| `lamp_position` | {x, y, z} | Godot's | `lamp` is true | The lamp's position, in metres. |
| `lamp_range` | number above 0, at most 10000 | Godot's | `lamp` is true | How far the lamp reaches, in metres. |
| `lamp_color` | {red, green, blue, alpha}, 0 to 16 | Godot's | `lamp` is true | The lamp's light colour. |
| `sky` | boolean | true | always | Add a WorldEnvironment named Environment whose Environment draws a procedural sky. |
| `sky_top_color` | {red, green, blue, alpha}, 0 to 16 | Godot's | `sky` is true | The colour at the top of the procedural sky. |
| `camera` | boolean | true | always | Add a Camera3D named Camera and make it the current camera. |
| `camera_position` | {x, y, z} | Godot's | `camera` is true | The camera's position, in metres. |
| `fov` | number, 1 to 179 | Godot's | `camera` is true | The camera's vertical field of view, in degrees. |

<!-- recipe-tune id=light-3d -->
```json
[
  { "op": "set_property", "node": "Sun", "property": "light_energy", "value": { "type": "float", "value": 0.8 } }
]
```

## Particles from a preset

A `GPUParticles2D` with a `ParticleProcessMaterial` set up as one of six presets. A preset is a
starting point to tune, not a generated effect:

* `fire` sends particles up in a narrow cone and fades them from yellow to grey;
* `smoke` rises slowly, grows and fades out;
* `sparks` bursts up in a wide cone and falls back under gravity, from white to orange;
* `rain` falls fast from a strip `width` pixels wide, centred on the node, in pale blue;
* `snow` drifts down from the same kind of strip, slowly and widely spread, in white;
* `explosion` is one shot: every particle leaves at once, in every direction, and slows down.

This call makes the fire. The scene's root is a `Node2D`.

<!-- recipe-call id=particles-2d path=scenes/fire.tscn root=Node2D -->
```json
{
  "path": "scenes/fire.tscn",
  "expectedSha256": "<the scene's current SHA-256>",
  "recipe": { "particles-2d": { "preset": "fire" } },
  "idempotencyId": "<a new unique id>"
}
```

It becomes these operations:

<!-- recipe id=particles-2d -->
```json
[
  { "op": "add_node", "parent": ".", "name": "Fire", "type": "GPUParticles2D" },
  { "op": "set_property", "node": "Fire", "property": "amount",
    "value": { "type": "integer", "value": 64 } },
  { "op": "set_property", "node": "Fire", "property": "lifetime",
    "value": { "type": "float", "value": 0.8 } },
  { "op": "set_subresource", "node": "Fire", "property": "process_material",
    "class": "ParticleProcessMaterial",
    "properties": { "direction": { "type": "vector3", "value": { "x": 0.0, "y": -1.0, "z": 0.0 } },
      "gravity": { "type": "vector3", "value": { "x": 0.0, "y": -40.0, "z": 0.0 } },
      "initial_velocity_max": { "type": "float", "value": 140.0 },
      "initial_velocity_min": { "type": "float", "value": 80.0 },
      "spread": { "type": "float", "value": 25.0 } } },
  { "op": "set_subresource", "node": "Fire", "property": "process_material:color_ramp",
    "class": "GradientTexture1D" },
  { "op": "set_subresource", "node": "Fire", "property": "process_material:color_ramp:gradient",
    "class": "Gradient",
    "properties": { "colors": { "type": "packed_color_array",
        "value": [
          { "red": 1.0, "green": 0.9, "blue": 0.3, "alpha": 1.0 },
          { "red": 1.0, "green": 0.3, "blue": 0.1, "alpha": 0.8 },
          { "red": 0.2, "green": 0.2, "blue": 0.2, "alpha": 0.0 }
        ] }, "offsets": { "type": "packed_float32_array", "value": [0.0, 0.6, 1.0] } } }
]
```

The other presets change the material and the ramp; `godot-mcp.recipe.particles-2d`, read
with a `preset`, shows the operations of each.

<!-- recipe-parameters id=particles-2d -->
| Parameter | Takes | Default | Applies when | Sets |
| --- | --- | --- | --- | --- |
| `name` | node name, 1 to 40 characters | Fire, Smoke, Sparks, Rain, Snow or Explosion, by `preset` | always | The emitter's name. |
| `preset` | fire, smoke, sparks, rain, snow, explosion | fire | always | The look: fire, smoke, sparks, rain, snow or a one-shot explosion. |
| `amount` | integer, 1 to 100000 | the preset's: fire 64, smoke 32, sparks 48, rain 200, snow 120, explosion 96 | always | How many particles are alive at once. |
| `lifetime` | number above 0, at most 600 | the preset's: fire 0.8, smoke 2.5, sparks 0.6, rain 1.2, snow 6, explosion 0.9 | always | How long each particle lives, in seconds. |
| `width` | number above 0, at most 100000 | 640 | `preset` is rain or snow | The width of the strip the drops fall from, in pixels, centred on the node. |
| `texture` | res:// path of a Texture2D | Godot's | always | A Texture2D each particle draws; without one a particle is a small square. |

<!-- recipe-tune id=particles-2d -->
```json
[
  { "op": "set_property", "node": "Fire", "property": "process_material:spread", "value": { "type": "float", "value": 40.0 } }
]
```

## Navigation: a floor and an agent

A `NavigationRegion2D` named `Floor` whose rectangular outline is baked, and a
`CharacterBody2D` named `Walker` with a `NavigationAgent2D` named `Agent` on the same
navigation layers. The bake runs in the engine and the batch goes on only once it finished, so
the saved polygon holds the baked vertices. The scene's root is a `Node2D`.

<!-- recipe-call id=navigation-2d path=scenes/arena.tscn root=Node2D -->
```json
{
  "path": "scenes/arena.tscn",
  "expectedSha256": "<the scene's current SHA-256>",
  "recipe": {
    "navigation-2d": {
      "size": { "x": 640, "y": 360 }, "navigation_layers": 3, "agent_radius": 8,
      "walker_radius": 12, "path_desired_distance": 4, "target_desired_distance": 8,
      "avoidance": true
    }
  },
  "idempotencyId": "<a new unique id>"
}
```

It becomes these operations:

<!-- recipe id=navigation-2d -->
```json
[
  { "op": "add_node", "parent": ".", "name": "Floor", "type": "NavigationRegion2D" },
  { "op": "set_property", "node": "Floor", "property": "navigation_layers",
    "value": { "type": "integer", "value": 3 } },
  { "op": "set_subresource", "node": "Floor", "property": "navigation_polygon",
    "class": "NavigationPolygon",
    "properties": { "agent_radius": { "type": "float", "value": 8.0 } } },
  { "op": "call_method", "node": "Floor", "resource": { "property": "navigation_polygon" },
    "method": "add_outline",
    "arguments": [
      { "type": "packed_vector2_array",
        "value": [
          { "x": 0.0, "y": 0.0 },
          { "x": 640.0, "y": 0.0 },
          { "x": 640.0, "y": 360.0 },
          { "x": 0.0, "y": 360.0 }
        ] }
    ] },
  { "op": "call_method", "node": "Floor", "method": "bake_navigation_polygon",
    "arguments": [{ "type": "bool", "value": false }] },
  { "op": "add_node", "parent": ".", "name": "Walker", "type": "CharacterBody2D" },
  { "op": "add_node", "parent": "Walker", "name": "Agent", "type": "NavigationAgent2D" },
  { "op": "set_property", "node": "Walker/Agent", "property": "path_desired_distance",
    "value": { "type": "float", "value": 4.0 } },
  { "op": "set_property", "node": "Walker/Agent", "property": "target_desired_distance",
    "value": { "type": "float", "value": 8.0 } },
  { "op": "set_property", "node": "Walker/Agent", "property": "radius",
    "value": { "type": "float", "value": 12.0 } },
  { "op": "set_property", "node": "Walker/Agent", "property": "avoidance_enabled",
    "value": { "type": "bool", "value": true } },
  { "op": "set_property", "node": "Walker/Agent", "property": "navigation_layers",
    "value": { "type": "integer", "value": 3 } }
]
```

<!-- recipe-parameters id=navigation-2d -->
| Parameter | Takes | Default | Applies when | Sets |
| --- | --- | --- | --- | --- |
| `size` | {x, y} above 0 | {x: 640, y: 360} | always | The floor's size, in pixels: the outline is a rectangle from the region's origin. |
| `navigation_layers` | integer, 1 to 4294967295 | Godot's | always | The navigation layers of the floor and the agent, as a bit field. |
| `agent_radius` | number, 0 to 10000 | Godot's | always | The clearance the bake keeps from the outline's edges, in pixels. |
| `walker` | boolean | true | always | Add a CharacterBody2D named Walker with a NavigationAgent2D named Agent. |
| `walker_radius` | number above 0, at most 10000 | Godot's | `walker` is true | The agent's radius, in pixels. |
| `path_desired_distance` | number above 0, at most 10000 | Godot's | `walker` is true | How close the agent gets to each path point before moving on, in pixels. |
| `target_desired_distance` | number above 0, at most 10000 | Godot's | `walker` is true | How close to the target the agent counts as arrived, in pixels. |
| `avoidance` | boolean | Godot's | `walker` is true | Whether the agent avoids other agents. |

<!-- recipe-tune id=navigation-2d -->
```json
[
  { "op": "set_property", "node": "Walker/Agent", "property": "target_desired_distance",
    "value": { "type": "float", "value": 12.0 } }
]
```

## A positional sound

An `AudioStreamPlayer2D` playing a sound file of the project on the `SFX` bus. The file must be
imported (`project.import.run`, or the editor's own import) before a recipe refers to it, and
the bus must exist in the project's bus layout: a player keeps `Master` for a bus the layout
lacks, so the batch is refused as `value_not_kept`. The scene's root is a `Node2D`.

<!-- recipe-call id=audio-2d path=scenes/footsteps.tscn root=Node2D -->
```json
{
  "path": "scenes/footsteps.tscn",
  "expectedSha256": "<the scene's current SHA-256>",
  "recipe": {
    "audio-2d": {
      "name": "Steps", "stream": "res://audio/step.wav",
      "volume_db": -6, "max_distance": 600, "bus": "SFX"
    }
  },
  "idempotencyId": "<a new unique id>"
}
```

It becomes these operations:

<!-- recipe id=audio-2d -->
```json
[
  { "op": "add_node", "parent": ".", "name": "Steps", "type": "AudioStreamPlayer2D" },
  { "op": "set_property", "node": "Steps", "property": "stream",
    "value": { "type": "resource",
      "value": { "resource": "res://audio/step.wav", "resource_type": "AudioStream" } } },
  { "op": "set_property", "node": "Steps", "property": "volume_db",
    "value": { "type": "float", "value": -6.0 } },
  { "op": "set_property", "node": "Steps", "property": "max_distance",
    "value": { "type": "float", "value": 600.0 } },
  { "op": "set_property", "node": "Steps", "property": "bus",
    "value": { "type": "string_name", "value": "SFX" } }
]
```

<!-- recipe-parameters id=audio-2d -->
| Parameter | Takes | Default | Applies when | Sets |
| --- | --- | --- | --- | --- |
| `name` | node name, 1 to 40 characters | "Sound" | always | The player's name. |
| `stream` | res:// path of an AudioStream | required | always | The imported sound file the player plays, such as res://audio/step.wav. |
| `volume_db` | number, -80 to 24 | Godot's | always | The volume, in decibels. |
| `max_distance` | number above 0, at most 1000000 | Godot's | always | How far away the sound can be heard, in pixels. |
| `bus` | string, 1 to 128 characters | Godot's | always | The audio bus; it must exist in the project's bus layout. |
| `autoplay` | boolean | Godot's | always | Whether the sound starts when the scene does. |

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
`ShaderMaterial`, with two of its uniforms set through property paths. The shader file must
exist before a recipe refers to it; `file.create` writes it:

<!-- recipe-file path=shaders/tint.gdshader -->
```glsl
shader_type canvas_item;

uniform float strength : hint_range(0.0, 1.0) = 0.0;
uniform vec4 tint : source_color = vec4(1.0);

void fragment() {
	COLOR = mix(COLOR, tint, strength);
}
```

A uniform is a property of the material named `shader_parameter/<uniform>`, so the path from
the node is `material:shader_parameter/<uniform>`. `shader.check` lists the uniforms a shader
declares. A control under a `Node2D` keeps its size as offsets, so the saved scene holds
`offset_right` and `offset_bottom` rather than `size`. The scene's root is a `Node2D`.

<!-- recipe-call id=shader-2d path=scenes/tinted.tscn root=Node2D -->
```json
{
  "path": "scenes/tinted.tscn",
  "expectedSha256": "<the scene's current SHA-256>",
  "recipe": {
    "shader-2d": {
      "name": "Tint", "shader": "res://shaders/tint.gdshader", "size": { "x": 64, "y": 64 },
      "uniforms": {
        "strength": { "type": "float", "value": 0.5 },
        "tint": { "type": "color", "value": { "red": 1, "green": 0.5, "blue": 0.2, "alpha": 1 } }
      }
    }
  },
  "idempotencyId": "<a new unique id>"
}
```

It becomes these operations:

<!-- recipe id=shader-2d -->
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

<!-- recipe-parameters id=shader-2d -->
| Parameter | Takes | Default | Applies when | Sets |
| --- | --- | --- | --- | --- |
| `name` | node name, 1 to 40 characters | "ShaderRect" | always | The rectangle's name. |
| `shader` | res:// path of a Shader (.gdshader) | required | always | The project's canvas_item shader the material draws with. |
| `size` | {x, y} above 0 | Godot's | always | The rectangle's size, in pixels. |
| `uniforms` | 1 to 64 uniform names to tagged values | Godot's | always | Shader uniforms mapped to tagged values: bool, integer, float, vector2, vector3, vector4, color, or resource for a texture. |

<!-- recipe-tune id=shader-2d -->
```json
[
  { "op": "set_property", "node": "Tint", "property": "material:shader_parameter/strength",
    "value": { "type": "float", "value": 0.8 } }
]
```

## A HUD

Lays the parent `Control` out full screen, using a theme, with a score label named `Score` in
the top-right corner and a pause button named `Pause` in the bottom-right one. The label's
colour and size override the theme for it alone. The scene's root is a `Control`, and this call
puts the HUD at the root. The theme it names is the one [A theme](#a-theme) makes; a theme a
call names must exist, so make it first.

Each control is anchored the way the editor saves it, in three operations: `layout_mode`, 1
(anchors) for a child and 3 (uncontrolled) for the scene's root; `set_anchors_and_offsets_preset`,
which moves the anchors and the offsets; and `anchors_preset`, which records the preset so the
inspector shows it. Without the first, `anchors_preset` does not take, and the batch is refused
as `value_not_kept`; without the last, the inspector shows the anchors as custom. Presets are
the `Control.LayoutPreset` values: 15 is full rect, 1 top right, 3 bottom right, 2 bottom left.

<!-- recipe-call id=hud path=ui/hud.tscn root=Control -->
```json
{
  "path": "ui/hud.tscn",
  "expectedSha256": "<the scene's current SHA-256>",
  "recipe": {
    "hud": {
      "theme": "res://ui/theme.tres",
      "score_color": { "red": 1, "green": 0.9, "blue": 0.3, "alpha": 1 }, "score_font_size": 28
    }
  },
  "idempotencyId": "<a new unique id>"
}
```

It becomes these operations:

<!-- recipe id=hud -->
```json
[
  { "op": "set_property", "node": ".", "property": "theme",
    "value": { "type": "resource",
      "value": { "resource": "res://ui/theme.tres", "resource_type": "Theme" } } },
  { "op": "set_property", "node": ".", "property": "layout_mode",
    "value": { "type": "integer", "value": 3 } },
  { "op": "call_method", "node": ".", "method": "set_anchors_and_offsets_preset",
    "arguments": [{ "type": "integer", "value": 15 }] },
  { "op": "set_property", "node": ".", "property": "anchors_preset",
    "value": { "type": "integer", "value": 15 } },
  { "op": "add_node", "parent": ".", "name": "Score", "type": "Label" },
  { "op": "set_property", "node": "Score", "property": "text",
    "value": { "type": "string", "value": "Score: 0" } },
  { "op": "set_property", "node": "Score", "property": "horizontal_alignment",
    "value": { "type": "integer", "value": 2 } },
  { "op": "set_property", "node": "Score", "property": "theme_override_colors/font_color",
    "value": { "type": "color", "value": { "red": 1.0, "green": 0.9, "blue": 0.3, "alpha": 1.0 } } },
  { "op": "set_property", "node": "Score", "property": "theme_override_font_sizes/font_size",
    "value": { "type": "integer", "value": 28 } },
  { "op": "set_property", "node": "Score", "property": "layout_mode",
    "value": { "type": "integer", "value": 1 } },
  { "op": "call_method", "node": "Score", "method": "set_anchors_and_offsets_preset",
    "arguments": [{ "type": "integer", "value": 1 }] },
  { "op": "set_property", "node": "Score", "property": "anchors_preset",
    "value": { "type": "integer", "value": 1 } },
  { "op": "add_node", "parent": ".", "name": "Pause", "type": "Button" },
  { "op": "set_property", "node": "Pause", "property": "text",
    "value": { "type": "string", "value": "Pause" } },
  { "op": "set_property", "node": "Pause", "property": "layout_mode",
    "value": { "type": "integer", "value": 1 } },
  { "op": "call_method", "node": "Pause", "method": "set_anchors_and_offsets_preset",
    "arguments": [{ "type": "integer", "value": 3 }] },
  { "op": "set_property", "node": "Pause", "property": "anchors_preset",
    "value": { "type": "integer", "value": 3 } }
]
```

<!-- recipe-parameters id=hud -->
| Parameter | Takes | Default | Applies when | Sets |
| --- | --- | --- | --- | --- |
| `theme` | res:// path of a Theme | Godot's | always | A Theme resource for the HUD and everything below it. |
| `score_text` | string, 0 to 256 characters | "Score: 0" | always | The score label's text. |
| `score_color` | {red, green, blue, alpha}, 0 to 16 | Godot's | always | The score label's font colour, overriding the theme for it alone. |
| `score_font_size` | integer, 1 to 1024 | Godot's | always | The score label's font size, overriding the theme for it alone. |
| `pause_text` | string, 0 to 256 characters | "Pause" | always | The pause button's text. |

<!-- recipe-tune id=hud -->
```json
[
  { "op": "set_property", "node": "Score", "property": "theme_override_font_sizes/font_size",
    "value": { "type": "integer", "value": 32 } }
]
```

## A grid map

A `GridMap` whose embedded `MeshLibrary` holds one box item, and cells painted with it. The
library is embedded in the scene; `call_method` with `resource` calls its methods. The scene's
root is a `Node3D`.

<!-- recipe-call id=gridmap-3d path=scenes/blocks.tscn root=Node3D -->
```json
{
  "path": "scenes/blocks.tscn",
  "expectedSha256": "<the scene's current SHA-256>",
  "recipe": {
    "gridmap-3d": {
      "name": "Grid", "cell_size": { "x": 1, "y": 1, "z": 1 }, "item_name": "Block",
      "cells": [
        { "x": 0, "y": 0, "z": 0 }, { "x": 1, "y": 0, "z": 0 },
        { "x": 2, "y": 0, "z": 0 }, { "x": 2, "y": 1, "z": 0 }
      ]
    }
  },
  "idempotencyId": "<a new unique id>"
}
```

It becomes these operations:

<!-- recipe id=gridmap-3d -->
```json
[
  { "op": "add_node", "parent": ".", "name": "Grid", "type": "GridMap" },
  { "op": "set_property", "node": "Grid", "property": "cell_size",
    "value": { "type": "vector3", "value": { "x": 1.0, "y": 1.0, "z": 1.0 } } },
  { "op": "set_subresource", "node": "Grid", "property": "mesh_library", "class": "MeshLibrary" },
  { "op": "call_method", "node": "Grid", "resource": { "property": "mesh_library" },
    "method": "create_item", "arguments": [{ "type": "integer", "value": 0 }] },
  { "op": "call_method", "node": "Grid", "resource": { "property": "mesh_library" },
    "method": "set_item_name",
    "arguments": [{ "type": "integer", "value": 0 }, { "type": "string", "value": "Block" }] },
  { "op": "call_method", "node": "Grid", "resource": { "property": "mesh_library" },
    "method": "set_item_mesh",
    "arguments": [
      { "type": "integer", "value": 0 },
      { "type": "new_resource", "value": { "class": "BoxMesh" } }
    ] },
  { "op": "call_method", "node": "Grid", "method": "set_cell_item",
    "repeat": [
      [{ "type": "vector3i", "value": { "x": 0, "y": 0, "z": 0 } }, { "type": "integer", "value": 0 }],
      [{ "type": "vector3i", "value": { "x": 1, "y": 0, "z": 0 } }, { "type": "integer", "value": 0 }],
      [{ "type": "vector3i", "value": { "x": 2, "y": 0, "z": 0 } }, { "type": "integer", "value": 0 }],
      [{ "type": "vector3i", "value": { "x": 2, "y": 1, "z": 0 } }, { "type": "integer", "value": 0 }]
    ] }
]
```

<!-- recipe-parameters id=gridmap-3d -->
| Parameter | Takes | Default | Applies when | Sets |
| --- | --- | --- | --- | --- |
| `name` | node name, 1 to 40 characters | "GridMap" | always | The grid map's name. |
| `cell_size` | {x, y, z} above 0 | Godot's | always | The size of one cell, in metres. |
| `item_name` | string, 1 to 128 characters | "Block" | always | The name of the library's one item, a box mesh. |
| `cells` | 1 to 4096 {x, y, z} cells | Godot's | always | The cells painted with the item, as `{x, y, z}` grid coordinates. Omitted: none. |

<!-- recipe-tune id=gridmap-3d -->
```json
[
  { "op": "call_method", "node": "Grid", "method": "set_cell_item", "arguments": [
    { "type": "vector3i", "value": { "x": 0, "y": 1, "z": 0 } }, { "type": "integer", "value": 0 } ] }
]
```

## An on-screen joystick

A `VirtualJoystick` in a bottom corner that drives four input actions. The class exists in
Godot 4.7.1 and 4.7.2 and not in 4.4.1, 4.5.1 or 4.6.1: there the recipe is refused, naming
the class, and nothing is written. On those builds a `TouchScreenButton` per action is the
nearest built-in node. The scene's root is a `Control`.

<!-- recipe-call id=joystick path=ui/touch.tscn root=Control since=4.7.1 -->
```json
{
  "path": "ui/touch.tscn",
  "expectedSha256": "<the scene's current SHA-256>",
  "recipe": {
    "joystick": {
      "name": "Stick", "mode": "dynamic", "deadzone": 0.2, "corner": "bottom_left",
      "actions": { "left": "move_left", "right": "move_right", "up": "move_up", "down": "move_down" }
    }
  },
  "idempotencyId": "<a new unique id>"
}
```

It becomes these operations; `joystick_mode` 1 is dynamic, so the joystick appears where the
touch starts:

<!-- recipe id=joystick -->
```json
[
  { "op": "add_node", "parent": ".", "name": "Stick", "type": "VirtualJoystick" },
  { "op": "set_property", "node": "Stick", "property": "joystick_mode",
    "value": { "type": "integer", "value": 1 } },
  { "op": "set_property", "node": "Stick", "property": "deadzone_ratio",
    "value": { "type": "float", "value": 0.2 } },
  { "op": "set_property", "node": "Stick", "property": "action_left",
    "value": { "type": "string_name", "value": "move_left" } },
  { "op": "set_property", "node": "Stick", "property": "action_right",
    "value": { "type": "string_name", "value": "move_right" } },
  { "op": "set_property", "node": "Stick", "property": "action_up",
    "value": { "type": "string_name", "value": "move_up" } },
  { "op": "set_property", "node": "Stick", "property": "action_down",
    "value": { "type": "string_name", "value": "move_down" } },
  { "op": "set_property", "node": "Stick", "property": "layout_mode",
    "value": { "type": "integer", "value": 1 } },
  { "op": "call_method", "node": "Stick", "method": "set_anchors_and_offsets_preset",
    "arguments": [{ "type": "integer", "value": 2 }] },
  { "op": "set_property", "node": "Stick", "property": "anchors_preset",
    "value": { "type": "integer", "value": 2 } }
]
```

<!-- recipe-parameters id=joystick -->
| Parameter | Takes | Default | Applies when | Sets |
| --- | --- | --- | --- | --- |
| `name` | node name, 1 to 40 characters | "Joystick" | always | The joystick's name. |
| `mode` | fixed, dynamic, following | Godot's | always | Fixed in place, appearing where the touch starts (dynamic), or following the touch. |
| `deadzone` | number, 0 to 1 | Godot's | always | The part of the radius that counts as no input. |
| `actions` | {left, right, up, down} action names | Godot's | always | The input actions the joystick presses, as `left`, `right`, `up` and `down`. |
| `corner` | bottom_left, bottom_right | bottom_left | always | The screen corner the joystick is anchored to. |

<!-- recipe-tune id=joystick -->
```json
[
  { "op": "set_property", "node": "Stick", "property": "deadzone_ratio", "value": { "type": "float", "value": 0.3 } }
]
```

## A theme

A `Theme` resource with a label colour and size, an outline constant and a flat panel style,
which the HUD above uses. These operations go to `resource.create` with `class` `Theme`; the
same operations go to `resource.batch.apply` to change an existing theme, with the SHA-256 of
its current bytes as `expectedSha256`.

<!-- recipe id=theme path=ui/theme.tres class=Theme -->
```json
[
  { "op": "call_method", "method": "set_color",
    "arguments": [
      { "type": "string_name", "value": "font_color" },
      { "type": "string_name", "value": "Label" },
      { "type": "color", "value": { "red": 0.95, "green": 0.95, "blue": 0.95, "alpha": 1.0 } }
    ] },
  { "op": "call_method", "method": "set_font_size",
    "arguments": [
      { "type": "string_name", "value": "font_size" },
      { "type": "string_name", "value": "Label" },
      { "type": "integer", "value": 20 }
    ] },
  { "op": "call_method", "method": "set_constant",
    "arguments": [
      { "type": "string_name", "value": "outline_size" },
      { "type": "string_name", "value": "Label" },
      { "type": "integer", "value": 2 }
    ] },
  { "op": "call_method", "method": "set_stylebox",
    "arguments": [
      { "type": "string_name", "value": "panel" },
      { "type": "string_name", "value": "Panel" },
      { "type": "new_resource",
        "value": { "class": "StyleBoxFlat",
          "properties": { "bg_color": { "type": "color",
              "value": { "red": 0.1, "green": 0.12, "blue": 0.16, "alpha": 0.9 } },
            "corner_radius_bottom_left": { "type": "integer", "value": 8 },
            "corner_radius_bottom_right": { "type": "integer", "value": 8 },
            "corner_radius_top_left": { "type": "integer", "value": 8 },
            "corner_radius_top_right": { "type": "integer", "value": 8 } } } }
    ] }
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

## Verified on

Godot 4.4.1, 4.5.1, 4.6.1, 4.7.1 and 4.7.2, official Linux x86_64 builds, by
`crates/godot-mcp-server/tests/live_recipes.rs` through the production MCP server, which reads
every call, operations block and tuning batch from this page:

* Each scene recipe's call is applied with `scene.recipe.apply`, as written here, to an empty
  scene `scene.create` made with the root type the section names. The answer must carry
  exactly the operations shown below the call, and the saved file and the scene tree are
  checked. The theme is made with `resource.create` from its operations, before the HUD that
  uses it.
* Six more variants of the body recipes (`body-2d` rigid with a circle, static with a
  rectangle, area with a capsule; `body-3d` static with a sphere, character with a capsule,
  area with a cylinder) and all six particle presets are applied with the registry's defaults,
  each into its own scene, and read back the same way.
* `scene.read` reads the grid map's cells and the navigation region's baked polygons back from
  the engine.
* In a headless editor running the add-on, `particles-2d` with the `smoke` preset is applied
  with `scene.recipe.apply` to a scene open there: the editor saves it, its undo history names
  the action `godot-mcp: scene.recipe.apply`, one `editor.undo` restores the empty scene's
  digest and one `editor.redo` the recipe's, and the scene saved with `editor.scene.save` and
  reloaded with `editor.scene.open` has the same digest.
* Each scene's tuning batch is applied with `editor.scene.edit`, undone, redone, saved and
  reloaded the same way, with no error in the editor's log; the theme's tuning batch goes to
  `resource.batch.apply`.

The joystick call is applied on 4.7.1 and 4.7.2 and refused on 4.4.1, 4.5.1 and 4.6.1,
naming `VirtualJoystick`, with the scene left as it was. With a debug build of the server on a
32-core Intel Xeon 6975P-C running Linux 7.0, one build after another, the five builds took
139.9 s, 96.5 s, 95.8 s, 101.0 s and 100.1 s, 533.3 s together. Windows and macOS editors: not
measured.
