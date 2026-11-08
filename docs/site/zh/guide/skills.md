---
description: 把 godot-mcp 的 Agent Skills 装给 Claude Code、Codex、VS Code、Cursor、Gemini CLI、Kiro 或 OpenCode，并保持最新。
---

# Agent Skills

godot-mcp 的每个工具只说明它自己。几个工具怎样配合完成一件事，比如先读取场景再修改，或者启动游戏、等它就绪、按下按钮、最后把它停掉，写在 godot-mcp 的 Agent Skills 里：这些说明文件只在任务需要时才由 Agent 加载，其余时候不占用 Agent 的上下文。

| Skill | Agent 在这些时候加载它 |
| --- | --- |
| `godot-mcp` | 通过 godot-mcp 读取或修改你的项目 |
| `godot-mcp-export` | 导出、构建或打包游戏 |
| `godot-mcp-playtest` | 运行、试玩或测试游戏 |
| `godot-mcp-setup` | 安装、连接或排查 godot-mcp |

## 安装

```sh
godot-mcp skill install
```

这条命令在你的主目录里查找各个 Agent 的配置目录（例如 Claude Code 的 `~/.claude`），把四个 skill 复制进找到的每个 Agent 的 skill 目录；一个都没找到时，装给 Claude Code。每个 skill 只有一个 `SKILL.md` 文件，由 `godot-mcp` 二进制自带，所以装上的总是与这个二进制同一版本的 skill。

要指定 Agent，就用 `--target` 写出它们的名字：

```sh
godot-mcp skill install --target claude,codex
```

| 客户端 | `--target` |
| --- | --- |
| Claude Code | `claude` |
| Codex | `codex` |
| VS Code（GitHub Copilot） | `claude` 或 `codex` |
| Cursor | `cursor` |
| Gemini CLI | `gemini` |
| Kiro | `kiro` |
| OpenCode | `opencode` |

只装你用得到的 target。有些客户端会读取不止一个 skill 目录，装了多个 target 时，Agent 可能把同一个 skill 列出两次。

如果希望 skill 只随某一个项目，就在项目根目录里加 `--local` 运行。skill 会放进 `.claude/skills` 这类隐藏目录，Godot 不会导入其中的文件：

```sh
cd /path/to/my-game
godot-mcp skill install --target claude --local
```

## Agent 运行在另一台机器上

skill 要装在运行 Agent 的机器上。Agent 从另一台机器通过 HTTP 连接 godot-mcp 时，在那台机器上运行 `godot-mcp skill install`，并使用与服务器相同版本的 godot-mcp。这条命令在那里既不需要项目，也不需要 Godot。

## 保持最新

更新 godot-mcp 之后，skill 也要随之更新：

```sh
godot-mcp skill status
godot-mcp skill update --dry-run --diff
godot-mcp skill update
```

`status` 报告每个 skill 是未安装、最新、可更新还是已改动，不做任何修改。`update --dry-run --diff` 列出更新会改动的内容，不写入任何文件。

你自己改过的 skill 会保持原样：`install` 和 `update` 跳过它并列出来，`uninstall` 也保留它。`godot-mcp skill update --force` 会用当前版本替换你改过的那份。

```sh
godot-mcp skill uninstall
```

这条命令删除 godot-mcp 写入的 skill，目录变空后一并删除。

这些命令遵循的全部规则见参考页 [Agent Skills](../../../operator/skills.md)（英文）。godot-mcp 0.6.0 之前的版本没有 `skill` 命令。
