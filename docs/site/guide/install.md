# Install

godot-mcp is one package per platform: the `godot-mcp` executable, the process supervisor beside
it, the project assets it installs, an example configuration, the licences, an SBOM and the schema
tree. Keep the package together; the server finds its supervisor and its assets next to its own
executable.

## Release installer

The installers pick the archive for your platform, refuse it unless its digest matches the
release's `SHA256SUMS.txt`, extract the whole package, run the package's own `self-check` and put
`godot-mcp` on your PATH.

::: code-group

```sh [Linux and macOS]
curl -fsSL https://raw.githubusercontent.com/sunerpy/godot-mcp/main/scripts/install.sh | sh
```

```powershell [Windows]
irm https://raw.githubusercontent.com/sunerpy/godot-mcp/main/scripts/install.ps1 | iex
```

:::

On Linux and macOS the package goes to `~/.local/share/godot-mcp` and a `godot-mcp` launcher to
`~/.local/bin`; on Windows the package goes to `%LOCALAPPDATA%\godot-mcp` and its `bin` directory
is added to your user PATH. Each script's header lists its settings:

| Setting | Effect |
| --- | --- |
| `GODOTMCP_VERSION` | Install this release, for example `v0.2.0`, instead of the latest |
| `GODOTMCP_INSTALL_ROOT` | Extract packages here |
| `GODOTMCP_BIN_DIR` | Write the launcher here (Linux and macOS) |
| `GODOTMCP_NO_PATH` | Set to `1` to leave the user PATH alone (Windows) |

The settings are prefixed `GODOTMCP_`, not `GODOT_MCP_`: the server reads every `GODOT_MCP_`
variable as its own configuration and refuses one it does not know.

## Release archives

| Platform | Target | Archive |
| --- | --- | --- |
| Linux x86_64 | `x86_64-unknown-linux-musl` | `.tar.gz` |
| macOS Apple silicon | `aarch64-apple-darwin` | `.tar.gz` |
| Windows x86_64 | `x86_64-pc-windows-msvc` | `.zip` |

The Linux executables are statically linked against musl and need nothing from the host; the
Windows executables link the C runtime statically; the macOS ones link only what macOS provides.
Download an archive and its checksums from the
[releases page](https://github.com/sunerpy/godot-mcp/releases), then check both:

```sh
sha256sum -c --ignore-missing SHA256SUMS.txt
tar -xzf godot-mcp-X.Y.Z-x86_64-unknown-linux-musl.tar.gz
godot-mcp-X.Y.Z-x86_64-unknown-linux-musl/bin/godot-mcp self-check \
  --package godot-mcp-X.Y.Z-x86_64-unknown-linux-musl
```

`self-check` re-derives the size and SHA-256 of every file the package's `MANIFEST.json` lists,
and fails when one is missing or changed. The archives are also attested by
GitHub; with the GitHub CLI:

```sh
gh attestation verify godot-mcp-X.Y.Z-x86_64-unknown-linux-musl.tar.gz --repo sunerpy/godot-mcp
```

## Build from source

For an unreleased commit, or a platform without an archive (Intel macOS, Linux arm64, Windows
ARM64), build the package on the machine you will run it on. The toolchain is pinned to Rust
1.96.0 in `rust-toolchain.toml`; on Linux the static build also needs `musl-tools`.

```sh
git clone https://github.com/sunerpy/godot-mcp.git
cd godot-mcp
make dist
```

`make dist` writes `target/release-dist/godot-mcp-<version>-<target>.<ext>` and its
`SHA256SUMS.txt`; extract it like a downloaded archive.

## Godot

Analysis needs no engine. For the tools that run Godot, install a Godot 4.4.0 or later editor
build yourself; the server looks for one in the usual install locations and on `PATH`, or uses the
`godot_binary` you name in a configuration file ([Quick start](quick-start.md#give-it-a-godot-build)).
Exports also need the export templates of exactly that build.

## Uninstall

Delete the package directory and the launcher (or the PATH entry on Windows). A project keeps
`.godot-mcp/` — the server's journal and state for that project — and, if you installed them, the
assets under `godot_mcp/` and the add-on under `addons/godot_mcp/`; remove those from the project
when you no longer use godot-mcp with it.
