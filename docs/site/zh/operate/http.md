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

## 一致性

HTTP 调用的是与 stdio 相同的内核：同样的工具、同样的 profile、同样的日志、同样的答复。HTTP 客户端能做的，同一个服务器的 stdio 客户端都能做，反过来也一样。
