---
description: 什么时候通过 HTTP 提供 godot-mcp、两种模式怎样认证请求，以及一个会话能持续多久。
---

# HTTP

stdio 是默认方式，也是首选：一个客户端、一个进程、没有套接字、没有凭据。只有当第二个进程确实需要连到一个已经在运行的服务器时，才打开 HTTP。监听器总是绑定回环地址，每个请求都要认证：能连上套接字并不能证明调用方是谁。

## 两种模式

| 模式 | 用途 | 请求被接受的条件 |
| --- | --- | --- |
| `trusted_local` | 同一用户运行的另一个本地进程 | 带着本次启动的密钥，并且 `Host` 和 `Origin` 与配置完全一致 |
| `proxied_oidc` | 经由本机反向代理访问的另一台主机上的客户端 | 经过代理转发、带着代理自己的凭据，并携带那一个已配置用户的有效令牌 |

## `trusted_local`

密钥在每次启动时重新生成，通过环境变量或受保护的文件交给服务器，从不写进配置文件：

<!-- docs-check:config expect="accept" -->
```toml
transport = "http"

[http]
bind = "127.0.0.1:8765"
mode = "trusted_local"
host = "127.0.0.1:8765"
origin = "http://127.0.0.1:8765"

[http.local_secret.env]
var = "GODOT_MCP_LAUNCH_SECRET"
```

用 `godot-mcp serve --project /path/to/my-game --config /path/to/godot-mcp.toml` 启动服务器，MCP 端点是监听地址上的 `/mcp`。加载器会拒绝非回环的 `bind`，所以这种模式的任何配置都无法从另一台主机访问。密钥怎样产生、作用范围有多大，见 [`trusted_local` 参考](../../../operator/http-trusted-local.md)。

## `proxied_oidc`

反向代理负责终结 TLS 并认证用户；服务器仍然只监听回环地址，只接受带着代理凭据的请求，并按你配置的签发方、受众、权限范围和签名密钥自己校验用户的令牌。令牌校验会拒绝未签名的令牌、意外的算法、令牌自带的密钥，以及任何私有或被重定向的密钥地址。完整配置以及你需要自己承担的残余风险见 [`proxied_oidc` 参考](../../../operator/http-proxied-oidc.md)。

## 会话

客户端用第一个请求打开一个 MCP 会话，之后的每个请求都带上这个会话。会话在以下两个时限中先到的那个结束：

| `[http]` 中的键 | 结束这样的会话 | 未设置时 |
| --- | --- | --- |
| `session_idle_timeout_ms` | 在这么多毫秒内既没有收到也没有发出任何消息 | 一小时 |
| `session_lifetime_ms` | 从打开起已过这么多毫秒，无论它有多忙 | `[limits]` 中的 `deadline_ms` × `max_queue`，默认限额下为 32 分钟 |

会话结束后，服务器对它答复 HTTP 404，客户端随即按 MCP 规范的要求打开一个新会话。旧会话产生的截取结果和导出产物在新会话中仍可读取，因为它们属于客户端的身份，而不属于某一个会话：`trusted_local` 下是本次启动的密钥，`proxied_oidc` 下是配置的那个用户。它们在保留期内一直可读，默认 15 分钟。`trusted_local` 下重启服务器会生成新的密钥，所以新一次启动的客户端无法读取上一次启动截取的内容。

使用默认值时，会话寿命先到期。如果会话需要持续一小时，请同时设置这两个键：

<!-- docs-check:config expect="accept" -->
```toml
transport = "http"

[http]
bind = "127.0.0.1:8765"
mode = "trusted_local"
host = "127.0.0.1:8765"
origin = "http://127.0.0.1:8765"
session_idle_timeout_ms = 3600000
session_lifetime_ms = 3600000

[http.local_secret.env]
var = "GODOT_MCP_LAUNCH_SECRET"
```

更长的寿命不会延长凭据的有效性：每个请求在查找会话之前，仍要先证明它的密钥或令牌。两个键的完整说明见[配置参考](../../../operator/configuration.md#session-timing)。

godot-mcp 0.3.0 及更早的版本在会话五分钟没有消息后就关闭它，并且只允许产生产物的那个会话读取产物。客户端如果在两次操作之间重新连接，就无法读取自己的截取结果；拒绝时答复的是 `-32601`（方法不存在），有些客户端会把它报告为服务器不支持读取资源。

## 一致性

HTTP 调用的是与 stdio 相同的内核：同样的工具、同样的 profile、同样的日志、同样的答复。HTTP 客户端能做的，同一个服务器的 stdio 客户端都能做，反过来也一样。
