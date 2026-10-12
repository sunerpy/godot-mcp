# 简介

godot-mcp 是为单个 Godot 项目服务的 [Model Context Protocol](https://modelcontextprotocol.io) 服务器。你的 MCP 客户端——Claude Code、Codex CLI 或编辑器里的 Agent——把它作为本地进程启动，通过 stdio 与它对话。客户端从不直接连 Godot 编辑器，你用哪个编辑器或 IDE 也无关紧要。

## 它给 Agent 带来什么

- **不需要引擎的答案。** 项目发现与校验、GDScript 的符号、引用、调用方与影响面、场景与资源的树、依赖图，都直接从项目文件读出，所以在没装 Godot 的机器上也能用。
- **被证实、而不是被假定的改动。** 每次写入都写明它要替换的内容的 SHA-256，并在执行前写进日志。过期的修改会以 `conflict` 被拒绝；改到一半崩溃，会在下一个请求之前被核清，绝不靠猜。
- **真实的引擎。** GDScript 检查、导入、测试和导出都在你配置的 Godot 里运行，处在服务器管理的进程边界之内。
- **它自己管理的游戏与编辑器。** 它能启动游戏或无界面编辑器，发送输入，读取实时场景树，截图和录屏，并通过编辑器的语言服务器和调试适配器进行调试。

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

任何 profile 里都没有执行任意 GDScript 或调用任意引擎方法的工具。godot-mcp 从不下载 Godot 或导出模板。它为一个用户服务一个项目，不是共享服务，也不是远程服务。另外，加载 Godot 项目就会运行项目里的代码，所以服务器的进程边界能限定并停止它启动的进程，却不能让一个不可信的项目变得可以放心打开——[威胁模型](../../../security/threat-model.md)写清楚了界线在哪里。

## 下一步

- [安装](install.md)二进制。
- [快速开始](quick-start.md)：在客户端里注册它，发出第一批调用。
