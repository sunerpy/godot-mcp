# 0012 — Script execution is declared: three tools behind one operator switch

**Status:** accepted for contract 0.4.0; amended by [ADR 0014](0014-open-http-mode.md),
which accepts the switch with the `open` HTTP mode. It supersedes
[ADR 0003](0003-no-arbitrary-script-execution.md), keeps `unsafe_script` a reserved name,
and leaves the typed tools of [ADR 0011](0011-live-editor-operability.md) as they are.

## Context

ADR 0003 refused every execution entry point and answered each need with a typed tool.
Contracts 0.2 and 0.3 grew that typed surface to 90 tools, and it still leaves gaps an agent
meets every day: a question about a project's live state that no tool asks (an autoload's
value, what a script class computes), a bulk change no batch operation spells, procedural
content, or an engine API nothing wraps. Each gap ends in a new tool, which is slow, or in a
hand-off to a person, which is what this project exists to remove.

The editor plugins this was compared with run caller code in the editor and in the
game. The project owner decided on 2026-11-05 to add the same
reach in three places, the headless engine, the editor and a running game, under an explicit
contract.

## Decision

### Three tools, one for each place code runs

| Tool | Where the source runs | Capability | Status |
| --- | --- | --- | --- |
| `script.run` | a supervised headless Godot on the project, or on a disposable copy of it | `project_write` | in 0.4.0 |
| `editor.script.run` | the connected editor, on its main thread | `editor_mutate` | in 0.4.0 |
| `runtime.script.run` | a managed game session, on its main thread, through its probe | `runtime_execute` | in 0.4.0 |

The source is a GDScript that defines `func run()`. Its return value comes back as a tagged
value when it has a plain one, and as Godot's own spelling of it when it holds an object or a
resource. The typed tools stay, and stay the first choice: they validate, journal and undo
what they change, which a script does not.

### One switch, set by the operator for one launch

A script tool runs nothing unless the operator turned script execution on for the server:
`allow_scripts = true` in the `--config` file, `GODOT_MCP_ALLOW_SCRIPTS=true` with a
`--config` file, or `godot-mcp serve --allow-scripts`. It is off by default. A call while it is
off answers `permission_denied`, names the switch, and writes nothing. When it is on, a call
needs no further confirmation: the switch is the operator's consent for that launch. The tools
are listed either way, so an agent can tell a server that refuses them from one that lacks them.

The switch is refused for a remote principal. `allow_scripts` together with
`http.mode = "proxied_oidc"` is a configuration error at startup, so a script never runs for a
caller the server only knows through a proxy. ADR 0014 accepts it with `http.mode = "open"`:
the operator who started that listener decided who can reach it, and the listener warns at
start when anyone who can reach the port could run code.

### What every run keeps to

- **Nothing confines it.** The source runs with the user's privileges. It may read and write
  any file the user can, start processes and open connections. The only boundary is the one
  every engine run has: the supervised process tree and its deadline.
- **It has an identity.** Each run is one journaled transaction, and its answer carries the
  `transaction_id`: `script.run` on the same process route as `project.run`, the editor and game
  tools on the bridge routes every editor change and game command takes. The journal records the
  request, the source included; audit records carry digests, never the source.
- **It is bounded.** The source is at most 262,144 characters, and so is Godot's spelling of
  the returned value; a longer value is reported by its length. Each output stream is captured
  up to 1 MiB and answered as a bounded excerpt. An answer lists at most 1,000 changed files,
  and a project of more than 100,000 files or 4 GiB is not compared, which the answer says.
- **It says what happened.** `classification` is `passed` when `run()` returned and the engine
  reported no error, `failed` when it reported one or exited non-zero, `compile_error` when the
  source did not compile and `run()` was not called, and `inconclusive` when the run ended with
  no report and no error, or when nothing could have reported one: an editor or a game reads its
  own log only on Godot 4.5.1 and later. Engine diagnostics name the line of the caller's source
  where the engine gives one, and an editor or game run answers what the code printed.
- **It undoes nothing, and says so.** Every answer carries `rolled_back: false`. In the project
  context the answer lists every file the run added, changed or removed, compared by content,
  with `applied: true`. In the copy context the copy's changes are listed and then discarded
  with the copy, and the answer lists what the copy left out (secrets, version control, the
  engine cache).
- **An ended run is reported as ended.** A run that its deadline or a cancellation ended
  answers the route's `timeout` or `cancelled` error, as every engine run does; in the project
  context its details list the files the run changed until then (`changed_paths`,
  `changed_total`), with `applied: true` and `rolled_back: false`. Whatever else the code did
  is not known, and the error says that.
