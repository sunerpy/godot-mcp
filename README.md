<div align="center">

# godot-mcp

**A local Model Context Protocol server for Godot projects.**

Static analysis of GDScript, scenes and resources; journaled edits; and Godot
games and editors it starts, drives, captures and debugs — one static Rust binary
that runs beside your project.

[![CI](https://github.com/sunerpy/godot-mcp/actions/workflows/ci.yml/badge.svg)](https://github.com/sunerpy/godot-mcp/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/sunerpy/godot-mcp)](https://github.com/sunerpy/godot-mcp/releases)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE-MIT)

[**English**](README.md) · [简体中文](docs/readme/README.zh-CN.md) ·
[Website](https://firlab.app/godot-mcp/) · [Documentation](docs/README.md) ·
[Install](#install) · [Quick start](#quick-start) · [Contributing](CONTRIBUTING.md)

</div>

---

## Why godot-mcp

Your MCP client talks to one local `godot-mcp` process, never to the Godot editor
directly, and the process works in your project the way a careful collaborator
would: it reads before it writes, it proves what it did, and it says so when it
cannot.

- **Analysis without the engine.** Project discovery and validation, GDScript
  symbols, references and impact, text scene and resource trees, and the
  dependency graph all answer on a machine with no Godot installed.
- **Edits as transactions.** File and script patches carry the expected SHA-256 of
  what they change; scene and resource batches are journaled. A crash mid-change
  is reconciled to the proven before- or after-state, or quarantined, before the
  next request is served.
- **The real engine, pinned.** Validation, import, tests and exports run against
  the Godot build you configure; CI checks every release against exact locked
  Godot builds from 4.4.1 to 4.7.1.
- **Games and editors it owns.** Start a game or an editor, send keyboard, mouse,
  action and sequence input, read the live scene tree, properties, signals and UI,
  take screenshots and recordings, and step through code over the editor's
  language server and debug adapter.
- **Refuses rather than guesses.** A binary `.scn` answers `unsupported` with the
  fix ("re-save it as text"); an outcome it cannot prove answers `uncertain`.
  There is no tool that runs arbitrary GDScript.
- **Local-first.** stdio by default, with no port and no credential. Optional
  loopback HTTP accepts only a per-launch secret or an authenticated proxy hop.
- **Verifiable packages.** Static Linux (musl) and `crt-static` Windows builds,
  a `MANIFEST.json` the binary checks itself against, a CycloneDX SBOM, and
  release archives attested by GitHub.

## Install

> [!NOTE]
> No release has been published yet, so the installers and archives below have
> nothing to download. Until the first release, [build from source](#build-from-source).

### Release installer

The installers pick the archive for your platform, refuse it unless its digest
matches the release's `SHA256SUMS.txt`, extract the whole package, run its
`self-check`, and put `godot-mcp` on your PATH.

```sh
# Linux x86_64, macOS on Apple silicon
curl -fsSL https://raw.githubusercontent.com/sunerpy/godot-mcp/main/scripts/install.sh | sh
```

```powershell
# Windows x86_64, PowerShell 5.1+
irm https://raw.githubusercontent.com/sunerpy/godot-mcp/main/scripts/install.ps1 | iex
```

Pin a release with `GODOTMCP_VERSION=vX.Y.Z`. The package goes to
`~/.local/share/godot-mcp` (`%LOCALAPPDATA%\godot-mcp` on Windows); the scripts'
headers list every setting.

### Release archives

| Platform            | Target                      | Archive   |
| ------------------- | --------------------------- | --------- |
| Linux x86_64        | `x86_64-unknown-linux-musl` | `.tar.gz` |
| macOS Apple silicon | `aarch64-apple-darwin`      | `.tar.gz` |
| Windows x86_64      | `x86_64-pc-windows-msvc`    | `.zip`    |

Each archive is one package directory: `bin/godot-mcp`, the process supervisor
beside it, the GDScript bridge assets, an example configuration, the licences,
the SBOM and the schema tree. Keep it together — the server finds its supervisor
and assets next to its own executable. Verify an extracted package, and the
archive's build provenance:

```sh
godot-mcp self-check --package /path/to/godot-mcp-X.Y.Z-x86_64-unknown-linux-musl
gh attestation verify godot-mcp-X.Y.Z-x86_64-unknown-linux-musl.tar.gz --repo sunerpy/godot-mcp
```

### Build from source

For an unreleased commit, or a platform without an archive, build the package for
the machine you are on (Rust 1.96.0 is pinned in `rust-toolchain.toml`; Linux
needs `musl-tools`):

```sh
git clone https://github.com/sunerpy/godot-mcp.git
cd godot-mcp
make dist        # writes target/release-dist/godot-mcp-<version>-<target>.<ext>
```

## Quick start

Point your MCP client at the binary and a project directory (the one holding
`project.godot`, or any directory below it):

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

`core` is read-only. Choose the profile deliberately:

| Profile   | Adds                                                           |
| --------- | -------------------------------------------------------------- |
| `core`    | Discovery and analysis; nothing in the project changes         |
| `editor`  | Authoring: files, scripts, scenes, resources, settings, assets |
| `runtime` | Running, testing, exporting and observing the game             |
| `full`    | All of the above                                               |

Engine-backed tools need a Godot executable. The server looks in the usual install
locations and on `PATH`; to choose one, set `godot_binary` in a configuration file
passed with `--config` ([Configuration](docs/operator/configuration.md)). Every tool
per profile is listed in the generated [tool inventory](docs/protocol/tool-inventory.md);
client wiring for HTTP and reverse proxies is in [MCP clients](docs/operator/mcp-clients.md).

## Using with an LLM

Install through the [Install](#install) section, register the server with your
client as above, then let the agent discover the rest:

<details>
<summary>Agent reference</summary>

- Call `capabilities.get` and `health.get` first: they name the profile, what the
  host can run (engine, supervisor, display) and why anything is unavailable.
- Send one request at a time and wait for its answer. The per-principal
  concurrency limit is small, and a burst answers
  `operational limit subject_concurrency was exceeded`.
- Every mutating call carries an `idempotencyId`; repeating a call with the same
  id returns the recorded outcome instead of acting twice.
- An answer is a result or a structured error with a `code` (`invalid_input`,
  `not_found`, `conflict`, `unsupported`, `unavailable`, `uncertain`, …).
  `uncertain` means the effect may or may not have happened; read before retrying.
- stdout carries MCP frames only; diagnostics go to stderr.

</details>

## Documentation

| Topic                                                 | Page                                                                 |
| ----------------------------------------------------- | -------------------------------------------------------------------- |
| Install, configure and make a first request           | [Quickstart: local stdio](docs/operator/quickstart-stdio.md)         |
| Every capability, with requests and measured answers  | [Usage and verification](docs/operator/usage-and-verification.md)    |
| Games and editors it runs, input, captures, debugging | [Managed sessions](docs/operator/managed-sessions.md)                |
| Profiles, outcomes, `unavailable` and `uncertain`     | [Profiles and tool outcomes](docs/operator/profiles-and-outcomes.md) |
| Crashes, survivors, quarantine and repair             | [Recovery](docs/operator/recovery.md)                                |
| Trust boundaries and residual risk                    | [Threat model](docs/security/threat-model.md)                        |

The full index is [docs/README.md](docs/README.md); the website,
<https://firlab.app/godot-mcp/>, has the guide in English and Chinese.

## Development

```sh
make help     # every target
make check    # fmt-check, clippy, workspace tests, locked build, cargo-deny
make dist     # this platform's release archive
```

Work test-first; [`CONTRIBUTING.md`](CONTRIBUTING.md) and the
[contributor workflow](docs/contributor/workflow.md) describe the gates. CI runs
every check of the release-gate catalogue, the packaged smokes on Linux, macOS and
Windows, and the workspace suite natively on macOS and Windows; `CI Success` is the
required check.

## License

MIT. See [LICENSE-MIT](LICENSE-MIT) and [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).
