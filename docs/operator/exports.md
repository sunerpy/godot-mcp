# Exports

Under the `runtime` and `full` profiles the server exports the project through one of its
presets, as a **managed export**: a supervised `--export-release`, `--export-debug`,
`--export-pack` or `--export-patch` run of the resolved Godot build that the server
starts, owns and can cancel, inside the same containment boundary as every other
process-backed tool (see [Process boundaries](process-boundaries.md)). The output never
goes into the project. Every file a successful export produces becomes an
[artifact](artifacts.md), and a build the host can run is run once to check that it
starts.

This build exports Linux, Windows Desktop and macOS presets on Linux, macOS and Windows
hosts, and Android presets, as a debug or release APK, on Linux hosts. Any preset also
exports as a pack or a patch of the project's files. With an Android SDK configured, the
server lists Android devices and deploys an exported APK to one; see
[Android devices](#android-devices).

## Presets

`export.presets.list` reads the project's `export_presets.cfg` and changes nothing. For
each preset it reports its name, platform, whether it is the runnable one, its export path
when that path is project-relative, and which export template it needs: the template file,
the build's template label (for example `4.7.1.stable`), whether exactly that template is
installed and, when it is not, why. A preset this server cannot export carries the reason
instead. A preset that has a secret written into it is reported with the **names** of
those options, never their values.

## What has to be installed

An export is only as exact as its template, so the server uses the resolved Godot build's
own templates and nothing else. Godot keeps them in an `export_templates/<label>/`
directory whose `version.txt` names the build it belongs to, under the data directory its
Export Template Manager installs to, read from the server's own environment:

| Host | Template directory |
| --- | --- |
| Linux | `$XDG_DATA_HOME/godot/export_templates` when that is an absolute path, `~/.local/share/godot/export_templates` otherwise |
| macOS | `~/Library/Application Support/Godot/export_templates` |
| Windows | `%APPDATA%\Godot\export_templates` |

Install the templates of exactly the build the server runs; a neighbouring version's
templates are never used. A pack or patch export goes through no template, so it needs
none.

The engine itself runs with the server's private home, so before the export starts the
server makes the verified templates visible there. On Linux and macOS that is one link to
the installed directory. Windows needs a privilege to create a directory link, so there
the server places hard links to the template files the export reads — copies, where a
hard link cannot be made — in a directory it marks as its own and replaces on the next
export. The installed templates are never modified.

## macOS presets

A macOS preset exports an app bundle, and this server always has Godot pack it into one
`.zip`: whatever extension the preset's export path names, the output is `<name>.zip`.
Godot packs the bundle into a zip only for such a name; a bundle directory is not one
file, and a disk image needs macOS tools. The build's one `macos.zip` template serves
both modes, and a preset that names no architecture is `universal`, as in Godot.

Godot itself refuses a macOS export whose preset has no valid
`application/bundle_identifier`, and a `universal` or `arm64` one whose project does not
import ETC2/ASTC textures; `project.settings.patch` sets
`rendering/textures/vram_compression/import_etc2_astc`. Such an export ends `failed`, with
Godot's reason in `project.run.logs`. Signing follows the preset: `codesign/codesign=1`
signs the bundle ad hoc on every host.

## Android presets

An Android preset exports an APK through the build's `android_debug.apk` or
`android_release.apk` template, on a Linux host, with the Android SDK and the JDK named in
the [`[android]` configuration](configuration.md#android). The server never downloads or
installs an SDK, a JDK, a build-tools version or a template: an export without one is
refused before it starts, naming what is missing. A preset that builds with Gradle
(`gradle_build/use_gradle_build`), exports an Android App Bundle
(`gradle_build/export_format`) or names a custom template is `unsupported`.

From the SDK an export needs `build-tools/<version>/apksigner`, and from the JDK `bin/java`
and, for the server's debug keystore, `bin/keytool`. Measured with Godot 4.7.1 and Amazon
Corretto 21.0.10 on Linux x86_64: Godot signs with the build-tools whose major version is
its target SDK, 36, and with only 35.0.0 or only 37.0.0 installed it signed with that one.
Given a build-tools directory holding nothing but `36.0.0/apksigner`, it still exported and
signed the APK, and `zipalign -c 4` of build-tools 36.0.0 found it aligned: Godot aligns
the APK itself and runs no `zipalign`.

The Godot that exports runs with a private home of its own. The server writes Godot's
editor settings there — `export/android/android_sdk_path`, `export/android/java_sdk_path`
and, for a debug export, the debug keystore — and never into your own Godot settings. The
SDK those settings name is a private view of yours: your `build-tools`, and in place of
`platform-tools/adb` a stand-in that exits with status 1, so an export never starts an adb
server or reaches a device. The home and the view are removed when the export ends.

Which keystore signs the APK:

* A **debug** export signs with the debug keystore the preset names in Godot's
  credentials store, `.godot/export_credentials.cfg` (`keystore/debug`,
  `keystore/debug_user` and `keystore/debug_password`), when all three are set. When none
  is set, it signs with the server's own debug keystore: created once with the JDK's
  `keytool` in the server's private state (`.godot-mcp/android/debug.keystore`, alias
  `androiddebugkey` and password `android`, as Android's debug keystores are), and used for
  every later debug export, so one export installs over another — `adb install -r`
  refuses an update signed with another key. A store naming some but not all three is
  refused.
* A **release** export signs with the release keystore the preset names in the same
  store: `keystore/release`, `keystore/release_user` and `keystore/release_password`.
  Without all three it is refused before launch.

The server reads only whether those options are set and never shows their values. The
status names which keystore signed: the `keystore` of the APK's `signature` is
`server-debug`, `project-debug` or `project-release`.

Measured with Godot 4.7.1: when `apksigner` cannot run, Godot writes an unsigned APK,
warns, and exits 0. So once Godot exits, the server checks the APK itself before it
registers anything. It reads the archive, decodes its `AndroidManifest.xml` from Android's
binary XML, requires the game data `assets/project.binary`, and runs `apksigner verify
--print-certs` from the newest build-tools of the SDK. An APK that is not an archive, lacks
the manifest or the game data, or does not verify ends the export `failed`, with nothing
registered. The status's `apk` reports the package, the launcher (on Godot 4.7.1 the
activity alias `com.godot.game.GodotAppLauncher` of `com.godot.game.GodotApp`), the
version code and name, the minimum and target SDK, whether it is debuggable, its ABIs and
entry counts, and the signature: `verified`, the `apksigner` that verified it, and the
first signer's certificate SHA-256.

Godot 4.7.1 writes `<name>.apk` and, beside it, `<name>.apk.idsig`, the APK Signature
Scheme v4 file; both become artifacts. Measured on Linux x86_64 with Godot 4.7.1 and its
templates: a debug export of a project with one scene, one script and two JSON files took
8.2 to 9.3 s and wrote an APK of 28,246,631 bytes declaring minimum SDK 24 and target SDK
36, with libraries for `arm64-v8a` only. With Godot 4.4.1, 4.5.1 and 4.6.1 only the
refusal without templates was measured; an APK export with those builds is not measured.

## Packs and patches

`kind` `pack` exports the project's files as Godot packs them for the preset — its export,
include and exclude filters and its features — into one pack, without a template: a PCK,
or a ZIP with `packFormat` `zip`. `kind` `patch` exports a PCK of what changed since one or
more base packs, named in `patches`. Measured with Godot 4.4.1, 4.5.1, 4.6.1 and 4.7.1: a
patch holds the files that are new or changed since its bases, and a removal entry for
each file deleted since.

A base pack is named by the artifact URI of a served output of an earlier export of yours,
or by a path inside the project (`bases/1.0.pck` or `res://bases/1.0.pck`). A host path, a
path outside the project or inside the server's private state, a file of more than
256 MiB, and a file that is not a readable PCK are refused before anything runs; an
encrypted base, or a PCK format this server does not read, is `unsupported`. A ZIP is not
accepted as a base, and a patch is always written as a PCK. Measured: Godot 4.4.1 ignores a
ZIP base and packs every file, and with 4.4.1 and 4.7.1 a patch written as a ZIP carries no
removal entry for a file deleted since its base. The server records each base's source,
size and SHA-256 and gives Godot a private copy, so a base cannot change under the export.

Once Godot exits, the server reads the pack back with its own reader: the PCK's format
version, the engine version it names, its flags and its file table — each file's path,
offset, size and MD5, every file's bytes checked against its MD5 — or, for a ZIP, every
entry checked against its CRC-32. A pack whose table or data do not hold together ends the
export `failed`. A PCK whose directory is encrypted is reported `readable: false` with the
reason, rather than guessed at. The status's `pack` lists at most 500 files and says
`truncated` when there are more. Measured: Godot 4.4.1 writes PCK format 2, 4.5.1 and 4.6.1
format 3, and 4.7.1 format 4.

For a patch, the pack's `patch` counts the entries that are `new`, `changed`, `delta`,
`removed` or `removed_absent` against the bases. An entry with the same size and MD5 as a
base's entry is `identical`: a patch that carries unchanged files says so, with
`identical`, up to 100 `identical_paths` and `changed_files_only`.

When nothing changed since the bases, Godot prints `No files or changes to export` and
fails; measured with all four builds, 4.4.1 then leaves no file and 4.5.1, 4.6.1 and 4.7.1 a
pack header with no files. The server reports that export `failed` with
`nothing_changed: true`, and registers nothing.

`mode` `debug` and `smoke` do not apply to a pack or a patch and are refused.

## One export at a time

Every Godot build writes temporaries of an export into the host's temporary directory under
fixed names. Measured with `strace` on Linux: `/tmp/tmpproject.binary`, the project
settings a pack carries, with Godot 4.4.1, 4.5.1, 4.6.1 and 4.7.1; `/tmp/packtmp`, every
packed file, with 4.4.1; and on 4.7.1 an Android export's unaligned APK, named by the second
the export reached it. Two exports at once could read each other's files, so the server
runs one export at a time: while one runs, `project.export` answers `unavailable`,
retryable, naming it. Godot 4.7.1 kept writing these files to `/tmp` with `TMPDIR` set
(measured), so the server cannot move them: an export that another program runs on the
same host at the same moment can still collide with one of the server's.

## Starting an export

`project.export` takes the preset's `preset` name, `kind` (`project`, the default, `pack`
or `patch`), `mode` (`release`, the default, or `debug`), `packFormat` (`pck`, the
default, or `zip`) for a pack, `patches` (one to sixteen base packs) for a patch, `smoke`,
`lifetimeSeconds` and an `idempotencyId`; a retried call answers from the journal instead
of exporting again. Before anything is journaled or launched it refuses:

| Refusal | Outcome |
| --- | --- |
| no `export_presets.cfg`, or no preset of that name | `not_found` |
| a preset with a secret written into `export_presets.cfg` | `invalid_input`, naming the options |
| `mode` `debug` or `smoke` with a pack or a patch; `packFormat` without `kind` `pack`; a patch without `patches`, or `patches` without a patch | `invalid_input` |
| a base pack named by a host path, outside the project or inside the server's private state, larger than 256 MiB, or not a readable PCK | `invalid_input` |
| a base pack that is no served output of an export of yours, or no file of the project | `not_found` |
| an encrypted base pack, or one of a PCK format this server does not read | `unsupported` |
| a platform or architecture this server does not export, or a custom template | `unsupported` |
| an Android preset on a host other than Linux, built with Gradle, or exported as an App Bundle | `unsupported` |
| a Godot build that is not an editor build | `unsupported` |
| this build's templates, or the one template file the preset needs, not installed | `unavailable`, naming the label |
| an Android export without the configured SDK or JDK, or without the tools it runs | `unavailable`, naming what is missing |
| a release Android export without its release keystore in Godot's credentials store, or a debug one naming only part of its debug keystore | `unavailable` |

A secret is any option whose last path segment names a password, a passphrase, a secret,
a token or a key, such as `script_encryption_key`. Keep such values in Godot's own
credential store instead of the presets file, which anyone who can read the project can
read.

