# 0017 — Stepping a game by physics frames

**Status:** accepted for contract 0.7.0.

## Context

Game logic that depends on frame timing — an input buffer, a coyote-time window, a jump that
reads `Input.is_action_just_pressed` in `_physics_process`, a physics body's speed after a
collision — cannot be checked frame by frame through a managed session. Synthetic input and
samples are timed by the wall clock and delivered in idle frames, so the physics frame an event
reaches is whichever one runs next, and a slow idle frame makes Godot run two or three physics
frames back to back to catch up. `runtime.time_scale.set` scales game time but cannot stop it
between two frames. An agent could not say "press jump in frame 5, then read the player's speed
in frame 6", nor freeze the game to read it at rest.

## Decision

### One tool, two fields, and the frame of a recorded event

| Change | What it does |
| --- | --- |
| `runtime.frames.step` | Advances the game by exactly `frames` physics frames (0 to 600), then holds it paused (`then` hold, the default) or lets it run (`then` resume). `frames` 0 holds a running game at its next physics frame, keeps a hold, or with resume releases it. `timeoutMs` (1 to 30000, default 20000) ends a step early, holding the game with what it advanced. In the `runtime` and `full` profiles, a journaled command with the `runtime_execute` capability, declared as `runtime.time_scale.set` is. |
| `runtime.sample` `cadence` | `interval`, the default, samples every `intervalMs`; `physics_frame` takes one sample at the start of every physics frame, before the game's nodes process it, and takes no `intervalMs`. |
| `input.replay` `schedule` | `time`, the default, delivers each event at its `t_ms`; `physics_frame` delivers it so that the game sees it in the physics frame its `frame` names, counted from the first physics frame after the replay starts. |
| A replayed event's `frame` | 0 to 30000 in the schema; the probe also refuses a frame later than 30 s of physics frames at the game's own rate. `t_ms` is no longer required by the schema: the check requires the field of the schedule and ignores the other. |
| A recorded event's `frame` | `input.recording.stop` answers each event's physics frame from `first_physics_frame`, the frame after the recording started, so a recording replays by frame as it is. |

