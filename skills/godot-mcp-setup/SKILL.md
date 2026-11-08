---
name: godot-mcp-setup
description: "Install, connect, configure and diagnose godot-mcp, the MCP server for Godot 4 projects. Use when godot-mcp is not installed, not registered with the agent or not answering, when it has to be set up for Claude Code, Codex, VS Code, Cursor, OpenCode or another MCP client, when a profile, a Godot build, the Godot editor add-on, HTTP access or these Agent Skills have to be chosen or installed, or when health.get reports a component that is not ready. Works before any godot-mcp tool is available, from the command line. Covers the installer and self-check, client configuration, profiles, godot_binary and the tested Godot builds, editor.addon.install, godot-mcp skill install, the HTTP modes and reading health.get. Editing the project is the godot-mcp skill; running the game is the godot-mcp-playtest skill."
license: MIT
compatibility: "Uses the godot-mcp command line, which installs and registers without a Godot build. The MCP tools it names need a connected server with the editor or full profile; the tools that run Godot need a Godot 4.4.0 or later editor build."
metadata:
  contract: "0.6.0"
  profiles: "editor,full"
---

# godot-mcp-setup: installing, connecting and diagnosing

godot-mcp is one executable, `godot-mcp`, that an MCP client starts beside one Godot project.
This skill gets it installed, registered with the client and connected to Godot and the Godot
editor, and reads `health.get` when something is missing. Until the client has started the
server, no godot-mcp tool exists, so the first steps run on the command line.

This skill describes contract 0.6.0, the `contract` of its metadata. `godot-mcp version` prints
the server and contract versions, and `health.get` reports the contract as `schema_version`;
when they differ, follow the tool schemas.

## 1. Install the package

Install it when the user asks for it. The release installers pick the archive for the platform,
refuse it unless its digest matches the release's `SHA256SUMS.txt`, run the package's own
self-check and put `godot-mcp` on the PATH:

```sh
curl -fsSL https://raw.githubusercontent.com/sunerpy/godot-mcp/main/scripts/install.sh | sh
```

```powershell
irm https://raw.githubusercontent.com/sunerpy/godot-mcp/main/scripts/install.ps1 | iex
```

On Linux and macOS the package goes to `~/.local/share/godot-mcp` and a launcher to
`~/.local/bin`; on Windows the package goes to `%LOCALAPPDATA%\godot-mcp` and its `bin`
directory joins the user PATH, so open a new terminal. Running the installer again updates to
the latest release; `GODOTMCP_VERSION` picks another, such as `v0.5.0`.

A downloaded archive is checked the same way by hand. Keep the extracted package together: the
server finds its supervisor and assets next to its own executable.

```sh
sha256sum -c --ignore-missing SHA256SUMS.txt
tar -xzf godot-mcp-X.Y.Z-x86_64-unknown-linux-musl.tar.gz
godot-mcp-X.Y.Z-x86_64-unknown-linux-musl/bin/godot-mcp self-check \
  --package godot-mcp-X.Y.Z-x86_64-unknown-linux-musl
```

`self-check` re-derives the size and SHA-256 of every file the package's `MANIFEST.json`
lists. It exits 0 when all match, 1 when one is missing or changed, and 2 when the binary does
not run from an extracted package. Archives exist for Linux x86_64, macOS on Apple silicon and
Windows x86_64; elsewhere, `make dist` in a clone builds the package.

## 2. Register the server with the client

Every client starts the same command. `--project` is the directory that holds `project.godot`,
or any directory below it; one server serves one project, so two projects need two servers.

```sh
claude mcp add godot-mcp -- godot-mcp serve --project /path/to/my-game --profile full
```

```sh
codex mcp add godot-mcp -- godot-mcp serve --project /path/to/my-game --profile full
```

Codex writes that into its `config.toml`, in `~/.codex` or in `$CODEX_HOME`, as:

```toml
[mcp_servers.godot-mcp]
command = "godot-mcp"
args = ["serve", "--project", "/path/to/my-game", "--profile", "full"]
```

VS Code reads `.vscode/mcp.json` in the workspace:

```json
{
  "servers": {
    "godot-mcp": {
      "type": "stdio",
      "command": "godot-mcp",
      "args": ["serve", "--project", "/path/to/my-game", "--profile", "full"]
    }
  }
}
```

OpenCode reads an `opencode.json` in the project or `~/.config/opencode/opencode.json`:

