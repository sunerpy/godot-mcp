# Project commands

A project can offer its own editor tooling to an agent as commands: rebuilding a level from a
seed, checking assets, exporting a table. It declares them in `godot_mcp_commands.json` at its
root, each with a description and a JSON Schema for its arguments, and implements them in a
`@tool` GDScript. `editor.commands.list` reads the file and lists the commands without running
anything; `editor.commands.call` checks a call's arguments against the command's schema and has
the connected editor run it ([ADR 0016](../contributor/adr/0016-project-commands.md)).

Both tools are in the `editor` and `full` profiles. A listing needs no editor and no Godot
process. A call needs a connected editor running the `godot_mcp` add-on 0.5.0 or later; see
[Editor bridge](editor-bridge.md).

## Declaring commands

`godot_mcp_commands.json` sits next to `project.godot`:

```json
{
  "version": 1,
  "providers": [
    {
      "id": "levels",
      "script": "res://tools/level_commands.gd",
      "commands": [
        {
          "name": "rebuild",
          "title": "Rebuild a level",
          "description": "Generate res://levels/level_<seed>.tscn from a seed: a Node2D with one Marker2D per room, placed at random. Replaces the scene when it exists.",
          "input_schema": {
            "type": "object",
            "properties": {
              "seed": { "type": "integer", "minimum": 0 },
              "rooms": { "type": "integer", "minimum": 1, "default": 8 }
            },
            "required": ["seed"],
            "additionalProperties": false
          },
          "mutates": true
        },
        {
          "name": "count",
          "title": "Count the levels",
          "description": "How many scenes res://levels holds.",
          "input_schema": { "type": "object", "additionalProperties": false },
          "mutates": false
        }
      ]
    }
  ]
}
```

| Field | Rule |
| --- | --- |
| `version` | `1`, the only format version. |
| `providers` | The providers: at most 16 providers in a file, with at most 64 commands over all of them. An empty list declares nothing. |
| `id` | A provider's id, unique in the file: 1 to 64 characters, a lowercase ASCII letter first, then lowercase letters, digits, `_`, `.` or `-`. |
| `script` | The `res://` path of the provider's `.gd` file: at most 1,024 characters, with no empty segment, no segment starting with `.` and no backslash. |
| `commands` | The provider's commands, at least one. |
| `name` | A command's name, unique within its provider, by the same rule as an id. |
| `title` | A short title of 1 to 128 characters. |
| `description` | What the command does, for an agent deciding whether to call it: 1 to 1,024 characters. |
| `input_schema` | The JSON Schema of the command's arguments, with `"type": "object"` at its top: at most 16,384 bytes as compact JSON. See [Argument schemas](#argument-schemas). |
| `mutates` | `true` when the command changes project files, `false` when it does not. |

The file is at most 262,144 bytes, and holds no key the format does not name. A `$schema` key
is allowed and ignored, so a text editor can check the file against
`schemas/domain/project-commands-v1.schema.json`, which ships in the release archive. The server
also refuses what that schema cannot say: a provider id declared twice in the file or a command
name twice in one provider, more commands than the limit over all providers, a script path with
an empty or dot-leading segment, and an input schema over its size.

A declaration that breaks any rule is invalid as a whole: `editor.commands.list` shows every
problem with a JSON pointer to its place, and no command of the file can be called until it is
fixed. The server reads the file again for every listing and every call, so an edit takes
effect at once; nothing is reloaded or restarted.

### Argument schemas

The server checks a call's arguments with the JSON Schema subset its own contracts are written
in: `type`, `enum`, `const`, `properties`, `required`, `additionalProperties`, `items`,
`minItems`, `maxItems`, `minLength`, `maxLength`, `minimum`, `maximum`, `pattern` and `oneOf`.

- `type` names `null`, `boolean`, `integer`, `number`, `string`, `array` or `object`, or a list
  of them; `additionalProperties` and `items` take a schema or a boolean.
