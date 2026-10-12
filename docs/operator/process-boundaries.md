# Process boundaries

Some operations start a real Godot process: `project.import.run`, `project.run`,
`project.test`, `script.check`, and the canonical writes `project.uid.update`,
`resource.update` and `scene.save`. When one of them does start a process, that process is created inside a
**containment boundary** whose identity is recorded durably before the process exists,
so a crash never leaves a process nobody can account for. That guarantee holds
unconditionally *once a launch begins* — it is not a configuration option and there is
no flag that disables it.

Whether a launch begins at all is a separate question, and it has preconditions. Read
them before you rely on any of the seven operations above.

## Preconditions for a Godot process to start

All four must hold. If any one fails, the operation is refused with a typed outcome
and nothing is launched.

**1. The selected profile must advertise the tool.** The `core` profile grants `read`
only and advertises no tool whose effect reaches the `process` plane, so no
configuration makes it launch anything. `project.import.run`, `project.uid.update`,
`resource.update` and `scene.save` are `editor` and `full` members; `project.run` and
`project.test` and `script.check` are `runtime` and `full` members. See
[Profiles and outcomes](profiles-and-outcomes.md) for profile membership. The
[What an answer means](profiles-and-outcomes.md#what-an-answer-means) section explains
how `project.test` and `script.check` classify engine-backed validation results.

**2. A Godot executable must be discoverable.** Resolution is explicit-first: the
operator's configured path, then this host's conventional install locations, then each
`PATH` entry. Exactly two settings name it — the `godot_binary` configuration key and
the `GODOT_MCP_GODOT_BINARY` environment variable — and **the environment variable is
only consulted when a configuration file is supplied**. `godot-mcp serve` without
`--config` reads no configuration at all, so `GODOT_MCP_GODOT_BINARY` on its own has
no effect; pass `--config` as well. If nothing resolves, the operation fails with
`not_found` and remediation naming the number of candidates searched. Nothing is ever
downloaded: this server does not fetch an engine build on your behalf.

**3. The build must pass capability probing.** After discovery, the resolved
executable is digested and probed, and its advertised command-line capabilities are
parsed from its own `--help` output. A build whose capabilities cannot be recognised
is **refused**, not assumed to behave like a supported one. Export-template (runtime)
builds are refused for the same reason: they are not editor builds. This is a real
gate, not a formality — Godot 4.7.1 emits `--help` with ANSI colour escapes even to a
pipe, and until that was handled the probe parsed zero advertised flags and rejected a
perfectly good engine as an unrecognised build.

**4. The bundled supervisor must be present.** Every launch goes through the
`godot-mcp-supervisor` executable, which ships in the release archive's `bin/`
directory beside `godot-mcp` and is declared in the archive manifest. The two are one
unit: installing or copying only the server executable leaves a build that cannot
create a contained process.

`health.get` reports preconditions 1, 2 and 4 in its `godot` component, and says which
of them it observed. It deliberately does not run precondition 3, because probing means
executing the engine, which a health query must not do.

## What does *not* start a Godot process

`project.validate` and `script.validate` are **static** validators in every profile.
They never start an engine, and every answer they return carries an
`engine_validation_not_performed` limitation saying so: script compilation and scene
instantiation were not checked. Do not read a static `valid` verdict as "the engine
says it compiles". Use runtime-profile `script.check` when that engine-backed answer is
required; it starts a contained editor build with `--check-only --script` after
validating the project-relative path and advertised command-line capabilities.

The language server and debug adapter are reached only inside a managed editor
session: an endpoint is dialled only through the server-owned launch receipt of the
editor this server started on it, and only while the listener is held by that editor's
own process. `debug.command` and `debug.session.get` are listed under the `runtime` and
`full` profiles; with no editor session the `lsp_dap` plane has nothing to serve and
those calls answer `unavailable`. See [Managed sessions](managed-sessions.md).

The safe-root file and supervised process boundaries are live in production. Operators
can use file effects and launch Godot through process supervision when the documented
preconditions hold.

Managed game and editor sessions are supervised launches inside the same boundaries,
held open after the start answers: the session's boundary identity is durable before the
start returns, and a stop, the session's lifetime or the next server's startup
reconciliation terminates it. The live editor plane is the `godot_mcp` add-on connected
to the editor bridge (see [Editor bridge](editor-bridge.md)); the runtime plane is a
managed game session's probe. A listed tool whose plane has nothing to serve — no add-on
connected, no game running — answers `unavailable` and names the plane.

There are two strengths of boundary, and which one is required depends on whether the
project's code is trusted.

## Which boundary a launch gets

`[process]` in the configuration chooses it once, when the server opens the project (see
[Configuration](configuration.md#process)); every launch of that server uses it, and every
launch result reports its `kind` and `strength`.

| `process.boundary` | Host | Strength | What contains the launch |
| --- | --- | --- | --- |
| unset or `weak` | Linux | weak | the trusted-Unix locator below; refused for an untrusted project |
| `cgroup` | Linux | weak | a cgroup v2 sub-tree delegated to the cgroup the server runs in, one child cgroup per launch, with process, memory and CPU limits and kill-on-close through the cgroup's kill file; refused for an untrusted project, because a member can move itself out (see below) |
| `adapter` | Linux | strong | the configured container or virtual-machine adapter, for example the reference namespace adapter below |
| unset or `weak` | macOS | weak | the trusted-Unix locator below, observed through `sysctl` and `libproc`; refused for an untrusted project |
| `adapter` | macOS | strong | the configured container or virtual-machine adapter |
| unset or `job_object` | Windows | strong | a Job Object with no breakaway and kill-on-close |

A boundary the host cannot provide — or, for `cgroup`, a server that was not started inside
a delegated cgroup — leaves the process plane unavailable: process-backed tools answer
`unavailable` with the reason, and `health.get` gives the same reason in its `godot`
component. Nothing falls back to a weaker boundary than the one configured.

Every launch gets a boundary of its own. A Windows Job Object is named, and the name is
shared by every process in the session, so a launch whose job name another job already
holds is refused rather than joined: sharing a job would count the other launch's
processes as this launch's and end them together.

When the server starts, it reconciles every launch a previous server left, whichever
boundary that launch was created in, so changing `process.boundary` never orphans a
process. A launch cgroup that the system removed after its last member ended counts as
empty.

## Read this first: loading a project runs the project's code

This is a property of Godot, not of this server. A `@tool` script executes inside the
editor, a `@tool` autoload runs as soon as the project loads, and `--import` implies
editor mode. Godot's own documentation notes that the editor includes no protection
against misuse of `@tool` scripts and that modifications made this way are permanent.

So the honest question is never "is the code prevented from running" — it runs. The
questions are *what may it change* and *what still bounds it*.

## Trusted: a documented weak boundary

For a project you trust, execution may use a weak boundary. It is weak in a specific,
documented way, and it is worth understanding rather than assuming.

The boundary's identity is a five-part tuple — process id, process start time, process
group id, a 256-bit nonce, and the control endpoint — together with the control
socket's inode. Reconciliation requires an exact match on all of it.

A process group id on its own is **never** identity and never proof. Process ids are
reused, and a group id says nothing about which program is in the group. Anything that
treated a bare group id as evidence would happily terminate an unrelated process after
a reboot or a wraparound.

The sequence closes the window where a process could exist that nothing durable knows
about:

1. A private durable locator is created first — a pre-bound control socket, a 256-bit
   nonce and the planned transaction id — and fsynced as the launch intent. No process
   exists yet, so a crash here leaves inert locator state and nothing else.
2. A minimal supervisor is spawned into a new process group, blocked, and its full
   identity tuple is captured and fsynced.
3. The supervisor creates Godot behind a second gate, which is fsynced.
4. Only then is the child released.

Each of the five crash phases has one defined safe reconciliation, and none of them can
conclude "probably nothing was started".

This sequence is delivered and has been executed end to end against a real Godot 4.7.1
build on Linux. A `project.import.run` call returns a `process_launch` proof carrying
the child's process id, its exit code, its release status, and both
`boundary_membership: confirmed` and `boundary_empty_after_exit: true` — and the run
really did populate the project's engine cache. This proof establishes the launch and
cleanup facts, not the import verdict: `project.import.run` separately requires parsed
diagnostics, a zero exit and every nominated artifact before reporting `passed`. Its
normalized diagnostics are derived from the already byte-capped captured streams and
returned with the verdict. `project.import.run` and `project.run` expose each stream's
retained byte count, actual byte count and truncation state, so a capped prefix is never
presented as complete output. A launch that produced no proof did not happen.

Each launch writes its gate and capture files under the project's private
`.godot-mcp/engine-launches/.godot-mcp/launch-<transaction>/` scratch tree. The capture
file can grow until that launch reaches its deadline; the response cap limits retained
evidence, not bytes written by the child. Once the terminal journal record is durable
and the boundary is closed, the launch directory is removed. Startup reconciliation
retains scratch for open, quarantined or still-live launches until it has used the gate
evidence, then sweeps terminal and untracked directories. Failure to prove safety keeps
the directory rather than deleting recovery evidence.

## Untrusted: a strong boundary or nothing

For untrusted code — `process.trust = "untrusted"` — a weak boundary is not offered.
Execution requires one of:

* a Windows Job Object configured with no-breakaway and kill-on-close, or
* a configured container or virtual-machine adapter, on Linux or macOS.

A delegated cgroup is not on this list. Its members run as the user the sub-tree is
delegated to, and that user owns the membership file of every cgroup in it, so a member
can write its own process ID into another cgroup there and leave its launch cgroup: the
cgroup limits and terminates what stays inside, but cannot keep anything in. It reports
`no-breakaway=false` and is offered to trusted projects only.

macOS has no kernel mechanism this server can use to contain a process tree, so an
untrusted project there needs an adapter.

### The reference namespace adapter on Linux

`scripts/adapters/linux-namespace-adapter.sh` is a complete adapter. Each boundary is a
cgroup of its own, with a 4096-task, 2 GiB memory and 200% CPU limit and kill-on-close
through the cgroup's kill file, and every launch runs inside it under bubblewrap, in new
user, PID, mount, IPC, UTS and cgroup namespaces, with no nested user namespace, a
private `/proc` that shows only the launch's own processes, and an empty
`/sys/fs/cgroup`. A member therefore never sees, and cannot write, a cgroup outside its
boundary, which is what makes the boundary strong. The launch shares the host's files,
display and loopback network, so Godot runs as it would outside: the boundary confines
processes and resources, not what the project may read or write.

It needs bubblewrap at `/usr/bin/bwrap` and cgroup v2 delegation from the systemd user
instance:

1. Hold a delegated cgroup for the boundaries: start a user scope with `Delegate=yes`
   (for example `systemd-run --user --scope -p Delegate=yes -- sleep infinity`), then
   move its own process into a child cgroup of the scope, so the scope itself holds none
   and its boundaries can use the pids, memory and CPU controllers.
2. Start the server in a user scope too, as for the `cgroup` boundary: cgroup v2 moves a
   process between two cgroups only when the user owns their common ancestor.
3. Set `process.boundary = "adapter"`, `process.adapter.program` to the script and
   `process.adapter.root` to the scope's directory under `/sys/fs/cgroup`.

If none is available, **the launch fails**. It does not fall back to the weak path.
That is the whole point: the untrusted case is exactly where a weak boundary would be
worthless, so there is no configuration in which it is used there.

## What a boundary guarantees, precisely

Cleanup covers descendants that remain **within** the boundary. A process that
deliberately detaches itself out of the boundary is outside that guarantee, and
containing it requires stronger external isolation than a boundary of this kind
provides.

There is no configuration flag that relaxes the untrusted requirement, and none that
disables boundary creation.

## Disposable projects: mutation containment

Where an untrusted project must be loaded, it can be loaded from a filtered,
throw-away copy. This changes **which tree** the run is able to mutate, and that is
all it changes. It does not restrict what the process may execute, which files it may
read, or where else it may write.

Specifically not contained by a copy:

* an absolute-path write anywhere the invoking account can write;
* a network connection made by project code;
* a read of any file the invoking account can read, including material excluded from
  the copy;
* an exec of any program the invoking account can run;
* resource exhaustion, which only the containment boundary bounds.

Confinement of the *process* is the boundary's job, and only a strong boundary does
it. The [threat model](../security/threat-model.md) states this posture in the
project's own words.

## What has been executed where

Every boundary below has run on a real host of its platform, through the real-client
matrix (`scripts/real_client/platform_matrix.py`: the M0–M6 smokes and the complete-game
E2E against Godot 4.4.1, 4.5.1, 4.6.1 and 4.7.1, each with its own export templates),
driving the executables of the release archive built for that platform from this source
tree, and the workspace tests.

**Linux.** The weak locator, the delegated cgroup and the reference namespace adapter
have been executed end to end: a one-shot run and a managed session inside a delegated
cgroup each ran in their own launch cgroup, a server started in another delegated scope
terminated what a killed server had left there, and a launch through the adapter could
neither move itself out of its boundary nor see the cgroup filesystem. The matrix passed
under the weak locator, inside delegated cgroups and, for an untrusted project, through
the reference namespace adapter.

**macOS** (Apple silicon, macOS 26.2). The workspace tests ran there, including the
process observations against live processes and the launch-protocol tests through the
weak locator and the reference container adapter. The matrix passed through the weak
locator in a logged-in user's GUI session (Aqua): windowed sessions with input and
captures, the editor bridge, LSP and DAP with a debug run stopping at a breakpoint, and
exports with the macOS app bundle smoke-run. A server in a background session, such as a
CI build's, has no window server, so a windowed session is refused there.

**Windows** (Windows Server 2022). The workspace tests ran in a container, including the
Job Object launch protocol: a gated launch inside a job, its exit status, the job proven
empty and reopened from its recorded identity, a deadline that ends the whole job, and
managed sessions started, stopped, terminated and reconciled through it. The matrix
passed in a logged-on desktop session, where Godot renders through ANGLE on Direct3D 11
(the Microsoft Basic Render Driver): windowed sessions with input and captures, the
editor bridge, LSP and DAP, and exports with the Windows build smoke-run. A Job Object is
kill-on-close, so a server that is killed takes its sessions with it and the next server
records them as reconciled. Server Core containers cannot start Godot at all, since they
lack `AVRT.dll`, which Godot imports.

## Related

* [Recovery](recovery.md) — what happens to a process that outlives the server.
* [Project leases](project-leases.md) — what bounds writes.
