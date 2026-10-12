# 参与开发

本页面向 godot-mcp 本身的开发：仓库的结构、改动必须通过的检查，以及发布和本站是怎样产生的。

## 仓库

godot-mcp 是一个 Rust workspace；`rust-toolchain.toml` 钉住 Rust 1.96.0，每个直接依赖都钉在确切版本上。

| 路径 | 内容 |
| --- | --- |
| `crates/godot-mcp-domain/` | 契约、错误分类、带类型的 Godot 值、配置与文件系统权限 |
| `crates/godot-mcp-analysis/` | GDScript、场景与资源的静态分析以及语义图 |
| `crates/godot-mcp-engine/` | 受控的 Godot 执行、语言服务器与调试适配器客户端、监督与限定 |
| `crates/godot-mcp-bridge/` | 编辑器桥接协议、编解码器以及随包提供的 GDScript 插件 |
| `crates/godot-mcp-server/` | MCP 内核、策略、日志、调度器、会话、HTTP 与诊断 |
| `crates/godot-mcp-cli/` | `godot-mcp` 二进制、打包、发布门禁与运维修复 |
| `schemas/` | 服务器发布的每个契约的 JSON schema |
| `tests/fixtures/godot-releases.lock.json` | 项目支持的每个 Godot 版本，以及官方下载地址和摘要 |
| `docs/` | 参考文档；`docs/site/` 就是本站 |

[`AGENTS.md`](../../../AGENTS.md) 是每个改动都要遵守的规则，人和编码 Agent 都一样：失败时关闭、stdout 只留给 MCP 帧、不开放未认证的监听、依赖版本精确、Godot 版本只来自锁文件。

## 构建与测试

```sh
git clone https://github.com/sunerpy/godot-mcp.git
cd godot-mcp
make hooks    # pre-commit 时格式化，pre-push 时运行 make lint test
make check    # 格式检查、clippy、工作区测试、锁定构建、cargo-deny
```

采用测试先行：先写会失败的测试，读懂它为什么失败，再让它通过。桥接测试套件里的 GDScript 编解码检查使用 `GODOT_MCP_GODOT_BINARY` 指定的 Godot，没有时打印一行 `SKIP`；`make godot-test` 针对一个按摘要获取并校验的确切锁定版本运行引擎兼容性套件。

CI 在各自的运行器上运行发布门禁目录（`make release-gate-specs`）里的每一项检查——工作区检查、Godot 引擎矩阵、显示检查、Linux、macOS 与 Windows 上的打包冒烟，以及 Docker 洁净室隔离检查——再由发布门禁汇总程序合并全部观察结果，只要有一项必需检查缺失或失败就判为失败。工作区测试还会在 macOS 和 Windows 上原生运行。`CI Success` 要求以上全部通过。

## 拉取请求与发布

- 拉取请求的标题遵循 Conventional Commits，例如 `fix(engine): stop a session whose state directory was removed`；Release Please 根据它们决定下一个版本号。
- 拉取请求以 squash 方式合入 `main`，不直接推送 `main`。
- Release Please 会保持一个发布拉取请求。合并它会给发布提交打上标签；同一次运行会等待这个提交上的 `CI Success`，在每个平台上为该平台构建归档，针对锁定的 Godot 版本冒烟测试，带着 `SHA256SUMS.txt` 和构建证明上传归档，核对远端字节，最后才把发布公开。

## 本站

本站的页面在 `docs/site/`，英文在根目录、中文在 `zh/`，与它们描述的代码放在一起，这样改变用户所见的改动会在同一个拉取请求里更新页面。[`docs/site/README.md`](../README.md) 列出了这些页面的规则。本站的主题和构建在 [sunerpy/firlab](https://github.com/sunerpy/firlab) 里，每次 `main` 有改动后，它会把页面复制过去。
