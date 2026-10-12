# 故障排查

先看 `health.get`：它会列出每个尚未就绪的组件，附带错误码和补救办法。下面是最常遇到的问题。

## 工具已列出，却答复 `unavailable`

profile 列出了这个工具，但它需要的东西此刻不在。答复会说明缺的是哪一个：

| `health.get` 里的码 | 含义 | 处理 |
| --- | --- | --- |
| `godot_binary_absent` | 没有找到 Godot | 安装一个，并在配置文件里设置 `godot_binary` |
| `game_not_running` | 没有托管游戏会话在运行 | 用 `project.run.start` 启动一个 |
| `editor_not_attached` | 没有装了插件的编辑器连接 | 用 `editor.addon.install` 安装插件，并在 Godot 里打开项目 |

带窗口的会话还需要显示环境：Linux 上服务器的环境里要有 `DISPLAY` 或 `WAYLAND_DISPLAY`，macOS 上要在已登录用户的会话里，Windows 上要在已登录的桌面上。

## 服务器无法启动

- `unknown godot-mcp environment variable`——每个以 `GODOT_MCP_` 开头的变量都是配置。删掉报出的那个，或者按[配置参考](../../../operator/configuration.md#environment-variables)的写法拼写。
- `startup failed`——无法获取项目的租约，或者无法打开它的日志。可能是另一个 `godot-mcp` 已经在服务这个项目（同一个目录的不同写法也算同一个项目），项目路径中有一段是符号链接（会被拒绝），或者你传入的路径及其上级目录里都没有 `project.godot`。
- `[http] is forbidden when the final transport is stdio`——stdio 配置里出现 HTTP 段会被当作含义不清而拒绝；删掉它，或者设置 `transport = "http"`。

`godot-mcp check-config --config <file>` 不启动服务器，只运行同样的检查。

## 写入答复 `conflict`

文件在被读取之后改变了——它的 SHA-256 不再与 `expectedSha256` 一致——或者已连接的编辑器占着它。重新读取文件（`file.get`、`project.files.list` 或 `scene.tree.get`），判断之后再对照新的哈希发送改动。不要盲目重试：一个改动正是这样覆盖掉另一个改动的。

## 答复是 `uncertain`

对分析来说，是代码让服务器无法确定——通过没有类型的变量调用、用字符串查找节点——答复会列出所有候选。对写入来说，是结果无法证实；服务器会在下一次写入之前核清它，所以重试之前先读一遍项目。

## 很多请求同时失败

`operational limit subject_concurrency was exceeded` 表示客户端同时发出的请求超过了每个调用方的上限。一次只发一个请求，等到答复再发下一个。

## 崩溃后写入被拒绝

服务器无法证明的事务会被隔离，写入闸门随之关闭；见[恢复](recovery.md)。
