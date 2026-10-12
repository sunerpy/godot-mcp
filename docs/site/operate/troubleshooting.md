# Troubleshooting

Start with `health.get`: it names every component that is not ready, with a code and a remedy.
The sections below are the questions that come up most.

## A tool is listed but answers `unavailable`

The profile lists the tool, and what it needs is not there right now. The answer says which:

| Code in `health.get` | Means | Do |
| --- | --- | --- |
| `godot_binary_absent` | No Godot build was found | Install one and set `godot_binary` in the configuration file |
| `game_not_running` | No managed game session runs | Start one with `project.run.start` |
| `editor_not_attached` | No editor with the add-on is connected | Install the add-on with `editor.addon.install` and open the project in Godot |

A windowed session also needs a display: on Linux `DISPLAY` or `WAYLAND_DISPLAY` in the server's
environment, on macOS a logged-in user's session, on Windows a logged-on desktop.

## The server does not start

- `unknown godot-mcp environment variable` — every `GODOT_MCP_`-prefixed variable is
  configuration. Remove the one named, or spell it as the
  [configuration reference](../../operator/configuration.md#environment-variables) does.
- `startup failed` — the project's lease could not be taken or its journal could not be opened.
  Another `godot-mcp` already serves this project (a different spelling of the same directory is
  the same project), a component of the project path is a symbolic link, which is refused, or no
  `project.godot` is at or above the path you passed.
- `[http] is forbidden when the final transport is stdio` — an HTTP section in a stdio
  configuration is refused as ambiguous; remove it, or set `transport = "http"`.

`godot-mcp check-config --config <file>` runs the same checks without starting the server.

## A write answers `conflict`

The file changed since it was read — its SHA-256 no longer matches `expectedSha256` — or the
connected editor holds it. Read the file again (`file.get`, `project.files.list` or
`scene.tree.get`), decide, and send the change against the new hash. Do not retry blindly: that is
how one change overwrites another.

## An answer is `uncertain`

For an analysis, the code does not let the server be sure — a call through an untyped variable, a
node found by a string — and the answer lists every candidate. For a write, the outcome could not
be proven; the server settles it before the next write, so read the project before trying again.

## Many requests fail at once

`operational limit subject_concurrency was exceeded` means the client sent more requests at once
than the per-caller limit allows. Send one request at a time and wait for its answer.

## Writes are refused after a crash

A transaction the server could not prove is quarantined and the write gate is closed; see
[Recovery](recovery.md).
