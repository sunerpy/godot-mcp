---
description: 在 Claude Code、Codex、OpenCode、Pi Agent 或用 JSON 配置的客户端里注册 godot-mcp，并设置它的环境变量。
---

# MCP 客户端

godot-mcp 是一个普通的 stdio MCP 服务器。任何能启动进程、并在它的标准输入输出上收发按行分隔的 JSON-RPC 的客户端都能用它，这也是首选的方式：不开端口、不需要凭据、不走网络。你用哪个编辑器或 IDE 无关紧要，客户端连接的是 `godot-mcp` 进程，而不是 Godot 编辑器。

## 命令

所有客户端用的都是同一条命令：

```sh
godot-mcp serve --project /path/to/my-game --profile core
```

| 参数 | 含义 |
| --- | --- |
| `--project` | 项目里的某个目录；服务器从它向上寻找 `project.godot`，找到真正的根目录 |
| `--profile` | `core`（默认）、`editor`、`runtime` 或 `full` |
| `--config` | 可选的配置文件，用来设置 `godot_binary`、限额、日志或 HTTP |
| `--workspace` | 代替 `--project`：不带项目启动，让客户端创建或绑定一个项目 |

一个服务器只服务一个项目。要同时处理两个项目，就注册两个服务器。

## Claude Code

```sh
claude mcp add godot-mcp -- godot-mcp serve --project /path/to/my-game --profile full
```

## Codex

```sh
codex mcp add godot-mcp -- godot-mcp serve --project /path/to/my-game --profile full
```

这条命令把服务器写进 Codex 的 `config.toml`（位于 `~/.codex`，设置了 `$CODEX_HOME` 时位于该目录）；`codex mcp list` 会列出它。

## OpenCode

OpenCode 从项目里的 `opencode.json` 或它的全局配置文件（`~/.config/opencode/opencode.json`）读取服务器：

```json
{
  "mcp": {
    "godot-mcp": {
      "type": "local",
      "command": ["godot-mcp", "serve", "--project", "/path/to/my-game", "--profile", "full"],
      "enabled": true
    }
  }
}
```

`opencode mcp list` 会显示它是否已连上。

## Pi Agent

```sh
pi mcp add godot-mcp -- godot-mcp serve --project /path/to/my-game --profile full
```

这条命令把服务器写进 `~/.pi/agent/mcp.json`，所有项目都会读取它。加上 `-l` 则写进项目的 `.pi/mcp.json`，Pi Agent 只在你已信任的项目里读取这个文件。`pi mcp list` 会显示服务器是否已连上，以及它列出的工具。

## 用 JSON 配置的客户端

Claude Desktop、Cursor 和大多数客户端读取下面这种形状的 JSON：

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

如果客户端启动进程时不带你 shell 里的 PATH，就把 `command` 写成启动器的完整路径，即 `command -v godot-mcp` 输出的路径：Linux 与 macOS 安装脚本把启动器装在你主目录下的 `.local/bin/godot-mcp`。路径要完整写出，因为客户端不经过 shell 运行 `command`，不会展开其中的 `~`。

## 环境变量

会给服务器传环境变量的客户端，只应传它确实要传的那些：每个以 `GODOT_MCP_` 开头的变量都是配置，服务器遇到不认识的就不会启动。`GODOT_MCP_PROFILE` 和 `GODOT_MCP_LOG_SINK` 是它认识的两个，[配置参考](../../../operator/configuration.md#environment-variables)列出了全部。

## 同一台机器上的第二个进程

如果另一个本地进程需要连到同一个正在运行的服务器，请打开回环 HTTP，而不是再启动一个服务器：见 [HTTP](../operate/http.md)。

## 另一台机器上的客户端

如果 Agent 运行在开发网络里的另一台机器上，用 `--http 0.0.0.0:8765` 启动服务器，再把服务器打印的 URL 交给客户端：见 [HTTP](../operate/http.md#open)。

## Agent Skills

要让 Agent 也用上 godot-mcp 的 Agent Skills，运行 `godot-mcp skill install`；每个客户端该用哪个 target，见 [Agent Skills](skills.md)。
