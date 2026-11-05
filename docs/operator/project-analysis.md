# Project-wide analysis

Seven read-only tools answer questions about a whole project at once: which files nothing uses,
which references form a cycle, how signals are wired, how large and how complex the code is,
where a string occurs, what a `uid://` names, which nodes match a query across scenes, and what
a scene's scripts export. They are in every profile, read the same coherent workspace snapshot
as every other static tool, start no process and need no Godot installation.

| Tool | Views | Answers |
| --- | --- | --- |
| `project.dependencies.audit` | `unused`, `cycles` | Files no root reaches, with why; strongly connected reference sets, classified by what Godot does with them |
| `project.signals.get` | `signals`, `problems`, `undeclared_connections` | Every declared signal with its scene connections, code connections and emissions; wiring problems |
| `project.metrics.get` | `statistics`, `complexity` | Counts by kind and extension and GDScript line classes; functions and scenes ranked by complexity |
| `project.search` | — | Literal or regular-expression matches in the project's text files |
| `project.uid.get` | `uid`, `path`, or the whole table | A UID as a path, or a path as its UID, from the files that record them |
| `scene.nodes.find` | — | Nodes across every text scene by type, group, name, script or stored property |
| `scene.exports.get` | — | The `@export` variables of one scene's scripted nodes, with defaults and stored values |

`project.files.list`, in the `editor`, `runtime` and `full` profiles, also takes a `glob`, in
the syntax below.

## What every view shares

**The envelope.** An answer carries the project's identity, the workspace revision it was read
at, and its provenance: `source` and `confidence`. The envelope's `confidence` is the weakest
confidence anywhere in the answer, and at most `inferred` when the answer's coverage is
incomplete. `exact` means the files name the fact; `inferred` means it was derived (an unused
file is always `inferred`, because a load computed at run time may still reach it);
`unresolved` means the static view cannot decide it.

**Coverage.** Every view returns `coverage`: `complete`, and a list of `gaps`, each with a stable
`code`, a message, a count and up to 20 named subjects. A gap says what the answer could not
see, for example `files_not_read`, `files_too_large`, `binary_documents_not_decoded`,
`damaged_scripts`, `c_sharp_not_analysed`, `dynamic_loads`, `dynamic_signal_names`,
`match_limit_reached` or `uid_cache_not_read`. An absence is claimed only over what was read.

**Files Godot ignores.** Godot's editor never scans a file whose name, or the name of a
directory above it, starts with `.`, nor a file under a directory that holds a `.gdignore`
(one at the project root is not honoured). It writes no `.uid` sidecar or import for such a
file and registers no `class_name` it declares; this was observed on Godot 4.4.1, 4.5.1, 4.6.1
and 4.7.1. The views follow Godot: such files are not read, listed, searched or counted, a
reference to one still resolves, and every answer reports how many it left out as
`ignored_files`, split into `hidden` and `gdignore`. An argument that names such a file is
refused with `not_found`, with the reason.

**Paging.** A list is returned a page at a time: `offset` (default 0) and `limit` (default
200, at most 1,000), answered with a `page` holding `offset`, `limit`, `returned`, `total`,
`truncated` and `next_offset`. The order is deterministic, so a caller can resume at
`next_offset`. Lists nested inside one item are capped and carry an exact count beside them;
the caps are given with each view. An answer larger than `limits.max_output_bytes` is refused
with `invalid_input`; ask for a smaller `limit`.

**Reading.** A view reads at most 512 MiB of file bytes per call. A text file larger than
8 MiB is counted but not parsed or searched. A file left unread for either reason is a coverage
gap, never a silent omission. Each call parses the files it needs again; nothing is cached
between calls.

## Globs

`include` and `exclude` (at most 32 globs each) and the `glob` of `project.files.list` select
project-relative paths:

| Pattern | Matches |
| --- | --- |
| `*` and `?` | Any run of characters and any one character, never across a `/` |
| `**` | Any number of whole directories |
| `[a-z]`, `[!a]` | One character in, or not in, the class |
| `{gd,tscn}` | Either alternative |
| `\*` | A literal `*` |

A glob without a `/` matches the file name at any depth, as a `.gitignore` line does: `*.tscn`
selects every scene. A leading `res://` is accepted and ignored. Matching is case-sensitive,
and a glob is at most 1,024 bytes. A file is selected when it matches an `include` glob (or
none was given) and no `exclude` glob.

## Unused files and cycles: `project.dependencies.audit`

The audit builds the project's file reference graph from what the files declare:

| Reference | From |
| --- | --- |
| `ext_resource`, `scene_instance`, `script_attachment`, `placeholder` | Scene and resource `[ext_resource]` declarations, a known `uid` winning over `path` as Godot loads them |
| `extends`, `preload`, `load`, `class_reference` | GDScript `extends`, `preload`, the `load` family with a literal path, and uses of a registered `class_name` |
| `path_literal`, `text_reference` | A quoted `res://` path in a script, scene or resource, or in any other text file; these are only evidence, so only `inferred` |
| `plugin_script`, `import_source` | The script a `plugin.cfg` names; the source an imported file was made from |

`view: "unused"` (the default) lists every scene, script, resource, shader and asset no root
reaches. The roots are the main scene, the autoloads, every `res://` or `uid://` value in
`project.godot`, `res://default_bus_layout.tres` when the bus layout setting is unset, the
enabled plugins, files an export preset selects, `.gdextension` descriptors and `EditorScript`s.
An `include_filter` is matched the way Godot matches it: `*` crosses `/`, `?` matches any
character but `.`, and case is ignored; the files it selects were the files Godot packed with
`--export-pack` on Godot 4.4.1, 4.5.1, 4.6.1 and 4.7.1. Each entry says why it is unused:
`unreferenced`, or `referenced_only_by_unused` with up to 10 of those referrers and their count,
and it names the sidecars that belong to it. The answer also lists the roots, the references
whose target does not exist (`broken_references`) and every load whose path is computed at run
time (`dynamic_loads`), each with its literal prefix; an unused file under such a prefix names
up to 10 of the sites that may load it, with their count. The used set is therefore a lower
bound. Up to 100 roots, dynamic loads and broken references are listed, each with an exact
count.

`view: "cycles"` lists every strongly connected set of files with its members and edges (up to
100 each, with exact counts), one concrete ring as a `witness`, and a classification. What
Godot does with each kind was observed on Godot 4.4.1, 4.5.1, 4.6.1 and 4.7.1:

| Classification | Made of | What Godot does | `breaks_loading` |
| --- | --- | --- | --- |
| `inheritance` | `extends` alone | No script of the ring compiles ("Could not resolve super class inheritance"); the 4.4.1 and 4.5.1 editors crash while scanning a project that holds one | `true` |
| `resource_load` | `[ext_resource]`s alone | The loader rejects the reference that closes the ring ("referenced non-existent resource") and loads the rest without it | `true` |
| `tolerated` | Anything that passes through `preload`, `load`, a class-name use or a path string | Every member loads | `false` |

`include` and `exclude` narrow what is reported, not what is analysed: an unused file is listed
when it is selected, and a cycle when any of its members is.

## Signal flow: `project.signals.get`

`view: "signals"` (the default) lists every signal a project script declares, with its
parameters, and three lists capped at 50 entries each, with exact counts:

- the scene `[connection]`s to it, resolved through instanced scenes and script inheritance to
  the emitter's script and the receiving method, which is `found`, `missing` or `unverified`;
- code connections, `signal_name.connect(callable)` and `connect("signal_name", callable)`;
- emissions, `signal_name.emit(...)` and `emit_signal("signal_name", ...)`.

`view: "problems"` lists:

| Kind | Meaning |
| --- | --- |
| `missing_handler` | A scene connection's method is declared by none of the receiving node's scripts; methods of the engine class at the bottom of that chain are not checked |
| `emitted_never_connected` | A signal is emitted, but no scene or script connects a signal of that name |
| `connected_never_declared` | A scene connects a signal the emitter's scripts never declare; it may be one of the engine class's own signals, so it is `unresolved` |

A missing handler matters because Godot does not report one: a scene whose connection names a
method its receiver lacks instantiates without an error, and when the signal fires the
connection is skipped, again without an error. This was observed on Godot 4.4.1, 4.5.1, 4.6.1
and 4.7.1.

`view: "undeclared_connections"` lists connections to signals no project script declares,
usually engine signals such as `pressed` or `tree_exited`. `signal` and `script` narrow every
view. A connection or emission whose signal name is computed at run time is a
`dynamic_signal_names` gap.

## Statistics and complexity: `project.metrics.get`

`view: "statistics"` (the default) counts the files Godot sees and their bytes, by kind and by
extension (the extension list is the one that is paged), and reports, for GDScript, the files,
their lines split into code, comment and blank, `class_name` registrations, inner classes,
functions, signals, members, `@export`ed variables and `@tool` scripts, and, for text scenes and
resources, the scenes, nodes, instances, scripted nodes, connections and resources.

`view: "complexity"` ranks `subject: "functions"` (the default) or `subject: "scenes"`:

| Subject | `sort` | Thresholds |
| --- | --- | --- |
| `functions` | `cyclomatic` (default), `lines`, `nesting` | `min_cyclomatic`, `min_lines`, `min_nesting` |
| `scenes` | `nodes` (default), `depth`, `instances` | `min_nodes`, `min_depth` |

