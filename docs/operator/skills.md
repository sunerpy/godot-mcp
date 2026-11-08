# Agent Skills

godot-mcp carries four Agent Skills: instruction files an agent loads only when a task
needs them, so how a multi-step job is done is in the agent's context while it does that
job and not otherwise. `godot-mcp skill install` copies them into the skill directories of
the agents you use. The `skill` commands write only those directories; they read no Godot
project, start no server and need no Godot build.

| Skill | The agent loads it when | It covers |
| --- | --- | --- |
| `godot-mcp` | reading or changing a Godot project through godot-mcp | what the server is made of, finding script errors, reading before writing, connecting signals, scenes the editor has open, recipes, `call_method` for an AnimationTree state machine, a TileMapLayer and a Theme, typed values, and what each answer means |
| `godot-mcp-export` | exporting, building or packaging the game | presets and export templates, a project export, a pack or a patch, following an export, reading its outputs, cancelling, and installing an APK on an Android device |
| `godot-mcp-playtest` | running, playing or testing the game | starting a session, waiting, input, reading the running game, a `test.scenario.run` scenario, captures and image comparisons, game speed, recording and replay, debugging a game that fails, and stopping the session |
| `godot-mcp-setup` | installing, connecting or diagnosing godot-mcp, before any of its tools exists | the installer and `self-check`, client configuration, profiles, the Godot build and what an export needs, the editor add-on, these skills, the HTTP modes and reading `health.get` |

Each skill is one `SKILL.md` file, in English, with no scripts and no links to other files.
The binary embeds them when it is built, so a `godot-mcp` binary always installs the skills
of its own version. Each skill's `metadata` names the contract it describes as `contract`;
when the `schema_version` that `health.get` reports differs, the skill tells the agent to
follow the tool schemas.

## Commands

```sh
godot-mcp skill install
godot-mcp skill status
godot-mcp skill update --dry-run --diff
godot-mcp skill update
godot-mcp skill uninstall
```

| Command | Does |
| --- | --- |
| `install` | Writes each skill into the selected agents' skill directories |
| `update` | The same decisions as `install`; `--dry-run` reports and writes nothing, and `--diff` prints a unified diff from each installed `SKILL.md` to this binary's |
| `uninstall` | Removes each skill godot-mcp wrote, with its marker and its directory once empty; keeps a `SKILL.md` that was changed after godot-mcp wrote it |
| `status` | Reports each skill as not installed, up to date, update available, modified or an error, and changes nothing |

Every command takes `--target`, which selects the agents, and `--global` (the default) or
`--local`, which select the directories under your home directory or under the current
directory. A `SKILL.md` that was changed after godot-mcp wrote it is skipped and listed, and
stays as it is; `godot-mcp skill install --force` and `godot-mcp skill update --force`
overwrite it. Use them only when the changes made to that file may be lost.

Every command prints one line per agent and skill, then a total:

```text
claude: godot-mcp: installed /path/to/my-game/.claude/skills/godot-mcp
claude: godot-mcp-export: installed /path/to/my-game/.claude/skills/godot-mcp-export
claude: godot-mcp-playtest: installed /path/to/my-game/.claude/skills/godot-mcp-playtest
claude: godot-mcp-setup: installed /path/to/my-game/.claude/skills/godot-mcp-setup
install: written 4, up to date 0, skipped 0, errors 0
```

The exit status is 1 when any skill could not be processed, and 0 otherwise, also when a
changed `SKILL.md` was skipped or kept. An invalid argument, such as an unknown target, exits
2 and lists the accepted values. Every command processes all of its agents and skills before
it reports, so one error does not stop the others.

## Where each agent reads skills

`--target` takes `auto` (the default), `all`, or a comma-separated list such as
`claude,codex`. Each target writes one directory per skill:

| Target | `--global` | `--local` | `auto` selects it when |
| --- | --- | --- | --- |
| `claude` | `$CLAUDE_CONFIG_DIR/skills`, else `~/.claude/skills` | `.claude/skills` | `$CLAUDE_CONFIG_DIR` or `~/.claude` exists; with `--local`, `.claude` |
| `codex` | `~/.agents/skills` | `.agents/skills` | `$CODEX_HOME` or `~/.codex` exists; with `--local`, `.codex` |
| `cursor` | `~/.cursor/skills` | `.cursor/skills` | `~/.cursor` exists; with `--local`, `.cursor` |
| `gemini` | `~/.gemini/skills` | `.gemini/skills` | `~/.gemini` exists; with `--local`, `.gemini` |
| `kiro` | `~/.kiro/skills` | `.kiro/skills` | `~/.kiro` exists; with `--local`, `.kiro` |
| `opencode` | `~/.config/opencode/skills` | `.opencode/skills` | `~/.config/opencode` exists; with `--local`, `.opencode` |

