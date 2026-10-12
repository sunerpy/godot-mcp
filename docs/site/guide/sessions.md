# Games and editors

Under the `runtime` and `full` profiles the server starts long-lived Godot processes and keeps
them: **game sessions**, started with `project.run.start`, and **editor sessions**, started with
`editor.session.start`. A session belongs to the one server that started it and is known only by
an opaque ID (`run_` or `editor_` followed by 32 hexadecimal digits). At most four sessions,
exports included, run at once, and each ends at its `lifetimeSeconds` — one hour by default, four
at most.

## Run the game

`project.run.start` runs the main scene, or `scene`, with a runtime probe of the server's own,
passed to Godot on the command line: nothing in the project is changed for it, no autoload is
installed and no setting moves. The call answers once the probe has connected back over a
loopback bridge private to that session, so a session it answers is ready to serve.

`display` chooses how the game draws: `window` into the display the server was started with,
`headless` not at all, and `auto`, the default, `window` when there is a display. Captures need a
window. On Linux a virtual X display such as Xvfb is enough; on macOS the server must run in a
logged-in user's session; on Windows on the visible desktop of a logged-on session.

`project.run.status` reports a session's state, scene, display, containment and held input;
`project.run.logs` reads its captured output page by page, while it runs and after it ends;
`project.run.stop` releases every held input, asks the game to quit and ends it if it does not.

## Send input

`input.key.send`, `input.mouse.send`, `input.action.send` and `input.sequence.send` press, release
or tap keys, mouse buttons and input actions, and move the mouse; a sequence runs up to 64 steps
with waits between them. Everything held is released when its hold ends (`holdMs`, 500 ms by
default), when it is released explicitly, when the sequence that pressed it ends or is cancelled,
when the session stops, and when the game loses its server. Every input call is journaled and
carries an `idempotencyId`, so a retried call answers from the journal instead of pressing again.

## Read the running game

`runtime.tree.get`, `runtime.properties.get`, `runtime.signals.get` and `runtime.ui.get` read the
game through its probe and change nothing in it: the scene tree with classes, scripts, groups and
visibility, a node's properties as tagged values, a node's signals and connections (with `watch`,
a count of every later emission), and the visible controls with their rectangles, text and focus.
Every answer names the probe's scene revision and the frame it was taken on.

## Capture

`runtime.screenshot.capture` captures the root viewport as a PNG; `runtime.recording.capture`
records up to 10 seconds at up to 30 frames per second as an animated PNG. Each frame is checked
against the digest the probe reported, and the answer names the capture as an artifact — an
opaque URI with its media type, size and SHA-256 — that the client reads with `resources/read`.
In a headless session a capture answers `unavailable`: the session renders nothing.

## Debug

`editor.session.start` starts a headless Godot editor on the project with its GDScript language
server and debug adapter on loopback ports the server allocates, and answers once both listen
and are held by that editor process. `debug.command` then sends one command through the
session's launch receipt:

| Server | Commands |
| --- | --- |
| Language server | `diagnostics`, `symbols` |
| Debug adapter | `breakpoints`, `launch`, `stopped`, `stack`, `scopes`, `variables`, `continue`, `pause`, `stop` |

Lines are one-based. There is no expression evaluation and no step-out, which Godot's debug
adapter does not implement. `debug.session.get` reports the connections, the debug launch, the
last stop and the breakpoints; `editor.session.stop` ends the editor and any game it runs under
the debugger.

## After a crash

A server that stops normally stops its sessions first. The next server for a project ends every
session a previous one left behind before it serves anything, and records each as `reconciled`,
or `quarantined` when its boundary cannot be proven empty; their status and output stay readable.
The [managed sessions reference](../../operator/managed-sessions.md) has every rule.
