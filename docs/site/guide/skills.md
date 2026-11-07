---
description: Install godot-mcp's Agent Skills for Claude Code, Codex, VS Code, Cursor, Gemini CLI, Kiro or OpenCode, and keep them up to date.
---

# Agent Skills

Each godot-mcp tool describes itself. How the tools combine into a job, such as reading a scene
before changing it, or starting the game, waiting for it, pressing a button and stopping it
again, is in godot-mcp's Agent Skills: instruction files an agent loads when a task needs them,
and not otherwise.

| Skill | The agent loads it when it |
| --- | --- |
| `godot-mcp` | reads or changes your project through godot-mcp |
| `godot-mcp-playtest` | runs, plays or tests the game |
| `godot-mcp-setup` | installs, connects or diagnoses godot-mcp |

## Install them

```sh
godot-mcp skill install
```

The command copies the three skills into the skill directory of every agent whose configuration
directory it finds in your home directory, such as `~/.claude` for Claude Code, and installs
them for Claude Code when it finds none. Each skill is one `SKILL.md` file that the `godot-mcp`
binary carries, so it always installs the skills of its own version.

To choose the agents, name them with `--target`:

```sh
godot-mcp skill install --target claude,codex
```

| Client | `--target` |
| --- | --- |
| Claude Code | `claude` |
| Codex | `codex` |
| VS Code with GitHub Copilot | `claude` or `codex` |
| Cursor | `cursor` |
| Gemini CLI | `gemini` |
| Kiro | `kiro` |
| OpenCode | `opencode` |

Install only the targets you use. Several clients read more than one skill directory, so with
several targets installed an agent can list a skill twice.

To keep the skills with one project instead, run the command in the project's root with
`--local`. They go to a hidden directory such as `.claude/skills`, which Godot does not import:

```sh
cd /path/to/my-game
godot-mcp skill install --target claude --local
```

## When the agent runs on another machine

Install the skills where the agent runs. When it reaches godot-mcp over HTTP from another
machine, run `godot-mcp skill install` on that machine, with the godot-mcp version the server
runs. The command needs no project and no Godot build there.

## Keep them up to date

After you update godot-mcp, update the skills as well:

```sh
godot-mcp skill status
godot-mcp skill update --dry-run --diff
godot-mcp skill update
```

`status` reports each skill as not installed, up to date, update available or modified, and
changes nothing. `update --dry-run --diff` shows what an update would change and writes nothing.

A skill you edited yourself stays as it is: `install` and `update` skip it and list it, and
`uninstall` keeps it. `godot-mcp skill update --force` replaces your edited copy with the
current one.

```sh
godot-mcp skill uninstall
```

removes the skills godot-mcp wrote, and their directories once they are empty.

The reference, with every rule the commands follow, is [Agent Skills](../../operator/skills.md).
godot-mcp 0.5.0 and earlier have no `skill` command.
