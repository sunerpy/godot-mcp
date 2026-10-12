# Profiles and answers

A profile is a deliberate choice about what the server may do to your project. It is not a
convenience setting, and the safe choice is the default.

## Four profiles

| Profile | What it adds | Choose it when |
| --- | --- | --- |
| `core` | Read-only discovery and analysis | You want answers about the project and nothing changed |
| `editor` | Project authoring — files, GDScript, scenes, resources, project settings and assets — the engine-backed import, and the editor add-on | You want the project changed |
| `runtime` | Running, testing, exporting and engine-checking the project, managed game and editor sessions, and the authoring reads | You want the project executed and observed |
| `full` | The union of the three | You want all of the above |

`full` is exactly `core`, `editor` and `runtime` together. Every tool and the profiles that list it
are in the generated [tool inventory](../../protocol/tool-inventory.md).

No profile has a tool that executes arbitrary GDScript or calls an arbitrary engine method. That
is the largest deliberate difference from servers that expose an "execute this script" entry
point: loading a Godot project already runs its code, and a direct execution tool on top of that
would remove what makes the surface reviewable.

## Membership is not availability

A tool appears in `tools/list` because the profile lists it. Whether what it needs is there right
now is answered per call: with no Godot build resolved, `project.test` answers `unavailable` and
says so; with no game running, `runtime.tree.get` answers `unavailable` and points at
`project.run.start`. The tool never disappears from discovery and never pretends to have acted, so
a client can rely on the tool list and learn about the machine from the answers.

## What an answer means

An answer is either a result or a structured error with one of ten codes:

| Code | Means |
| --- | --- |
| `invalid_input` | The arguments do not match the tool's schema or its rules |
| `not_found` | The file, node, session or preset does not exist |
| `conflict` | The project changed under the request, or a file is held by the open editor |
| `permission_denied` | The path, the caller or the profile is not allowed |
| `unsupported` | The project uses something this build cannot handle, such as a binary `.scn` |
| `unavailable` | What the tool needs is not there right now; the answer says what and how to get it |
| `timeout` | The deadline passed before the work finished |
| `cancelled` | The client cancelled the request before it started |
| `uncertain` | The effect may or may not have happened, or an analysis cannot be sure |
| `internal` | A fault in the server itself |

`uncertain` is the honest answer, not a softer `internal`: a static analysis that cannot resolve a
dynamic call says so and lists every candidate, and a write whose outcome cannot be proved is
settled by the journal before anything else is served. Read the project again before retrying an
`uncertain` write. The full vocabulary, with every limitation code, is the
[outcomes reference](../../protocol/error-taxonomy.md).

## Repeating a call

Every call that changes something carries an `idempotencyId`. Repeating a call with the same id
and the same arguments returns the recorded outcome instead of acting twice, so a client can retry
after a lost answer without doubling the change. The same id with different arguments is refused
with `conflict`.