- **A repeated request never runs the source again.** The same `idempotencyId` answers from
  the journal with `replayed: true`. The journal keeps the run's outcome and transaction, not
  its value or its changes, and the answer says that too.
- **An unknown outcome stays unknown.** A server that stops while a run is in flight
  reconciles the transaction like any other process route on its next start: proven, or
  quarantined, never assumed.
- **It does not write underneath an editor.** `script.run` in the project context is refused
  while an editor is connected; the answer points to `editor.script.run` and to the copy
  context.

### Code in a live editor or game

Code that runs inside the editor or a game shares that process's main thread, and Godot cannot
stop a script part way through. The add-on and the runtime probe share one runner
(`script_runner.gd`) that compiles the source as a tool script, `@tool` written in front of its
first line when the source lacks it so its line numbers are kept, and awaits `run()`. Compiling
runs the source's static initializers. A server that did not offer the method in the session's
capabilities is refused by the runner too.

- **A deadline ends the wait, not the code.** The server waits the caller's `deadline_ms` and a
  two-second grace, then answers `uncertain`, with the files the run had changed so far. The
  code goes on.
- **In the editor, an unanswered run blocks the project.** The editor may still save scenes and
  resources, so the transaction covers the whole project, as `editor.filesystem.scan` and
  `editor.play.start` do: an `uncertain` run is quarantined and every change is refused until an
  operator resolves it (see [Recovery](../../operator/recovery.md)). Only closing the editor
  ends the code. While a run awaits, the editor's other requests wait behind it, and code that
  holds the main thread for longer than the bridge's 30-second liveness window ends the session
  the same way.
- **In a game, stopping the session ends it.** A game's writes are outside the server's journal,
  as the game's own code's are, so a game run takes no project scope and blocks nothing. When
  the server stops waiting — the deadline passed, the caller cancelled, or `project.run.stop`
  withdrew what the session runs — the probe answers `uncertain` at its next frame and serves
  the requests behind it; the code goes on until it returns or the stop ends the game.

### What stays as it was

- `unsafe_script` stays a reserved name. It is not a capability, a tool or a switch; the script
  tools use the capabilities above.
- The absence audit becomes an audit of the declared surface. The declared tools are the only
  execution-shaped names any profile advertises, `allow_scripts` is the only key that turns
  anything on and it is accepted at the top level only, the kernel refuses every other
  execution-shaped name, and the byte scan for the reserved name still runs.

## Why the reasoning of ADR 0003 no longer decides it

**"It collapses the reviewable surface to one entry point."** With the switch off, which is the
default, the surface is as enumerable as before. With it on, the operator chose that for one
launch, and each run is still a journaled transaction whose project changes are listed file by
file. What a run does outside the project is not recorded; the documentation says so plainly
instead of implying a containment that does not exist.

**"Loading a project already runs code."** This still holds, and it is why the switch exists:
a script runs with no more privilege than the project's own `@tool` code, but the caller
composes it. That is also why a remote principal can never run one.

**"An unsafe path that exists gets used."** That risk is accepted. Keeping it small is the job
of the default, the per-launch switch, the refusal for remote principals, and documentation
that calls it what it is.

## Consequences

- An agent can answer a one-off question or make a bulk change in one call instead of waiting
  for a tool or a person.
- While the switch is on, the capability profile no longer bounds what can happen. An operator
  should treat `--allow-scripts` like handing the agent a shell in the project directory.
- The threat model, the configuration reference and the profile documentation describe the
  switch, the tools and their limits.

## Enforcement

- A server without the switch lists the three tools and refuses each with `permission_denied`
  before anything is written or sent (`tests/script_run.rs`, `tests/editor_script.rs`,
  `tests/live_runtime_script.rs`); a server with it sends the editor and the probe the method,
  and shapes their reports (`tests/editor_script.rs`, `src/live_script.rs`).
- The configuration is off by default, is turned on by the file, the environment or the flag,
  is refused with `proxied_oidc` and accepted with `open`
  (`crates/godot-mcp-domain/tests/config_layering.rs`).
- The absence audit proves the declared set and the single switch
  (`crates/godot-mcp-server/src/audit/absence.rs`, `tests/audit_limits.rs`).
- The live suites run every case above on real Godot builds: a project run with its changes and
  a replay, a copy run, compile, runtime and object-value failures, and a deadline
  (`tests/script_run.rs`); an editor run against the open project, its failures, and a run that
  outlasts its deadline and goes on in the editor (`tests/live_editor_script.rs`); a game run
  against the running scene and its autoloads, a run that outlasts its deadline, and a stop that
  ends a run that never returns (`tests/live_runtime_script.rs`). All are `--ignored`.
