<div align="center">

# godot-mcp

**面向 Godot 项目的本地 Model Context Protocol 服务器。**

静态分析 GDScript、场景与资源，以事务方式改动项目，并启动、操控、截取和调试它自己管理的 Godot 游戏与编辑器——全部由一个跑在项目旁边的静态 Rust 二进制完成。

[![CI](https://github.com/sunerpy/godot-mcp/actions/workflows/ci.yml/badge.svg)](https://github.com/sunerpy/godot-mcp/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/sunerpy/godot-mcp)](https://github.com/sunerpy/godot-mcp/releases)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](../../LICENSE-MIT)

[English](../../README.md) · [**简体中文**](README.zh-CN.md) ·
[网站](https://firlab.app/godot-mcp/zh/) · [文档](../README.md) ·
[安装](#安装) · [快速开始](#快速开始) · [参与贡献](../../CONTRIBUTING.md)

</div>

---

## 为什么用 godot-mcp

MCP 客户端只和一个本地 `godot-mcp` 进程对话，从不直接连 Godot 编辑器；这个进程在你的项目里像一个谨慎的协作者那样做事：先读后写，做了什么就证明什么，做不到就直说。

- **不需要引擎的分析。** 项目发现与校验、GDScript 的符号、引用与影响面、文本格式场景与资源的树、依赖图，在没装 Godot 的机器上都能给出答案。
- **改动即事务。** 文件与脚本补丁带上被改内容的预期 SHA-256；场景与资源的批量改动写入日志。改到一半进程崩溃，下一个请求被服务之前，它会被核对为已证实的改前或改后状态，或者被隔离。
- **真实引擎，版本钉死。** 校验、导入、测试与导出在你配置的 Godot 上执行；CI 让每次发布都跑过锁文件里从 4.4.1 到 4.7.1 的每个确切版本。
- **它自己管理的游戏与编辑器。** 启动游戏或编辑器，发送键盘、鼠标、动作与序列输入，读取运行中的场景树、属性、信号与 UI，截图与录制，并通过编辑器的语言服务器和调试适配器单步调试。
- **宁可拒绝，也不猜。** 二进制 `.scn` 返回 `unsupported` 并附上补救办法（另存为文本格式）；无法证实的结果返回 `uncertain`。没有任何工具能执行任意 GDScript。
- **本地优先。** 默认走 stdio，没有端口，也没有凭据。可选的回环 HTTP 只接受每次启动生成的密钥，或经过认证的代理转发。
- **可以自证的发布包。** Linux 用 musl 静态链接，Windows 用 `crt-static`；包里的 `MANIFEST.json` 供二进制自检，附 CycloneDX SBOM；发布归档带 GitHub 签发的构建证明。

## 安装

### 发布安装脚本

安装脚本会挑出与你的平台对应的归档，只有当它的摘要与发布里的 `SHA256SUMS.txt` 一致时才继续，然后解压整个包、运行包自带的 `self-check`，再把 `godot-mcp` 放进 PATH。

```sh
# Linux x86_64、Apple 芯片的 macOS
curl -fsSL https://raw.githubusercontent.com/sunerpy/godot-mcp/main/scripts/install.sh | sh
```

```powershell
# Windows x86_64，PowerShell 5.1 及以上
irm https://raw.githubusercontent.com/sunerpy/godot-mcp/main/scripts/install.ps1 | iex
```

用 `GODOTMCP_VERSION=vX.Y.Z` 固定版本。包安装到 `~/.local/share/godot-mcp`（Windows 上是 `%LOCALAPPDATA%\godot-mcp`）；所有设置都写在脚本开头的注释里。

### 发布归档

| 平台                | 目标三元组                  | 归档      |
| ------------------- | --------------------------- | --------- |
| Linux x86_64        | `x86_64-unknown-linux-musl` | `.tar.gz` |
| macOS Apple 芯片    | `aarch64-apple-darwin`      | `.tar.gz` |
| Windows x86_64      | `x86_64-pc-windows-msvc`    | `.zip`    |

每个归档就是一个包目录：`bin/godot-mcp` 和旁边的进程监督程序、GDScript 桥接资产、示例配置、许可证、SBOM 以及完整的 schema 树。不要把它们拆开——服务器在自己的可执行文件旁边找监督程序和资产。校验解压出来的包，以及归档的构建来源：

```sh
godot-mcp self-check --package /path/to/godot-mcp-X.Y.Z-x86_64-unknown-linux-musl
gh attestation verify godot-mcp-X.Y.Z-x86_64-unknown-linux-musl.tar.gz --repo sunerpy/godot-mcp
```

### 从源码构建

要用尚未发布的提交，或者你的平台没有现成的归档，就在当前机器上构建包（`rust-toolchain.toml` 钉住 Rust 1.96.0；Linux 需要 `musl-tools`）：

```sh
git clone https://github.com/sunerpy/godot-mcp.git
cd godot-mcp
make dist        # 输出 target/release-dist/godot-mcp-<版本>-<目标>.<扩展名>
```

## 快速开始

让 MCP 客户端启动这个二进制，并指向项目目录（放 `project.godot` 的目录，或它下面的任意目录）：

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

`core` 是只读的。profile 要有意识地选：

| profile   | 增加的能力                                         |
| --------- | -------------------------------------------------- |
| `core`    | 发现与分析，项目里什么都不会改                     |
| `editor`  | 编写：文件、脚本、场景、资源、项目设置、资产       |
| `runtime` | 运行、测试、导出与观察游戏                         |
| `full`    | 以上全部                                           |

依赖引擎的工具需要一个 Godot 可执行文件。服务器会在常见安装位置和 `PATH` 上查找；要指定某一个，就在通过 `--config` 传入的配置文件里设置 `godot_binary`（见[配置](../operator/configuration.md)）。每个 profile 包含哪些工具，见生成的[工具清单](../protocol/tool-inventory.md)；HTTP 与反向代理的接法见 [MCP 客户端](../operator/mcp-clients.md)。

## 配合 LLM 使用

先按[安装](#安装)一节装好，再像上面那样在客户端里注册服务器，其余的让 Agent 自己发现：

<details>
<summary>给 Agent 的速查</summary>

- 先调 `capabilities.get` 和 `health.get`：它们说明当前 profile、这台主机能跑什么（引擎、监督程序、显示环境），以及哪里不可用、为什么。
- 一次只发一个请求，等到答复再发下一个。每个主体的并发上限很小，一次灌入一批会收到 `operational limit subject_concurrency was exceeded`。
- 每个会改动项目的调用都带 `idempotencyId`；用同一个 id 重复调用，拿到的是记录下来的结果，而不会再执行一次。
- 答复要么是结果，要么是带 `code` 的结构化错误（`invalid_input`、`not_found`、`conflict`、`unsupported`、`unavailable`、`uncertain` 等）。`uncertain` 表示效果可能发生也可能没发生；重试之前先读一遍现状。
- stdout 只承载 MCP 帧，诊断信息走 stderr。

</details>

## 文档

| 主题 | 页面 |
| --- | --- |
| 安装、配置并发出第一个请求 | [快速开始：本地 stdio](../operator/quickstart-stdio.md) |
| 每个能力的请求与实测答复 | [能力实操与验证复现](../operator/usage-and-verification.md) |
| 它运行的游戏与编辑器、输入、截取、调试 | [托管会话](../operator/managed-sessions.md) |
| profile、答复、`unavailable` 与 `uncertain` | [Profile 与工具答复](../operator/profiles-and-outcomes.md) |
| 崩溃、遗留进程、隔离与修复 | [恢复](../operator/recovery.md) |
| 信任边界与残余风险 | [威胁模型](../security/threat-model.md) |

完整索引在 [docs/README.md](../README.md)；网站 <https://firlab.app/godot-mcp/zh/> 提供中英文的使用指南。

## 开发

```sh
make help     # 列出所有目标
make check    # fmt-check、clippy、工作区测试、锁定构建、cargo-deny
make dist     # 当前平台的发布归档
```

开发采用测试先行；门禁见 [`CONTRIBUTING.md`](../../CONTRIBUTING.md) 与[贡献者工作流程](../contributor/workflow.md)。CI 运行发布门禁目录里的每一项检查、Linux、macOS 与 Windows 上的打包冒烟，以及 macOS 与 Windows 上的原生工作区测试；必需检查是 `CI Success`。

## 许可证

MIT。见 [LICENSE-MIT](../../LICENSE-MIT) 与 [THIRD-PARTY-NOTICES.md](../../THIRD-PARTY-NOTICES.md)。
