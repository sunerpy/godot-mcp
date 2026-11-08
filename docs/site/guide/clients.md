---
description: Register godot-mcp with Claude Code, Codex, OpenCode, Pi Agent or a client configured with JSON, and set its environment.
---

# MCP clients

godot-mcp is a plain MCP server over stdio. Any client that can start a process and speak
newline-delimited JSON-RPC over its stdin and stdout can use it, and that is the arrangement to
prefer: no port, no credential and no network. Which editor or IDE you use does not matter; the
client connects to the `godot-mcp` process, not to the Godot editor.

## The command

Every client gets the same command:

```sh
godot-mcp serve --project /path/to/my-game --profile core
```

| Argument | Meaning |
| --- | --- |
| `--project` | A directory in the project; the real root is found upward from it by `project.godot` |
| `--profile` | `core` (the default), `editor`, `runtime` or `full` |
| `--config` | An optional configuration file, for `godot_binary`, limits, logging or HTTP |
| `--workspace` | Instead of `--project`: start without a project, so the client can create or bind one |

One server serves one project. To work on two projects, register two servers.

## Claude Code

```sh
claude mcp add godot-mcp -- godot-mcp serve --project /path/to/my-game --profile full
```

## Codex

```sh
codex mcp add godot-mcp -- godot-mcp serve --project /path/to/my-game --profile full
```

The command writes the server into Codex's `config.toml` (in `~/.codex`, or in `$CODEX_HOME` when
that is set); `codex mcp list` shows it.

## OpenCode

OpenCode reads its servers from an `opencode.json` in the project, or from its global
configuration file (`~/.config/opencode/opencode.json`):

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

`opencode mcp list` shows whether it connected.

## Pi Agent

```sh
pi mcp add godot-mcp -- godot-mcp serve --project /path/to/my-game --profile full
```

This writes the server to `~/.pi/agent/mcp.json`, which every project reads. With `-l` it goes to
the project's `.pi/mcp.json` instead, which Pi Agent reads only in a project you have trusted.
`pi mcp list` shows whether the server connected and which tools it lists.

## Clients configured with JSON

Claude Desktop, Cursor and most other clients read a JSON block of this shape:

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

If the client starts processes without your shell's PATH, give `command` the launcher's full path,
as `command -v godot-mcp` prints it: the Linux and macOS installer puts the launcher at
`.local/bin/godot-mcp` in your home directory. Write the path out in full, since the client does
not run `command` through a shell and so does not expand a `~` in it.

## Environment variables

A client that passes environment variables to the server should pass only the ones it means:
every `GODOT_MCP_`-prefixed variable is configuration, and one the server does not recognise
stops it from starting. `GODOT_MCP_PROFILE` and `GODOT_MCP_LOG_SINK` are two it does know; the
[configuration reference](../../operator/configuration.md#environment-variables) lists them all.

## A second process on the same machine

When another local process has to reach the same running server, turn on loopback HTTP instead
of starting a second server: see [HTTP](../operate/http.md).

## A client on another machine

When the agent runs on another machine of your development network, start the server with
`--http 0.0.0.0:8765` and give the client the URL the server prints: see
[HTTP](../operate/http.md#open).

## Agent Skills

To give the agent godot-mcp's Agent Skills as well, run `godot-mcp skill install`; see
[Agent Skills](skills.md) for the target each client uses.
