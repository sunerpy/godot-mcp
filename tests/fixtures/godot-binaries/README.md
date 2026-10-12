# Fake Godot executables (probe fixtures)

These are **not** Godot. They are committed, deterministic stand-ins that
reproduce the exact command-line surface `godot-mcp-engine` probes, so the
discovery/probe suite never needs a real Godot installation or network access.

Each scenario exists twice with identical behaviour:

* `unix/<scenario>.sh` — POSIX shell, used on Unix hosts.
* `windows/<scenario>.cmd` — batch, used on Windows hosts.

Contract implemented by every fake:

| invocation | behaviour |
| --- | --- |
| `--version` | print one version line to stdout, exit 0 |
| `--help` | print a Godot-shaped help screen listing exactly the flags this build advertises, exit 0 |
| anything else | exit 0 and print nothing (i.e. *silently ignore* the flag) |

The "silently ignore" row is load-bearing: the export-template fake accepts
`--editor` with exit status 0 and empty output, which is precisely why the probe
must classify a build from advertised capabilities rather than from the exit
status of a probe invocation.

Scenarios:

| scenario | version line | advertises | purpose |
| --- | --- | --- | --- |
| `godot-4.7.1-editor` | `4.7.1.stable.official.…` | full editor flag set | supported matrix, happy path |
| `godot-4.6.1-editor` | `4.6.1.stable.official.…` | full editor flag set | supported matrix |
| `godot-4.5.1-editor` | `4.5.1.stable.official.…` | full editor flag set | supported matrix |
| `godot-4.4.1-editor` | `4.4.1.stable.official.…` | full editor flag set | supported matrix floor |
| `godot-4.7.1-template` | `4.7.1.stable.official.…` | template flag set only | silently ignores `--editor` |
| `godot-4.3.0-editor` | `4.3.0.stable.official.…` | full editor flag set | below the supported floor |
| `godot-malformed-version` | not a version at all | full editor flag set | unparsable `--version` |
| `godot-hanging-version` | never returns | n/a | `--version` exceeds the deadline |
| `godot-unknown-flag` | `4.7.1.stable.official.…` | nothing (help fails) | `--help` rejected by the build |

Platform *path* shapes (`/usr/local/bin/godot`,
`/Applications/Godot.app/Contents/MacOS/Godot`,
`C:\Program Files\Godot\Godot.exe`, …) are owned by
`godot_mcp_engine::discovery::platform_candidates` and asserted for all three
platforms by the suite; they do not need an executable fixture.
