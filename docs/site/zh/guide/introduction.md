---
description: godot-mcp 能为处理 Godot 项目的 Agent 做什么、它怎样构成和组织，以及它不做什么。
---

# 简介

godot-mcp 是为单个 Godot 项目服务的 [Model Context Protocol](https://modelcontextprotocol.io) 服务器。你的 MCP 客户端——Claude Code、Codex CLI 或编辑器里的 Agent——把它作为本地进程启动，通过 stdio 与它对话。客户端从不直接连 Godot 编辑器，你用哪个编辑器或 IDE 也无关紧要。

## 它给 Agent 带来什么

- **不需要引擎的答案。** 项目发现与校验、GDScript 的符号、引用、调用方与影响面、场景与资源的树、依赖图，都直接从项目文件读出，所以在没装 Godot 的机器上也能用。
- **被证实、而不是被假定的改动。** 每次写入都写明它要替换的内容的 SHA-256，并在执行前写进日志。过期的修改会以 `conflict` 被拒绝；改到一半崩溃，会在下一个请求之前被核清，绝不靠猜。
- **真实的引擎。** GDScript 检查、导入、测试和导出都在你配置的 Godot 里运行，处在服务器管理的进程边界之内。
- **它自己管理的游戏与编辑器。** 它能启动游戏或无界面编辑器，发送输入，读取实时场景树，截图和录屏，并通过编辑器的语言服务器和调试适配器进行调试。
- **常见设置的配方。** `scene.recipe.apply` 按名字把物理体、灯光、粒子、导航、音效、着色器材质、HUD、网格地图、摇杆或 AnimationTree 状态机加进场景，作为一次写入日志的改动；`resource.create` 用同样的方式生成主题或 3D 材质。[覆盖范围](capabilities.md)列出项目各个部分分别用哪些工具。

## Rust 服务器，不只是编辑器插件

godot-mcp 由 Rust 和 GDScript 两部分组成，各自用在最合适的地方。服务器是一个静态的 Rust 二进制，不需要安装任何语言运行时：它负责 MCP 通信，不借助引擎就能读取 GDScript、场景和资源，维护让每次改动都可证实的日志，并启动和管控它用到的每一个 Godot 进程。GDScript 在 Godot 内部运行，而且只用在需要引擎的地方：

- **无界面辅助脚本**：一个按 SHA-256 固定的脚本。服务器改动的每个场景和资源，其文本都由 Godot 自己通过它写出；只有引擎才能回答的读取，也由它完成。
- **运行时探针**：服务器启动每个游戏时，通过命令行把它交给游戏。不会安装自动加载，也不会修改项目设置，所以游戏停止后无需恢复任何内容。
- **`godot_mcp` 编辑器插件**：只有当你希望把正在使用的编辑器纳入工作时才需要安装。

只做成编辑器插件的工具运行在编辑器内部：只有编辑器运行时它才能工作，也只能处理编辑器持有的内容。godot-mcp 则在项目旁边独立运行。分析和文件修改完全不需要 Godot；检查、导入、测试、导出和运行游戏只需要一个 Godot 可执行文件；插件再把你正在使用的编辑器接进来，服务器对在那里进行的修改同样保持日志、策略和恢复规则。

## 它是怎样组织的

每个工具都需要以下五种条件之一，[工具清单](../../../protocol/tool-inventory.md)为每个工具都标明了：

| 需要 | 含义 | 举例 |
| --- | --- | --- |
| `project_files` | 只需要项目文件，有没有 Godot 都行 | `scene.tree.get`、`gdscript.references.get`、`file.patch` |
| `headless_engine` | 一个 Godot 可执行文件，由服务器以无窗口方式启动 | `script.check`、`project.test`、`project.export` |
| `game_session` | 服务器用 `project.run.start` 启动的游戏 | `runtime.tree.get`、`input.key.send`、`runtime.screenshot.capture` |
| `editor_session` | 服务器用 `editor.session.start` 启动的无界面编辑器 | `debug.command` |
| `live_editor` | 你自己的 Godot 编辑器，通过 `godot_mcp` 插件连进来 | `editor.scene.tree.get`、`editor.undo` |

**profile** 决定哪些工具会被列出：`core` 只读，`editor` 增加写入和编辑器插件，`runtime` 增加运行与观察游戏，`full` 是三者之和。已列出的工具如果它需要的引擎、游戏或编辑器此刻不在，就答复 `unavailable` 并说明缺的是什么。所以客户端可以信任工具列表，再从答复里了解这台机器的情况。见 [Profile 与答复](profiles.md)。

## 它不是什么

只有用 `--allow-scripts` 启动服务器后，Agent 自己写的 GDScript 才会被执行；类型化工具只调用它们那一组固定的引擎方法。godot-mcp 从不下载 Godot 或导出模板。它为一个用户服务一个项目，不是共享服务。另外，加载 Godot 项目就会运行项目里的代码，所以服务器的进程边界能限定并停止它启动的进程，却不能让一个不可信的项目变得可以放心打开——[威胁模型](../../../security/threat-model.md)写清楚了界线在哪里。

## 下一步

- [安装](install.md)二进制。
- [快速开始](quick-start.md)：在客户端里注册它，发出第一批调用。
- [覆盖范围](capabilities.md)：动画、物理、3D、粒子、导航以及项目其他部分用到的工具。
