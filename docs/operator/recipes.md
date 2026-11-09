# Recipes

Common setups by name: fifteen that `scene.recipe.apply` adds to a scene — a 2D or 3D physics
body, lights and a sky, 2D and 3D particles, 2D and 3D navigation, a 2D or 3D sound, a shader
material, a HUD, a grid map, an on-screen joystick, and an AnimationTree state machine or blend
space — and two resources that `resource.create` makes, a theme and a 3D material. A recipe is a
named batch of the typed operations every authoring tool takes, with parameters: the call names
the recipe and its parameters, the server expands them into operations and applies them as one
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

The theme and the material are recipes for a resource rather than for nodes: `resource.create`
takes them in its `recipe` field, and `scene.recipe.apply` refuses them and names
`resource.create`. Their operations also go to `resource.batch.apply` to change a resource that
exists.

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
one, names its main node; the other nodes have fixed names below it. A few rules join several
values, such as a transition that must leave a state the same call declares: the parameter
schema cannot state them, so the tool checks them and refuses a call that breaks one, naming
the rule.

`resource.create` takes a resource recipe the same way, in its `recipe` field and in place of
`class` and `operations`: `path` is the new resource's `.tres` path, which must not exist yet,
and `recipe`, `dryRun` and `idempotencyId` are as above. Its answer also names every parameter
the recipe used and carries the operations it applied.

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

## 2D particles from a preset

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

## 3D particles from a preset

A `GPUParticles3D` with a `ParticleProcessMaterial` set up as one of the same six presets, in
metres and seconds with +Y up. Each particle draws a square `quad_size` metres wide that faces the
camera: the draw pass is a `QuadMesh` with an unshaded `StandardMaterial3D` that takes the
particle's colour, and draws `texture` on it when the call gives one.

Rain and snow start in a box `area` metres in size, centred on the node, and fall from it. Godot
culls a particle system whose visibility box is off screen, and its default box is 8 metres on a
side, so these two presets also set `visibility_aabb` to a box that holds the whole fall: the
emission box, reaching down as far as the fastest particle falls in its lifetime under gravity
and out to each side as far as the spread lets it drift. With the rain's defaults that is
13.44 m down and 0.42 m to each side. The other four presets keep Godot's box. This call makes
the rain. The scene's root is a `Node3D`.

<!-- recipe-call id=particles-3d path=scenes/rain.tscn root=Node3D -->
```json
{
  "path": "scenes/rain.tscn",
  "expectedSha256": "<the scene's current SHA-256>",
  "recipe": { "particles-3d": { "preset": "rain" } },
  "idempotencyId": "<a new unique id>"
}
```

It becomes these operations:

<!-- recipe id=particles-3d -->
```json
[
  { "op": "add_node", "parent": ".", "name": "Rain", "type": "GPUParticles3D" },
  { "op": "set_property", "node": "Rain", "property": "amount",
    "value": { "type": "integer", "value": 200 } },
  { "op": "set_property", "node": "Rain", "property": "lifetime",
    "value": { "type": "float", "value": 1.2 } },
  { "op": "set_property", "node": "Rain", "property": "visibility_aabb",
    "value": { "type": "aabb",
      "value": { "position": { "x": -10.418793960430012, "y": -13.94, "z": -10.418793960430012 },
        "size": { "x": 20.837587920860024, "y": 14.44, "z": 20.837587920860024 } } } },
  { "op": "set_subresource", "node": "Rain", "property": "process_material",
    "class": "ParticleProcessMaterial",
    "properties": { "color": { "type": "color",
        "value": { "red": 0.6, "green": 0.7, "blue": 1.0, "alpha": 0.7 } },
      "direction": { "type": "vector3", "value": { "x": 0.0, "y": -1.0, "z": 0.0 } },
      "emission_box_extents": { "type": "vector3", "value": { "x": 10.0, "y": 0.5, "z": 10.0 } },
      "emission_shape": { "type": "integer", "value": 3 },
      "gravity": { "type": "vector3", "value": { "x": 0.0, "y": -2.0, "z": 0.0 } },
      "initial_velocity_max": { "type": "float", "value": 10.0 },
      "initial_velocity_min": { "type": "float", "value": 8.0 },
      "scale_max": { "type": "float", "value": 2.0 },
      "scale_min": { "type": "float", "value": 1.0 }, "spread": { "type": "float", "value": 2.0 } } },
  { "op": "set_subresource", "node": "Rain", "property": "draw_pass_1", "class": "QuadMesh",
    "properties": { "size": { "type": "vector2", "value": { "x": 0.05, "y": 0.05 } } } },
  { "op": "set_subresource", "node": "Rain", "property": "draw_pass_1:material",
    "class": "StandardMaterial3D",
    "properties": { "billboard_mode": { "type": "integer", "value": 3 },
      "shading_mode": { "type": "integer", "value": 0 },
      "transparency": { "type": "integer", "value": 1 },
      "vertex_color_use_as_albedo": { "type": "bool", "value": true } } }
]
```

