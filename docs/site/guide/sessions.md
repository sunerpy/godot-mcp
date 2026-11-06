---
description: "Games and editors the server starts and keeps: run the game, send it input, read and capture it, test it and debug it."
---

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

`runtime.performance.get` reads the game's performance monitors — frame rate and frame times,
object and node counts, memory, rendering, physics, audio and navigation figures — by the names
your Godot build gives them, such as `TIME_FPS` and `OBJECT_NODE_COUNT`, with the project's own
custom monitors. Name `monitors` to read only those; a name the build does not have is refused.

## Drive and watch the game

`runtime.properties.set` writes properties of a node, or the Engine's `time_scale`,
`physics_ticks_per_second` and `max_fps`, as tagged values. A value is converted to the
property's type only where nothing is lost — 1.5 is never written to an `int` — and a write that
cannot be made changes nothing. Every property is read back afterwards, so a setter that clamps or
ignores the value shows as `matched: false`. `runtime.properties.batch.get` reads several nodes,
and the Engine, in one call.

`runtime.wait` waits, at most 30 seconds, until a node exists or is gone, a property compares with
a value (`==`, `!=`, `<`, `<=`, `>`, `>=`, `contains`, or `approx` within a tolerance), a signal
has fired a number of times, or a control shows a text. It answers what it saw last, whether the
condition held or the time ran out. `runtime.sample` reads properties and performance monitors at
an interval for at most 10 seconds and answers each series with its minimum, maximum, mean and
last value, and whether the game was paused at each sample.

`runtime.nodes.find` finds nodes by class (inherited ones included), script, group, name pattern
and distance, nearest first. `runtime.navigation.path.get` asks the navigation map a node uses for
a path; `runtime.node.move` puts a node at a point or walks it along that path. A move sets the
node's position directly: the game's own movement code and its collisions do not run for it.

`runtime.ui.press` clicks a button, named by its path or by its text, with a left click at its
centre and answers whether its `pressed` signal fired. A headless session's window is 64 by 64
pixels, so a button outside that area needs a windowed session.

`input.recording.start` and `input.recording.stop` record the input the game receives, marking
which events the server sent and which came from elsewhere, and keep the recording as an artifact.
`input.replay` sends recorded events again at their original times and releases whatever it still
holds when it ends.

The probe runs one request at a time, so a long wait, sample, walk or replay holds back the
session's other requests, and commands also wait for one another across the project. Cancel the
call to end it at once; `project.run.stop` ends whatever runs before it stops the game.

`runtime.script.run` runs GDScript an agent writes in the running game, once you start the server
with `--allow-scripts`. The script sees the current scene, the autoloads and every engine API, and
the answer gives what its `run()` returned, what it printed and the errors it raised with their
line (from Godot 4.5.1 on), and every project file that differs afterwards. Godot cannot stop a
script part way through: when the server stops waiting — after `deadline_ms` and two seconds
more, or when the call is cancelled — the answer is `uncertain` and the code goes on in the game.
Stopping the session ends it.

## Capture

`runtime.screenshot.capture` captures the root viewport as a PNG; `runtime.recording.capture`
records up to 10 seconds at up to 30 frames per second as an animated PNG. Each frame is checked
against the digest the probe reported, and the answer names the capture as an artifact — an
opaque URI with its media type, size and SHA-256 — that the client reads with `resources/read`.
The artifact stays readable for 15 minutes by default, from any session the same client opens: a
client that reconnects between the capture and the read still reads it (see
[HTTP sessions](../operate/http.md#sessions)). In a headless session a capture answers
`unavailable`: the session renders nothing.

## Test the game

`runtime.time_scale.set` speeds the running game up or slows it down: it sets `Engine.time_scale`
(0 to 100; 1 is real time, 0 stops game time) and reads it back. The change lives in the running
game only, nothing is written to the project, and it holds until you change it again or the
session ends.

`runtime.image.compare` captures the game after its next frame and compares it with a PNG in the
project, pixel by pixel. It answers whether the images match, how many pixels differ by more than
`tolerance` in a colour channel and what share of the image that is, the largest difference and
the rectangle around the differences, with the capture and a difference image as artifacts. Two
images of different sizes do not match; nothing is resized. It needs a windowed session.

`test.scenario.run` runs a list of up to 64 steps in one call and answers a report: input, waits,
property writes, button presses, time-scale changes, assertions on a property or on a control's
text, screenshots, image comparisons, and stress steps that add up to 1000 instances of a scene,
watch the frame rate and the node count, and check that every instance is freed afterwards. Each
step is `passed`, `failed` (with what was expected and what was found), `error`, `skipped` or
`uncertain`. After the first failure the rest are skipped unless `stopOnFailure` is `false`, a
step whose outcome cannot be decided ends the run, and `maxDurationMs` bounds the whole of it.
The verdict is `passed` only when every step passed, and the report is also kept as a JSON
artifact. A text assertion reads the control's `text` property; nothing reads the screen as text.

<ScreenFigure src="/screens/game-scenario.webp" width="960" height="540" alt="The demo game after a five-step scenario. The robot is back at the left edge, one coin still floats above a wooden crate, and the coin counter shows 3." caption="The screenshot the last step of a test.scenario.run scenario kept, from the demo project in examples/site-demo." />

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
