# 快速开始

本页在 MCP 客户端里注册 godot-mcp、发出第一批调用，然后给服务器指定一个 Godot。这里假设 `godot-mcp` 已经[安装](install.md)并且在 PATH 上。

## 在客户端里注册

客户端启动服务器时传入项目目录（放 `project.godot` 的目录，或它下面的任意目录）和一个 profile。以 Claude Code 为例：

```sh
claude mcp add godot-mcp -- godot-mcp serve --project /path/to/my-game
```

其他会启动进程的客户端都用同样的命令和参数，多数接受下面这种写法（更多见 [MCP 客户端](clients.md)）：

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

不写 `--profile` 时，服务器以 `core` 启动，只读项目，什么都不改。

## 发出第一批调用

向 Agent 提关于项目的问题——「主场景里有什么」「谁调用了 `take_damage`」。一个规矩的 Agent 会先调两个工具：

- `capabilities.get` 列出每个 profile 的工具和资源。
- `health.get` 给出服务器当前运行的 profile，并说明这台机器此刻能提供什么：是否找到了 Godot、是否有游戏或编辑器在运行、日志和请求队列的状态；缺什么都附有补救办法。

接下来是分析工具：`project.info.get`、`scene.tree.get`、`gdscript.symbols.get`、`gdscript.references.get`、`gdscript.impact.get`、`project.dependencies.get`。它们从文件给出答案，所以没装 Godot 的机器得到的答案也一样。每个答复都带着来源和确定程度；无法确定时，分析会答复 `inferred` 或 `uncertain`，而不是去猜。

一次只发一个请求，等到答复再发下一个。服务器限制了每个调用方的并发请求数，一次灌入一批的客户端会对超出上限的那些收到 `operational limit subject_concurrency was exceeded`。

## 给它一个 Godot

会运行 Godot 的工具需要 Godot 4.4.0 或更新的编辑器版本。服务器会在常见安装位置和 `PATH` 上查找；要指定某一个，就把它写进配置文件：

<!-- docs-check:config expect="accept" -->
```toml
transport = "stdio"
godot_binary = "/opt/godot/godot"
```

再用 `--config` 传入这个文件，并选一个列出你想用的工具的 profile：

```sh
claude mcp add godot-mcp -- godot-mcp serve --project /path/to/my-game --profile full --config /path/to/godot-mcp.toml
```

`godot-mcp check-config --config /path/to/godot-mcp.toml` 运行与服务器相同的加载器和全部检查，并说明一个文件为什么被拒绝。之后 `health.get` 会报告它解析到的 Godot、版本以及探测过哪些能力。所有配置键见[配置参考](../../../operator/configuration.md)。

## 选择 profile

| Profile | 增加的能力 | 适用场景 |
| --- | --- | --- |
| `core` | 发现与分析 | 你只要答案，什么都不改 |
| `editor` | 文件、脚本、场景、资源、设置、资产、编辑器插件 | 你要修改项目 |
| `runtime` | 运行、测试、导出与观察游戏 | 你要运行并观察项目 |
| `full` | 以上全部 | 以上都要 |

`editor` 或 `full` 下的每次写入都是一个写入日志的事务，对照它要替换的确切字节；下一页 [Profile 与答复](profiles.md)解释每种答复的含义。
