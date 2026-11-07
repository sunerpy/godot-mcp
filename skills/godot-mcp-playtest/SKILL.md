---
name: godot-mcp-playtest
description: "Run, play and test a Godot 4 game through a connected godot-mcp MCP server. Use when the game has to be started, played, watched or checked, when input or button presses are sent to it, when its nodes, properties or UI text are read while it runs, when screenshots, recordings or image comparisons are taken, when a repeatable test scenario is written, or when game speed is changed or input is recorded and replayed. Covers project.run.start in a window or headless, runtime.wait, synthetic input and runtime.ui.press, runtime.properties.get, test.scenario.run, captures and runtime.image.compare, and always ending with project.run.stop. Changing the project is the godot-mcp skill; installing and connecting the server is the godot-mcp-setup skill."
license: MIT
compatibility: "Needs a godot-mcp MCP server (godot-mcp serve) connected for one Godot 4.4.0 or later project with the runtime or full profile, and a Godot editor build the server can find. Screenshots, recordings and image comparisons need a windowed session, so the server needs a display."
metadata:
  contract: "0.5.0"
  profiles: "runtime,full"
---

# godot-mcp-playtest: running and testing the game

The server runs the game as a managed session it owns. A runtime probe, the server's own
script passed to Godot on the command line, talks to the running game, so the project is not
changed for it: no autoload is added and no setting is touched. Every runtime tool names the
session by the id `project.run.start` returned, `run_` followed by 32 hexadecimal digits.

This skill describes contract 0.5.0, the `contract` of its metadata. `health.get` reports the
contract the server speaks as `schema_version`; when the two differ, follow the tool schemas.

## The loop

1. Start: `project.run.start`.
2. Wait until the game is where you need it: `runtime.wait`. Do not sleep; a wait checks every
   frame and says what it saw.
3. Act: `input.action.send`, `input.key.send`, `input.mouse.send`, `input.sequence.send` or
   `runtime.ui.press`.
4. Look: `runtime.properties.get`, `runtime.tree.get`, `runtime.ui.get`, or the assertions of
   `test.scenario.run`.
5. Capture: `runtime.screenshot.capture`, `runtime.recording.capture`, `runtime.image.compare`.
6. Stop: `project.run.stop`, also after a failure.

`health.get` reports the `runtime` component `ready` while a session runs with its probe
connected, and `game_not_running` otherwise. Runtime tools answer `unavailable` with no session.

The examples below play one game. Its main scene shows a menu whose button `Menu/Start` reads
Start; pressing it adds the level as the child `Level`, which holds the player `Level/Player`,
and shows the score in the HUD label `HUD/Score`. The input map has the actions `move_right` and
`jump`. Use the names of the game you test.

## Starting a session

```json tool=project.run.start
{
  "idempotencyId": "playtest-1",
  "display": "window",
  "resolution": { "width": 1280, "height": 720 },
  "lifetimeSeconds": 900
}
```

- The session runs the main scene; `scene` runs another scene instead.
- `display`: `window` draws into the display the server was started with; on Linux the server
  needs `DISPLAY` or `WAYLAND_DISPLAY`. `headless` draws nothing. `auto`, the default, is
  `window` when the server has a display. Screenshots, recordings and image comparisons need a
  window; a headless session's root viewport is 64 by 64 pixels.
- `lifetimeSeconds` ends the session at the latest after that long (default 3600, at most
  14400). At most four sessions, exports included, run at once.
- The start answers once the probe has connected. `project.run.status` reports a session's
  state, or every session's without `session`. `project.run.logs` reads the game's stdout or
  stderr one page at a time, also after it ended.
- `editor.play.start` and `editor.play.stop` run the project in the user's open editor instead,
  as its Play button does. That game has no probe, so the runtime tools need a session from
  `project.run.start`.

## Waiting

`runtime.wait` takes one `condition` and checks it every frame until it holds or `timeoutMs`
passes (default 5000, at most 30000):

| Condition | Holds when |
| --- | --- |
| `node_exists`, `node_gone` | the `node` is in the running game, or is not |
| `property` | the node's `property` compares with `value` by `op`: `==`, `!=`, `<`, `<=`, `>`, `>=`, `contains`, or `approx` within `tolerance` |
| `signal` | the node emitted `signal` `count` times after the wait began |
| `ui_text` | a visible Control shows `text`, exactly or with `match: contains` |

