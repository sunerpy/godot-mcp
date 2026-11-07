<div align="center">

# godot-mcp

**面向 Godot 项目的本地 Model Context Protocol 服务器。**

静态分析 GDScript、场景与资源，以事务方式改动项目，并启动、操控、截取和调试它自己管理的 Godot 游戏与编辑器——全部由一个跑在项目旁边的静态 Rust 二进制完成。

[![CI](https://github.com/sunerpy/godot-mcp/actions/workflows/ci.yml/badge.svg)](https://github.com/sunerpy/godot-mcp/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/sunerpy/godot-mcp)](https://github.com/sunerpy/godot-mcp/releases)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](../../LICENSE-MIT)

[English](../../README.md) · [**简体中文**](README.zh-CN.md) ·
[网站](https://firlab.app/godot-mcp/zh/) · [文档](../README.md) ·
[安装](#安装) · [快速开始](#快速开始) · [参与贡献](../../CONTRIBUTING.md)

</div>

---

## 为什么用 godot-mcp

MCP 客户端只和一个本地 `godot-mcp` 进程对话，从不直接连 Godot 编辑器；这个进程在你的项目里像一个谨慎的协作者那样做事：先读后写，做了什么就证明什么，做不到就直说。

- **Rust 服务器，不只是编辑器插件。** 一个静态 Rust 二进制，不需要安装任何语言运行时，负责 MCP 通信、分析项目、把每次改动写入日志，并管控它启动的每一个 Godot 进程。GDScript 只在需要引擎的地方于 Godot 内部运行：一个按 SHA-256 固定的无界面辅助脚本，服务器改动的每个场景和资源都由 Godot 自己通过它写出；启动每个游戏时通过命令行交给游戏的探针，因此不会安装自动加载，也不会修改项目设置；以及 `godot_mcp` 编辑器插件，只有想把你正打开的编辑器接进来时才需要安装。即使没有打开编辑器，它照样能读取、修改、运行、测试和导出项目，分析和文件修改则完全不需要 Godot。
- **不需要引擎的分析。** 项目发现与校验、GDScript 的符号、引用与影响面、文本格式场景与资源的树、依赖图，以及项目级视图——未使用文件、引用环、信号流向、度量、内容搜索、UID 查询、跨场景的节点与导出变量查询——在没装 Godot 的机器上都能给出答案。
- **改动即事务。** 文件与脚本补丁带上被改内容的预期 SHA-256；场景与资源的批量改动写入日志。改到一半进程崩溃，下一个请求被服务之前，它会被核对为已证实的改前或改后状态，或者被隔离。一次批量改动可以在一次 Godot 运行里改动最多 16 个场景和资源，并作为一个事务全部落地，要么一个都不落地。
- **真实引擎，版本钉死。** 校验、导入、测试与导出在你配置的 Godot 上执行，只有引擎知道的事实也由它来读：已绘制的图块格子、动画关键帧、烘焙好的导航多边形、着色器的 uniform 与编译错误。导出覆盖资源包、补丁和 Android APK；`android.deploy` 通过 adb 把 APK 安装到设备上并启动，目前只对替身 adb 测过。CI 让每次发布都跑过锁文件里从 4.4.1 到 4.7.2 的每个确切版本。
- **它自己管理的游戏与编辑器。** 启动游戏或编辑器，发送键盘、鼠标、动作与序列输入并录制、回放，读取运行中的场景树、属性、信号与 UI，写入运行时属性、等待条件成立、查找和移动节点，改变游戏速度，截图与录制并与参考图比对，并通过编辑器的语言服务器和调试适配器单步调试。由输入、等待、对节点属性和 Control 文本的断言、图像比对和压力窗口组成的测试场景，最后汇总成一份报告。
- **你已经打开的编辑器。** 启用内置的 `godot_mcp` 插件后，Agent 可以查看并修改编辑器持有的场景，作为可撤销的编辑器操作，并保存、关闭它们；通过一张固定的编辑调用表画图块、给动画打关键帧，并读回只有引擎知道的值；截取编辑器画面，读取它的日志，修改编辑器的设置和快捷键，移动它的 3D 相机，回答它的对话框，在不会丢失任何内容时重启它，接收变化的文件，并从编辑器运行游戏。编辑器里的面板显示连接状态和它最近答复过的 500 个请求。
- **常见设置的配方。** `scene.recipe.apply` 按名字把十种常见设置之一加进场景——2D 或 3D 物理体、带天空和相机的灯光、六种预设的粒子（火焰、烟雾、火花、雨、雪、爆炸）、导航、位置音效、着色器材质、HUD、网格地图和屏幕摇杆——带上参数，作为一个写入日志的批量改动，或者在编辑器里作为一个可撤销的操作。主题配方交给 `resource.create`。每个配方同时也是一个 MCP prompt，名为 `godot-mcp.recipe.<名字>`，Agent 不占用工具名额就能找到它们（见[配方](../operator/recipes.md)）。
- **宁可拒绝，也不猜。** 二进制 `.scn` 返回 `unsupported` 并附上补救办法（另存为文本格式）；无法证实的结果返回 `uncertain`。
- **脚本只在你允许时运行。** 用 `--allow-scripts` 启动服务器后，Agent 可以在无头引擎、打开的编辑器或运行中的游戏里执行自己写的 GDScript，并看到它返回了什么、打印了什么、改了哪些文件。默认关闭；一旦打开，脚本不受任何限制。
- **本地优先。** 默认走 stdio，没有端口，也没有凭据。可选的 HTTP 可以只监听回环地址，凭每次启动生成的密钥访问，或放在经过认证的代理后面；也可以用 `godot-mcp serve --http 0.0.0.0:8765` 在开发网络里提供服务，不要凭据或只要一个令牌（见 [HTTP：`open`](../operator/http-open.md)）。
- **可以自证的发布包。** Linux 用 musl 静态链接，Windows 用 `crt-static`；包里的 `MANIFEST.json` 供二进制自检，附 CycloneDX SBOM；发布归档带 GitHub 签发的构建证明。

## 覆盖范围

下表列出 Godot 项目的各个部分分别用哪些工具。“编辑”指 `scene.batch.apply`、`editor.scene.edit` 或 `resource.batch.apply` 里的 `call_method` 操作，或者 `scene.create`、`resource.create` 的操作里的同类操作；“读取”指 `scene.read`、`resource.read` 或 `editor.scene.read`。每个类能调用哪些方法，见[编辑器桥](../operator/editor-bridge.md#editing-calls)。

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

`full` profile 共有 130 个工具；生成的[工具清单](../protocol/tool-inventory.md)逐个列出它们所属的 profile。

## 安装

### 发布安装脚本

安装脚本会挑出与你的平台对应的归档，只有当它的摘要与发布里的 `SHA256SUMS.txt` 一致时才继续，然后解压整个包、运行包自带的 `self-check`，再把 `godot-mcp` 放进 PATH。

```sh
# Linux x86_64、Apple 芯片的 macOS
curl -fsSL https://raw.githubusercontent.com/sunerpy/godot-mcp/main/scripts/install.sh | sh
```

```powershell
# Windows x86_64，PowerShell 5.1 及以上
irm https://raw.githubusercontent.com/sunerpy/godot-mcp/main/scripts/install.ps1 | iex
```

用 `GODOTMCP_VERSION=vX.Y.Z` 固定版本。包安装到 `~/.local/share/godot-mcp`（Windows 上是 `%LOCALAPPDATA%\godot-mcp`）；所有设置都写在脚本开头的注释里。

### 发布归档

| 平台                | 目标三元组                  | 归档      |
| ------------------- | --------------------------- | --------- |
| Linux x86_64        | `x86_64-unknown-linux-musl` | `.tar.gz` |
| macOS Apple 芯片    | `aarch64-apple-darwin`      | `.tar.gz` |
| Windows x86_64      | `x86_64-pc-windows-msvc`    | `.zip`    |

每个归档就是一个包目录：`bin/godot-mcp` 和旁边的进程监督程序、GDScript 桥接资产、示例配置、许可证、SBOM 以及完整的 schema 树。不要把它们拆开——服务器在自己的可执行文件旁边找监督程序和资产。校验解压出来的包，以及归档的构建来源：

```sh
godot-mcp self-check --package /path/to/godot-mcp-X.Y.Z-x86_64-unknown-linux-musl
gh attestation verify godot-mcp-X.Y.Z-x86_64-unknown-linux-musl.tar.gz --repo sunerpy/godot-mcp
```

### 从源码构建

要用尚未发布的提交，或者你的平台没有现成的归档，就在当前机器上构建包（`rust-toolchain.toml` 钉住 Rust 1.96.0；Linux 需要 `musl-tools`）：

```sh
git clone https://github.com/sunerpy/godot-mcp.git
cd godot-mcp
make dist        # 输出 target/release-dist/godot-mcp-<版本>-<目标>.<扩展名>
```

## 快速开始

让 MCP 客户端启动这个二进制，并指向项目目录（放 `project.godot` 的目录，或它下面的任意目录）：

```json
{
  "mcpServers": {
    "godot-mcp": {
      "command": "godot-mcp",
      "args": ["serve", "--project", "/path/to/my-game", "--profile", "core"]
    }
  }
}
```

`core` 是只读的。profile 要有意识地选：

| profile   | 增加的能力                                         |
| --------- | -------------------------------------------------- |
| `core`    | 发现与分析，项目里什么都不会改                     |
| `editor`  | 编写：文件、脚本、场景、资源、项目设置、资产，以及已连接的编辑器 |
| `runtime` | 运行、测试、导出与观察游戏，也可以从已连接的编辑器运行           |
| `full`    | 以上全部                                           |

依赖引擎的工具需要一个 Godot 可执行文件。服务器会在常见安装位置和 `PATH` 上查找；要指定某一个，就在通过 `--config` 传入的配置文件里设置 `godot_binary`（见[配置](../operator/configuration.md)）。每个 profile 包含哪些工具，见生成的[工具清单](../protocol/tool-inventory.md)；HTTP 与反向代理的接法见 [MCP 客户端](../operator/mcp-clients.md)。

## 配合 LLM 使用

先按[安装](#安装)一节装好，再像上面那样在客户端里注册服务器，其余的让 Agent 自己发现。要让 Agent 也用上 godot-mcp 的 Agent Skills，就把它们复制进 Agent 的 skill 目录（见 [Agent Skills](../operator/skills.md)）：

```sh
godot-mcp skill install
```

<details>
<summary>给 Agent 的速查</summary>

- 先调 `capabilities.get` 和 `health.get`：它们说明当前 profile、这台主机能跑什么（引擎、监督程序、显示环境），以及哪里不可用、为什么。
- 一次只发一个请求，等到答复再发下一个。每个主体的并发上限很小，一次灌入一批会收到 `operational limit subject_concurrency was exceeded`。
- 每个会改动项目的调用都带 `idempotencyId`；用同一个 id 重复调用，拿到的是记录下来的结果，而不会再执行一次。
- 答复要么是结果，要么是带 `code` 的结构化错误（`invalid_input`、`not_found`、`conflict`、`unsupported`、`unavailable`、`uncertain` 等）。`uncertain` 表示效果可能发生也可能没发生；重试之前先读一遍现状。
- stdout 只承载 MCP 帧，诊断信息走 stderr。

</details>

## 文档

| 主题 | 页面 |
| --- | --- |
| 安装、配置并发出第一个请求 | [快速开始：本地 stdio](../operator/quickstart-stdio.md) |
| 每个能力的请求与实测答复 | [能力实操与验证复现](../operator/usage-and-verification.md) |
| 它运行的游戏与编辑器、输入、截取、调试 | [托管会话](../operator/managed-sessions.md) |
| 操作你已经打开的编辑器 | [编辑器桥](../operator/editor-bridge.md) |
| 常见设置的配方，按名字套用 | [配方](../operator/recipes.md) |
| 从开发网络里的另一台机器连接 | [HTTP：`open`](../operator/http-open.md) |
| profile、答复、`unavailable` 与 `uncertain` | [Profile 与工具答复](../operator/profiles-and-outcomes.md) |
| 崩溃、遗留进程、隔离与修复 | [恢复](../operator/recovery.md) |
| 信任边界与残余风险 | [威胁模型](../security/threat-model.md) |

完整索引在 [docs/README.md](../README.md)；网站 <https://firlab.app/godot-mcp/zh/> 提供中英文的使用指南。

## 开发

```sh
make help     # 列出所有目标
make check    # fmt-check、clippy、工作区测试、锁定构建、cargo-deny
make dist     # 当前平台的发布归档
```

开发采用测试先行；门禁见 [`CONTRIBUTING.md`](../../CONTRIBUTING.md) 与[贡献者工作流程](../contributor/workflow.md)。CI 运行发布门禁目录里的每一项检查、Linux、macOS 与 Windows 上的打包冒烟，以及 macOS 与 Windows 上的原生工作区测试；必需检查是 `CI Success`。

## 许可证

MIT。见 [LICENSE-MIT](../../LICENSE-MIT) 与 [THIRD-PARTY-NOTICES.md](../../THIRD-PARTY-NOTICES.md)。