The fire, smoke, sparks and explosion change the material and add the 2D presets' colour ramps;
`godot-mcp.recipe.particles-3d`, read with a `preset`, shows the operations of each.

<!-- recipe-parameters id=particles-3d -->
| Parameter | Takes | Default | Applies when | Sets |
| --- | --- | --- | --- | --- |
| `name` | node name, 1 to 40 characters | Fire, Smoke, Sparks, Rain, Snow or Explosion, by `preset` | always | The emitter's name. |
| `preset` | fire, smoke, sparks, rain, snow, explosion | fire | always | The look: fire, smoke, sparks, rain, snow or a one-shot explosion. |
| `amount` | integer, 1 to 100000 | the preset's: fire 64, smoke 32, sparks 48, rain 200, snow 120, explosion 96 | always | How many particles are alive at once. |
| `lifetime` | number above 0, at most 600 | the preset's: fire 0.8, smoke 2.5, sparks 0.6, rain 1.2, snow 6, explosion 0.9 | always | How long each particle lives, in seconds. |
| `quad_size` | number above 0, at most 100 | the preset's: fire 0.25, smoke 0.5, sparks 0.05, rain 0.05, snow 0.08, explosion 0.25 | always | The side of the square each particle draws facing the camera, in metres. |
| `area` | {x, y, z} above 0 | {x: 20, y: 1, z: 20} | `preset` is rain or snow | The size of the box the drops start in, in metres, centred on the node. |
| `texture` | res:// path of a Texture2D | Godot's | always | A Texture2D each particle draws, tinted by its colour; without one a particle is a square of its colour. |
<!-- recipe-tune id=particles-3d -->
```json
[
  { "op": "set_property", "node": "Rain", "property": "amount", "value": { "type": "integer", "value": 400 } }
]
```

## 2D navigation: a floor and an agent

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

## 3D navigation: a floor and an agent

A `NavigationRegion3D` named `Floor` over a box: a `StaticBody3D` named `Ground`, centred below
the region's origin with its top face level with it, holding a `BoxShape3D` named `Shape` and,
unless `mesh` is false, a `MeshInstance3D` named `Mesh` that draws the box. The region's
`NavigationMesh` is baked from the static colliders below the region, in the engine, before the
batch goes on, so the saved mesh holds the baked polygons. Unless `walker` is false, a
`CharacterBody3D` named `Walker` with a `NavigationAgent3D` named `Agent` goes on the same
navigation layers. The scene's root is a `Node3D`.

The bake works in cells of 0.25 m, Godot's default: an `agent_radius` or `agent_height` that is
not a multiple of 0.25 is rounded up, with a warning, so this call gives multiples.

<!-- recipe-call id=navigation-3d path=scenes/yard.tscn root=Node3D -->
```json
{
  "path": "scenes/yard.tscn",
  "expectedSha256": "<the scene's current SHA-256>",
  "recipe": {
    "navigation-3d": {
      "size": { "x": 30, "y": 1, "z": 20 }, "navigation_layers": 3, "agent_radius": 0.75,
      "agent_height": 2, "walker_radius": 0.75, "path_desired_distance": 0.5,
      "target_desired_distance": 1.5, "avoidance": true
    }
  },
  "idempotencyId": "<a new unique id>"
}
```

It becomes these operations:

<!-- recipe id=navigation-3d -->
```json
[
  { "op": "add_node", "parent": ".", "name": "Floor", "type": "NavigationRegion3D" },
  { "op": "set_property", "node": "Floor", "property": "navigation_layers",
    "value": { "type": "integer", "value": 3 } },
  { "op": "set_subresource", "node": "Floor", "property": "navigation_mesh",
    "class": "NavigationMesh",
    "properties": { "agent_height": { "type": "float", "value": 2.0 },
      "agent_radius": { "type": "float", "value": 0.75 },
      "geometry_parsed_geometry_type": { "type": "integer", "value": 1 } } },
  { "op": "add_node", "parent": "Floor", "name": "Ground", "type": "StaticBody3D" },
  { "op": "set_property", "node": "Floor/Ground", "property": "position",
    "value": { "type": "vector3", "value": { "x": 0.0, "y": -0.5, "z": 0.0 } } },
  { "op": "add_node", "parent": "Floor/Ground", "name": "Shape", "type": "CollisionShape3D" },
  { "op": "set_subresource", "node": "Floor/Ground/Shape", "property": "shape",
    "class": "BoxShape3D",
    "properties": { "size": { "type": "vector3", "value": { "x": 30.0, "y": 1.0, "z": 20.0 } } } },
  { "op": "add_node", "parent": "Floor/Ground", "name": "Mesh", "type": "MeshInstance3D" },
  { "op": "set_subresource", "node": "Floor/Ground/Mesh", "property": "mesh", "class": "BoxMesh",
    "properties": { "size": { "type": "vector3", "value": { "x": 30.0, "y": 1.0, "z": 20.0 } } } },
  { "op": "call_method", "node": "Floor", "method": "bake_navigation_mesh",
    "arguments": [{ "type": "bool", "value": false }] },
  { "op": "add_node", "parent": ".", "name": "Walker", "type": "CharacterBody3D" },
  { "op": "add_node", "parent": "Walker", "name": "Agent", "type": "NavigationAgent3D" },
  { "op": "set_property", "node": "Walker/Agent", "property": "path_desired_distance",
    "value": { "type": "float", "value": 0.5 } },
  { "op": "set_property", "node": "Walker/Agent", "property": "target_desired_distance",
    "value": { "type": "float", "value": 1.5 } },
  { "op": "set_property", "node": "Walker/Agent", "property": "radius",
    "value": { "type": "float", "value": 0.75 } },
  { "op": "set_property", "node": "Walker/Agent", "property": "avoidance_enabled",
    "value": { "type": "bool", "value": true } },
  { "op": "set_property", "node": "Walker/Agent", "property": "navigation_layers",
    "value": { "type": "integer", "value": 3 } }
]
```

<!-- recipe-parameters id=navigation-3d -->
| Parameter | Takes | Default | Applies when | Sets |
| --- | --- | --- | --- | --- |
| `size` | {x, y, z} above 0 | {x: 20, y: 1, z: 20} | always | The ground box's size, in metres: centred below the region's origin, with its top face level with it. |
| `navigation_layers` | integer, 1 to 4294967295 | Godot's | always | The navigation layers of the floor and the agent, as a bit field. |
| `agent_radius` | number, 0 to 10000 | Godot's | always | The clearance the bake keeps from the ground's edges, in metres. One that is not a multiple of the 0.25 m cell size is rounded up, with a warning. |
| `agent_height` | number above 0, at most 10000 | Godot's | always | The headroom an agent needs, in metres. One that is not a multiple of the 0.25 m cell height is rounded up, with a warning. |
| `mesh` | boolean | true | always | Add a MeshInstance3D named Mesh below Ground that draws the ground box. |
| `walker` | boolean | true | always | Add a CharacterBody3D named Walker with a NavigationAgent3D named Agent. |
| `walker_radius` | number above 0, at most 10000 | Godot's | `walker` is true | The agent's radius, in metres. |
| `path_desired_distance` | number above 0, at most 10000 | Godot's | `walker` is true | How close the agent gets to each path point before moving on, in metres. |
| `target_desired_distance` | number above 0, at most 10000 | Godot's | `walker` is true | How close to the target the agent counts as arrived, in metres. |
| `avoidance` | boolean | Godot's | `walker` is true | Whether the agent avoids other agents. |
<!-- recipe-tune id=navigation-3d -->
```json
[
  { "op": "set_property", "node": "Walker/Agent", "property": "target_desired_distance",
    "value": { "type": "float", "value": 2.0 } }
]
```

## A 2D positional sound

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

