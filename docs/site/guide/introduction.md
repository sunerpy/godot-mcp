# Introduction

godot-mcp is a [Model Context Protocol](https://modelcontextprotocol.io) server for one Godot
project. Your MCP client — Claude Code, Codex CLI, an editor's agent — starts it as a local
process and talks to it over stdio. The client never talks to the Godot editor directly, and it
does not matter which editor or IDE you use.

## What it gives an agent

- **Answers about the project without the engine.** Project discovery and validation, GDScript
  symbols, references, callers and impact, scene and resource trees, and the dependency graph
  are read from the project's files, so they work on a machine with no Godot installed.
- **Changes that are proved, not assumed.** Every write names the SHA-256 of what it replaces and
  is journaled before it runs. A stale edit is refused with `conflict`; a crash part-way is
  settled before the next request, never guessed.
- **The real engine.** GDScript checks, imports, tests and exports run in the Godot build you
  configure, inside a process boundary the server owns.
- **Games and editors it owns.** It starts the game or a headless editor, sends input, reads the
  live scene tree, captures screenshots and recordings, and debugs through the editor's language
  server and debug adapter.

## How it is organised

Every tool needs one of five things, and the [tool inventory](../../protocol/tool-inventory.md)
names it for each:

| Needs | Means | Examples |
| --- | --- | --- |
| `project_files` | The project's files only, on a machine with or without Godot | `scene.tree.get`, `gdscript.references.get`, `file.patch` |
| `headless_engine` | A Godot executable, which the server starts without a window of its own | `script.check`, `project.test`, `project.export` |
| `game_session` | A game the server started with `project.run.start` | `runtime.tree.get`, `input.key.send`, `runtime.screenshot.capture` |
| `editor_session` | A headless editor the server started with `editor.session.start` | `debug.command` |
| `live_editor` | Your Godot editor, connected through the `godot_mcp` add-on | `editor.scene.tree.get`, `editor.undo` |

A **profile** decides which tools are listed at all: `core` reads, `editor` adds the writes and
the editor add-on, `runtime` adds running and observing the game, and `full` is all three. A
listed tool whose engine, game or editor is not there right now answers `unavailable` and says
which, so a client can rely on the tool list and learn about the machine from the answers. See
[Profiles and answers](profiles.md).

## What it is not

There is no tool that executes arbitrary GDScript or calls an arbitrary engine method, in any
profile. godot-mcp never downloads Godot or export templates. It serves one project for one user;
it is not a shared or remote service. And loading a Godot project runs that project's code, so
the server's process boundaries contain and stop what it starts without making an untrusted
project safe to open — the [threat model](../../security/threat-model.md) says exactly where the
lines are.

## Next

- [Install](install.md) the binary.
- [Quick start](quick-start.md): register it with a client and make the first calls.