A step answers `outcome` (`completed`, `timed_out`, `cancelled`, `interrupted` or
`interfered`), `frames_requested`, `frames_advanced`, the `first_frame` and `last_frame` it
advanced (Godot's `Engine.get_physics_frames()` in them, `null` when none), `then`, `held`,
`paused` (`SceneTree.paused` afterwards), `elapsed_ms` and `interference`. A replay keeps every
field it answered and adds `schedule`, `first_physics_frame`, `deliveries` (each event's index in
the caller's list and the physics frame the game sees it in, in delivery order),
`scheduled_frames` and `max_lag_frames`; under the time schedule the two frame fields are `null`,
and under the frame schedule `scheduled_ms` and `max_lag_ms` are.

No tool schema states a rule across its fields at its top level: the schemas allow `intervalMs`
beside `cadence` physics_frame and an event without `t_ms`, and the checks refuse what the
schedule or cadence does not allow, with `invalid_input`, before anything is sent.

### The probe acts at the start of every physics frame

The probe sets its `process_physics_priority` to the lowest value there is. Godot runs
`_physics_process` by priority, then in tree order, so the probe's runs before every game node's
except a node at the same priority that comes before it in the tree, such as an autoload: the
launcher adds the probe after the autoloads. The probe emits a signal there, at the start of
every physics frame, and what is done per frame is done in it:

- **A hold** is the probe setting `SceneTree.paused` at the start of a physics frame, before the
  game's nodes process it. Godot checks whether each node can process as it reaches it, and
  setting the pause makes the physics servers inactive at once, so neither the game's pausable
  nodes nor the physics server run that frame.
- **A step from a hold** releases it at the start of its first frame, in the same call, so the
  game's nodes process that frame and no idle frame of the game runs before it. It counts each
  frame at its start and holds again at the start of the frame after the last, before the game's
  nodes process it. From a running game it counts from the next physics frame.
- **A per-frame sample** reads its series there, so each sample sees the state the previous
  frame left.

### A replay by frame delivers at the end of the frame before

Measured on Godot 4.4.1, 4.5.1, 4.6.1, 4.7.1 and 4.7.2, official Linux x86_64 builds, headless
and windowed: an action event parsed during physics frame *k* is just pressed in frame *k* + 1,
not *k*, and one parsed without a flush waits for the next idle frame. Delivering the events of
frame *k* at the start of frame *k* therefore landed them one frame late. The probe instead
delivers them from a call deferred to the end of frame *k* − 1, once every node has processed
it, and flushes them: the game's `_input` and `_unhandled_input` receive each event in frame
*k* − 1, and its `_physics_process` in frame *k* sees it pressed and just pressed, however many
physics frames an idle frame runs. The events of frame 0 are delivered when the replay starts,
between physics frames, with a flush. Events of one frame keep their order in the caller's list.

### The game's own pause is never changed

- A step, of any kind, is refused with `conflict` while the game paused itself:
  `SceneTree.paused` is true and the probe did not set it. Nothing changes.
- A pausable child of the probe is notified whenever the tree's pause changes. A change the probe
  did not make, while it holds or steps the game, makes the pause the game's: the probe holds
  nothing from then on, and a step in flight ends at the next frame with `outcome` interfered and
  `interference` naming the physics frame and the new pause.
- A game that sets `SceneTree.paused` to true while the probe holds it changes nothing Godot
  notifies, so the probe cannot tell; its release then unpauses the game.
- A hold lasts until the next step, a step of 0 frames with resume, the session's stop, or the
  probe losing its server. A step that times out or that the client withdraws holds the game
  where it stopped; one the session's stop or a lost connection interrupts releases it.
- A replay by frame is refused with `conflict` while the probe holds the game: no frame it names
  would reach the game's nodes.

### What a hold does not stop

Measured on the five builds above, by the live suite: during a hold, a node whose process mode
is always processes every frame, handlers of the SceneTree's `physics_frame` signal run every
frame, and a SceneTreeTimer, which processes always unless it is told otherwise, keeps firing.
The game's pausable nodes, its `Timer` nodes and the physics server stand still. An autoload at
the probe's own priority sees a step one frame later than the scene: from a hold it processes
the frames after the first through the one after the last. Rendering, the engine's frame counters
and its clocks go on. Audio, animations and tweens set to process always, threads and native
code were not measured.

### Fail closed

- A probe that predates the tool does not list its method. The step is refused with
  `unsupported`, and so are a sample and a replay that ask for a physics-frame cadence, which such
  a probe would carry out by time instead; nothing is journaled or sent.
- A step whose answer is lost is `uncertain`: how many frames advanced, and whether the game is
  held, are unknown.

## Why

**Why the game's own pause.** It is what Godot gives every script on every build the server
supports, it stops the scene's pausable nodes and the physics servers together, and it needs
nothing in the project. What it does not stop is what the game itself chose to keep running while
paused, and the answer and the reference say so rather than hide it.

**Why count at the start of a frame.** The hold has to take effect before the game's nodes process
the frame after the last one, or the game would run one frame more than asked; and a sample taken
there reads what one whole frame left. The probe's own priority puts it there for everything but
the nodes that share it.

**Why `frame` counts from the next physics frame.** It is the first frame in which a game can see
an event delivered now as just pressed, so a recording's frame and a replay's frame mean the same
thing: the physics frame the game first sees the event in.

**Why 600 frames.** Ten seconds at Godot's default 60 physics frames per second is longer than a
frame-level check needs. A longer advance is a release, a wait and another hold.

## Consequences

- An agent can freeze a game, read it at rest, advance it by a counted number of physics frames,
  and deliver input into named frames, with answers that say which frames advanced.
- What the game keeps running while paused also runs while it is held. An agent that needs those
  nodes frozen too has to change their process mode in the game, which the server does not do.
- The probe's physics priority is the lowest there is in every session, which moves it ahead of
  every game node but the autoloads that share that priority.
- [Managed sessions](../../operator/managed-sessions.md#stepping-by-physics-frames) describes the
  tool, the cadence, the schedule and what was measured.

## Enforcement

- The schemas and checks agree on every case, no tool schema combines its fields at its top level,
  and the payloads and timeouts are what the probe is sent
  (`crates/godot-mcp-server/src/engine_plane/session/tools/frames.rs`,
  `crates/godot-mcp-server/src/engine_plane/session/tools/control.rs`).
- Against the stand-in game: the answers, the refusals before anything is sent, an older probe and
  a lost answer (`crates/godot-mcp-server/tests/runtime_control.rs`).
- The live suite checks, on every locked Linux editor, that a step advances exactly its frames and
  the physics server steps once each, what a hold stops and does not, timeouts, withdrawals,
  conflicts and interference, the release on stop and on a lost server, per-frame samples under
  stalled idle frames, a recording's frames and replays by frame and by time, and a held window
  (`crates/godot-mcp-server/tests/live_runtime_control.rs`, `--ignored`).