## A 3D positional sound

An `AudioStreamPlayer3D`, with the same two conditions as the 2D player: the sound file must be
imported, and the bus must exist in the project's bus layout. Its distances are in metres.
`unit_size` scales the distance over which the sound fades, and `attenuation_model` chooses how
the volume falls with distance. This call makes footsteps heard up to 40 m away, fading with the
inverse square of the distance. The scene's root is a `Node3D`.

<!-- recipe-call id=audio-3d path=scenes/footsteps-3d.tscn root=Node3D -->
```json
{
  "path": "scenes/footsteps-3d.tscn",
  "expectedSha256": "<the scene's current SHA-256>",
  "recipe": {
    "audio-3d": {
      "name": "Steps", "stream": "res://audio/step.wav", "volume_db": -6,
      "max_distance": 40, "unit_size": 4, "attenuation_model": "inverse_square", "bus": "SFX"
    }
  },
  "idempotencyId": "<a new unique id>"
}
```

It becomes these operations:

<!-- recipe id=audio-3d -->
```json
[
  { "op": "add_node", "parent": ".", "name": "Steps", "type": "AudioStreamPlayer3D" },
  { "op": "set_property", "node": "Steps", "property": "stream",
    "value": { "type": "resource",
      "value": { "resource": "res://audio/step.wav", "resource_type": "AudioStream" } } },
  { "op": "set_property", "node": "Steps", "property": "volume_db",
    "value": { "type": "float", "value": -6.0 } },
  { "op": "set_property", "node": "Steps", "property": "max_distance",
    "value": { "type": "float", "value": 40.0 } },
  { "op": "set_property", "node": "Steps", "property": "unit_size",
    "value": { "type": "float", "value": 4.0 } },
  { "op": "set_property", "node": "Steps", "property": "attenuation_model",
    "value": { "type": "integer", "value": 1 } },
  { "op": "set_property", "node": "Steps", "property": "bus",
    "value": { "type": "string_name", "value": "SFX" } }
]
```

<!-- recipe-parameters id=audio-3d -->
| Parameter | Takes | Default | Applies when | Sets |
| --- | --- | --- | --- | --- |
| `name` | node name, 1 to 40 characters | "Sound" | always | The player's name. |
| `stream` | res:// path of an AudioStream | required | always | The imported sound file the player plays, such as res://audio/hum.ogg. |
| `volume_db` | number, -80 to 24 | Godot's | always | The volume, in decibels. |
| `max_distance` | number above 0, at most 1000000 | Godot's | always | How far away the sound can be heard, in metres. |
| `unit_size` | number above 0, at most 100 | Godot's | always | How far the sound carries before it fades: a larger factor keeps it loud farther away. |
| `attenuation_model` | inverse, inverse_square, logarithmic, disabled | Godot's | always | How the volume falls with distance: as its inverse, its inverse square or its logarithm, or not at all. |
| `bus` | string, 1 to 128 characters | Godot's | always | The audio bus; it must exist in the project's bus layout. |
| `autoplay` | boolean | Godot's | always | Whether the sound starts when the scene does. |
<!-- recipe-tune id=audio-3d -->
```json
[
  { "op": "set_property", "node": "Steps", "property": "unit_size", "value": { "type": "float", "value": 6.0 } }
]
```

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

## An AnimationTree state machine

An `AnimationTree` whose root is an `AnimationNodeStateMachine`: one `AnimationNodeAnimation`
state for each entry of `states`, the transitions between them, and a transition from `Start`
that enters the `start` state by itself, with `advance_mode` auto. The state machine editor
shows the states four to a row.

The recipe does not read the scene. `anim_player` is a path from the `AnimationTree` to an
`AnimationPlayer`, and the player and its animations must exist for the states to play: the
default `parent` puts the tree beside a player that is a child of the scene's root, and
`scene.read` lists the animations a player has, calling `get_animation_list` on its node, as
`editor.scene.read` does for a scene open in the editor. The scene's root here is a `Node`.

Some rules join several values, so the parameter schema cannot state them; the tool refuses a
call that breaks one and names the rule. No two states share a name. Each transition leaves a
state of `states` and enters another one or `End`, never the state it leaves, and no two
transitions join the same two states the same way round. `start` is one of `states`.

