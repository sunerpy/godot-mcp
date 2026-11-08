---
name: godot-mcp-export
description: "Export, build or package a Godot 4 game through a connected godot-mcp MCP server. Use when the project has to be exported or built for Linux, Windows, macOS or Android, in release or debug, as a PCK or ZIP pack or as a patch; when export presets or export templates have to be checked; when an exported build has to be run once to see that it starts; when the exported files have to be read; or when an APK has to be installed and started on an Android device. Covers export.presets.list, project.export with kind project, pack or patch, following an export with project.export.status until it ends, reading its outputs as artifacts within 15 minutes, project.export.cancel, android.devices.list and android.deploy. Installing the server, a Godot build and its export templates is the godot-mcp-setup skill; running and testing the game is the godot-mcp-playtest skill."
license: MIT
compatibility: "Needs a godot-mcp MCP server (godot-mcp serve) connected for one Godot 4.4.0 or later project with the runtime or full profile, and a Godot editor build the server can find. A project export needs the export templates of exactly that build; an Android export needs a Linux host and the Android SDK and JDK the server was started with."
metadata:
  contract: "0.7.0"
  profiles: "runtime,full"
---

# godot-mcp-export: exporting and packaging the game

The server exports the project through one of its presets as a managed export: a supervised run
of the resolved Godot build that it starts, owns and can cancel. The output never goes into the
project. Every file a finished export produces becomes an artifact, and a build this host can
run is started once, on request, to check that it runs.

This skill describes contract 0.7.0, the `contract` of its metadata. `health.get` reports the
contract the server speaks as `schema_version`; when the two differ, follow the tool schemas.

## The loop

1. Check the presets and their templates: `export.presets.list`.
2. Start the export: `project.export`. It answers at once with an export ID.
3. Follow it with `project.export.status` until it has ended.
4. Read the outputs with MCP `resources/read`, within 15 minutes.
5. For Android, install and start the APK: `android.devices.list`, then `android.deploy`.

## Checking the presets

`export.presets.list` reads the project's `export_presets.cfg` and changes nothing. Each preset
has its `name`, `platform`, whether it is the `runnable` one, its `export_path` when that path is
inside the project, and `secret_options`. Its `template` names the template `file` it needs, the
build's template `label`, such as `4.7.1.stable`, whether exactly that template is `installed`
and, when it is not, the `reason`; a preset this server cannot export has `unsupported` there
instead.

- A project without `export_presets.cfg` answers `presets_file: false`. Presets are made in the
  Godot editor's Export dialog; ask the user to add one.
- This server exports Linux, Windows Desktop and macOS presets on Linux, macOS and Windows hosts,
  and Android presets on Linux hosts. Any preset also exports as a pack or a patch.
- `secret_options` names the options of a preset that has a password, passphrase, secret, token
  or key written into `export_presets.cfg`, never their values. Such a preset is refused with
  `invalid_input`; its values belong in Godot's credentials store,
  `.godot/export_credentials.cfg`.

## What has to be installed

| Export | Needs |
| --- | --- |
| `kind` `project` | the export templates of exactly the Godot build the server runs |
| `kind` `pack` or `patch` | no template |
| an Android preset | also a Linux host, and the Android SDK and the JDK the server was started with |

Templates go where Godot's Export Template Manager installs them, in an
`export_templates/<label>/` directory:

| Host | Directory |
| --- | --- |
| Linux | `$XDG_DATA_HOME/godot/export_templates` when that is an absolute path, else `~/.local/share/godot/export_templates` |
| macOS | `~/Library/Application Support/Godot/export_templates` |
| Windows | `%APPDATA%\Godot\export_templates` |

A neighbouring version's templates are never used; without the right ones the export answers
`unavailable`, naming the label. The SDK and the JDK come from `--android-sdk` and
`--android-java-home` on the `serve` command, or `sdk` and `java_home` in the `[android]` section
of its configuration file; without them the Android tools answer `unavailable`. The server
installs none of these. Installing them, and restarting the server with them, is the
godot-mcp-setup skill.

## Starting an export

```json tool=project.export
{ "preset": "Linux", "kind": "project", "mode": "release", "smoke": true, "idempotencyId": "export-linux-1" }
```

- `kind` is `project`, the default, for the preset's build; `pack` for one pack of the project's
  files, a PCK or, with `packFormat` `zip`, a ZIP; or `patch` for a PCK of what changed since
  the base packs `patches` names.
- `mode` is `release`, the default, or `debug`. `smoke: true` runs the exported build once,
  headless, when this host can run it: an x86_64 Linux build on Linux, an x86_64 Windows build on
  Windows, a macOS build on a Mac that runs its architecture. `debug` and `smoke` do not apply to
  a pack or a patch and are refused.
- A base pack is the artifact URI of an output of an earlier export of yours, or a PCK inside the
  project, such as `res://bases/1.0.pck`. A ZIP is not accepted as a base.
- `lifetimeSeconds` ends the export at the latest after that long (default 1800, at most 3600).
- The answer comes at once, before Godot has finished. Its `export` is the export's record, whose
  `session` is the export ID: `export_` followed by 32 hexadecimal digits.
- One export runs at a time: while one runs, another answers `unavailable`, retryable, naming it.
  An export also counts toward the four managed sessions, games and editors included, that can
  run at once.
- A retry with the same `idempotencyId` answers from the journal and exports nothing again, so use
  a new id for a new export.
