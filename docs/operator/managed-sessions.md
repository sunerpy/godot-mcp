# Managed sessions

Under the `runtime` and `full` profiles the server starts, owns and stops long-lived
Godot processes: **game sessions**, started with `project.run.start`, **editor
sessions**, started with `editor.session.start`, and managed exports, started with
`project.export` and described in [Exports](exports.md). Each one is a supervised launch inside
the same containment boundary as every other process-backed tool (see
[Process boundaries](process-boundaries.md)), and the start answers once the session is
ready to serve, not when the process exits. Sessions belong to the one server process
that started them, and are identified only by opaque IDs: `run_`, `editor_` or `export_`
followed by 32 hexadecimal digits. No answer names a process ID, a boundary or a private
path.

At most four sessions, exports included, run at once. A session ends at its
`lifetimeSeconds` (default 3600, at most 14400) at the latest; its boundary is then
terminated.

## Game sessions

`project.run.start` runs the main scene, or `scene`, with a session-scoped runtime probe.
The probe is the server's own script, passed to Godot on the command line; the project is
not modified for it — no autoload is installed and no project setting is changed, so
there is nothing to restore when the session ends. The probe connects back to a
loopback bridge private to that one session, authenticating with a secret only the
session's private rendezvous file carries, and the start answers once the probe has
connected and described itself.

`display` selects how the game draws:

* `window` draws into the display the server was started with. On Linux it needs
  `DISPLAY` or `WAYLAND_DISPLAY` in the server's environment, and the game inherits only
  the display variables; a virtual X display (for example Xvfb) with the Compatibility
  renderer is enough. On macOS the server must run in a logged-in user's GUI session
  (`launchctl managername` answers `Aqua`, not `Background`). On Windows its window
  station must be the visible one of a logged-on session, not the hidden one of a service
  or a container; Windows Server Core containers cannot start Godot at all, since they
  lack `AVRT.dll`. Captures need a windowed session.
* `headless` draws nothing.
* `auto` (the default) is `window` when the server has a display.

`resolution` sets the window size. `project.run.status` reports one session, or every
session without `session`: its state, scene, display, timestamps, containment strength,
exit facts, whether its probe is connected and which input it holds. The states are:

| State | Meaning |
| --- | --- |
| `starting` | launched; the probe has not connected yet |
| `running` | ready to serve |
| `stopping` | a stop is in progress |
| `exited` | the process ended; its exit facts are recorded |
| `failed` | the session did not become ready; whatever was launched was terminated |
| `reconciled` | a previous server left it running; this server terminated it and proved its boundary empty |
| `quarantined` | a previous server left it running and its boundary could not be proven empty |

`project.run.logs` reads one page (at most 64 KiB) of the captured stdout or stderr from
a byte offset, while the session runs and after it ended. Up to 32 MiB of each stream is
archived when the session ends.

`project.run.stop` asks the probe to release every held input and quit, and terminates
the boundary when the game has not ended by itself after 10 s. The answer carries the exit
facts and the input that was still held. Stopping a session that already ended answers its
record.

## Live inspection

`runtime.tree.get`, `runtime.properties.get`, `runtime.signals.get` and `runtime.ui.get`
read the running game through its probe and change nothing in it. Every answer carries
the session ID, the probe's scene revision — which moves whenever a node is added,
removed or renamed — and the frame it was taken on. Properties are encoded with the same
tagged-value codec the editor bridge uses; a value it cannot represent is reported as
such rather than approximated. `runtime.signals.get` with `watch` also counts every later
emission of the node's signals, and later reads report the counts.

The probe answers one request at a time. While an input sequence runs, reads to the same
session wait for it.

## Synthetic input, and when it is released

`input.action.send`, `input.key.send`, `input.mouse.send` and `input.sequence.send` are
journaled commands: each needs an `idempotencyId`, and a retried call answers from the
journal instead of pressing again. Input belongs to the one session it was sent to, and
everything held is released:

* when its hold ends — a `press` is held for `holdMs` (default 500 ms, at most 30 s);
* when it is released explicitly with `state: release`;
* when the sequence that pressed it ends, fails or is cancelled. A client's MCP
  cancellation of an `input.sequence.send` in flight is passed on to the probe, which
  stops between steps and inside waits;
* when the session is stopped;
* when the probe loses its server — the bridge closing, or the server process dying —
  and the probe quits on its own 15 s after that.

