# 0016 — Commands a project declares

**Status:** accepted for contract 0.6.0. It amends [ADR 0012](0012-declared-script-execution.md):
the absence audit proves a second switch, `project_commands`, and a fourth execution-shaped tool,
`editor.commands.call`.

## Context

A project often carries its own editor tooling: a level generator, an asset check, an export of
its localisation tables, written as `@tool` scripts a developer starts from a menu or a dock. An
agent has two ways to reach that tooling today, and both are poor. It can write a script that
calls it, which needs `--allow-scripts`, hands the agent code execution in general, and leaves it
guessing the tooling's arguments; or it can ask a person to run it.

The project owner asked on 2026-11-07 for an extension mechanism: the project says which of its
code an agent may call, with what arguments, and an agent lists and calls it.

## Decision

### A declaration file, read without running anything

The project lists its commands in `godot_mcp_commands.json` at its root. Each provider names one
`res://` GDScript file; each command has a name, a title, a description, a JSON Schema for its
arguments and a `mutates` flag. The file is JSON so that the server can read it without starting
Godot and without trusting the project's code: a listing runs nothing.

The format is strict and bounded: version `1`; at most 16 providers and 64 commands; names of 1 to
64 characters drawn from `[a-z][a-z0-9_.-]*`; a title of at most 128 characters and a description
of at most 1,024; an input schema of at most 16,384 bytes; a file of at most 262,144 bytes; and no
key the format does not name. A file that breaks any rule is invalid as a whole and nothing in it
can be called; the listing names every problem with a JSON pointer.
`schemas/domain/project-commands-v1.schema.json` holds the structural rules for editors.

An input schema uses the JSON Schema subset the server's own contracts use
(`godot_mcp_domain::jsonschema`), checked as a whole before the command can be called. A command
whose schema leaves the subset stays in a valid file, is listed with the construct that keeps it
from being called, and refuses calls with `unsupported`.

### Two tools

| Tool | Capability | What it does |
| --- | --- | --- |
| `editor.commands.list` | `read` | Lists the declaration: every command with whether it can be called and why not, every problem, whether the server allows project commands and whether a connected editor serves them. Needs no Godot process. |
| `editor.commands.call` | `editor_mutate` | Checks a call against the declaration and the provider's script as they are on disk, and has the connected editor load the provider and await its `mcp_call(command, args)`. |

Both are in the `editor` and `full` profiles. The provider is a `@tool` script, because the editor
runs nothing else; its `mcp_call` answers `{"ok": true, "value": ...}` or
`{"ok": false, "error": "..."}`. The add-on reads the script from its file on every call and
answers through the method `editor.commands.call` of the bridge, which add-on 0.5.0 implements;
with an older add-on the tool answers `unsupported` and says to update it.

### One switch, on unless the caller is behind a proxy

`project_commands` decides whether commands are called. Unset, it follows the transport: on for
stdio, `trusted_local` and `open`, off for `proxied_oidc`. `project_commands = true` turns it on
for a proxied principal and `false` turns it off anywhere, from the `--config` file or
`GODOT_MCP_PROJECT_COMMANDS` alongside it. While it is off, the call answers `permission_denied`,
the session's capabilities do not offer the add-on the method, and the add-on refuses it too; the
listing still shows the declaration, every command unavailable and the reason.

### What a call keeps to

- **It is refused before it is sent when the declaration does not allow it.** An absent or
  invalid file, an unknown provider or command, a missing provider script or one without `@tool`,
  arguments the schema refuses or the bridge cannot carry exactly, and a deadline outside 1,000 to
  600,000 milliseconds are refused with nothing journaled. So is a call while the switch is off.
- **It has an identity.** A call the editor began is one journaled transaction, recorded as
  covering the whole project, as `editor.script.run` is, because the command may write any file.
  An exact retry answers from the journal before the declaration is read again, so a command that
  rewrote the declaration still replays.
- **It says what happened.** `passed`, `failed`, `compile_error` or `inconclusive`, by the rules
  of ADR 0012; the value as a tagged value and as Godot's spelling of it; the command's error
  text; what the editor logged; and every project file that differs from before the call.
- **It undoes nothing, and `mutates` is not enforced.** Every answer carries
  `rolled_back: false`. A command declared with `mutates: false` that changed files is answered
  with `declaration_mismatch`, not refused: the server cannot stop the code, only report it.
- **A deadline ends the wait, not the code.** As for `editor.script.run`: the server waits the
  caller's deadline and two seconds more, then answers `uncertain`, with the files changed so far,
  and quarantines the project, because the editor may still be writing. The command's answer,
  when it returns, settles the transaction ([ADR 0015](0015-unanswered-editor-changes.md)).

## Why

**Why a switch that is on by default, when script execution is off by default.** A script is
code the caller writes. A command is code the project wrote, and the editor already runs a
project's `@tool` code as it opens the project and its scenes; a call picks one of the declared
commands and passes arguments the project's own schema accepts. The project's author decided
what is exposed, and the operator decided which project to serve. A remote principal, whom the
server knows only through a proxy, is the exception: it reaches the project's code only when the
operator says so.

**Why two tools, not one tool per command.** The tool list is the contract: it is fixed per
profile and must not change with the project
(see [Schema evolution](../../protocol/schema-evolution.md)). Two tools serve any number of
commands, and `editor.commands.list` gives an agent what a tool list would: the description and
the argument schema of each command.

**Why JSON, not a GDScript registry.** Listing must run nothing, and a registry would have to be
loaded to be read. The declaration also stays reviewable on its own, in version control, next to
`project.godot`.

**Why the schema subset.** The server already ships a deterministic validator whose patterns run
in linear time; arguments checked by it cannot stall the server, and a construct it cannot check
is reported instead of passed through unchecked.

**Why the editor and not a headless Godot.** Project tooling usually needs what only the editor
has: the open scenes, `EditorInterface`, the resource file system. Commands in a headless Godot or
in a running game are not part of this decision.

## Consequences

- An agent can run a project's own tooling in one call, knowing its arguments, without
  `--allow-scripts`.
- Anyone who can call the tools of the `editor` or `full` profile can run the declared commands:
  on an `open` listener without a token, anyone who can reach the port. The listener's start-up
  warning says that anyone who can reach it can use every tool of the profile, and a line of its
  own says that they can run the project's declared commands while the switch is on.
- A command is the project's code and nothing confines it; its declaration describes it and does
  not bound it. The threat model says so, and an operator serving a project they did not write
  should read its declaration, or turn the switch off, before serving it.
- The configuration reference, the editor bridge, the threat model and
  [Project commands](../../operator/project-commands.md) describe the switch, the tools and their
  limits.

## Enforcement

- The declaration's rules, the schema subset, the argument bounds and the committed declaration
  schema (`crates/godot-mcp-domain/tests/project_commands.rs`); the operator page's example
  declaration, provider and limits (`crates/godot-mcp-domain/tests/project_commands_page.rs`);
  the switch's defaults and layers (`crates/godot-mcp-domain/tests/config_layering.rs`).
- The listing, the refusals before anything is sent, the answer's shape, replay, a lost answer,
  the switch and an older add-on, against a stand-in editor
  (`crates/godot-mcp-server/tests/editor_commands.rs`).
- The absence audit proves the declared tools and both switches
  (`crates/godot-mcp-server/src/audit/absence.rs`, `crates/godot-mcp-server/tests/audit_limits.rs`).
- The live suite runs calls, refusals, replays, failures, a deadline and the add-on's own cases on
  every locked Linux editor (`crates/godot-mcp-server/tests/live_editor_commands.rs`, `--ignored`).
