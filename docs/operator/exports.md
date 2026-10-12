# Exports

Under the `runtime` and `full` profiles the server exports the project through one of its
presets, as a **managed export**: a supervised `--export-release` or `--export-debug` run
of the resolved Godot build that the server starts, owns and can cancel, inside the same
containment boundary as every other process-backed tool (see
[Process boundaries](process-boundaries.md)). The output never goes into the project.
Every file a successful export produces becomes an [artifact](artifacts.md), and a build
the host can run is run once to check that it starts.

This build exports Linux, Windows Desktop and macOS presets, on Linux, macOS and Windows
hosts.

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
templates are never used.

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

## Starting an export

`project.export` takes the preset's `preset` name, `mode` (`release`, the default, or
`debug`), `smoke` and an `idempotencyId`; a retried call answers from the journal instead
of exporting again. Before anything is journaled or launched it refuses:

| Refusal | Outcome |
| --- | --- |
| no `export_presets.cfg`, or no preset of that name | `not_found` |
| a preset with a secret written into `export_presets.cfg` | `invalid_input`, naming the options |
| a platform or architecture this server does not export, or a custom template | `unsupported` |
| a Godot build that is not an editor build | `unsupported` |
| this build's templates, or the one template file the preset needs, not installed | `unavailable`, naming the label |

A secret is any option whose last path segment names a password, a passphrase, a secret,
a token or a key, such as `script_encryption_key`. Keep such values in Godot's own
credential store instead of the presets file, which anyone who can read the project can
read.

An accepted export answers at once with its export ID — `export_` followed by 32
hexadecimal digits — and runs on. It counts toward the four managed sessions that can run
at once (see [Managed sessions](managed-sessions.md)), and ends at its `lifetimeSeconds`
(default 1800, at most 3600) at the latest. Godot writes into the export's private output
directory; the preset's export path only names the output file.

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
  URI.
* `failed`: the process did not exit cleanly, wrote no output file, or produced more than
  the 256 MiB the artifact store can hold. The reason is recorded and nothing is
  registered: when one output of an export cannot be registered, the ones registered
  before it are taken back.
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