A function's cyclomatic complexity is 1 plus one for each `if`, `elif`, `for`, `while`, match
arm, match guard, conditional expression and `and`/`or` operator. Its nesting is the deepest
nesting of `if`/`elif`/`else`, `for`, `while`, match-arm and lambda bodies. A scene's depth
counts its root as 0. A function that overlaps a damaged region is marked `damaged`, and its
counts are lower bounds. A threshold, sort or subject that does not apply to the chosen subject
is refused with `invalid_input`.

## Content search: `project.search`

`query` is a literal string unless `regex` is `true`, in which case it is a regular expression
in the syntax of the Rust `regex` crate. Both compile to a matcher that never backtracks and
whose compiled form is capped at 1 MiB, so no pattern can make a search run away; a pattern is
at most 1,024 bytes. `case_sensitive` and `whole_word` default to `false`. A match never spans
lines.

Each match carries its path, its 1-based line and column, its length, the line itself (a
500-character window around the match when the line is longer) and `context` lines on each
side (0 to 10, each cut to 200 characters). The search stops after `max_matches` (1 to 10,000,
default 1,000) and says so with `match_limit_reached`; more matches may then exist, so the
answer is `inferred`. `skipped` counts the files not searched: `excluded` by the globs,
`binary`, `too_large`, `not_read` and `after_limit`.

## UID lookup: `project.uid.get`

Godot keeps its authoritative UID map in its engine cache, `.godot/uid_cache.bin`, which this
server never reads. The same facts are recorded in project files, and the table is built from
those:

| Source | Where the UID is written |
| --- | --- |
| `document_header` | The `uid="uid://…"` attribute of a `.tscn` or `.tres` header |
| `uid_sidecar` | A `<file>.uid` sidecar beside a script or shader (Godot 4.4.1 and later) |
| `import_sidecar` | The `uid=` entry of an asset's `<file>.import` |

Pass `uid` to look a UID up as a path, `path` to look a file up as its UID, or neither to page
the whole table. The `status` is `resolved` when one existing file and one UID agree,
`conflict` when the records disagree (several UIDs for one file, or one UID for several files),
`orphaned` when every record names a missing file, and `unknown` when nothing in the project
records it. `unknown` is `unresolved`, not "has no UID":
the engine cache may still hold one. A lookup also lists, up to 100 with an exact count, the
`[ext_resource]` declarations that name the UID or the file; those record what a referrer
believed, not what the file records.

On Godot 4.4.1, 4.5.1, 4.6.1 and 4.7.1, `--import` writes a `.uid` sidecar for every script and
shader and a `uid=` entry for every imported asset, but adds no UID to a text scene or resource
whose header lacks one, and `ResourceLoader.get_resource_uid` then reports no UID for that
file either. On each of those builds the table agreed with Godot's own answers in both
directions.

## Nodes across scenes: `scene.nodes.find`

Every criterion given must hold:

| Argument | Matches a node when |
| --- | --- |
| `type` | Its type is that engine class exactly, or it runs a script whose `class_name`, or an inherited one, is `type` |
| `group` | It is in the group |
| `name` | Its name matches the pattern (`*` and `?`, case-sensitive) |
| `script` | It runs the script, directly or through inheritance |
| `property`, `value` | It stores the property, with exactly that value text (surrounding whitespace aside) when `value` is given |

A node that instances a scene takes its type and script from that scene's root unless it
overrides them, so a query finds instances too, and each match says how its type matched.
Engine classes are opaque to a static reader, so `type: "Node2D"` does not match a `Sprite2D`,
and the answer says so. Only nodes a scene file declares are searched: the nodes inside an
instanced scene are found by searching that scene. `include` and `exclude` choose the scenes.

## Exported variables: `scene.exports.get`

For one text `scene`, every scripted node's `@export` variables, through script inheritance:
the annotation, the type and where it came from, the script default, the value the scene or an
instanced scene stores (each cut to 1,000 characters, with a flag), and which applies:
`scene`, `instanced_scene`, `script_default` or `none`. `node` narrows the answer to one node
path. Built-in scripts are listed but not analysed, and a script that cannot be resolved is
named. A binary `.scn` answers `unsupported`, with the remedy.

## Measured cost

On a generated project of 5,301 files and 2.8 MB (2,000 scripts of 49 or 50 lines, 500 scenes
of 9 or 10 nodes in one 500-scene instancing chain, 200 resources and 300 imported images),
served by a release build on a 32-core Linux host, each view other than `project.search`
answered in 1.05 to 1.20 s over two calls each, and `project.search` in 0.10 s. Nothing larger
has been measured.