<!-- recipe-call id=animation-state-machine path=scenes/character.tscn root=Node -->
```json
{
  "path": "scenes/character.tscn",
  "expectedSha256": "<the scene's current SHA-256>",
  "recipe": {
    "animation-state-machine": {
      "anim_player": "../AnimationPlayer",
      "states": [
        { "name": "idle", "animation": "idle" },
        { "name": "walk", "animation": "walk" },
        { "name": "run", "animation": "run" }
      ],
      "transitions": [
        { "from": "idle", "to": "walk", "xfade_time": 0.2 },
        { "from": "walk", "to": "idle", "xfade_time": 0.2 },
        { "from": "walk", "to": "run", "xfade_time": 0.2 },
        { "from": "run", "to": "walk", "xfade_time": 0.2 }
      ]
    }
  },
  "idempotencyId": "<a new unique id>"
}
```

It becomes these operations:

<!-- recipe id=animation-state-machine -->
```json
[
  { "op": "add_node", "parent": ".", "name": "AnimationTree", "type": "AnimationTree" },
  { "op": "set_property", "node": "AnimationTree", "property": "anim_player",
    "value": { "type": "node_path", "value": "../AnimationPlayer" } },
  { "op": "set_subresource", "node": "AnimationTree", "property": "tree_root",
    "class": "AnimationNodeStateMachine" },
  { "op": "call_method", "node": "AnimationTree", "resource": { "property": "tree_root" },
    "method": "add_node",
    "repeat": [
      [
        { "type": "string_name", "value": "idle" },
        { "type": "new_resource",
          "value": { "class": "AnimationNodeAnimation",
            "properties": { "animation": { "type": "string_name", "value": "idle" } } } },
        { "type": "vector2", "value": { "x": 200.0, "y": 250.0 } }
      ],
      [
        { "type": "string_name", "value": "walk" },
        { "type": "new_resource",
          "value": { "class": "AnimationNodeAnimation",
            "properties": { "animation": { "type": "string_name", "value": "walk" } } } },
        { "type": "vector2", "value": { "x": 400.0, "y": 250.0 } }
      ],
      [
        { "type": "string_name", "value": "run" },
        { "type": "new_resource",
          "value": { "class": "AnimationNodeAnimation",
            "properties": { "animation": { "type": "string_name", "value": "run" } } } },
        { "type": "vector2", "value": { "x": 600.0, "y": 250.0 } }
      ]
    ] },
  { "op": "call_method", "node": "AnimationTree", "resource": { "property": "tree_root" },
    "method": "add_transition",
    "repeat": [
      [
        { "type": "string_name", "value": "Start" },
        { "type": "string_name", "value": "idle" },
        { "type": "new_resource",
          "value": { "class": "AnimationNodeStateMachineTransition",
            "properties": { "advance_mode": { "type": "integer", "value": 2 } } } }
      ],
      [
        { "type": "string_name", "value": "idle" },
        { "type": "string_name", "value": "walk" },
        { "type": "new_resource",
          "value": { "class": "AnimationNodeStateMachineTransition",
            "properties": { "xfade_time": { "type": "float", "value": 0.2 } } } }
      ],
      [
        { "type": "string_name", "value": "walk" },
        { "type": "string_name", "value": "idle" },
        { "type": "new_resource",
          "value": { "class": "AnimationNodeStateMachineTransition",
            "properties": { "xfade_time": { "type": "float", "value": 0.2 } } } }
      ],
      [
        { "type": "string_name", "value": "walk" },
        { "type": "string_name", "value": "run" },
        { "type": "new_resource",
          "value": { "class": "AnimationNodeStateMachineTransition",
            "properties": { "xfade_time": { "type": "float", "value": 0.2 } } } }
      ],
      [
        { "type": "string_name", "value": "run" },
        { "type": "string_name", "value": "walk" },
        { "type": "new_resource",
          "value": { "class": "AnimationNodeStateMachineTransition",
            "properties": { "xfade_time": { "type": "float", "value": 0.2 } } } }
      ]
    ] }
]
```

