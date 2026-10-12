# Third-party license notices

This project reuses behavior, fixtures, and interface conventions from
several open-source projects. Their license notices are preserved here.

## codegraph-rust (MIT)

The self-contained Godot/GDScript static-analysis layer derives its behavior
and fixtures from the MIT-licensed `codegraph-rust` project. There is no
runtime dependency on the sibling checkout.

Derived files and the upstream source they adapt:

| This repository | Upstream `codegraph-rust` source | What is adapted |
| --- | --- | --- |
| `crates/godot-mcp-analysis/src/gdscript/parser.rs` | `crates/codegraph-extract/src/lang/gdscript.rs` (`GdscriptSpec`) | GDScript node-kind / field vocabulary (`function_definition`, `constructor_definition`, `class_definition`, `enum_definition`, `call`; `name` / `parameters` / `return_type` / `body` / `class_body` / `enumerator_list` fields) and the `_init` constructor-name, `static_keyword` static-flag, parameter-list signature and return-type extraction rules. |
| `crates/godot-mcp-analysis/src/gdscript/nodes.rs` | same | The same node-kind and field vocabulary, re-expressed as workspace-owned typed records independent of any CodeGraph store or graph type. |
| `crates/godot-mcp-analysis/tests/fixtures/gdscript/*.gd` | `reference/golden/godot` GDScript language fixtures | Fixture shape and normalized golden semantics for classes, functions, signals, enums, `preload` and calls. |
| `crates/godot-mcp-analysis/src/project/settings.rs` | `crates/codegraph-resolve/src/frameworks/godot_project.rs` (`parse_project_godot`) and `godot_common.rs` (`map_res_path`, `strip_quotes`, `quoted_strings`) | The recognized `project.godot` section/key vocabulary (`[application] run/main_scene`, `[autoload] Name="[*]res://…"`, `[input] action={…}`, `[editor_plugins] enabled=PackedStringArray(…)`), the leading-`*` autoload singleton-marker rule, the `res://` → project-relative mapping, and the brace-depth tolerance that keeps a multi-line `[input]` dictionary value from being read as new keys. |
| `crates/godot-mcp-analysis/src/project/settings.rs` | `crates/codegraph-resolve/src/frameworks/godot_project.rs` | `project.godot` INI vocabulary (`[application] run/main_scene`, `[autoload] Name="[*]res://…"`, `[input] action={…}`, `[editor_plugins] enabled=PackedStringArray(…)`), the leading-`*` singleton-marker rule, the `res://` → project-relative mapping and the brace-depth tolerance for multi-line `[input]` dictionary values. |
| `crates/godot-mcp-analysis/src/scene/parser.rs` | `crates/codegraph-resolve/src/frameworks/godot_scene.rs` (`parse_tscn`, `parse_scene_uid`, `parse_attrs`) and `godot_resource.rs` (`parse_tres`) | The `.tscn` / `.tres` section vocabulary (`[gd_scene]` / `[gd_resource]` descriptors with `format` and `uid`, `[ext_resource type/uid/path/id]`, `[sub_resource type/id]`, `[node name/type/parent]`, `[resource]`, `[connection signal/from/to/method]`), the in-file `id` → path identifier table with `ExtResource("id")` / `SubResource("id")` handles, the `script =` attachment, `groups = [...]` membership and `instance=` sub-scene instantiation rules, `.` as the scene-root marker for node parents and connection endpoints, and the balanced-delimiter header-attribute scan that keeps `instance=ExtResource("2")` whole. |
| `crates/godot-mcp-analysis/src/scene/model.rs` | same | The same section/attribute vocabulary, re-expressed as workspace-owned typed records with source spans and provenance, independent of any CodeGraph node, edge or store type. |
| `crates/godot-mcp-analysis/src/graph/builder.rs` | `crates/codegraph-resolve/src/frameworks/godot.rs` (`GodotResolver::extract`, `resolve`, `post_extract`, `autoload_script_bindings`) | The cross-document Godot relationship vocabulary: autoload singletons as project-level names whose backing script is confirmed only after the whole project is indexed, `class_name` as a global script class, scene→script binding through `script = ExtResource(...)`, `[connection method=...]` → handler-function wiring, and the posture that dynamic GDScript dispatch (`connect`, `get_node`, `$`, `%`, group queries, autoload member access) is not resolvable by syntax alone. |
| `crates/godot-mcp-analysis/src/graph/model.rs` | same | The same relationship vocabulary, re-expressed as workspace-owned typed graph records with source spans, per-candidate provenance and an explicit exact / inferred / unresolved classification, independent of any CodeGraph node, edge, store or `ResolvedBy` type. |
| `crates/godot-mcp-analysis/tests/fixtures/graph/golden/semantic-graph.json` | `reference/golden/godot` (`nodes.json`, `edges.json`, `refs.json`) | The byte-stable golden-graph fixture shape: a committed, deterministically ordered serialization of the derived nodes and classified edges, compared byte for byte by the acceptance test. |

Only behavior is adapted. No upstream source file is copied verbatim, and
nothing in this workspace links to or reads from that checkout at build or run
time.

```
MIT License

Copyright (c) 2026 sunerpy

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## rmcp — modelcontextprotocol/rust-sdk (Apache-2.0)

The MCP protocol crate `rmcp` (pinned exactly to `=2.2.0`) is distributed
under the Apache License, Version 2.0. See
<https://www.apache.org/licenses/LICENSE-2.0> for the full text.

## tree-sitter-gdscript (MIT)

The GDScript grammar crate `tree-sitter-gdscript` (pinned exactly to `=6.1.0`)
is distributed under the MIT License.
