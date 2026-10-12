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

如果客户端启动进程时不带你 shell 里的 PATH，就把 `command` 写成启动器的完整路径（Linux 与 macOS 安装脚本装在 `~/.local/bin/godot-mcp`）。

## 环境变量

会给服务器传环境变量的客户端，只应传它确实要传的那些：每个以 `GODOT_MCP_` 开头的变量都是配置，服务器遇到不认识的就不会启动。`GODOT_MCP_PROFILE` 和 `GODOT_MCP_LOG_SINK` 是它认识的两个，[配置参考](../../../operator/configuration.md#environment-variables)列出了全部。

## 同一台机器上的第二个进程

如果另一个本地进程需要连到同一个正在运行的服务器，请打开回环 HTTP，而不是再启动一个服务器：见 [HTTP](../operate/http.md)。
