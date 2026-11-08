---
description: godot-mcp 用哪些工具处理动画、AnimationTree、物理、3D、粒子、导航、音频、着色器、主题、TileMap 和运行中的游戏。
---

# 覆盖范围

本页按 Godot 项目的各个部分，列出创建、修改它们的工具，以及读回它们、在运行中的游戏里观察它们的工具。表里的工具都在 `full` profile 里；它们分别属于哪个更小的 profile，见 [Profile 与答复](profiles.md)。

表里有两个词各代表一组工具：

- **编辑**：`scene.batch.apply`、`editor.scene.edit` 或 `resource.batch.apply` 里的 `call_method` 操作，或者 `scene.create`、`resource.create` 的操作里的同类操作。它调用一张固定表里的一个方法，例如 `Animation` 的 `add_track`；每个类能调用哪些方法，见[编辑器桥接参考](../../../operator/editor-bridge.md#editing-calls)。
- **读取**：调用 `scene.read`、`resource.read`，或者对编辑器正持有的场景调用 `editor.scene.read`，向引擎询问只有它知道的事实，例如图块地图上画了哪些格子。

| 领域 | 创建与修改 | 读回、运行与观察 |
| --- | --- | --- |
| 动画 | 编辑 `AnimationPlayer`、`AnimationLibrary` 与 `Animation`：`add_animation`、`add_track`、`track_insert_key`、`set_loop_mode`；逐帧动画用 `SpriteFrames` | 读取 `get_animation_list`、`track_get_key_count`、`track_get_key_value`；游戏运行时用 `runtime.signals.get` 与 `runtime.wait` |
| AnimationTree | 编辑 `AnimationNodeStateMachine`（`add_node`、`add_transition`）、`AnimationNodeBlendTree`（`connect_node`）和两种混合空间（`add_blend_point`） | 读取 `get_node_list`、`get_transition_count`、`get_blend_point_count` |
| 物理 | `scene.recipe.apply` 配合 `body-2d` 或 `body-3d`：带碰撞形状的角色体、刚体、静态体或区域；任意形状用 `set_subresource`；`TileSet` 的物理层 | `scene.tree.get`；游戏运行时用 `runtime.nodes.find` 查找某点附近的节点，以及 `runtime.properties.get`、`runtime.sample`、`runtime.time_scale.set` |
| 3D | `scene.recipe.apply` 配合 `body-3d`、`light-3d`（太阳光、点光源、天空、相机）或 `gridmap-3d`；编辑 `GridMap`（`set_cell_item`）与 `MeshLibrary`（`create_item`） | 读取 `get_used_cells`、网格的 `get_aabb`、`Skeleton3D` 的骨骼；`editor.camera.get`、`editor.camera.set`，以及对 3D 视口的 `editor.screenshot.capture` |
| 粒子 | `scene.recipe.apply` 配合 `particles-2d` 和一种预设：fire、smoke、sparks、rain、snow 或 explosion；`ParticleProcessMaterial` 用 `set_subresource` | 游戏运行时用 `runtime.screenshot.capture`、`runtime.recording.capture`、`runtime.image.compare` |
| 导航 | `scene.recipe.apply` 配合 `navigation-2d`；编辑 `NavigationPolygon`（`add_outline`），并用导航区域的 `bake_navigation_polygon` 与 `bake_navigation_mesh` 烘焙 | 读取 `get_polygon_count`、`get_vertices`；游戏运行时用 `runtime.navigation.path.get`，并用 `runtime.node.move` 沿路径移动 |
| 音频 | 音频文件用 `asset.put` 与 `project.import.run`，`scene.recipe.apply` 配合 `audio-2d`，总线用 `resource.batch.apply` 修改总线布局（`default_bus_layout.tres`）；换用另一个布局文件用 `project.settings.patch` | `asset.status`，对总线布局用 `resource.tree.get`；游戏运行时对播放器用 `runtime.properties.get` |
| 着色器 | `.gdshader` 用 `file.create`；`scene.recipe.apply` 配合 `shader-2d` 生成 `ShaderMaterial` 及其 uniform；编辑 `VisualShader`（`add_node`、`connect_nodes`） | `shader.check` 给出编译错误与 uniform；读取 `get_shader_uniform_list` |
| 主题 | 用 `resource.create` 或 `resource.batch.apply` 编辑 `Theme`（`set_color`、`set_font_size`、`set_constant`、`set_stylebox`），`theme` 配方就是这样做的；`scene.recipe.apply` 配合 `hud`；编辑 `Control` 的锚点 | 读取 `get_color`、`get_type_list`；游戏运行时用 `runtime.ui.get` 与 `runtime.ui.press` |
| TileMap | 编辑 `TileSet` 与 `TileSetAtlasSource`（`add_source`、`create_tile`、地形）以及 `TileMapLayer`（`set_cell`、`erase_cell`、`set_cells_terrain_connect`） | 读取 `get_used_cells`、`get_cell_source_id`、`get_cell_atlas_coords` |
| 运行时控制 | `project.run.start` 与 `project.run.stop`；`input.action.send`、`input.key.send`、`input.mouse.send`、`input.sequence.send`、`input.replay` | `runtime.tree.get`、`runtime.properties.get`、`runtime.properties.set`、`runtime.wait`、`runtime.ui.get`、`runtime.screenshot.capture`；完整的测试场景用 `test.scenario.run` |
| 项目工具 | `editor.commands.call` 在连接的编辑器里运行项目在 `godot_mcp_commands.json` 中声明的命令，参数先按该命令声明的 schema 检查 | `editor.commands.list` 列出这些命令及其说明和参数 schema，不运行任何代码；调用的答复给出命令返回的值和它改动的每个文件 |

## 配方

`scene.recipe.apply` 按名字把一种常见设置加进场景，带上参数，并在答复里给出用到的每个值。它把配方展开成一批上面那样的操作，写入日志，并对照场景的 SHA-256 执行；场景正在你的编辑器里打开时，则成为一次可撤销操作。可以加进场景的配方有十个：`body-2d`、`body-3d`、`light-3d`、`particles-2d`、`navigation-2d`、`audio-2d`、`shader-2d`、`hud`、`gridmap-3d` 和 `joystick`，最后一个需要 Godot 4.7.1 或 4.7.2，这两个版本才有 `VirtualJoystick` 类。`theme` 配方生成的是资源，交给 `resource.create`。

```json
{
  "path": "scenes/fire.tscn",
  "expectedSha256": "<场景当前的 SHA-256>",
  "recipe": { "particles-2d": { "preset": "smoke" } },
  "idempotencyId": "<一个新的唯一 id>"
}
```

Agent 不看本页也能找到这些配方：每个配方同时也是一个 MCP prompt，名为 `godot-mcp.recipe.<名字>`，在 `editor` 和 `full` profile 里，正文给出配方的参数、一次调用和它展开成的操作。prompt 不占用工具名额。[配方参考](../../../operator/recipes.md)给出每个配方的调用、操作和参数。

## 其他部分

[工具参考](../../../protocol/tool-inventory.md)列出全部 132 个工具及其所属的 profile。表里没有编辑方法或配方的部分，同样可以处理：一批改动可以用 `set_property` 设置节点存储的属性，用 `set_subresource` 在其中嵌入资源；用 `--allow-scripts` 启动服务器之后，`script.run`、`editor.script.run` 和 `runtime.script.run` 可以运行 Agent 自己写的 GDScript。