```json
{
  "mcp": {
    "godot-mcp": {
      "type": "local",
      "command": ["godot-mcp", "serve", "--project", "/path/to/my-game", "--profile", "full"],
      "enabled": true
    }
  }
}
```

Claude Desktop, Cursor and most other clients read this shape:

```json
{
  "mcpServers": {
    "godot-mcp": {
      "command": "godot-mcp",
      "args": ["serve", "--project", "/path/to/my-game", "--profile", "full"]
    }
  }
}
```

- A client that starts processes without the shell's PATH needs the launcher's full path as
  `command`, such as `~/.local/bin/godot-mcp` after the Linux and macOS installer.
- `--workspace <dir>` instead of `--project` starts without a project, so the agent can create
  one with `project.create` or pick one with `project.bind`.
- Every `GODOT_MCP_`-prefixed environment variable is configuration, and one the server does
  not know stops it from starting. Pass only the ones you mean.
- The client starts the server itself. After a change, restart the client or reconnect the
  server; the new tools appear in its `tools/list`.

## 3. Choose the profile

| Profile | Adds | Choose it when |
| --- | --- | --- |
| `core` | Discovery and analysis; it changes nothing | the user wants answers only |
| `editor` | Files, scripts, scenes, resources, settings, assets and the editor add-on | the user wants the project changed |
| `runtime` | Running, testing, exporting and observing the game | the user wants the game run and watched |
| `full` | All three | the user wants all of it |

Without `--profile` the server starts in `core`. The profile is fixed for the server's lifetime:
changing it means changing the registration and restarting the server. `capabilities.get`
lists every profile's tools.

## 4. Give the server a Godot build

Analysis needs no engine. The tools that run Godot need an editor build of Godot 4.4.0 or later;
the server looks in the usual install locations and on PATH. The tested builds are Godot 4.4.1,
4.5.1, 4.6.1, 4.7.1 and 4.7.2. To name a build, write a configuration file:

```toml
transport = "stdio"
godot_binary = "/opt/godot/godot"
```

and add `--config /path/to/godot-mcp.toml` to the `serve` arguments.
`godot-mcp check-config --config /path/to/godot-mcp.toml` runs the server's own loader and
checks without starting it: exit 0 is valid, and exit 2 says why the file is refused. Exports
also need the export templates of exactly that Godot build. `health.get` then reports the build
it resolved and its version under `engine`.

A project with a GDExtension needs one more step on Godot 4.7.1 and 4.7.2, measured with one
godot-rust extension on Linux: the first import of a project that was never imported aborts,
and `project.import.run` answers `failed` with signal `SIGABRT`. That import still records the
extension, so run `project.import.run` again, or open the project in the Godot editor once;
until then the extension's classes are not loaded. On those builds `script.run` with
`context: "copy"` answers `unavailable`, because a copy is imported from scratch every time:
use `context: "project"`, or `editor.script.run` while an editor is connected.

## 5. Connect the Godot editor

The editor tools need the `godot_mcp` add-on running in an open Godot editor on the project. A
server with the editor or full profile installs it:

1. `editor.addon.status` compares `addons/godot_mcp/` with the add-on this server carries and
   says what an install or update would do.
2. `editor.addon.install` writes the add-on; `enable: true` also enables the plugin in
   `project.godot`. It refuses a path that is occupied by files it did not write.
3. The editor loads the plugin when it starts with the plugin enabled. If the project is
   already open in Godot, install without `enable` and ask the user to enable `godot_mcp` under
   Project Settings, Plugins, before anything else is written: the server cannot see an editor
   that does not run the plugin.
4. `editor.status.get` then reports the connected editor, its Godot and plugin versions and
   whether the plugin differs from the add-on this server carries.

```json tool=editor.addon.install
{ "idempotencyId": "install-godot-mcp-addon-1", "enable": true }
```

After the server is updated, `editor.addon.status` shows the difference, `editor.addon.update`
replaces the files the user did not edit and keeps an edited one beside the new file as
`<name>.godot-mcp-backup`, and `editor.addon.reload` loads the new files into the open editor.
An editor tool that an older plugin lacks answers `unsupported` and says the same. One editor
is served at a time.

## 6. Install these skills

`godot-mcp skill install` copies the skills this binary carries, `godot-mcp`,
`godot-mcp-playtest` and `godot-mcp-setup`, into the skill directories of the agents in use.
It reads no Godot project and writes only those skill directories.

