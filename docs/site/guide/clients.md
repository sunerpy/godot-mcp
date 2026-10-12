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

If the client starts processes without your shell's PATH, give `command` the launcher's full path
(`~/.local/bin/godot-mcp` after the Linux and macOS installer).

## Environment variables

A client that passes environment variables to the server should pass only the ones it means:
every `GODOT_MCP_`-prefixed variable is configuration, and one the server does not recognise
stops it from starting. `GODOT_MCP_PROFILE` and `GODOT_MCP_LOG_SINK` are two it does know; the
[configuration reference](../../operator/configuration.md#environment-variables) lists them all.

## A second process on the same machine

When another local process has to reach the same running server, turn on loopback HTTP instead
of starting a second server: see [HTTP](../operate/http.md).
