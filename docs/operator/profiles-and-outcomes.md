# Profiles and tool outcomes

A profile is a deliberate choice about what this server is allowed to do to your
project. It is not a convenience setting, and the safe choice is the default.

## The four profiles

| Profile | What it adds | Choose it when |
| --- | --- | --- |
| `core` | Read-only discovery and analysis | You want answers about the project and nothing changed |
| `editor` | Project authoring — files, GDScript, project settings and assets — plus the engine-backed import and canonical re-saves | You want the project changed |
| `runtime` | Running, testing and engine-checking the project, plus the authoring reads | You want the project executed and observed |
| `full` | The union of the three | You want all of the above |

`full` is exactly `core` ∪ `editor` ∪ `runtime`, computed from the same canonical
manifests, so it can never drift from its parts. The complete generated list of
tools and resources per profile is in
[Profiles, tools and resources](../protocol/tool-inventory.md).

## There is no arbitrary-script-execution tool

`unsafe_script` is a reserved identifier and nothing else. It is not a capability, it
has no handler, it is in no profile, it is not a configuration option, and it is not
in the shipping binary. A request naming it is refused the same way any unknown tool
is.

This is the single largest deliberate difference from tools that expose an
"execute this GDScript" entry point. Loading a Godot project is already arbitrary
code execution — see [Process boundaries](process-boundaries.md) — and adding a
direct execution tool on top of that removes the only thing that makes the surface
reviewable.

## Membership is not availability

A tool appears in `tools/list` because it belongs to the selected profile. Whether
the plane behind it can be reached right now is a separate question, answered per
request.

When the plane is not reachable, the call returns `unavailable` with the reason.
It does not silently succeed, it does not pretend to have acted, and the tool does
not disappear from discovery. That is what keeps the contract stable across
deployments: a client can rely on the tool list, and it learns about the environment
from the outcome.

A method enters discovery only together with its production handler, so a plane
with no implementation contributes no listed method. The live-editor methods are
listed in the `editor` and `full` profiles and answer `unavailable`, naming the editor
plane, while no editor running the `godot_mcp` add-on is connected; `scene.save`, which
must consult a live editor before it may write, answers the same way. The managed
session methods are listed in the `runtime` and `full` profiles: a runtime read, input
or capture answers for a running game session, and a debug command for a running
editor session. See [Editor bridge](editor-bridge.md),
[Managed sessions](managed-sessions.md) and
[ADR 0010](../contributor/adr/0010-complete-development-bridge-strategy.md).

## Four planes

| Plane | Needs a Godot executable | Needs an already-running Godot | Needs the editor plug-in |
| --- | --- | --- | --- |
| `static` | No | No | No |
| `project` | Yes | No — it starts its own | No |
| `editor` | Yes | Yes | Yes |
| `runtime` | Yes | Yes — a managed session the server starts | No |

The static and project planes are structurally headless in different senses. The
`static` plane's code paths contain no process spawn at all, so no configuration can
make them launch Godot — `project.validate` and `script.validate` are static in every
profile and always disclose that engine validation was not performed. The `project`
plane is where controlled *headless engine* work happens: it does start Godot, without
an editor session and without a running game.

The table describes what each plane would require. What this release actually binds is:

| Boundary | Bound to | Behaviour |
| --- | --- | --- |
| `file` | the real safe-root file boundary | project writes execute |
| `process` | the real supervised-process boundary | Godot launches inside a containment boundary; verified against a real Godot 4.7.1 build |
| `lsp_dap` | a managed editor session's language server and debug adapter, dialled only through that editor's launch receipt (`runtime`, `full`) | debug commands answer `unavailable` until an editor session runs |
| `editor` | the editor bridge the `godot_mcp` add-on connects to (`editor`, `full`) | editor tools answer `unavailable` until an add-on is connected; `scene.save`, which must consult a live editor first, answers the same way |
| `runtime` | a managed game session's runtime probe (`runtime`, `full`) | runtime reads, input and captures answer `unavailable` while no game session runs |

Every boundary is real in the profiles that serve it; a profile that does not serve a
plane attaches no boundary for it, and its tools are not listed. The `lsp_dap`, `editor`
and `runtime` boundaries additionally need something to serve: a running editor
session, a connected add-on, a running game session. An arbitrary loopback listener is
never treated as proof of ownership.

Discovery lists only methods this build implements. A listed method whose plane has
nothing to serve, such as `scene.save` with no editor connected, answers `unavailable`
and names the plane.