| `--target` | Writes to (`--global`, the default) | With `--local` | Selected by `auto` when |
| --- | --- | --- | --- |
| `claude` | `$CLAUDE_CONFIG_DIR/skills`, else `~/.claude/skills` | `.claude/skills` | `$CLAUDE_CONFIG_DIR` or `~/.claude` exists |
| `codex` | `~/.agents/skills` | `.agents/skills` | `$CODEX_HOME` or `~/.codex` exists |
| `cursor` | `~/.cursor/skills` | `.cursor/skills` | `~/.cursor` exists |
| `gemini` | `~/.gemini/skills` | `.gemini/skills` | `~/.gemini` exists |
| `kiro` | `~/.kiro/skills` | `.kiro/skills` | `~/.kiro` exists |
| `opencode` | `~/.config/opencode/skills` | `.opencode/skills` | `~/.config/opencode` exists |

- `--target` takes `auto` (the default: every target whose directory exists, else `claude`),
  `all`, or a comma-separated list such as `claude,codex`. With `--local`, `auto` looks for the
  directories in the current directory, `.codex` for `codex`.
- VS Code reads both `~/.claude/skills` and `~/.agents/skills`, so `claude` or `codex` serves
  it.
- Install on the machine the agent runs on. Over HTTP that may not be the machine with the
  project.
- `godot-mcp skill status` reports each skill as not installed, up to date, update available or
  modified; `godot-mcp skill update --dry-run --diff` shows what an update would change;
  `godot-mcp skill uninstall` removes what godot-mcp wrote and keeps files the user changed.
- A `SKILL.md` changed after godot-mcp wrote it is skipped and listed; the skipped file stays as
  it is. Overwrite it only when the user says so, with `godot-mcp skill install --force`.

## 7. HTTP, only when stdio cannot work

stdio is the default and needs no port and no credential. HTTP is for a second process that has
to reach a server already running:

| Mode | For | A request is accepted when |
| --- | --- | --- |
| `trusted_local` | another local process of the same user | it carries the per-launch secret and the configured `Host` and `Origin` |
| `open` | a client on another machine of a development network whose machines are the user's | it carries the token, when one is set, and no `Origin` but the configured one |
| `proxied_oidc` | a client on another host, through a reverse proxy on this one | it comes through the proxy hop with its credential and a valid token of the one configured user |

`open` starts with one flag and prints the URL another machine dials:

```sh
godot-mcp serve --project /path/to/my-game --profile full --http 0.0.0.0:8765
```

A client that takes a URL then needs only `"type": "http"` and the printed URL, such as
`"url": "http://192.0.2.10:8765/mcp"`, in its server entry. Anyone who can reach the port can
use every tool of the profile, so `--http-token-env <VAR>` makes every request present
`Authorization: Bearer <token>`. Use `open` only when the user asks for it, on a network whose
machines are theirs.

## 8. Read health.get

`health.get` names every component that is not ready, each with a code and a remedy:

| Code | Means | Do |
| --- | --- | --- |
| `godot_binary_absent` | no Godot build was found | install one and set `godot_binary`, as in step 4 |
| `process_plane_unavailable` | no process boundary can contain a Godot launch here | read the `godot` component's message; the tools that run Godot answer `unavailable` until it is fixed |
| `editor_not_attached` | no editor with the add-on is connected | step 5 |
| `game_not_running` | no managed game session runs | start one; see the godot-mcp-playtest skill |
| `http_disabled` | HTTP is off, the default | nothing, unless a second process needs it |

`readiness` says whether reads, writes, the editor and the runtime are served now. A non-zero
`journal` quarantine count means the write gate is closed. A change the editor is still running
keeps it closed until the editor answers; otherwise writes are refused until the user resolves
it on the project's machine with `godot-mcp transaction repair`. For a change whose editor was
closed or crashed before it answered, that is `--operation acknowledge-safe-state` with a
`writer-ended` proof, once the editor's process has ended.

When the server does not start at all, the client's log shows its stderr:

- `unknown godot-mcp environment variable`: remove or correct the `GODOT_MCP_` variable named.
- `startup failed`: another `godot-mcp` already serves the project (another spelling of the same
  directory is the same project), a component of the project path is a symbolic link, or no
  `project.godot` is at or above the path.
- `[http] is forbidden when the final transport is stdio`: remove the `[http]` section, or set
  `transport = "http"`.

## Leave these to the user

- `--allow-scripts` lets `script.run` and `editor.script.run` run caller-written GDScript with
  the user's privileges. Add it only when the user asks for script execution.
- `open` HTTP mode, as above.
- Repairing a quarantined transaction with `godot-mcp transaction repair`.
