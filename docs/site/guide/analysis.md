---
description: Tools that read the project, its GDScript, scenes, resources and dependencies without starting Godot.
---

# Analysis

Every tool on this page reads the project's files and nothing else. They work in every profile, on
a machine with no Godot installed, and they never start a process.

## The project

| Tool | Answers |
| --- | --- |
| `project.info.get` | The project's identity, a summary of its settings and the workspace revision |
| `project.discover` | Every script, scene, resource and other file, with its kind |
| `project.files.list` | Every file, or those under a prefix or matching a glob, with its kind, size and SHA-256 — the hash a later write names |
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

## The whole project at once

| Tool | Answers |
| --- | --- |
| `project.dependencies.audit` | The files nothing in the project reaches, each with why, or with `view: "cycles"` the files that reference each other in a ring and what Godot does with that ring |
| `project.signals.get` | Every signal a script declares, with the scenes and scripts that connect it and the lines that emit it, or with `view: "problems"` connections to missing methods, signals emitted but never connected and connections to undeclared signals |
| `project.metrics.get` | Files and bytes by kind and extension, with GDScript code, comment and blank lines, or with `view: "complexity"` functions ranked by complexity, length or nesting and scenes by nodes, depth or instances |
| `project.search` | Every line matching a literal string or a regular expression, with context lines |
| `project.uid.get` | The file a `uid://` names, or the UID of a file, from the scene headers, `.uid` files and `.import` files the project holds |
| `scene.nodes.find` | Nodes in every scene by type, group, name, script or stored property, instanced scenes included |
| `scene.exports.get` | The exported variables of one scene's scripted nodes, with each script default and the value the scene stores |

The unused list is a starting point, not a verdict: a script that builds a path at run time,
such as `load("res://levels/" + name + ".tscn")`, can load files nothing names. The answer lists
every such call, and every unused file under its literal prefix names the calls that may load
it.

Godot's editor skips hidden files and folders and every folder that holds a `.gdignore`; these
tools skip the same files, and every answer says how many it left out. A scene connection to a
method its receiver lacks is reported as a problem because Godot is silent about it: the scene
loads and the signal fires without an error. A UID that only Godot's cache in `.godot/` holds
is reported as `unknown`, not as missing.

`include` and `exclude` take globs: `*` and `?` stay within one folder, `**` crosses folders,
and a pattern without a `/` matches file names in any folder, as `*.tscn` does. The
[project-wide analysis reference](../../operator/project-analysis.md) lists every view, field
and bound.

## Reading the answers

Every analysis answer carries the same envelope: the project's identity and generation, the
workspace revision and whether the index was coherent, the provenance of the answer (`source` and
`confidence`) and, for lists, a page with `limit`, `offset` and `next_offset`.