The `project` plane is genuinely delivered — `project.import.run`, `project.run`,
`project.test`, `script.check`, `project.uid.update`, `resource.update` and `scene.save`
all launch a real, contained engine — but it is not unconditional. It needs a discoverable Godot
executable, a build whose advertised capabilities the probe recognises, and the bundled
supervisor beside the server executable. [Process boundaries](process-boundaries.md)
lists every precondition, and `health.get`'s `godot` component reports which of them
currently hold. The [What an answer means](#what-an-answer-means) section below narrows
the `project.import.run`, `project.test` and `script.check` result contracts.

## Five capabilities

`read`, `project_write`, `editor_mutate`, `runtime_execute` and
`export_destructive`. Each operation consumes exactly one, and a profile grants a
fixed set. There is no capability for arbitrary execution, so no profile can be
combined into one.

## Authoring

The `editor` and `full` profiles author a project through journaled, compare-and-swap
mutations. The rules are the same for every one of them:

* Every mutation needs an `idempotencyId`. Repeating the exact call with the same
  identifier answers the recorded result instead of applying twice; reusing the
  identifier for different arguments is a `conflict`.
* Replacing, moving or deleting a file needs its current SHA-256 as
  `expectedSha256` — read it with `project.files.list` or `file.get`. Creating a
  file needs the path to be absent. A stale hash is a `conflict` that names the
  current one, and nothing is journaled.
* `expectedRevision` refuses the mutation unless the workspace is still at that
  revision; `dryRun` validates and returns the bounded diff without writing.
* An answer carries the workspace revision that already contains the change, so the
  next read sees it.
* `project.godot` changes only through `project.settings.patch`, whose typed
  operations keep unrelated keys and comments. A file's `.import` and `.uid`
  sidecars move and are deleted with it. The engine cache, the private state root
  and the helper scratch area are neither readable nor writable, and neither is a
  file named `.cas-<pid>-<n>.tmp`: every write stages its bytes under such a name
  beside its target and then publishes them, so the workspace never counts one as
  project source.
* `script.create` and `script.patch` parse the result statically and refuse parse
  errors unless `allowParseErrors` is set. Nothing is executed; `script.check` is
  the engine's verdict.
* `asset.put` takes small assets inline and larger ones as ordered chunks that are
  published only once complete and verified. `project.import.run` with `assets`
  imports them and reports each one's import state and imported artifact hashes;
  `asset.status` reads the same state without importing.

A server started with `serve --workspace <dir>` instead of `--project` begins
with only `health.get`, `capabilities.get`, `workspace.projects.list`,
`project.create` and `project.bind`. Binding one project switches the tool list to
that project's tools, once, for the rest of the process lifetime.

`project.create` binds its `idempotencyId` to the exact request, for the calling
principal: a retry answers the recorded result with `replayed: true`, the same id
with other arguments is a conflict, and a project another id created is a conflict
too, never a replay. The records live in a private `.godot-mcp-bootstrap/`
directory at the top of the workspace, which is never a project and cannot be
created or bound into.

## What an answer means

Every outcome carries a typed result and, for static answers, provenance and a
confidence level. The vocabulary is generated in
[Outcome taxonomy](../protocol/error-taxonomy.md).

### `project.import.run` requires process and import evidence

`project.import.run` keeps the normal mutating process route: router admission,
capability permit, serialized scheduling, audit and idempotency all apply before the
contained Godot process starts. Its `process_launch` proof establishes how that process
ended; it does not by itself prove that importing succeeded.

An import reports `passed` only when the capped engine output parses without errors, the
exit status is zero and every nominated expected artifact is present. Parsed error
diagnostics report `failed` even when Godot exits zero or an artifact was already present.
A missing artifact or truncated clean-looking output remains `inconclusive`; a supervisor
deadline remains `timed_out`. The response includes the normalized, bounded `diagnostics`
and a `diagnostics_truncated` flag so clients can inspect the same evidence used by the
verdict. Both `project.import.run` and `project.run` also return `stdout` and `stderr`
metadata with `retained_bytes`, `total_bytes` and `truncated`; the response does not
embed the captured stream bodies.

### `project.test` reports the framework verdict

`project.test` detects the project's already-committed GUT or gdUnit4 installation,
builds that framework's documented command line, captures the supervised engine's
output and parses the framework's own end-of-run summary. It never installs a framework
or invents test counts.

A `passed` verdict requires both a framework-documented clean exit status and a parsed
summary reporting zero failures. A parsed failure summary produces `failed`; a deadline
produces `timed_out`, never `failed`; and a zero exit without a parsable summary remains
`inconclusive`. No framework, or more than one installed framework, produces a
structured `unsupported` report with the detection evidence. Counts are omitted unless
the completed run is conclusively `passed` or `failed`.

Every launched run still goes through the durable containment journal. The report
returns the exact argument array, captured stdout and stderr, normalized engine
diagnostics and its transaction id, while the journal retains the process-boundary
evidence needed for recovery. See [Recovery](recovery.md) for the caller-visible
contract when a launch does not complete normally.

### `script.check` is the engine-backed script check

`script.check` is deliberately separate from static `script.validate`. It validates a
project-relative script path, probes the selected editor build for `--headless`,
`--path`, `--check-only` and `--script`, then runs that exact script through the real
contained engine. A syntactically valid script can therefore return `passed`, and
engine parse diagnostics produce `failed`; a deadline remains `timed_out`.

If the selected build does not advertise one of those flags, the tool returns a
structured `unsupported` report and launches nothing. Discovery, probe, supervisor and
other launch failures remain typed tool errors rather than being mislabeled as script
verdicts. The default per-call deadline is 30 seconds, capped by the operator's
`limits.deadline_ms` setting.

### Static answers are honest about what they cannot prove

GDScript is dynamically typed and a project can build a resource path at runtime. A
static analyser that always claimed certainty would be wrong in exactly the cases
that matter most, so it does not:

* `exact` — statically proven.
* `inferred` — one best candidate from heuristics; not guaranteed to be the only
  one, and not guaranteed to be right.
* `unresolved` — could not be resolved at all.

When a question cannot be answered soundly, the outcome is `uncertain`. That is not
an internal error and not a synonym for failure — it is the analyser declining to
guess. Treat `uncertain` as "ask a different question or run the engine", never as
"probably fine".

### Failures are typed

The ten error codes are generated in
[Outcome taxonomy](../protocol/error-taxonomy.md). Three are worth knowing before
you start:

* `unavailable` — the plane exists in this profile but cannot be reached now.
* `conflict` — another actor changed the thing under you, or an idempotency identity
  was reused across a different context. Nothing was written.
* `uncertain` — the answer could not be proven; see above.

## Choosing

Start with `core`. Move up only when you have a task the profile cannot express, and
read [Recovery](recovery.md) before you enable a profile that writes: a mutation is
durable, and the recovery contract is what you rely on if a process dies in the
middle of one.