- A `pattern` is anchored, `^…$`, and built from literals, escapes, character classes and the
  quantifiers `*`, `+`, `?`, `{n}` and `{n,m}`, optionally as one alternation at its top
  (`^(a|b)$`). It is matched in time linear in the text.
- `$schema`, `$id`, `title`, `description`, `examples` and `default` are annotations and are
  ignored. A `default` is not filled in: the provider sees only the arguments the caller sent,
  which is why the example's provider supplies its own value for `rooms`.

A command whose schema uses anything else, such as `exclusiveMaximum`, `anyOf`, `$ref`, `format`, or a
pattern with a nested group, a back-reference or the `.` wildcard, or whose top is not
`"type": "object"`, stays in a valid declaration. It is listed with `available: false` and an
`unavailable_reason` that names the construct and its place, and a call to it is refused with
`unsupported`.

The arguments travel to the editor as the bridge's tagged values, which carry them exactly
within these bounds: 262,144 bytes as compact JSON in all, nested at most 16 levels, at most
4,096 items in one array or entries in one object, strings of at most 65,536 characters, and
integers no larger than 2^53 in magnitude, which the editor reads exactly. A call beyond them is
refused before the editor is asked.

## Writing a provider

The provider of the example, at `res://tools/level_commands.gd`:

```gdscript
@tool
extends RefCounted

const LEVELS := "res://levels"


func mcp_call(command: String, args: Dictionary):
	match command:
		"rebuild":
			return _rebuild(args["seed"], args.get("rooms", 8))
		"count":
			return {"ok": true, "value": _levels().size()}
	return {"ok": false, "error": "unknown command %s" % command}


func _rebuild(seed: int, rooms: int) -> Dictionary:
	var root := Node2D.new()
	root.name = "Level%d" % seed
	var random := RandomNumberGenerator.new()
	random.seed = seed
	for index in rooms:
		var room := Marker2D.new()
		room.name = "Room%d" % index
		room.position = Vector2(random.randi_range(0, 2048), random.randi_range(0, 2048))
		root.add_child(room)
		room.owner = root
	var scene := PackedScene.new()
	var packed := scene.pack(root)
	root.free()
	if packed != OK:
		return {"ok": false, "error": "the level did not pack: %s" % error_string(packed)}
	DirAccess.make_dir_recursive_absolute(LEVELS)
	var path := "%s/level_%d.tscn" % [LEVELS, seed]
	var saved := ResourceSaver.save(scene, path)
	if saved != OK:
		return {"ok": false, "error": "%s did not save: %s" % [path, error_string(saved)]}
	return {"ok": true, "value": {"path": path, "rooms": rooms}}


func _levels() -> PackedStringArray:
	var found := PackedStringArray()
	for file in DirAccess.get_files_at(LEVELS):
		if file.ends_with(".tscn"):
			found.append(file)
	return found
```

- **It is a tool script.** The editor runs only scripts that carry `@tool`. A provider without
  it is refused before anything is loaded, and the listing says why.
- **`mcp_call(command, args)` is its entry point.** It receives the command's name and the
  arguments as a `Dictionary` of plain values: an object arrives as a `Dictionary`, an array as
  an `Array`, a number written without a fraction or an exponent as an `int`, any other number
  as a `float`. It may `await`; the call ends when it returns.
- **It answers a `Dictionary` with a boolean `ok`.** `{"ok": true, "value": ...}` answers the
  value; `{"ok": false, "error": "..."}` reports a failure with its text. Anything else is
  answered `failed`, with Godot's spelling of what it returned.
- **It is read from its file on every call.** An edited provider takes effect at the next call,
  and loading it runs its static initializers. A provider that extends `Node` is added under the
  add-on's plugin while it runs, so `get_tree()` works, and is freed afterwards; any other
  provider is released when the call ends.
- **It is the project's code, and nothing confines it.** It runs on the editor's main thread with
  your privileges, as every `@tool` script does. Nothing it does is undone, and a change it makes
  to an open scene is in the editor's undo history only when the provider puts it there.