An accepted export answers at once with its export ID — `export_` followed by 32
hexadecimal digits — and runs on. It counts toward the four managed sessions that can run
at once (see [Managed sessions](managed-sessions.md)), no other export runs beside it (see
[One export at a time](#one-export-at-a-time)): a second one answers `unavailable`,
retryable, before it launches. It ends at its `lifetimeSeconds` (default 1800, at most
3600) at the latest. Godot writes into the export's private output directory; the
preset's export path only names the output file.

## Following an export

`project.export.status` reports one export, or every export of the calling principal:
an export belongs to the principal that started it, and another principal's export
answers `not_found`, to `project.export.status` and `project.export.cancel` alike. While it
runs, `progress_percent` follows Godot's own progress output, and `project.run.logs` with
the export ID reads that output. Once it ends, the status carries the exit facts and one of
three outcomes:

* `succeeded`: every output file is listed with its name, size, SHA-256, artifact URI and
  expiry. The bytes were read back, hashed and registered before the outcome was
  recorded; an output whose artifact has expired keeps its size and digest and loses its
  URI. A pack or patch also carries `pack`, what the server read from it, and each base
  in `bases`; an Android export carries `apk`.
* `failed`: the process did not exit cleanly, wrote no output file, produced more than the
  256 MiB the artifact store can hold, or wrote a pack or an APK that did not pass the
  server's check. A patch with nothing to carry also has `nothing_changed: true`. The
  reason is recorded and nothing is registered: when one output of an export cannot be
  registered, the ones registered before it are taken back.
* `cancelled`: see below.

With `smoke`, a build this host runs is run once, headless, as its own supervised launch,
and the status records whether it exited cleanly and, as `executable`, which file ran: an
x86_64 Linux build on Linux, an x86_64 Windows build on Windows, and on a Mac a macOS build
whose architecture the Mac runs, `universal` or its own. A macOS build is unpacked first,
into a private directory that is removed afterwards, and the file that runs is the one the
bundle's `Contents/Info.plist` names. The archive was written by a process that ran
project code, so it is checked before anything is unpacked: an entry that would land
outside that directory, a symbolic link, a local header that names another file than the
archive's directory does, or more than 2 GiB unpacked, and nothing is unpacked or run —
the smoke run records that reason instead. A build this host cannot run, such as a Windows
one on Linux, is not run, and the status says so.

## Cancelling

`project.export.cancel` terminates a running export inside its boundary and removes its
partial output, so nothing of it becomes an artifact; the export is `cancelled`.
Cancelling an export that already ended answers its record.

## After the server stops

The next server for the project terminates an export a killed server left running, like
any other managed session, and records it as `reconciled` or `quarantined`. Nothing of
such an export is registered. What it had written is kept in the server's private state
as recovery evidence, and its status reports only that evidence is retained and how many
bytes it holds; it is never served as an artifact and never removed by artifact
retention.

## Android devices

`android.devices.list` and `android.deploy` run the `platform-tools/adb` of the configured
SDK; without one they answer `unavailable`. adb runs with the server's private home,
`.godot-mcp/android/adb-home`.

`android.devices.list` runs `adb devices -l` and answers every device adb lists: its
serial, its state (`device`, `unauthorized`, `offline`, `no permissions`), whether it is
ready, its model, product and device names, its transport (`usb`, `tcp`, `emulator` or
`unknown`) and transport ID, and `started_server`, whether this call started adb's
server. When no adb server runs, adb starts one, and it keeps running after the call: the
server never stops it, because other programs may use it. An adb server started this way
has the server's private home, so it makes a new key there, which a device authorizes
once; until then the device is listed `unauthorized`. Start adb yourself beforehand
(`adb start-server`) and the server's calls go to your adb server instead. Measured with
platform-tools 37.0.1 on Linux x86_64 and no device attached: the first
`android.devices.list` started adb's server and the second did not. Authorizing a device
is not measured.

`android.deploy` installs and starts the APK of an Android export on one device. It takes
`export`, the ID of a finished Android export of yours whose APK is still served — an
artifact expires after its retention, 15 minutes by default (see
[Artifacts](artifacts.md)) — `serial`, one device as `android.devices.list` names it,
`runningTimeoutMs` (1000 to 60000, default 15000), `logcatLines` (1 to 1000, default 200)
and an `idempotencyId`. A deploy that cannot start — no SDK, an export that is not yours,
not an Android export, not finished, or whose APK has expired — is refused before anything
is journaled or sent. A deploy is a mutation with an effect outside the project, and it is
journaled like the others.

It runs five phases in order, each with its own deadline:

| Phase | Runs | Succeeds when |
| --- | --- | --- |
| `device` | `adb devices -l` (60 s) | the serial is listed in state `device` |
| `install` | `adb -s <serial> install -r <apk>` (600 s) | adb prints `Success` |
| `launch` | `adb -s <serial> shell am start -W -n <package>/<launcher>` (120 s) | the activity manager reports `Status: ok` |
| `running` | `adb -s <serial> shell pidof <package>` every 500 ms, until `runningTimeoutMs` | a process ID is printed |
| `logcat` | `adb -s <serial> logcat -d -t <logcatLines> --pid=<pid>` (60 s) | adb answers |

A deploy succeeds only when all five do. Its answer — and when it fails, the error's
details — carries every phase with its outcome (`succeeded`, `failed`, `unknown` or
`not_run`), what it found, adb's exit status and its bounded output, together with the
`package`, the `component` started, `installed`, the `pid` and the logcat excerpt. A
failure names its phase in `failed_phase` and in its message. What each failure proves:

| Failure | Outcome | `installed` |
| --- | --- | --- |
| the device is not listed | `not_found` | `false` |
| the device is `unauthorized`, `offline` or without permissions | `unavailable` | `false` |
| the device's package manager refused the APK (`Failure [INSTALL_FAILED_...]`) | `conflict` | `false` |
| adb gave no verdict on the install: its connection closed, its deadline passed or the call was cancelled | `uncertain` | unknown (`null`) |
| the activity manager did not start the launcher | `unavailable` | `true` |
| no process of the package within `runningTimeoutMs` | `timeout` | `true` |
| logcat did not answer | `unavailable` | `true` |

The server never retries an install whose outcome it cannot prove. A deploy that a killed
server left running is reconciled by the next server as failed and `uncertain`: the
project holds its pre-state, and whether the device installed or started the app is
unknown.

Deploying to a real device is not measured: the phases have been exercised only against
the test suite's stand-in for adb.

## Verification

`scripts/real_client/m6_smoke.py` drives the built binary through the official MCP Python
SDK against a real Godot editor build: preset listing, the refusals above, a Linux, a
Windows and a macOS export through the exact templates, ranged artifact reads checked
against the reported SHA-256 over stdio and over authenticated HTTP, a restart, and a
cancellation while the project still imports. On every host the build of the host's own
platform must be smoke-run and exit cleanly — the Linux build on Linux, the Windows build
on Windows, the macOS app bundle on a Mac — and the other two must report that they were
not run. With `--require-templates`, which the
[platform matrix](usage-and-verification.md) always passes, a run without the build's
templates fails instead of checking only that refusal. It passes against Godot 4.4.1,
4.5.1, 4.6.1 and 4.7.1, each with its own templates installed, on a Linux x86_64 host, on
an Apple silicon Mac (macOS 26.2) and on Windows Server 2022.

Packs, patches and Android exports are checked by two suites of the server crate.
`deploy_sessions` drives the shipped server against test-only stand-ins for Godot, adb,
`apksigner`, `keytool` and `java`: every refusal above, the pack and APK checks, the
server's debug keystore, each deploy phase failing in turn, and a deploy a killed server
left mid-install. `live_deploy` runs real Godot builds: a pack, a ZIP pack, a patch with
nothing changed, a patch with a changed and a new file, and a patch with a removed file
against a base named by its project path, with Godot 4.4.1, 4.5.1, 4.6.1 and 4.7.1 on Linux
x86_64; and an Android debug export, verified again with the SDK's `apksigner` and exported
a second time to check it is signed by the same key, with Godot 4.7.1 and its templates,
an SDK holding build-tools 35.0.0, 36.0.0 and 37.0.0, and Amazon Corretto 21.0.10. With 4.4.1, 4.5.1 and 4.6.1, whose
templates were not installed, it checks the refusal that names them.
