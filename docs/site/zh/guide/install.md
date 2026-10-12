# 安装

godot-mcp 每个平台一个包：`godot-mcp` 可执行文件和旁边的进程监督程序、它会装进项目的资产、一份示例配置、许可证、SBOM，以及完整的 schema 树。请保持包的完整，服务器会在自己的可执行文件旁边寻找监督程序和资产。

## 发布安装脚本

安装脚本会挑出你的平台对应的归档，只有当它的摘要与发布里的 `SHA256SUMS.txt` 一致时才继续，然后解压整个包、运行包自带的 `self-check`，再把 `godot-mcp` 放进 PATH。

::: code-group

```sh [Linux 与 macOS]
curl -fsSL https://raw.githubusercontent.com/sunerpy/godot-mcp/main/scripts/install.sh | sh
```

```powershell [Windows]
irm https://raw.githubusercontent.com/sunerpy/godot-mcp/main/scripts/install.ps1 | iex
```

:::

在 Linux 和 macOS 上，包装到 `~/.local/share/godot-mcp`，`godot-mcp` 启动器写到 `~/.local/bin`；在 Windows 上，包装到 `%LOCALAPPDATA%\godot-mcp`，它的 `bin` 目录会加进用户 PATH。每个脚本开头都列出了它的设置：

| 设置 | 作用 |
| --- | --- |
| `GODOTMCP_VERSION` | 安装指定的发布版本，例如 `v0.2.0`，而不是最新版 |
| `GODOTMCP_INSTALL_ROOT` | 把包解压到这里 |
| `GODOTMCP_BIN_DIR` | 把启动器写到这里（Linux 与 macOS） |
| `GODOTMCP_NO_PATH` | 设为 `1` 时不修改用户 PATH（Windows） |

这些设置以 `GODOTMCP_` 开头，而不是 `GODOT_MCP_`：服务器把每个 `GODOT_MCP_` 变量都当作自己的配置，遇到不认识的就拒绝启动。

## 发布归档

| 平台 | 目标三元组 | 归档 |
| --- | --- | --- |
| Linux x86_64 | `x86_64-unknown-linux-musl` | `.tar.gz` |
| macOS Apple 芯片 | `aarch64-apple-darwin` | `.tar.gz` |
| Windows x86_64 | `x86_64-pc-windows-msvc` | `.zip` |

Linux 可执行文件静态链接 musl，不依赖宿主的任何库；Windows 可执行文件静态链接 C 运行时；macOS 版只链接系统自带的库。从[发布页](https://github.com/sunerpy/godot-mcp/releases)下载归档和校验文件，然后两样都检查：

```sh
sha256sum -c --ignore-missing SHA256SUMS.txt
tar -xzf godot-mcp-X.Y.Z-x86_64-unknown-linux-musl.tar.gz
godot-mcp-X.Y.Z-x86_64-unknown-linux-musl/bin/godot-mcp self-check \
  --package godot-mcp-X.Y.Z-x86_64-unknown-linux-musl
```

`self-check` 会重新计算包里 `MANIFEST.json` 列出的每个文件的大小和 SHA-256，只要有一个缺失或被改动就失败。归档还带有 GitHub 签发的构建证明，可以用 GitHub CLI 核验：

```sh
gh attestation verify godot-mcp-X.Y.Z-x86_64-unknown-linux-musl.tar.gz --repo sunerpy/godot-mcp
```

## 从源码构建

要用尚未发布的提交，或者你的平台没有现成归档（Intel 版 macOS、Linux arm64、Windows ARM64），就在要运行它的机器上构建包。`rust-toolchain.toml` 钉住了 Rust 1.96.0；在 Linux 上，静态构建还需要 `musl-tools`。

```sh
git clone https://github.com/sunerpy/godot-mcp.git
cd godot-mcp
make dist
```

`make dist` 输出 `target/release-dist/godot-mcp-<版本>-<目标>.<扩展名>` 和它的 `SHA256SUMS.txt`，像下载来的归档一样解压即可。

## Godot

分析不需要引擎。要用会运行 Godot 的工具，请自己安装 Godot 4.4.0 或更新的编辑器版本；服务器会在常见安装位置和 `PATH` 上查找，或者使用你在配置文件里写的 `godot_binary`（见[快速开始](quick-start.md#给它一个-godot)）。导出还需要这个版本对应的导出模板。

## 卸载

删除包目录和启动器（Windows 上删掉 PATH 里的那一项）。项目里还会留下 `.godot-mcp/`——服务器为这个项目保存的日志和状态——以及你安装过的 `godot_mcp/` 下的资产和 `addons/godot_mcp/` 下的插件；不再用 godot-mcp 时，把它们从项目里删掉即可。
