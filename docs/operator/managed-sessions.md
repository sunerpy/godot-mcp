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

At most four sessions, exports included, run at once, and at most one of them is an
export (see [Exports](exports.md#one-export-at-a-time)). A session ends at its
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

`runtime.properties.batch.get` reads up to 32 `targets` in one answer: each a `node` with at
most 64 named `properties` (its first 64 stored and editor-visible ones when none is named), or
the `Engine` singleton, whose `time_scale`, `physics_ticks_per_second` and `max_fps` it reads.
A target that cannot be read carries its own `error` — `not_found` for a node the game does not
have — and the others are read all the same; property names a node lacks are listed in its
`missing`. Once an answer holds about 2.5 MB of values, the targets after it are answered as
`skipped` and the answer is `truncated`.

`runtime.performance.get` reads the game's performance monitors: every monitor of its Godot
build's Performance monitor list, by the name the build gives it (`TIME_FPS`,
`OBJECT_NODE_COUNT`, `MEMORY_STATIC`, ...), and every custom monitor the project added, or only
the named `monitors`. The list differs between builds, so every value carries its name and an
unknown name is refused with `invalid_input`; a value that is not a finite number is `null`.
Measured on Godot 4.4.1, 4.5.1, 4.6.1 and 4.7.1, official Linux x86_64 builds, by the live
suite: frame rate, process time, object, node and static memory figures are numbers.

The probe answers one request at a time; see [Requests that span frames](#requests-that-span-frames)
for what that means for waits, samples, walks, presses and replays.

## Writing properties

`runtime.properties.set` writes up to 32 properties of one `target`: a `node`, or the `Engine`
singleton, whose writable properties here are `time_scale` (0 to 100),
`physics_ticks_per_second` (1 to 1000) and `max_fps` (0 to 1000); no other singleton is written.
Values are tagged values, as reads answer them. It is a journaled command and needs an
`idempotencyId`.

Every value is converted to its property's declared type first, and only where nothing is lost:
an integer into a `float` property, a whole float into an `int`, a `Vector2` with whole
components into a `Vector2i`, a `String`, `StringName` or `NodePath` into one another, a typed
array or typed dictionary item by item, and anything into an untyped property. Godot itself would
truncate a float written to an `int` and drop an untyped array written to a typed one without a
word; the probe refuses such a value instead. A value that does not convert without loss, a
property the node does not have (`not_found`), a native property without a setter, such as
`multiplayer` (`invalid_input`, read-only), `script`, and an Engine value outside its range each
refuse the whole write before any property changes.

Each property is then written and read back. The answer lists, per property, its `before`,
`requested` and `after` values and `matched`, which says whether the property holds exactly the
requested value afterwards, and `all_matched` for the write. A setter that clamps or ignores a
value is reported as `matched: false` with the value the property holds, never as a write that
took.

## Waiting and sampling

`runtime.wait` evaluates one `condition` every frame until it holds or `timeoutMs` passes
(default 5000, at most 30000):

| Condition | Holds when |
| --- | --- |
| `node_exists`, `node_gone` | the `node` is in the running game, or is not |
| `property` | the `node`'s `property` compares with `value` by `op`: `==`, `!=`, `<`, `<=`, `>`, `>=`, `contains` (a substring, an array element or a dictionary key), or `approx` within `tolerance` (numbers, vectors by distance, colors and quaternions per component) |
| `signal` | the `node`'s `signal` was emitted at least `count` times (1 to 1000) after the wait started |
| `ui_text` | a visible control shows `text`, exactly or, with `match: contains`, within its text |

A comparison that can never hold for a typed property — an `int` against a string, `<` between
strings — is refused with `invalid_input` instead of being waited out; a node or signal that does
not exist is `not_found`. The answer says `satisfied`, the `outcome` (`satisfied`, `timeout`,
`cancelled` or `interrupted`), `elapsed_ms`, `frames_waited` and `observed`: what the last check
saw, such as the node's path, the property's value, the emission count or the control and its
text. Measured by the live suite on Godot 4.4.1, 4.5.1, 4.6.1 and 4.7.1, official Linux x86_64
builds, in headless sessions: a 300 ms wait timed out after 302 to 304 ms of game time.

`runtime.sample` reads up to 32 `series` — a `property` of a `node` or of the `Engine`, or a
performance `monitor` — every `intervalMs` (default 100, at most 1000) for `durationMs` (default
1000, at most 10000), at most `maxSamples` (600) samples. Each sample has its time from the
start, its process and physics frame, whether the game was `paused` and its window `focused`,
and the Engine's `time_scale`; each series answers its values in order and, when they are numbers,
their `count`, `min`, `max`, `mean` and `last`. A sample keeps booleans, numbers, short strings
(at most 256 characters), vectors, rectangles, colors, quaternions, planes, boxes, bases and
transforms; any other value is answered as `null` with the reason in `unsampled`. `time_base`
names the clock and the frame range, and `focus_observable` is `false` under a headless display
server, which reports every window focused. The answer stops at about 2.5 MB of values and says
so in `stopped_by`. Measured as above: samples 100 ms apart were taken 94 to 109 ms apart, and a
headless session drew 88 frames in 600 ms.

With `cadence` `physics_frame`, `runtime.sample` takes one sample at the start of every physics
frame instead, before the game's nodes process it, within the same `durationMs` and `maxSamples`;
it takes no `intervalMs`, and a request that names one is refused with `invalid_input`. Each
sample sees what the previous physics frame left: a property the game sets in `_physics_process`
holds the previous frame's value, while a handler of the SceneTree's `physics_frame` signal, which
Godot runs before any node processes a frame, has already run for this one. Sampling goes on while
the scene tree is paused, by `runtime.frames.step` or by the game, each sample with `paused`
true. `time_base` names the `cadence`, with `interval_ms` `null`. Measured by the live suite on
Godot 4.4.1, 4.5.1, 4.6.1, 4.7.1 and 4.7.2, official Linux x86_64 builds, in headless sessions
whose idle frames each stalled 35 ms, so that up to three physics frames ran in one idle frame: 60
samples by physics frame saw 60 consecutive physics frames, while samples 1 ms apart, which are
taken in idle frames, skipped physics frames between every two of them.

## Finding and moving nodes

`runtime.nodes.find` answers the game's nodes under `root` that match every given filter:
`class` (an engine class or a script's `class_name`, inherited ones included), `script` (the
attached script, or one it extends), `group`, `name` (a pattern with `*` and `?`) and `near`
(within `radius` of a `point` or of another `node`, among nodes of the same dimension), nearest
first with `near` and in tree order otherwise, at most `limit` (default 100, at most 500). It
examines at most 20000 nodes and says when it stopped there (`scan_truncated`). A class name
that is neither an engine class nor in the project's global class list is still matched against
scripts' `class_name` and answered with `class_known: false`.

`runtime.navigation.path.get` asks the navigation map a node uses — that of its
`NavigationAgent2D` or `NavigationAgent3D` child, with the agent's layers, or its world's
default map — for a path from the node's position, or `from`, to `to`. It first waits, at most
`syncTimeoutMs` (default 1000, at most 5000), until the map has synchronized its regions: a map
with no region, or one whose regions have not synchronized in time, is `unavailable` with the
reason. The answer has the `path`, its `length`, the `end_distance` from its end to `to`, and
`reaches_target`, which is true when that distance is within the map's cell size. Godot 4.5.1,
4.6.1 and 4.7.1 synchronize a region's navigation mesh apart from its map, so the probe waits for
one more map iteration once every region has synchronized; on Godot 4.4.1 a map that has iterated
once answers paths over its regions.

`runtime.node.move` moves a `Node2D` or `Node3D`. `teleport` sets its global position and says,
one physics frame later, whether it still `held` there. `along_path` walks it along that path at
`speed` units per second, setting its global position every physics frame, until it `arrived`,
until 30 physics frames pass without progress (`blocked` — game code puts the node back), or
until `timeoutMs` passes (`timeout`; default 10000, at most 30000). Either way the node is moved
directly: the game's own movement code, `move_and_slide` and collision responses do not run for
the move, so the answer reports where the probe put the node, not where the game's physics
would have taken it. A walk is a journaled command. Measured as above: a 509.9-unit walk at 600
units per second took 51 physics frames, 841 to 855 ms.

## Pressing a button

`runtime.ui.press` clicks one button: named by `node`, or by the `text` it shows, exactly or with
`match: contains`; a text several visible buttons show is a `conflict` that names them. The probe
moves the mouse to the centre of the button's rectangle and presses and releases the left mouse
button through the same synthetic input `input.mouse.send` uses, holding it `holdMs` (default
100, at most 5000). The answer says whether the button's `pressed` signal fired, how often
`pressed`, `button_down`, `button_up` and `toggled` were emitted, and the button's state
afterwards. A disabled or invisible button, one that ignores the mouse or the left button, and a
node that is no button are refused with `invalid_input`; a button whose centre lies outside the
root viewport is `unavailable` — a headless session's root viewport is 64 by 64 pixels — and one
in a `SubViewport` is `unsupported`. A press is a journaled command, and the mouse button is
never left held.

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
* when the replay that pressed it ends or is withdrawn, and when a press of
  `runtime.ui.press` ends;
* when the session is stopped;
* when the probe loses its server — the bridge closing, or the server process dying —
  and the probe quits on its own 15 s after that.

The real-client smoke `scripts/real_client/m5_smoke.py` checks every one of these paths
against the game's own output on Godot 4.4.1, 4.5.1, 4.6.1 and 4.7.1. The live suite
`crates/godot-mcp-server/tests/live_runtime_control.rs` measures them with a fixture game that
times its own key events. On Godot 4.4.1, 4.5.1, 4.6.1 and 4.7.1, official Linux x86_64 builds,
in headless sessions: a key pressed with `holdMs` 100 was held 103 to 105 ms and one with 400
was held 400 to 409 ms; a sequence's 300 ms `wait` between a press and a release held the key
331 ms; a key still held when the session stopped was released before the game quit; and when
its server was killed, the probe released a held key within 20 ms.

## Recording and replaying input

`input.recording.start` records every input event the game's root window receives — keys, mouse
buttons and motion, joypad buttons and axes, input actions — with its time from the start, at
most `maxEvents` (default and limit 3000) for at most `maxDurationMs` (default and limit 30000);
a session keeps one recording at a time, and a second start is a `conflict`. Each recorded event
says its `source` — `probe` for an event this server injected, `external` for one the game, the
window or a device delivered — and which input `actions` it matched. It also has its physics
`frame`, counted from `first_physics_frame`, the physics frame after the recording started, which
both answers name: the frame in which the game first sees the event, the one in which
`Input.is_action_just_pressed` or `is_action_just_released` holds for it. An event that arrives
during a physics frame, or between two, belongs to the next one. `input.recording.stop` ends
the recording and answers every event it kept with their counts, and keeps the recording as an
artifact of media type `application/json`, read with `resources/read`. When the artifact cannot be
kept, the answer still carries the events, with `artifact: null` and the reason in
`artifact_error`. Both are journaled commands.

`input.replay` delivers up to 3000 recorded events again at their times from now (at most
30000 ms), in time order: the events `input.recording.stop` answers can be passed back as they
are, and `other` events are skipped and counted in `events_skipped`. The answer has the
`outcome`, how many events were sent, `max_lag_ms` (how late the latest event was delivered) and
`released`: the input the replay pressed and still held when it ended, which it released then. A
replay that is withdrawn, or whose session stops, releases the same way. Measured as above: a
recorded 250 ms tap that the game saw held 255 ms was held 261 to 263 ms when it was replayed, no
event was delivered more than 6 ms late, and a joypad button and an action event each triggered
the game's input action.

With `schedule` `physics_frame`, `input.replay` delivers each event so that the game sees it in
the physics frame its `frame` names (0 to 30000, and at most 30 s of physics frames at the game's
own rate), counted from `first_physics_frame`, the first physics frame after the replay starts.
Events of one frame keep their order in the list, and a recording replays by frame as it is. Each
event needs the field of its schedule, `t_ms` by time and `frame` by frame, and the other is
ignored; an event without it is refused with `invalid_input`. Under either schedule the answer adds
`schedule`, `first_physics_frame` and `deliveries`: each event's index in the caller's list and the
physics frame the game sees it in, in delivery order. By frame it answers `scheduled_frames` and
`max_lag_frames`, how many frames late the latest event landed, and `scheduled_ms` and
`max_lag_ms` are `null`; by time it is the other way round. While `runtime.frames.step` holds the
game, a replay by frame is refused with `conflict`.

Measured by the live suite on Godot 4.4.1, 4.5.1, 4.6.1, 4.7.1 and 4.7.2, official Linux x86_64
builds, in headless sessions whose idle frames each stalled 35 ms, eight key, action and joypad
events replayed by frame landed in the frames they named, with `max_lag_frames` 0, as follows:

| Event of frame | The game's `_input` and `_unhandled_input` receive it | Its `_physics_process` sees it |
| --- | --- | --- |
| 0 | when the replay starts, between physics frames | pressed and just pressed, or just released, in frame `first_physics_frame` |
| *k* > 0 | at the end of physics frame `first_physics_frame` + *k* − 1, within it | pressed and just pressed, or just released, in frame `first_physics_frame` + *k* |
| a press and a release of one action in one frame | both, in list order | just pressed and just released in that frame, and not pressed |

A recording's frames matched the frames in which the game saw its events, and replayed by time,
every delivery named the frame in which the game saw its event just pressed or just released.

## Requests that span frames

A wait, a sample, a path query waiting for its map, a walk, a press, a sequence, a replay, a
step, an image comparison and a stress window each span frames, and the probe answers one request
at a time: every other request to the same session waits for the one that runs. Commands —
replays, steps, walks, presses, sequences, property writes, input, comparisons and scenarios — are
also serialized with every other command of the project in its command lane, so a 30 s replay or
walk delays them all. Each such request ends at its next frame, with `outcome` `cancelled` or
`interrupted` and what it did until then:

* when the client cancels the call (MCP `notifications/cancelled`), which the server passes on to
  the probe;
* when the session is stopped: `project.run.stop` withdraws what the probe runs for the session
  before it waits for the command lane, so the stop is not queued behind it. Measured as above,
  a stop sent during a 20 s replay answered, with the game ended, after 382 to 407 ms on 4.5.1,
  4.6.1 and 4.7.1 and after 882 to 884 ms on 4.4.1;
* when the probe loses its server.

A probe says which methods it implements when it connects. A tool whose method the session's
probe does not implement — a probe from an older server — is refused with `unsupported` before
anything is journaled or sent. Where Godot builds differ (navigation map synchronization, and
whether the display server reports window focus), the probe decides once, when it starts, and
answers the same shape on every build.

## Running a script in the game

`runtime.script.run` compiles caller-written GDScript in the game and calls its `run()` on the
game's main thread through the probe, awaiting it when it is a coroutine
([ADR 0012](../contributor/adr/0012-declared-script-execution.md)). It runs nothing until the
operator starts the server with `--allow-scripts`; until then it answers `permission_denied`,
and the session's capabilities do not offer the probe the method, which the probe also checks.

- **It runs in the running game.** The current scene, the autoloads and every engine API are in
  reach; a source extending Node is added under the probe, in the scene tree, and released
  after. It shares the editor add-on's runner, so it compiles as the editor would: `@tool` is
  written in front of the first line when missing, and static initializers run on compile.
- **It answers what happened**: what `run()` returned as a tagged value and as Godot's own
  spelling; on Godot 4.5.1 and later the errors logged while it compiled and ran, with the line of
  the source, and what it printed (on 4.4.1 a run that returned is `inconclusive`); and every
  project file that differs from before the run, the game's own writes meanwhile included.
- **Nothing confines it, and nothing is undone.** It runs with your privileges. Like the game's
  own code, its file writes are outside the server's journal, so a run whose outcome is unknown
  blocks nothing.
- **A deadline ends the wait, not the code.** The server waits `deadline_ms` (60 s by default,
  600 s at most) and a further two seconds. When it stops waiting — the deadline passed, the
  client cancelled the call, or `project.run.stop` withdrew what the session runs — the probe
  answers `uncertain` at its next frame and serves the requests behind it while the code goes
  on. Stopping the session ends the code.

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

## Game speed

`runtime.time_scale.set` writes the running game's `Engine.time_scale` to `timeScale` (0 to 100;
1 is real time, 0 stops game time) with the probe's own property write and reads it back. The
answer has the scale `before`, the one `requested`, the one read back `after`, `matched` and
`persisted: false`. It changes the running game only: nothing is written to the project or saved,
the scale holds until it is changed again or the session ends, and the next session starts at 1
unless the game's code sets another. A scale out of range is refused with `invalid_input` before
anything is journaled; a write that reads back otherwise, or whose answer does not say how it
reads back, is answered as `uncertain`, with the three scales in the error's details. It is a
journaled command and needs an `idempotencyId`.

Game code that counts time with the delta Godot passes it runs faster or slower; the engine's tick
clock (`Time.get_ticks_msec()`) does not. Measured on Godot 4.4.1, 4.5.1, 4.6.1 and 4.7.1,
official Linux x86_64 builds, in headless sessions, by the live suite: at a scale of 0.25 the
fixture's game time advanced 0.250 to 0.251 seconds per second of the tick clock, at 3 it advanced
2.986 to 3.015, and at 0 it did not advance over 300 ms.

## Stepping by physics frames

`runtime.frames.step` advances the game by exactly `frames` physics frames (0 to 600), then holds
it paused, with `then` `hold` (the default), or lets it run on, with `resume`
([ADR 0017](../contributor/adr/0017-frame-stepping.md)). The probe holds a game by setting
`SceneTree.paused` at the start of a physics frame, before the game's nodes process it, and counts
each frame at its start:

* from a running game, a step counts from the game's next physics frame, and holds it from the
  frame after the last;
* from a hold, it releases the hold at the start of its first frame, so the game's nodes process
  that frame and no idle frame of the game runs before it;
* `frames` 0 holds a running game at the start of its next physics frame, or keeps a hold; with
  `resume` it releases a hold and advances nothing.

The answer has the `outcome`, `frames_requested`, `frames_advanced`, the `first_frame` and
`last_frame` it advanced — the values of `Engine.get_physics_frames()` in those frames, `null`
when it advanced none — `then`, whether the probe now `held` the game, whether the scene tree is
`paused`, `elapsed_ms` and `interference`. It is a journaled command and needs an
`idempotencyId`; `frames`, `then` or `timeoutMs` out of range is refused with `invalid_input`
before anything is journaled.

| Outcome | The step ended | Afterwards the game is |
| --- | --- | --- |
| `completed` | after its `frames` | held, or running with `resume` |
| `timed_out` | when `timeoutMs` (1 to 30000, default 20000) passed first | held where it stopped |
| `cancelled` | when the call was withdrawn: the client cancelled it, or `project.run.stop` withdrew it | held where it stopped, until a stop releases it |
| `interrupted` | when the probe lost its server, so the server answers the call `uncertain` | running |
| `interfered` | when something else changed the scene tree's pause; `interference` names the physics frame and the new pause | as the game left it |

The probe never changes the game's own pause. A step of any kind is refused with `conflict`
while the game paused itself — `SceneTree.paused` is true and the probe did not set it — and
changes nothing. When anything else pauses or unpauses the tree while the probe holds or steps
the game, the pause is the game's from then on: the probe holds nothing, and a step in flight ends
at the next frame as `interfered`. A game that sets `SceneTree.paused` to true while the probe
already holds it changes nothing Godot reports, so the probe cannot tell, and its release then
unpauses the game.

A hold lasts until the next step, a step of 0 frames with `resume`, `project.run.stop`, which
releases it before the game quits, or the probe losing its server, which releases it at once.
While the game is held, reads, samples, property writes and captures work as before, and a replay
by physics frame is refused with `conflict`: no frame it names would reach the game's nodes.

A hold does not stop what the game keeps running while it is paused. Measured by the live suite
on Godot 4.4.1, 4.5.1, 4.6.1, 4.7.1 and 4.7.2, official Linux x86_64 builds, in headless sessions,
over 20 held frames: a node whose `process_mode` is always processed every frame, handlers of the
SceneTree's `physics_frame` signal ran every frame, and a 50 ms SceneTreeTimer, which processes
always unless it is created otherwise, fired 5 or 6 times, while the scene's pausable nodes, its
`Timer` node and its physics body stood still. An autoload at the probe's own physics priority,
the lowest, comes before the probe in the scene tree and sees a step one frame late: from a hold
it processed the frames after the first through the one after the last. Rendering, the engine's
frame counters and its clocks go on. Audio, and animations and tweens set to process always, were
not measured.

Measured as above, at Godot's default 60 physics frames per second, over three runs of the suite
on each build: steps of 1, 3, 10 and 60 frames from a hold each advanced exactly that many frames
of the scene and steps of its physics server, in 20 to 34, 54 to 69, 172 to 187 and 1,000 to
1,022 ms; a step of 600 frames with `timeoutMs` 200 advanced 12 frames and held the game; and a
game that paused itself in the fifth frame of a 60-frame step ended it as `interfered` after 5
frames, in which its physics server took 4 steps, since the pause in the fifth frame stopped the
server before that frame's step. When the server was killed, the held game ran again within 5 to
27 ms, as the suite observed it. In a window under Xvfb, a held game was captured without
advancing.

## Comparing with a reference image

`runtime.image.compare` captures the root viewport after the next drawn frame, as
`runtime.screenshot.capture` does, and compares it with the project PNG that `reference` names by
its `path` (`res://` or project-relative). The server reads the reference through the held project,
at most 16 MiB and never from `.godot`, `.godot-mcp` or `godot_mcp_helpers`, and sends it to the
probe in chunks of at most 1 MiB that the probe checks against the image's SHA-256. The probe
decodes both images with Godot's own Image API and compares them as RGBA8: a pixel differs when one
of its four channels differs by more than `tolerance` (0 to 255, default 0). The answer has:

| Field | Holds |
| --- | --- |
| `matched` | both images are of one size and no pixel differs |
| `capture_size`, `reference` | both sizes; the reference's path, byte length and SHA-256 |
| `same_size`, `total_pixels` | whether the sizes are equal, and the capture's pixel count |
| `differing_pixels`, `ratio` | how many pixels differ, and their share of `total_pixels` |
| `max_channel_difference` | the largest difference of one channel over every pixel |
| `bounding_box` | `x`, `y`, `width` and `height` of the smallest rectangle around the differing pixels, or `null` |
| `reason` | why the images do not match, or `null` |
| `capture`, `difference` | the capture, and, when pixels differ and `differenceImage` is not `false`, a difference image: a darkened copy of the capture with every differing pixel opaque red; both are PNG artifacts |

Images of two sizes are not compared and nothing is resized: the answer is `matched: false` and
`same_size: false`, its `reason` names both sizes, and the counts are `null`. A session that renders
nothing, a headless one, is refused with `unavailable` before anything is journaled; a reference
that does not exist is `not_found`, and one that is not a PNG is `invalid_input`. A comparison is a
journaled command and needs an `idempotencyId`; it changes nothing in the game. It lets a frame
pass after every 40 ms of work, so a cancellation withdraws it.

Measured by the live suite on Godot 4.4.1, 4.5.1, 4.6.1 and 4.7.1, official Linux x86_64 builds,
in 320 by 240 windows on Xvfb with the Compatibility renderer: a capture of an unchanged frame
matched a screenshot taken before it, with no differing pixel; after the fixture's 100 by 50
swatch was recoloured, 5000 pixels differed (ratio 0.065104), each by 31 in one channel, inside a
bounding box equal to the swatch's rectangle; at a tolerance of 31 none differed, and at 30 all
5000 did; a 2 by 2 reference failed as a size mismatch; and a 640 by 480 reference of 1229438
bytes, sent in two chunks, reached the probe whole, which answered its size, byte length and
SHA-256. As with captures, compare with a reference taken on the same Godot build: builds can
rasterize one frame differently.

## Test scenarios

`test.scenario.run` runs an ordered list of 1 to 64 `steps` against one game session and answers
one report. Each step sends the probe the request of the tool it mirrors, with the same arguments
and limits, so it behaves as that tool does:

| `kind` | Arguments | Mirrors | Passes when |
| --- | --- | --- | --- |
| `input` | `steps`: 1 to 64 input steps, waiting at most 10000 ms in total | `input.sequence.send` | every input step was sent |
| `wait` | `condition`, `timeoutMs` | `runtime.wait` | the condition held |
| `set_property` | `target` | `runtime.properties.set` | every property reads back as written |
| `press` | `target`, `holdMs` | `runtime.ui.press` | the button was clicked and its `pressed` signal fired |
| `time_scale` | `timeScale` | `runtime.time_scale.set` | the scale reads back as asked |
| `assert_property` | `target`, `op`, `value`, `tolerance` | the comparison `runtime.wait` makes | the live value compares with `value` |
| `assert_ui_text` | `node`, `op`, `text` | the text `runtime.ui.get` reports | the Control's text is `text`, or contains it |
| `screenshot` | none | `runtime.screenshot.capture` | the capture was kept |
| `compare_image` | `reference`, `tolerance`, `differenceImage` | `runtime.image.compare` | the images match |
| `stress` | `scene`, `count`, `parent`, `durationMs`, `intervalMs` | none | every instance was added, and none is left after they were freed |

Every step may carry an `id`: 1 to 64 letters, digits, `_`, `.` or `-`, unique in the scenario,
and `step-<n>` by default.

* `assert_property` reads the `property` of a `node`, or of the `Engine` singleton
  (`time_scale`, `physics_ticks_per_second` or `max_fps`), and compares it with the tagged
  `value` in the game, by the engine's own arithmetic: `op` is `equals`, `not_equals`, `less`,
  `greater` or `approx` within `tolerance` (0 to 1e9; numbers by their difference, vectors by
  distance, colors and quaternions per component). A value that is not a number, or that holds
  one, never compares as near anything; the record names it in `special` (`nan`, `inf`, `-inf`,
  or `non_finite` for a vector or color that holds one), and `not_equals` with a NaN holds, as it
  does in GDScript.
* `assert_ui_text` reads the `text` property of the Control `node`, as `runtime.ui.get` reports
  it, and matches it exactly (`op: equals`, the default) or as a part (`op: contains`); nothing
  reads the screen as text.
* `compare_image` compares with a project PNG (`reference: {path}`) or with the screenshot an
  earlier `screenshot` step took (`reference: {step}`, that step's `id`).
* `stress` adds `count` (1 to 1000) instances of the project scene `scene` under `parent` (the
  current scene by default), samples the frame rate, the process times and the object, node and
  orphan node counts as `runtime.sample` does, every `intervalMs` (default 100, at most 1000) for
  `durationMs` (default 1000, at most 10000), then frees every instance and counts those still
  there two frames later. Its record has the counts before, during and after the window.

Each step ends `passed`, `failed`, `error`, `skipped` or `uncertain`:

* `failed`: it ran and what it checks does not hold; its record has `expected` and `actual`.
* `error`: it could not run as asked. The probe refused it (the record's `error` has the probe's
  kind, such as `not_found`), the comparison does not apply to the value (`invalid_input`, with the
  reason, such as `<` between vectors), an image step ran in a session that renders nothing
  (`unavailable`), or a stress window could not add every instance.
* `uncertain`: whether it did what it asked cannot be decided: a wait that ran out, a request whose
  answer was lost, a time scale that reads back otherwise, a step the deadline or a cancellation
  withdrew.
* `skipped`: it did not run, and its `reason` says why.

After a `failed` or `error` step the rest are skipped while `stopOnFailure` is true, the default;
after an `uncertain` step they are skipped whatever it is. `maxDurationMs` (default 60000, at most
600000) bounds the run: the step that runs when it passes is withdrawn from the probe, as a client's
cancellation withdraws it, and the rest are skipped. `stopped_by` says what ended the run early,
`failure`, `uncertain`, `deadline` or `cancelled`, or is `null`. The `verdict` is `passed` only
when every step passed; otherwise it is `failed` when a step failed, else `error` when a step could
not run, else `uncertain`.

The report has the `verdict`, `stopped_by`, the `counts` of each status, `elapsed_ms`, the Godot
version and one record per step: its `index`, `id`, `kind`, `status`, `started_ms`, `elapsed_ms`,
`expected`, `actual`, `detail` (what the probe answered), `error`, `reason` and `artifacts`, the
screenshots, captures and difference images it kept. A field of a record larger than 64 KiB is
replaced by its byte length and SHA-256. The report is also kept as an artifact of media type
`application/json`, named in `artifact` (`null`, with the reason in `artifact_error`, when it could
not be kept) and read with `resources/read`; there is no separate report tool.

A scenario is one journaled command and needs an `idempotencyId`: it is committed whatever its
verdict, and a retried call with the same `idempotencyId` runs nothing again: it answers the
recorded status (`replayed: true`) without the report, which stays readable as the artifact the
first answer named. Every other command of the project waits for it in the command lane. What the
schema cannot check is refused before anything is journaled or sent: two steps with one `id`, a
`{step}` reference that does not name an earlier screenshot step, more than 16 screenshot and
comparison steps, an `input` step that waits more than 10000 ms, a reference image that does not
exist or is not a PNG, references of more than 64 MiB together, and a step whose probe method the
session's probe does not implement (`unsupported`).

Measured by the live suite on Godot 4.4.1, 4.5.1, 4.6.1 and 4.7.1, official Linux x86_64 builds,
in headless sessions: a 13-step scenario of text assertions, a press, a wait, property writes and
assertions, time-scale changes, input and a 300 ms stress window of 50 instances passed in 576 to
593 ms, and the window raised the game's node count from 9 to 59 and back to 9; a failed text
assertion skipped the two steps after it, which left the game unchanged; a scenario with a
`maxDurationMs` of 500 withdrew its 10 s wait and answered after 604 to 626 ms. Under Xvfb with the
Compatibility renderer, a scenario compared the game with its own screenshot and with a project
reference, and a recoloured swatch failed its comparison and skipped the step after it.

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
