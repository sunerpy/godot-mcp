# Analysis

Every tool on this page reads the project's files and nothing else. They work in every profile, on
a machine with no Godot installed, and they never start a process.

## The project

| Tool | Answers |
| --- | --- |
| `project.info.get` | The project's identity, a summary of its settings and the workspace revision |
| `project.discover` | Every script, scene, resource and other file, with its kind |
| `project.files.list` | Every file, or those under a prefix, with its kind, size and SHA-256 — the hash a later write names |
| `project.settings.get` | The typed settings: application name, main scene, autoloads, input actions and their events, enabled plugins, with the SHA-256 of `project.godot` |
| `project.validate` | Parse errors, dangling references and invalid settings across the project, stating that the engine was not run |

Answers come from one coherent snapshot of the project's files and name the workspace revision
they were taken from; a write that commits advances the revision.

## GDScript

| Tool | Answers |
| --- | --- |
| `gdscript.symbols.get` | The classes, functions, signals, variables and constants one script declares |
| `gdscript.references.get` | The references to and callers of one script, or of one function, signal or class name it declares |
| `gdscript.impact.get` | The scenes, resources and scripts that depend on a script or a symbol, transitively |
| `gdscript.diagnostics.get` | The parser's and the analysis's diagnostics for the project or for one script |
| `script.validate` | A static check of one script, stating that the engine was not run |

An edge carries a `confidence`: `exact` when the code names its target, `inferred` when the
target is resolved through a typed variable or a class name, and `uncertain` when it cannot be
pinned down — then the answer lists every candidate instead of picking one. An answer also names
its limitations: GDScript calls a node or a signal by a string (`get_node("Path")`,
`connect("hit", …)`), and the answer says when such an edge cannot be followed rather than leaving
it out silently.

```text
gdscript.references.get  script: player/player.gd  symbol: take_damage
→ main.gd:6  player.take_damage()  calls  inferred
```

To check a script against the engine itself, use `script.check`, which runs Godot's own
`--check-only` ([Running the engine](engine.md)).

## Scenes and resources

`scene.tree.get` reads a text `.tscn` file: every node with its type, parent, groups, script and
instanced scene, the signal connections with their methods, the file's SHA-256, and a `handle` per
node that `scene.batch.apply` accepts until the file changes. `resource.tree.get` reads a text
`.tres` file the same way: its type, properties, sub-resources and references.

Binary `.scn` and `.res` files answer `unsupported`, with the remedy: re-save the file in Godot as
text. The server does not guess at a binary format.

## Dependencies

| Tool | Answers |
| --- | --- |
| `project.dependencies.get` | Every dependency edge between the project's scripts, scenes and resources, resolved or dangling |
| `scene.dependencies.get` | The scripts, sub-scenes and resources one scene depends on, and how each resolves |
| `resource.dependencies.get` | The external and sub-resource dependencies of one resource, and how each resolves |

An edge whose target does not exist is reported as dangling rather than dropped, and
`project.validate` lists every dangling reference in the project.

## Reading the answers

Every analysis answer carries the same envelope: the project's identity and generation, the
workspace revision and whether the index was coherent, the provenance of the answer (`source` and
`confidence`) and, for lists, a page with `limit`, `offset` and `next_offset`.
