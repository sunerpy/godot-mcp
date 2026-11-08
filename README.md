<div align="center">

# godot-mcp

**A local Model Context Protocol server for Godot projects.**

Static analysis of GDScript, scenes and resources; journaled edits; and Godot
games and editors it starts, drives, captures and debugs — one static Rust binary
that runs beside your project.

[![CI](https://github.com/sunerpy/godot-mcp/actions/workflows/ci.yml/badge.svg)](https://github.com/sunerpy/godot-mcp/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/sunerpy/godot-mcp)](https://github.com/sunerpy/godot-mcp/releases)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE-MIT)

[**English**](README.md) · [简体中文](docs/readme/README.zh-CN.md) ·
[Website](https://firlab.app/godot-mcp/) · [Documentation](docs/README.md) ·
[Install](#install) · [Quick start](#quick-start) · [Contributing](CONTRIBUTING.md)

</div>

---

## Why godot-mcp

Your MCP client talks to one local `godot-mcp` process, never to the Godot editor
directly, and the process works in your project the way a careful collaborator
would: it reads before it writes, it proves what it did, and it says so when it
cannot.

- **A Rust server, not only an editor plugin.** One static Rust binary, with
  no language runtime to install, serves MCP, analyses the project, journals
  every change and contains every Godot process it starts. GDScript runs inside
  Godot only where the engine is needed: a headless helper pinned by its
  SHA-256, with which Godot itself writes every scene and resource the server
  changes; a probe passed on the command line to each game it starts, so no
  autoload is installed and no project setting changes; and the `godot_mcp`
  editor plugin, which you install only to bring in the editor you have open.
  With no editor open it still reads, edits, runs, tests and exports the
  project, and analysis and file edits need no Godot at all.
- **Analysis without the engine.** Project discovery and validation, GDScript
  symbols, references and impact, text scene and resource trees, the dependency
  graph, and project-wide views — unused files, reference cycles, signal flow,
  metrics, content search, UID lookups, and node and export queries across
  scenes — all answer on a machine with no Godot installed.
- **Edits as transactions.** File and script patches carry the expected SHA-256 of
  what they change; scene and resource batches are journaled. A crash mid-change
  is reconciled to the proven before- or after-state, or quarantined, before the
  next request is served. One batch can change up to 16 scenes and resources in
  one Godot run and land them all in one transaction, or none of them.
- **The real engine, pinned.** Validation, import, tests and exports run against
  the Godot build you configure, and so do reads of what only the engine knows:
  painted tile cells, animation keys, baked navigation polygons, a shader's
  uniforms and compile errors. Exports cover packs, patches and Android APKs;
  `android.deploy` installs and starts an APK on a device over adb, so far
  measured only against a stand-in adb. CI checks every release against exact
  locked Godot builds from 4.4.1 to 4.7.2.
- **Games and editors it owns.** Start a game or an editor, send keyboard, mouse,
  action and sequence input and record and replay it, read the live scene tree,
  properties, signals and UI, write live properties, wait for a condition, find
  and move nodes, change the game's speed, take screenshots and recordings and
  compare them with reference images, and step through code over the editor's
  language server and debug adapter. A test scenario of input, waits, assertions
  on node properties and Control text, image comparisons and stress windows ends
  in one report.
- **The editor you already have open.** With the bundled `godot_mcp` add-on, an
  agent inspects and edits the scenes your editor holds as undoable editor
  actions, saves and closes them, paints tile maps and keys animations through
  one fixed table of editing calls, reads back what only the engine knows about
  them, captures the editor, reads its log, changes its settings and shortcuts,
  moves its 3D cameras, answers its dialogs, restarts it once nothing it holds
  can be lost, picks up changed files and runs the game from the editor. A panel
  in the editor shows the connection and the newest 500 requests it answered.
- **Recipes for common setups.** `scene.recipe.apply` adds one of ten setups to a scene
  by name — a 2D or 3D physics body, lights with a sky and a camera, particles from six
  presets (fire, smoke, sparks, rain, snow, explosion), navigation, a positional sound, a
  shader material, a HUD, a grid map and an on-screen joystick — with its parameters, as one
  journaled batch, or as one undoable action in the editor. A theme recipe goes to
  `resource.create`. Each recipe is also an MCP prompt, `godot-mcp.recipe.<name>`, so an
  agent finds them without a tool slot ([Recipes](docs/operator/recipes.md)).
- **Refuses rather than guesses.** A binary `.scn` answers `unsupported` with the
  fix ("re-save it as text"); an outcome it cannot prove answers `uncertain`.
- **Scripts only when you allow them.** Start the server with `--allow-scripts`
  and an agent can run its own GDScript headless, in the open editor or in a
  running game, and see what it returned, printed and changed. It is off by
  default, and nothing confines a script once it is on.
- **Local-first.** stdio by default, with no port and no credential. Optional HTTP
  listens on loopback with a per-launch secret or behind an authenticated proxy, or, with
  `godot-mcp serve --http 0.0.0.0:8765`, on a development network with no credential or one
  token ([HTTP: `open`](docs/operator/http-open.md)).
- **Verifiable packages.** Static Linux (musl) and `crt-static` Windows builds,
  a `MANIFEST.json` the binary checks itself against, a CycloneDX SBOM, and
  release archives attested by GitHub.

## What it covers

Which tools reach each part of a Godot project. "Edit" means a `call_method` operation in
`scene.batch.apply`, `editor.scene.edit` or `resource.batch.apply`, or one in the operations
of `scene.create` and `resource.create`; "read" means `scene.read`, `resource.read` or
`editor.scene.read`. The [editor bridge](docs/operator/editor-bridge.md#editing-calls) lists
every method each class takes.

| Area            | Make and change                                                                                                                                                                                                                    | Read back, run and observe                                                                                                                                                       |
| --------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Animation       | Edit `AnimationPlayer`, `AnimationLibrary` and `Animation`: `add_animation`, `add_track`, `track_insert_key`, `set_loop_mode`; `SpriteFrames` for frame animations                                                                 | Read `get_animation_list`, `track_get_key_count`, `track_get_key_value`; in a game, `runtime.signals.get` and `runtime.wait`                                                     |
| AnimationTree   | Edit `AnimationNodeStateMachine` (`add_node`, `add_transition`), `AnimationNodeBlendTree` (`connect_node`) and the blend spaces (`add_blend_point`)                                                                                | Read `get_node_list`, `get_transition_count`, `get_blend_point_count`                                                                                                            |
| Physics         | `scene.recipe.apply` with `body-2d` or `body-3d`: a character, rigid, static or area body with its shape; `set_subresource` for any shape; `TileSet` physics layers                                                                | `scene.tree.get`; in a game, `runtime.nodes.find` near a point, `runtime.properties.get`, `runtime.sample`, `runtime.time_scale.set`                                             |
| 3D              | `scene.recipe.apply` with `body-3d`, `light-3d` (sun, lamp, sky, camera) or `gridmap-3d`; edit `GridMap` (`set_cell_item`) and `MeshLibrary` (`create_item`)                                                                       | Read `get_used_cells`, a mesh's `get_aabb`, a `Skeleton3D`'s bones; `editor.camera.get`, `editor.camera.set`, `editor.screenshot.capture` of a 3D viewport                       |
| Particles       | `scene.recipe.apply` with `particles-2d` and a preset: fire, smoke, sparks, rain, snow or explosion; `set_subresource` for a `ParticleProcessMaterial`                                                                             | In a game, `runtime.screenshot.capture`, `runtime.recording.capture`, `runtime.image.compare`                                                                                    |
| Navigation      | `scene.recipe.apply` with `navigation-2d`; edit `NavigationPolygon` (`add_outline`) and bake with the regions' `bake_navigation_polygon` and `bake_navigation_mesh`                                                                | Read `get_polygon_count`, `get_vertices`; in a game, `runtime.navigation.path.get` and `runtime.node.move` along a path                                                          |
| Audio           | `asset.put` and `project.import.run` for the sound, `scene.recipe.apply` with `audio-2d`, `resource.batch.apply` on the bus layout (`default_bus_layout.tres`) for its buses; `project.settings.patch` chooses another layout file | `asset.status`, `resource.tree.get` on the bus layout; in a game, `runtime.properties.get` on the player                                                                         |
| Shaders         | `file.create` for a `.gdshader`; `scene.recipe.apply` with `shader-2d` for a `ShaderMaterial` and its uniforms; edit `VisualShader` (`add_node`, `connect_nodes`)                                                                  | `shader.check` for compile errors and uniforms; read `get_shader_uniform_list`                                                                                                   |
| Themes          | `resource.create` or `resource.batch.apply` editing a `Theme` (`set_color`, `set_font_size`, `set_constant`, `set_stylebox`), as the `theme` recipe does; `scene.recipe.apply` with `hud`; edit `Control` anchors                  | Read `get_color`, `get_type_list`; in a game, `runtime.ui.get` and `runtime.ui.press`                                                                                            |
| TileMap         | Edit `TileSet` and `TileSetAtlasSource` (`add_source`, `create_tile`, terrains) and `TileMapLayer` (`set_cell`, `erase_cell`, `set_cells_terrain_connect`)                                                                         | Read `get_used_cells`, `get_cell_source_id`, `get_cell_atlas_coords`                                                                                                             |
| Runtime control | `project.run.start` and `project.run.stop`; `input.action.send`, `input.key.send`, `input.mouse.send`, `input.sequence.send`, `input.replay`                                                                                       | `runtime.tree.get`, `runtime.properties.get`, `runtime.properties.set`, `runtime.wait`, `runtime.ui.get`, `runtime.screenshot.capture`; `test.scenario.run` for a whole scenario |
| Project tooling | `editor.commands.call` runs a command the project declares in `godot_mcp_commands.json`, in the connected editor, with arguments checked against the command's schema                                                              | `editor.commands.list` lists the commands with their descriptions and argument schemas and runs nothing; a call answers what the command returned and every file it changed      |

The `full` profile holds 132 tools; the generated
[tool inventory](docs/protocol/tool-inventory.md) lists each one with its profiles.

## Install

### Release installer

The installers pick the archive for your platform, refuse it unless its digest
matches the release's `SHA256SUMS.txt`, extract the whole package, run its
`self-check`, and put `godot-mcp` on your PATH.

```sh
# Linux x86_64, macOS on Apple silicon
curl -fsSL https://raw.githubusercontent.com/sunerpy/godot-mcp/main/scripts/install.sh | sh
```

```powershell
# Windows x86_64, PowerShell 5.1+
irm https://raw.githubusercontent.com/sunerpy/godot-mcp/main/scripts/install.ps1 | iex
```

Pin a release with `GODOTMCP_VERSION=vX.Y.Z`. The package goes to
`~/.local/share/godot-mcp` (`%LOCALAPPDATA%\godot-mcp` on Windows); the scripts'
headers list every setting.

### Release archives

| Platform            | Target                      | Archive   |
| ------------------- | --------------------------- | --------- |
| Linux x86_64        | `x86_64-unknown-linux-musl` | `.tar.gz` |
| macOS Apple silicon | `aarch64-apple-darwin`      | `.tar.gz` |
| Windows x86_64      | `x86_64-pc-windows-msvc`    | `.zip`    |

Each archive is one package directory: `bin/godot-mcp`, the process supervisor
beside it, the GDScript bridge assets, an example configuration, the licences,
the SBOM and the schema tree. Keep it together — the server finds its supervisor
and assets next to its own executable. Verify an extracted package, and the
archive's build provenance:

```sh
godot-mcp self-check --package /path/to/godot-mcp-X.Y.Z-x86_64-unknown-linux-musl
gh attestation verify godot-mcp-X.Y.Z-x86_64-unknown-linux-musl.tar.gz --repo sunerpy/godot-mcp
```

### Build from source

For an unreleased commit, or a platform without an archive, build the package for
the machine you are on (Rust 1.96.0 is pinned in `rust-toolchain.toml`; Linux
needs `musl-tools`):

```sh
git clone https://github.com/sunerpy/godot-mcp.git
cd godot-mcp
make dist        # writes target/release-dist/godot-mcp-<version>-<target>.<ext>
```

## Quick start

Point your MCP client at the binary and a project directory (the one holding
`project.godot`, or any directory below it):

```json
{
  "mcpServers": {
    "godot-mcp": {
      "command": "godot-mcp",
      "args": ["serve", "--project", "/path/to/my-game", "--profile", "core"]
    }
  }
}
```

`core` is read-only. Choose the profile deliberately:

| Profile   | Adds                                                                            |
| --------- | ------------------------------------------------------------------------------- |
| `core`    | Discovery and analysis; nothing in the project changes                          |
| `editor`  | Authoring: files, scripts, scenes, resources, settings, assets, the live editor |
| `runtime` | Running, testing, exporting and observing the game, also from the live editor   |
| `full`    | All of the above                                                                |

Engine-backed tools need a Godot executable. The server looks in the usual install
locations and on `PATH`; to choose one, set `godot_binary` in a configuration file
passed with `--config` ([Configuration](docs/operator/configuration.md)). Every tool
per profile is listed in the generated [tool inventory](docs/protocol/tool-inventory.md);
client wiring for HTTP and reverse proxies is in [MCP clients](docs/operator/mcp-clients.md).

## Using with an LLM

Install through the [Install](#install) section, register the server with your
client as above, then let the agent discover the rest. To give the agent
godot-mcp's Agent Skills too, copy them into its skill directory
([Agent Skills](docs/operator/skills.md)):

```sh
godot-mcp skill install
```

<details>
<summary>Agent reference</summary>

- Call `capabilities.get` and `health.get` first: they name the profile, what the
  host can run (engine, supervisor, display) and why anything is unavailable.
- Send one request at a time and wait for its answer. The per-principal
  concurrency limit is small, and a burst answers
  `operational limit subject_concurrency was exceeded`.
- Every mutating call carries an `idempotencyId`; repeating a call with the same
  id returns the recorded outcome instead of acting twice.
- An answer is a result or a structured error with a `code` (`invalid_input`,
  `not_found`, `conflict`, `unsupported`, `unavailable`, `uncertain`, …).
  `uncertain` means the effect may or may not have happened; read before retrying.
- stdout carries MCP frames only; diagnostics go to stderr.

</details>

## Documentation

| Topic                                                 | Page                                                                 |
| ----------------------------------------------------- | -------------------------------------------------------------------- |
| Install, configure and make a first request           | [Quickstart: local stdio](docs/operator/quickstart-stdio.md)         |
| Every capability, with requests and measured answers  | [Usage and verification](docs/operator/usage-and-verification.md)    |
| Games and editors it runs, input, captures, debugging | [Managed sessions](docs/operator/managed-sessions.md)                |
| Driving the editor you have open                      | [Editor bridge](docs/operator/editor-bridge.md)                      |
| Recipes for common setups, applied by name            | [Recipes](docs/operator/recipes.md)                                  |
| Serving another machine of a development network      | [HTTP: `open`](docs/operator/http-open.md)                           |
| Profiles, outcomes, `unavailable` and `uncertain`     | [Profiles and tool outcomes](docs/operator/profiles-and-outcomes.md) |
| Crashes, survivors, quarantine and repair             | [Recovery](docs/operator/recovery.md)                                |
| Trust boundaries and residual risk                    | [Threat model](docs/security/threat-model.md)                        |

The full index is [docs/README.md](docs/README.md); the website,
<https://firlab.app/godot-mcp/>, has the guide in English and Chinese.

## Development

```sh
make help     # every target
make check    # fmt-check, clippy, workspace tests, locked build, cargo-deny
make dist     # this platform's release archive
```

Work test-first; [`CONTRIBUTING.md`](CONTRIBUTING.md) and the
[contributor workflow](docs/contributor/workflow.md) describe the gates. CI runs
every check of the release-gate catalogue, the packaged smokes on Linux, macOS and
Windows, and the workspace suite natively on macOS and Windows; `CI Success` is the
required check.

## License

MIT. See [LICENSE-MIT](LICENSE-MIT) and [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).