- A macOS preset always comes out as one zip of the app bundle. Godot refuses a macOS export
  without a valid `application/bundle_identifier`, and a `universal` or `arm64` one unless the
  project imports ETC2/ASTC textures, the project setting
  `rendering/textures/vram_compression/import_etc2_astc`; such an export ends `failed`.

This exports a patch of what changed since a release pack kept in the project:

```json tool=project.export
{ "preset": "Linux", "kind": "patch", "patches": ["res://bases/1.0.pck"], "idempotencyId": "patch-1-1" }
```

## Following an export

```json tool=project.export.status
{ "export": "export_0123456789abcdef0123456789abcdef" }
```

- Call `project.export.status` with the export ID until its `state` is no longer `starting` or
  `running`. While it runs, `progress_percent` follows Godot's own progress output, and
  `project.run.logs` with the export ID reads what Godot printed. Without `export`, the status
  lists every export of yours.
- Once it has ended, its `export` has the `outcome`:
  - `succeeded`: `outputs` lists every file with its `name`, `bytes`, `sha256`, `artifact` URI
    and expiry. A pack or a patch also has `pack`, what the server read back from it; an Android
    export has `apk`, with its package, launcher, SDK levels, ABIs and signature.
  - `failed`: Godot did not exit cleanly, wrote no output, wrote more than the 256 MiB the
    artifact store holds, or wrote a pack or an APK that did not pass the server's check. The
    reason is in `failure` and Godot's own in `project.run.logs`; nothing is kept. A patch with
    nothing to carry has `nothing_changed: true`.
  - `cancelled`: it was cancelled.
- With `smoke`, `smoke` says whether the build `ran` and `passed`, exiting cleanly, which
  `executable` ran, or the `reason` it was not run, as for a Windows build on Linux. A smoke run
  that did not pass leaves the export `succeeded`, so read `passed`.
- An export that a stopped server left running is ended by the next server, with its `state`
  `reconciled` or `quarantined`, and nothing of it is served.

## Reading the outputs

- Read an output with MCP `resources/read` on its `artifact` URI. It stays readable for 15
  minutes; after that `outputs` keeps its size and digest, `artifact` is null and `expired` is
  true, and only a new export makes it readable again.
- One read returns at most the server's `max_output_bytes` limit, 8 MiB by default. A larger
  file is read in inclusive byte ranges: a `resources/read` whose `_meta` holds
  `{"io.godot-mcp/artifactRange": {"start": 0, "endInclusive": 8388607}}` reads the first 8 MiB,
  and the next range follows on. The last range ends at the output's `bytes` minus 1; one
  past it is refused. Check the joined bytes against `sha256`.
- No tool names a host path for an output. To keep a build, read it and write it where the user
  wants it.

## Cancelling

`project.export.cancel` terminates a running export and removes its partial output, so nothing
of it becomes an artifact; the export ends `cancelled`. Cancelling an export that already ended
answers its record.

```json tool=project.export.cancel
{ "export": "export_0123456789abcdef0123456789abcdef", "idempotencyId": "cancel-export-1" }
```

## Android

- An Android preset exports an APK, debug or release, on a Linux host. A debug export signs with
  the debug keystore the project's credentials name, or else with the server's own debug
  keystore, so one debug export installs over another. A release export needs the release
  keystore named in Godot's credentials store and is refused without it. A preset that builds
  with Gradle or exports an App Bundle is `unsupported`.
- `android.devices.list` lists what adb reports: each device's `serial`, `state` and whether it
  is `ready`. A device listed `unauthorized` waits for the user to accept this computer's key on
  its screen.
- `android.deploy` installs the APK of a finished Android export of yours, while that APK is
  still served, on one device, starts it and reads its logcat. It answers each phase, `device`,
  `install`, `launch`, `running` and `logcat`, and succeeds only when all of them do; a failure
  names its phase in `failed_phase`.

```json tool=android.deploy
{ "export": "export_0123456789abcdef0123456789abcdef", "serial": "emulator-5554", "idempotencyId": "deploy-1" }
```

## What an answer means

| Answer | It means | Do |
| --- | --- | --- |
| `unavailable` | another export runs; the templates the preset needs, the Android SDK or JDK, or a release keystore are missing; a device is unauthorized or offline; or the app did not start | wait for the running export to end; otherwise tell the user what to install or allow, as the message names it |
| `invalid_input` | a preset with a secret written into it, `debug` or `smoke` with a pack or a patch, `packFormat` without a pack, or a patch without `patches` | correct the call; a secret moves to Godot's credentials store |
| `not_found` | no `export_presets.cfg` or no preset of that name, a base pack that is neither an output of yours nor a project file, an export that is not yours, or a device adb does not list | read `export.presets.list`, `project.export.status` or `android.devices.list` |
| `unsupported` | a platform, architecture or custom template this server does not export, Gradle or an App Bundle, an encrypted base pack, or a Godot build that is not an editor build | export another preset, or a pack |
| `conflict` | the device refused the APK, for example over another app signed with another key | read the deploy's `install` phase; the user removes the other app first |
| `timeout` | the app was installed and launched, but no process of it appeared within `runningTimeoutMs` | deploy again with a longer `runningTimeoutMs`, or ask the user what the device shows |
| `uncertain` | adb gave no verdict on an install | do not resend it; ask the user to check the device |

## Leave these to the user

- Installing export templates, the Android SDK or the JDK, and making signing keys.
- `android.deploy`, which installs an app on the user's device: deploy when the user asks.
- Moving a secret out of `export_presets.cfg`.