<!-- recipe-parameters id=animation-state-machine -->
| Parameter | Takes | Default | Applies when | Sets |
| --- | --- | --- | --- | --- |
| `name` | node name, 1 to 40 characters | "AnimationTree" | always | The AnimationTree's name. |
| `anim_player` | node path, 1 to 256 characters | required | always | The AnimationPlayer whose animations the states play, as a path from the AnimationTree, such as ../AnimationPlayer. |
| `states` | 1 to 32 {name, animation} states | required | always | The states, each playing one animation of the AnimationPlayer. No two share a name. |
| `transitions` | 1 to 128 {from, to, switch_mode, advance_mode, advance_condition, xfade_time} transitions | Godot's | always | The transitions. Each leaves a state of `states` and enters another one or End, and no two join the same two states the same way round. |
| `start` | state name, 1 to 40 characters, not Start or End | the first state | always | The state the machine starts in, one of `states`: a transition from Start enters it by itself. |
Tuning it later adds a `move` state that blends walk into run: a state whose node is a new
`AnimationNodeBlendSpace1D`, its points added through the property path
`tree_root:states/move/node`, and a transition into it from idle. The same three operations put
a blend space into any state machine a scene already has.

<!-- recipe-tune id=animation-state-machine -->
```json
[
  { "op": "call_method", "node": "AnimationTree", "resource": { "property": "tree_root" },
    "method": "add_node",
    "arguments": [
      { "type": "string_name", "value": "move" },
      { "type": "new_resource", "value": { "class": "AnimationNodeBlendSpace1D" } },
      { "type": "vector2", "value": { "x": 400.0, "y": 400.0 } }
    ] },
  { "op": "call_method", "node": "AnimationTree",
    "resource": { "property": "tree_root:states/move/node" }, "method": "add_blend_point",
    "repeat": [
      [
        { "type": "new_resource", "value": { "class": "AnimationNodeAnimation",
            "properties": { "animation": { "type": "string_name", "value": "walk" } } } },
        { "type": "float", "value": 0.0 }
      ],
      [
        { "type": "new_resource", "value": { "class": "AnimationNodeAnimation",
            "properties": { "animation": { "type": "string_name", "value": "run" } } } },
        { "type": "float", "value": 1.0 }
      ]
    ] },
  { "op": "call_method", "node": "AnimationTree", "resource": { "property": "tree_root" },
    "method": "add_transition",
    "arguments": [
      { "type": "string_name", "value": "idle" },
      { "type": "string_name", "value": "move" },
      { "type": "new_resource", "value": { "class": "AnimationNodeStateMachineTransition" } }
    ] }
]
```

## An AnimationTree blend space

An `AnimationTree` whose root is an `AnimationNodeBlendSpace1D` with one `AnimationNodeAnimation`
for each blend point, such as idle, walk and run along a speed axis. The axis runs from -1 to 1
unless the call sets its ends. Each point's `position` lies on it, from `min_space` to
`max_space`, ends included, and `min_space` is below `max_space`: the parameter schema cannot
state these two rules, so the tool refuses a call that breaks one and names the rule. Like the
state machine, the recipe does not read the scene, and its `anim_player` must reach an
`AnimationPlayer` with these animations. The scene's root is a `Node`.

`min_space` must also be below 1. When Godot loads a blend space it sets `min_space` before
`max_space`, and turns a `min_space` that is not below the default `max_space` of 1 into 0, so a
saved axis from 5 to 10 would load as 0 to 10. The recipe writes `min_space` first for the same
reason. On Godot 4.7.1 and 4.7.2 adding the points logs one warning per point, `No name provided,
using safe index as reference`: each point is named after its index, the saved file keeps the
names, and loading it does not warn again. To blend inside a state machine instead, add the blend
space as a state: the state machine's tuning batch above does that.

<!-- recipe-call id=blend-space-1d path=scenes/locomotion.tscn root=Node -->
```json
{
  "path": "scenes/locomotion.tscn",
  "expectedSha256": "<the scene's current SHA-256>",
  "recipe": {
    "blend-space-1d": {
      "anim_player": "../AnimationPlayer",
      "points": [
        { "animation": "idle", "position": 0 },
        { "animation": "walk", "position": 0.5 },
        { "animation": "run", "position": 1 }
      ]
    }
  },
  "idempotencyId": "<a new unique id>"
}
```

It becomes these operations:

<!-- recipe id=blend-space-1d -->
```json
[
  { "op": "add_node", "parent": ".", "name": "AnimationTree", "type": "AnimationTree" },
  { "op": "set_property", "node": "AnimationTree", "property": "anim_player",
    "value": { "type": "node_path", "value": "../AnimationPlayer" } },
  { "op": "set_subresource", "node": "AnimationTree", "property": "tree_root",
    "class": "AnimationNodeBlendSpace1D" },
  { "op": "call_method", "node": "AnimationTree", "resource": { "property": "tree_root" },
    "method": "add_blend_point",
    "repeat": [
      [
        { "type": "new_resource",
          "value": { "class": "AnimationNodeAnimation",
            "properties": { "animation": { "type": "string_name", "value": "idle" } } } },
        { "type": "float", "value": 0.0 }
      ],
      [
        { "type": "new_resource",
          "value": { "class": "AnimationNodeAnimation",
            "properties": { "animation": { "type": "string_name", "value": "walk" } } } },
        { "type": "float", "value": 0.5 }
      ],
      [
        { "type": "new_resource",
          "value": { "class": "AnimationNodeAnimation",
            "properties": { "animation": { "type": "string_name", "value": "run" } } } },
        { "type": "float", "value": 1.0 }
      ]
    ] }
]
```

<!-- recipe-parameters id=blend-space-1d -->
| Parameter | Takes | Default | Applies when | Sets |
| --- | --- | --- | --- | --- |
| `name` | node name, 1 to 40 characters | "AnimationTree" | always | The AnimationTree's name. |
| `anim_player` | node path, 1 to 256 characters | required | always | The AnimationPlayer whose animations the points play, as a path from the AnimationTree, such as ../AnimationPlayer. |
| `points` | 2 to 16 {animation, position} points | required | always | The blend points, each an animation of the AnimationPlayer at a position on the axis, from `min_space` to `max_space`, ends included. |
| `min_space` | number from -100000, below 1 | Godot's | always | The lower end of the axis (Godot's default -1), below `max_space`. It must be below 1: Godot loads it before `max_space` and would turn 1 or more into 0. |
| `max_space` | number, -100000 to 100000 | Godot's | always | The upper end of the axis (Godot's default 1), above `min_space`. |
Tuning it later moves the walk point through a property path:

<!-- recipe-tune id=blend-space-1d -->
```json
[
  { "op": "set_property", "node": "AnimationTree", "property": "tree_root:blend_point_1/pos",
    "value": { "type": "float", "value": 0.4 } }
]
```

## A theme

A `Theme` resource with a label colour and size, an outline constant and a flat panel style,
which the HUD above uses. The recipe takes no parameters. Its operations also go to
`resource.create` as `class` `Theme` and `operations`, without a `recipe`, and to
`resource.batch.apply` to change an existing theme, with the SHA-256 of its current bytes as
`expectedSha256`.

<!-- recipe-call id=theme path=ui/theme.tres class=Theme -->
```json
{ "path": "ui/theme.tres", "recipe": { "theme": {} }, "idempotencyId": "<a new unique id>" }
```

It becomes these operations:

<!-- recipe id=theme -->
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

## A 3D material

A `StandardMaterial3D` resource with the colours, textures and surface settings the call gives;
everything else keeps Godot's values, and a call gives at least one parameter. A texture is
named by its res:// path and must be imported first. This call makes the crate's material from
the 3D body above as a file that any number of meshes can share: a `MeshInstance3D` takes it in
`material_override`, or in `surface_material_override/0` for its first surface, as the value
`{ "type": "resource", "value": { "resource": "res://materials/crate.tres", "resource_type": "StandardMaterial3D" } }`.
The same operations go to `resource.batch.apply` to change an existing material, with the
SHA-256 of its current bytes as `expectedSha256`.

<!-- recipe-call id=material-3d path=materials/crate.tres class=StandardMaterial3D -->
```json
{
  "path": "materials/crate.tres",
  "recipe": {
    "material-3d": {
      "albedo_color": { "red": 0.8, "green": 0.5, "blue": 0.2, "alpha": 1 }, "roughness": 0.7
    }
  },
  "idempotencyId": "<a new unique id>"
}
```

It becomes these operations:

<!-- recipe id=material-3d -->
```json
[
  { "op": "set_property", "property": "albedo_color",
    "value": { "type": "color", "value": { "red": 0.8, "green": 0.5, "blue": 0.2, "alpha": 1.0 } } },
  { "op": "set_property", "property": "roughness", "value": { "type": "float", "value": 0.7 } }
]
```