## Listing the commands

`editor.commands.list` takes no arguments. It reads the declaration and the provider scripts it
names, and runs neither:

| Field | Holds |
| --- | --- |
| `file`, `status` | `godot_mcp_commands.json`, and `absent`, `valid` or `invalid`. |
| `sha256`, `bytes` | The file's digest and size, or `null` when it is absent. |
| `enabled`, `disabled_reason` | Whether this server allows project commands, and why not. |
| `editor` | `connected`, whether an editor running the add-on is connected, and `serves_commands`, whether its session carries `editor.commands.call`. |
| `commands` | How many commands the file declares. |
| `providers` | Each provider's `id`, `script` and the `script_sha256` of the script as it is now, with its `commands`: `name`, `title`, `description`, `input_schema`, `mutates`, `available` and `unavailable_reason`. |
| `problems` | Every rule the file breaks, as a JSON `pointer` and a `message`. |
| `workspace` | The workspace revision the answer was read at. |

A command is `available` when the server allows project commands, the declaration is valid, its
schema stays in the subset, and its provider's script exists and carries `@tool`. Whether an
editor is connected does not change it; `editor` says that.

## Calling a command

`editor.commands.call` takes the `provider`, the `command`, its `arguments` (an empty object when
omitted), an optional `deadline_ms` and the `idempotencyId` every mutation needs:

```json
{
  "provider": "levels",
  "command": "rebuild",
  "arguments": { "seed": 7, "rooms": 4 },
  "deadline_ms": 30000,
  "idempotencyId": "<a new unique id>"
}
```

The server checks the call against the declaration and the provider's script as they are on disk
at that moment. A call it cannot make is refused before the editor is asked, with nothing
journaled:

| Answer | When |
| --- | --- |
| `permission_denied` | The server does not allow project commands; see [Turning them off and on](#turning-them-off-and-on). |
| `invalid_input` | The file is absent or invalid; it declares no such provider or command; the provider's script is missing or has no `@tool` line; the arguments break the command's schema or the bounds above; `deadline_ms` is outside 1,000 to 600,000. |
| `unsupported` | The command's schema leaves the subset; or the connected add-on predates the method, and the answer names its version and says to run `editor.addon.update` and then `editor.addon.reload`. |
| `unavailable` | No editor running the add-on is connected. |

The add-on checks the request again before it loads anything. While its panel pauses agent edits
it answers `unavailable`, and it refuses a request the server did not offer
(`permission_denied`) or whose script is gone or lost its `@tool` line since (`invalid_input`).
Each of these answers is `not_started`: nothing changed, and nothing is blocked. Once the add-on
has loaded the script, the call is one journaled transaction over the whole project, as
`editor.script.run` is, because the command may write any file. Its answer carries:

| Field | Holds |
| --- | --- |
| `classification` | `passed` when `mcp_call()` answered `ok: true` and the editor logged no error meanwhile; `failed` when it answered `ok: false` or without a boolean `ok`, when the script defines no `mcp_call`, or when errors were logged while it loaded and ran; `compile_error` when the script did not compile and `mcp_call()` was not called; `inconclusive` when it answered `ok: true` on a Godot build whose log a plugin cannot read (4.4.1), so an error inside it cannot be seen. |
| `reason` | Why, in one sentence. |
| `command` | The `provider`, `name` and `mutates` flag of the command called. |
| `result`, `result_literal` | The answered `value` as a tagged value, and as Godot's own spelling of it, which is all an object or a resource has; `result_unavailable` says why there is no value. |
| `error` | The command's `error` text, when it answered `ok: false`. |
| `diagnostics`, `output` | What the editor logged while the script loaded and ran: errors and warnings with their file and line, and what it printed. `log_capture` says whether the log could be read. |
| `changes` | Every project file that differs from before the call, compared by content: `added`, `modified` and `removed`, with their `total`. `changes_unavailable` says why the project was not compared. |
| `declaration_mismatch` | Set when a command declared with `mutates: false` changed project files, and how many. |
| `applied`, `rolled_back` | Always `true` and `false`: nothing a command does is undone. |
| `transaction_id`, `mutation_status`, `replayed` | The journaled transaction, as for every editor mutation. |

- **A deadline ends the wait, not the code.** The server waits `deadline_ms` (60,000 by default)
  and a further two seconds. A call not answered by then is `uncertain`, with the project files
  the command had changed until then in `changed_paths`. The editor may still be running it, and
  since it may still write files, every change is refused until the command's answer, when it
  returns, settles the transaction, or an operator settles it once the editor has ended, as after
  an unanswered [rescan](editor-bridge.md#rescanning-the-project); see
  [Recovery](recovery.md#late-editor-answers). Reads still work, the editor stays connected, and
  the command goes on there until it returns. Only closing the editor ends a command that never
  returns.
- **It holds the editor.** Requests run one at a time, so the editor's other requests wait while
  a command awaits, and code that holds the main thread past the bridge's 30-second liveness
  window can end the session; the command's answer then comes on the next one.
- **A retry replays.** The same `idempotencyId` with the same arguments answers the journaled
  outcome with `replayed: true` and does not call the command again. A replay carries no value
  and no file list, which `result_unavailable` says.
- **`mutates` is what the project says, not what the server enforces.** A command declared with
  `mutates: false` can still write files; the answer then sets `declaration_mismatch`.

## Turning them off and on

Project commands are on unless the operator turns them off, except behind a proxy:

| Server | Default |
| --- | --- |
| stdio, `trusted_local` and `open` HTTP | on |
| `proxied_oidc` HTTP | off |

`project_commands = false` in the `--config` file, or `GODOT_MCP_PROJECT_COMMANDS=false` with
`--config`, turns them off; `project_commands = true` turns them on for `proxied_oidc`. While they
are off, `editor.commands.list` still lists the declaration, with every command unavailable and
the reason; `editor.commands.call` answers `permission_denied` and sends nothing; and the editor's
session does not offer the add-on the method, which the add-on checks too. See
[Configuration](configuration.md#project-commands). An `open` listener without a token, on an
address other hosts can reach, warns at start that anyone who reaches it can run these commands
([HTTP: `open`](http-open.md)).

They are on by default, unlike [script execution](configuration.md#script-execution), because
the code they run is the project's own: the editor already runs a project's `@tool` code as it
opens the project and its scenes, and a call picks one of the commands the project declares and
passes arguments the project's own schema accepts. A caller the server only knows through a
proxy reaches that code only when the operator says so. The
[threat model](../security/threat-model.md#project-commands) has the rest.

## Limits

- A command's `value` is answered when Godot's spelling of it is at most 262,144 characters;
  a longer one is reported by its length.
- An `error` text is cut at 4,096 characters.
- An answer carries at most 200 log entries; `log_truncated` says when more were logged.
- An answer lists at most 1,000 changed files, and says `truncated` when there are more. A
  project of more than 100,000 files or 4 GiB is not compared, which `changes_unavailable` says.
- `deadline_ms` is from 1,000 to 600,000.

## Verified on

Godot 4.4.1, 4.5.1, 4.6.1, 4.7.1 and 4.7.2, official Linux x86_64 builds, headless editors, by
`crates/godot-mcp-server/tests/live_editor_commands.rs` through the production MCP server: calls
refused before the editor is asked, a command that answers its value and the files it wrote, one
declared with `mutates: false` that wrote one, one that reports an error, a provider that does not
compile, a retry that replays, a call that outlasts its deadline and is finished by the editor,
whose answer then settles it and reopens writes, and the add-on's own cases
(`crates/godot-mcp-bridge/tests/fixtures/addon/project_commands_cases.gd`). `make godot-test` runs
the suite against each locked Linux editor. Windows and macOS editors: not measured.
