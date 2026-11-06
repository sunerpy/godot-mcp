---
description: Tools that run Godot to check scripts, import assets, run tests, ask the engine and export the project.
---

# Running the engine

The tools on this page run Godot: they need an editor build of Godot 4.4.0 or later that the server
can resolve, and the `runtime` or `full` profile (`project.import.run` belongs to `editor` and
`full`). Each run is a supervised process inside a containment boundary the server owns, with a
deadline, bounded output and its exit facts recorded.

## Which Godot it runs

The server uses the `godot_binary` of its configuration file, or otherwise the first Godot build it
finds in the usual install locations and on `PATH`. Before an engine tool runs, the server probes
the build for its version and for the command-line features the tool relies on; `health.get`
reports the build, its version and what was probed. It never downloads an engine.

## Check, import and test

| Tool | Does |
| --- | --- |
| `script.check` | Compiles one script with the engine's own `--check-only`, after a calibration run proves the flag is honoured, and returns the engine's diagnostics |
| `project.import.run` | Runs the importer headlessly over the project and reports which expected artifacts it produced, with each asset's import state |
| `project.test` | Runs the project's own GUT or gdUnit4 suite, or a selection of it, and returns the parsed counts and verdict |
| `project.run` | Runs the main scene, or another, until it exits or a deadline passes, and returns the verdict, exit facts and output |

`project.test` runs a test framework the project already has committed; a project without one
answers `unsupported`, and nothing is installed. A verdict is classified from what the engine
printed and how it exited, so a crash, a script error or a "started" line that was never
followed by a result is never reported as a pass.

`project.run` is a one-shot run. To keep a game running and interact with it, start a managed
session instead ([Games and editors](sessions.md)).

## Ask the engine

Some facts a scene or resource holds are not one stored property a file parser can show: which
cells of a tile map are painted, the keys of an animation track, the states of a state machine,
the polygons of a baked navigation mesh. These tools ask the engine itself, through a fixed table
of read-only methods, and change nothing.

| Tool | Does |
| --- | --- |
| `scene.read` | Calls reading methods on nodes of a text scene, or on resources they hold, and returns each value as a tagged value with the engine errors the call raised |
| `resource.read` | The same for a text resource |
| `shader.check` | Compiles a shader with the engine's shader compiler and returns whether it compiles, its errors with file and line, its mode and its uniforms |

A read loads the saved file in a headless Godot, so the project's autoloads start; the scene
never enters the tree, so no `_ready` runs. To read what an open editor holds, unsaved changes
included, use `editor.scene.read` ([The editor add-on](editor.md)). The
[editor bridge reference](../../operator/editor-bridge.md#reading-calls) lists the methods.

## Export

`export.presets.list` reads `export_presets.cfg`: each preset's platform, whether it is the
runnable one, the export template it needs and whether exactly that template of the resolved
Godot build is installed. A preset with a secret written into it is listed with the option names,
never their values.

`project.export` exports one preset as a managed export: it answers at once with an export ID and
runs on, and `project.export.status` follows it — its progress, and when it ends, every output
file with its size, SHA-256 and artifact URI. `project.export.cancel` stops it and removes its
partial output. The output goes to the server's private storage, never into the project, and with
`smoke` a build this host can run is started headless once to check that it starts.

An export uses only the templates of exactly the Godot build the server runs, from the directory
Godot's Export Template Manager installs into; a neighbouring version's templates are never used.
Linux, Windows Desktop and macOS presets export on Linux, macOS and Windows hosts, and a macOS
preset is packed into one `.zip` app bundle. The [exports reference](../../operator/exports.md)
lists every refusal and where the templates are looked for.

### Packs and patches

A preset also exports as a pack of the project's files (`kind` `pack`: a PCK, or a ZIP with
`packFormat` `zip`), or as a patch of what changed since earlier packs (`kind` `patch`, with the
base packs in `patches`, each named by the artifact URI of an earlier export or by a path in the
project). Packs and patches need no export template. When the export ends, the server reads the
pack back and reports its format and the files it holds, each checked against its checksum; for a
patch it counts the files that are new, changed or removed since the bases, and flags the ones it
carries unchanged. A patch with nothing to carry ends `failed` with `nothing_changed: true`.

### Android

An Android preset exports a debug or release APK on a Linux host, with the Android SDK and JDK
named in the `[android]` section of the [configuration](../../operator/configuration.md); the
server installs neither. A debug export signs with the server's own debug key unless the project's
credentials store names one, and a release export signs only with the release keystore the
credentials store names. Before the APK becomes an artifact, the server checks it with the SDK's
`apksigner` and reports its package, its launcher and its signer.

The server runs one export at a time: Godot writes some temporaries of every export under the same
names, so a second export answers `unavailable` until the first ends.

`android.devices.list` lists the devices the SDK's `adb` sees, with their state.
`android.deploy` installs the APK of a finished Android export on one device, starts it, waits for
its process and reads its log. Each of the five phases — `device`, `install`, `launch`, `running`
and `logcat` — reports its own outcome, and the deploy succeeds only when all five do. An install
whose outcome adb does not report is `uncertain` and is never retried. When no adb server runs,
the first call starts one, which keeps running afterwards, and a device then authorizes the
server's adb key once.

## Containment

Every Godot process — a check, an import, a test, a run, an export, a game or an editor session —
starts inside a containment boundary that the server records before the process can run any
project code, and ends with that boundary proven empty. On Linux the default is a process group,
or a delegated cgroup or a container adapter when configured; on macOS a process group or a
container adapter; on Windows a Job Object. The boundary contains and stops what the server
starts; it does not make an untrusted project safe to open. The
[process boundaries reference](../../operator/process-boundaries.md) has the details.