<!-- recipe-parameters id=material-3d -->
| Parameter | Takes | Default | Applies when | Sets |
| --- | --- | --- | --- | --- |
| `albedo_color` | {red, green, blue, alpha}, 0 to 16 | Godot's | always | The base colour. |
| `albedo_texture` | res:// path of a Texture2D | Godot's | always | A Texture2D the base colour multiplies. |
| `metallic` | number, 0 to 1 | Godot's | always | How metallic the surface is. |
| `roughness` | number, 0 to 1 | Godot's | always | How rough the surface is: 0 is a mirror. |
| `normal_texture` | res:// path of a Texture2D | Godot's | always | A normal map; giving one turns normal mapping on. |
| `emission` | {red, green, blue, alpha}, 0 to 16 | Godot's | always | The colour the surface gives off; giving one turns emission on. |
| `emission_energy` | number, 0 to 1000 | Godot's | always | How bright the emission is, as a multiplier; giving one turns emission on. |
| `transparency` | disabled, alpha, alpha_scissor, alpha_hash, depth_pre_pass | Godot's | always | How the alpha is drawn: not at all, blended, cut at a threshold, hashed, or blended after a depth pre-pass. |
| `cull_mode` | back, front, disabled | Godot's | always | Which faces are not drawn: the back ones, the front ones, or none. |
| `unshaded` | boolean | Godot's | always | true draws the colours without lighting; false writes nothing. |
<!-- recipe-tune id=material-3d -->
```json
[
  { "op": "set_property", "property": "metallic", "value": { "type": "float", "value": 0.3 } }
]
```

## Verified on

Godot 4.4.1, 4.5.1, 4.6.1, 4.7.1 and 4.7.2, official Linux x86_64 builds, by
`crates/godot-mcp-server/tests/live_recipes.rs` through the production MCP server, which reads
every call, operations block and tuning batch from this page:

* Each scene recipe's call is applied with `scene.recipe.apply`, as written here, to an empty
  scene `scene.create` made with the root type the section names. The answer must carry
  exactly the operations shown below the call, and the saved file and the scene tree are
  checked. The theme and the material are made with `resource.create` from their calls, before
  the HUD that uses the theme, and the answer must carry the operations shown.
* Six more variants of the body recipes (`body-2d` rigid with a circle, static with a
  rectangle, area with a capsule; `body-3d` static with a sphere, character with a capsule,
  area with a cylinder), all six particle presets in 2D and in 3D, and the 3D floor without its
  mesh and walker are applied with the registry's defaults, each into its own scene, and read
  back the same way.
* `scene.read` reads back from the engine the grid map's cells, the polygons the 2D and 3D
  navigation bakes left in their regions, the state machine's three states, `Start` and five
  transitions, and the blend space's three points. A state machine with 32 states and 128
  transitions reads back 129 transitions, the one from `Start` included, and a blend space with
  16 points reads back 16. A blend space whose axis runs from -10 to -5 reads back its points at
  -10, -7.5 and -5, and its file holds `min_space` before `max_space`.
* In a headless editor running the add-on, `particles-2d` with the `smoke` preset and the state
  machine's call are each applied with `scene.recipe.apply` to an empty scene open there, the
  second with no `AnimationPlayer` in it: the editor saves the scene, its undo history names
  the action `godot-mcp: scene.recipe.apply`, one `editor.undo` restores the empty scene's
  digest and one `editor.redo` the recipe's, and the scene saved with `editor.scene.save` and
  reloaded with `editor.scene.open` has the same digest.
* Each scene's tuning batch is applied with `editor.scene.edit`, undone, redone, saved and
  reloaded the same way, with no error in the editor's log; the theme's and the material's
  tuning batches go to `resource.batch.apply`.

The joystick call is applied on 4.7.1 and 4.7.2 and refused on 4.4.1, 4.5.1 and 4.6.1,
naming `VirtualJoystick`, with the scene left as it was. With a debug build of the server on a
32-core Intel Xeon 6975P-C running Linux 7.0, one build after another, the five builds took
282.0 s, 209.8 s, 183.6 s, 182.9 s and 184.3 s, 1,042.6 s together. Windows and macOS
editors: not measured.