This waits up to 10 s for the menu's button:

```json tool=runtime.wait
{
  "session": "run_0123456789abcdef0123456789abcdef",
  "condition": { "node_exists": { "node": "Menu/Start" } },
  "timeoutMs": 10000
}
```

A node is named by an absolute path (`/root/...`) or by a path relative to the current scene. A
`property` names one property the node lists, such as `position` or a script variable; a path
into it, such as `position:x`, is `not_found`. The answer has `satisfied`, the `outcome`
(`satisfied`, `timeout`, `cancelled` or `interrupted`) and what the last check `observed`. A
comparison that can never hold, an `int` against a string, is refused with `invalid_input`.

## Input

- Prefer the game's input actions to raw keys: `input.action.send` with an `action` of the
  project's input map. `project.settings.get` lists the input map.
- `state` is `tap` by default: press, hold `holdMs` (default 500), release. `press` holds until
  a `release`, until `holdMs` passes, or until the session stops; `release` lets go.
- `input.sequence.send` sends up to 64 steps, with `wait` steps of `ms` between them, at most
  10 s of waiting in all. Whatever it pressed is released when it ends.
- `runtime.ui.press` clicks one button, named by `node` or by the `text` it shows, and answers
  whether its `pressed` signal fired. A disabled or invisible button is `invalid_input`; one
  outside the root viewport is `unavailable`; one in a `SubViewport` is `unsupported`; a text
  that several visible buttons show is a `conflict` naming them.
- Every input call is a journaled command with an `idempotencyId`. A retry with the same id
  answers from the journal and presses nothing again, so use a new id for every new press.
- Input is never left held: it is released when its hold ends, when the sequence that pressed it
  ends, when the session stops, and when the probe loses the server.

This taps `jump` for 150 ms:

```json tool=input.action.send
{
  "session": "run_0123456789abcdef0123456789abcdef",
  "action": "jump",
  "state": "tap",
  "holdMs": 150,
  "idempotencyId": "jump-1"
}
```

## Reading the running game

- `runtime.tree.get` reads the scene tree; `runtime.nodes.find` finds nodes by `class`,
  `group`, `name` pattern, `script` or distance (`near`).
- `runtime.properties.get` reads one node's properties as tagged values;
  `runtime.properties.batch.get` reads up to 32 nodes, or the `Engine` singleton, at once.
- `runtime.ui.get` lists the visible Controls with their rectangles, text and focus.
- `runtime.signals.get` with `watch` counts a node's later emissions.
- `runtime.sample` reads properties or performance monitors every `intervalMs`, with their
  minimum, maximum and mean; `runtime.performance.get` reads the monitors once.
- `runtime.properties.set` writes properties and reads each one back; `matched: false` means a
  setter kept another value.

Every answer carries the frame it was taken on and the probe's scene revision, which moves
whenever a node is added, removed or renamed.

## A test scenario

`test.scenario.run` runs 1 to 64 steps against one session and answers one report. A step's
`kind` is `input`, `wait`, `set_property`, `press`, `time_scale`, `assert_property`,
`assert_ui_text`, `screenshot`, `compare_image` or `stress`; each behaves as the tool it mirrors.
This one goes from the menu to the level, runs right for a second and checks the result against
a reference image of the project:

```json tool=test.scenario.run
{
  "session": "run_0123456789abcdef0123456789abcdef",
  "idempotencyId": "menu-to-level-1",
  "steps": [
    { "id": "menu", "kind": "wait", "condition": { "ui_text": { "text": "Start" } } },
    { "id": "start", "kind": "press", "target": { "text": "Start" } },
    { "id": "level", "kind": "wait", "timeoutMs": 10000,
      "condition": { "node_exists": { "node": "Level/Player" } } },
    { "id": "run-right", "kind": "input", "steps": [
      { "kind": "action", "action": "move_right", "state": "press" },
      { "kind": "wait", "ms": 1000 },
      { "kind": "action", "action": "move_right", "state": "release" } ] },
    { "id": "moved", "kind": "assert_property",
      "target": { "node": "Level/Player", "property": "position" }, "op": "not_equals",
      "value": { "type": "vector2", "value": { "x": 100, "y": 300 } } },
    { "id": "score", "kind": "assert_ui_text", "node": "HUD/Score", "op": "contains", "text": "0" },
    { "id": "shot", "kind": "screenshot" },
    { "id": "looks-right", "kind": "compare_image",
      "reference": { "path": "res://tests/level_start.png" }, "tolerance": 8 }
  ]
}
```