- `auto` selects every target whose directory exists. When none does, it installs for
  `claude` alone and prints `auto: no agent configuration directory was found; using claude`.
- `~` is `HOME`, or `USERPROFILE` when `HOME` is not set. A relative `CLAUDE_CONFIG_DIR` or
  `CODEX_HOME` is taken from the current directory.
- `--local` directories are relative to the current directory, so run the command in the
  project's root. Each starts with `.`, and the Godot editor skips such directories when it
  scans the project, so the skills are never imported as project files.
- A skill directory, or a directory above it, may be a symbolic link to a directory; godot-mcp
  writes through it, and `uninstall` removes the files and keeps the link.

## Which target each client uses

| Client | Target |
| --- | --- |
| Claude Code | `claude` |
| Codex | `codex` |
| VS Code (GitHub Copilot) | `claude` or `codex`: VS Code reads both `.claude/skills` and `.agents/skills`, and their user-wide equivalents |
| Cursor | `cursor` |
| Gemini CLI | `gemini` |
| Kiro | `kiro` |
| OpenCode | `opencode` |

Install only the targets you use. VS Code, Cursor and OpenCode also read `.claude/skills` and
`.agents/skills`, and Gemini CLI reads `.agents/skills`, so an agent can list the same skill
twice when several targets are installed. Each client documents where it reads skills:
[Claude Code](https://code.claude.com/docs/en/skills),
[Codex](https://developers.openai.com/codex/skills),
[VS Code](https://code.visualstudio.com/docs/copilot/customization/agent-skills),
[Cursor](https://cursor.com/docs/skills), [Kiro](https://kiro.dev/docs/skills/),
[OpenCode](https://opencode.ai/docs/skills/) and Gemini CLI's `docs/cli/skills.md` in its
repository, [google-gemini/gemini-cli](https://github.com/google-gemini/gemini-cli).

Install the skills on the machine the agent runs on. With stdio that is the machine with the
project; over HTTP it may be another one. Run the `godot-mcp` of the server's version there:
the `skill` commands need no project and no Godot build.

## How install and update decide

Beside each `SKILL.md` it writes, godot-mcp keeps a marker, `.godot-mcp-skill.json`: a JSON
object whose `sha256` is the SHA-256 of the `SKILL.md` bytes godot-mcp wrote, whose `version` is
the package version that wrote them and whose `installed_at` is the Unix time in seconds. Only
`sha256` takes part in a decision; the other two are shown in reports.

`install` and `update` decide each skill the same way. Forced means
`godot-mcp skill install --force` or `godot-mcp skill update --force`:

| On disk | Not forced | Forced |
| --- | --- | --- |
| No `SKILL.md` | write it | write it |
| A `SKILL.md` equal to this binary's | leave it; write the marker when it is missing or names another SHA-256 | overwrite it |
| A different `SKILL.md` whose SHA-256 the marker names: an older version godot-mcp wrote | update it | overwrite it |
| A different `SKILL.md` the marker does not name: someone changed it | skip it and list it | overwrite it |
| A different `SKILL.md` with no usable marker | skip it and list it | overwrite it |

A marker that is missing, cannot be read or is not JSON with a `sha256` string counts as no
usable marker, and the line says which. The decision compares bytes only, so a `SKILL.md` that
is not UTF-8 text is a different file like any other; `--diff` then prints one line with the
byte count and SHA-256 of the installed file and of this binary's, instead of a diff. Every
write goes to a temporary file in the same directory and is renamed over the old one, the
`SKILL.md` before the marker.

These cases are errors, for every command and also when forced: nothing is written or removed
there, and the command exits 1 after the other skills were processed.

- `SKILL.md` is a symbolic link, a directory or anything but a regular file.
- Reading `SKILL.md` fails for any reason other than its absence, such as a missing read
  permission.
- Something other than a directory, or a link to one, is where the skill directory goes.
- A write, a rename or a removal fails.

`uninstall` removes a `SKILL.md` only when it is this binary's or the one the marker names, so
a skill someone changed is kept and listed. It removes the marker too, and the skill directory
once nothing else is in it; it leaves the `skills` directory and the agent's directory above
it.