The real-client smoke `scripts/real_client/m5_smoke.py` checks every one of these paths
against the game's own output on Godot 4.4.1, 4.5.1, 4.6.1 and 4.7.1.

## Captures

`runtime.screenshot.capture` captures the game's root viewport as one PNG, and
`runtime.recording.capture` records up to 10 s at up to 30 frames per second as one
animated PNG. The probe encodes each frame; the server reads the bytes back in bounded
chunks, checks each frame against the digest the probe reported, assembles a recording's
frames without re-encoding them and registers the result as an artifact. Nothing about a
capture is committed until that registration is. The answer names the artifact by its
opaque URI with its media type, size, frame count and SHA-256; read the bytes with
`resources/read` (see [Artifacts](artifacts.md)). A client's MCP cancellation of a
recording in flight withdraws it from the probe, which keeps none of its frames, and
nothing is registered.

Two captures of an unchanged frame are byte-identical on one Godot build. Different
builds can rasterize the same frame differently — the smoke fixture's frame has one
SHA-256 on 4.4.1 and 4.5.1 and another on 4.6.1 and 4.7.1 — so compare captures taken
with the same build.

## Editor sessions and debugging

`editor.session.start` starts a headless Godot editor on the project, with the GDScript
language server and debug adapter on two loopback ports the server allocates. A port is
never one that an earlier editor launch recorded in the project's journal named, because
an endpoint is found by its port. The start answers once both listeners exist **and are held by the
editor process itself**, which the server checks through the socket table rather than by
connecting: something that merely listens on a port proves nothing about who it is. A
listener held by any other process fails the start. A game the debugger runs draws into
the server's display when it has one.

`debug.command` reaches those servers only through the session's launch receipt. Every
connection first verifies that the receipt still holds — the editor's process ID and
start time, the binary that process runs, the project root and its generation — and that
the listener is still held by that process; a session whose editor has exited is never
dialled again. The commands are:

| Protocol | Command | What it does |
| --- | --- | --- |
| Language server | `diagnostics` | the engine's own diagnostics for one script |
| Language server | `symbols` | one script's document symbols |
| Debug adapter | `breakpoints` | replaces one script's breakpoints with `lines` |
| Debug adapter | `launch` | runs the main scene under the debugger |
| Debug adapter | `stopped` | waits, at most `timeoutMs`, for a breakpoint or a pause |
| Debug adapter | `stack`, `scopes`, `variables` | read the stopped game |
| Debug adapter | `continue`, `pause` | resume or pause it |
| Debug adapter | `stop` | ends the debugged game |

Lines are one-based and scripts are named project-relative or by `res://`. There is no
expression evaluation and no step-out, which Godot's debug adapter does not implement.
Breakpoints in a `@tool` script are refused: Godot's debugger cannot break in code that
runs inside the editor. `debug.session.get` reports, per editor session, whether each
endpoint is still owned by the receipt, whether it is connected, the debug launch state,
the last stop, the breakpoints set and recent debug events.

`editor.session.stop` closes both connections and terminates the editor, together with any
game it runs under the debugger.

## After the server stops

A server that stops normally stops its sessions first. On Linux and macOS one that is
killed leaves them running, so the next server for the project terminates every session
the previous one left behind before it serves anything. On Windows a Job Object is
kill-on-close, so a killed server's sessions end with it, before a game's probe can let
go of the input it holds. Either way the next server archives their output and records
each as `reconciled` or `quarantined`, and their status and output stay readable through
`project.run.status` and `project.run.logs`.

## Health

`health.get` reports the runtime component `ready` while a game session runs with its
probe connected, and `game_not_running` with the remedy otherwise. The planes report
shows `runtime` and `lsp_dap` attached under these profiles, and usable while a game or
an editor session runs.

## Platform notes

Sessions are created in the boundary `[process]` selects, as every supervised launch is
(see [Process boundaries](process-boundaries.md#which-boundary-a-launch-gets)), and a
session reports it as `boundary_kind` and `boundary_strength`: on Linux the weak locator
by default, or a delegated cgroup or a container adapter when configured; on macOS the
weak locator or a container adapter; on Windows a Job Object. Attributing a listener to
the editor process asks the operating system, never a connect: on Linux and macOS the
editor's own process must hold the listening socket, and on Windows the listener's owner
must descend from the launch's barrier the way its processes do — the barrier runs
Godot's console executable, which runs the windowed engine that listens. On a platform
where this server cannot attribute a listener, `editor.session.start` answers
`unavailable`.