- Each step ends `passed`, `failed` (with `expected` and `actual`), `error`, `skipped` or
  `uncertain`. After a `failed` or `error` step the rest are skipped while `stopOnFailure` is
  true, the default; after an `uncertain` step they are skipped always.
- The `verdict` is `passed` only when every step passed. `maxDurationMs` (default 60000) bounds
  the whole run.
- `assert_property` compares by `equals`, `not_equals`, `less`, `greater` or `approx` within
  `tolerance`; `less` and `greater` take numbers. It also reads the `Engine` singleton's
  `time_scale`, `physics_ticks_per_second` and `max_fps`.
- `compare_image` takes a project PNG (`{ "path": ... }`) or an earlier `screenshot` step
  (`{ "step": "shot" }`), and needs a window, like `screenshot`.
- `stress` adds `count` instances of a scene, samples the frame rate and counts, and checks that
  none is left after they are freed.
- The report is also kept as a JSON artifact. A retry with the same `idempotencyId` runs
  nothing again and answers the recorded status with `replayed: true`; the report stays
  readable as the artifact the first answer named.

## Captures and comparisons

- `runtime.screenshot.capture` captures the root viewport as a PNG, and
  `runtime.recording.capture` records up to 10 s at up to 30 frames per second as one animated
  PNG. The answer names the artifact by URI, with its size and SHA-256; read the bytes with MCP
  `resources/read`. An artifact stays readable for 15 minutes.
- `runtime.image.compare` captures the next frame and compares it with a project PNG, pixel by
  pixel within `tolerance` (0 to 255, default 0). It answers `matched`, the count and share of
  differing pixels, their bounding box and, when pixels differ, a difference image. Images of
  two sizes are not compared and nothing is resized.
- A headless session renders nothing, so captures and comparisons answer `unavailable`.
- Godot builds can rasterize one frame differently: compare captures taken with one build.

## Game speed, recording and replay

- `runtime.time_scale.set` sets `Engine.time_scale` from 0 to 100 (1 is real time, 0 stops game
  time) and reads it back. It changes only the running game; nothing is saved, and the next
  session starts at 1 unless the game's code sets another scale. Game code that counts the
  delta it is given runs faster or slower; `Time.get_ticks_msec()` does not.
- `input.recording.start` records every input event the game window receives, at most 3000
  events and 30 s. `input.recording.stop` answers the events and keeps them as a JSON artifact.
  `input.replay` sends recorded events again at their times, and releases what it pressed when
  it ends.

## Always stop the session

Call `project.run.stop` when you are done, and after a failure too. It releases every held
input, asks the game to quit and ends it when it has not quit after 10 s; the answer carries the
exit facts. Stopping a session that already ended answers its record. A session left running
holds one of the four session slots until its `lifetimeSeconds` runs out, so before you finish,
check `project.run.status` without `session` for sessions you started.

```json tool=project.run.stop
{ "session": "run_0123456789abcdef0123456789abcdef", "idempotencyId": "stop-playtest-1" }
```

## What an answer means

| Answer | It means | Do |
| --- | --- | --- |
| `unavailable` | no session runs, the session renders nothing for a capture, or the button lies outside the root viewport | start a session, a windowed one for captures; `health.get` names the cause |
| `unsupported` | the session's probe does not implement the method, as a probe of an older server does, or the button is in a `SubViewport` | stop the session and start a new one; for a `SubViewport`, use input instead |
| `uncertain` | whether a step or command did what it asked cannot be decided: a wait ran out, an answer was lost, a time scale reads back otherwise | do not resend it; read the game's state first |
| `conflict` | a second recording, a button text several buttons show, or an `idempotencyId` reused with other arguments | correct the call; use a new id for a new action |
| `not_found` | the node or property does not exist | read `runtime.tree.get` and the node's properties |
| `invalid_input` | a comparison that cannot hold, or a disabled or invisible button | correct the call from the message |
| `permission_denied` | `runtime.script.run` before the operator started the server with `--allow-scripts` | use the typed tools; do not ask for the switch unless the user wants a script run |
