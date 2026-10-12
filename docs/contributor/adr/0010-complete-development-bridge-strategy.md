# 0010 — Complete development uses a GDScript bridge with an optional gdext backend

**Status:** accepted for post-0.1 implementation. This record supersedes
[ADR 0004](0004-editor-plane-cancelled.md) and supersedes the post-0.1 restriction in
[ADR 0001](0001-gdext-reference-only.md). Their first-release history remains true and is
not rewritten.

## Context

Version 0.1 deliberately delivered static analysis and a controlled headless-engine plane while
leaving the editor and live-runtime implementations absent. That was an honest first-release
boundary, but it cannot satisfy the new product objective: an MCP client must be able to create,
edit, run, observe, test and export a Godot game without an out-of-band authoring step.

The codebase already contains a useful bridge envelope, authenticated loopback rendezvous,
three-state mutation semantics, process supervision, durable recovery and type-shaped domain
values. The missing work is therefore a production Godot-side endpoint and complete authoring and
runtime services, not a new MCP transport or a new persistence mechanism.

A second decision is required because the main Godot 4 Rust binding,
[godot-rust/gdext](https://github.com/godot-rust/gdext), is mature enough to be a credible
implementation option. As observed on 2026-10-08, the repository had 5,257 stars, 318 forks,
about 128 contributors, active commits during October 2026 and six releases from 0.5.0 through
[0.5.5](https://github.com/godot-rust/gdext/releases/tag/v0.5.5) between March and August 2026.
The `godot` crate had about 511,000 total downloads. It is active rather than abandoned, but its
own README still describes occasional breaking changes, and the editor API surface needed here
uses the explicitly experimental Godot API feature.

## Decision

### Restore the editor and runtime planes

The editor and runtime planes are required post-0.1 capabilities. The implementation will ship a
real Godot add-on, main-thread dispatcher, typed reflection catalogue, undoable mutations,
runtime inspection, synthetic input and capture services. The historical cancellation remains a
fact about version 0.1 only.

The active implementation plan is
[Complete Godot development implementation plan](../complete-game-development.md). New work gets
new tests and qualification evidence; cancelled first-release task descriptors and historical
receipts are not relabelled as passes.

### Keep the standalone Rust MCP server

The Rust process remains the owner of MCP transport, policy, scheduling, project authority,
idempotency, durable transaction state, process supervision and artifact authorization. No
Godot-side implementation receives MCP or authorization-server responsibilities.

The Godot-side bridge is an execution adapter behind the Rust policy boundary. It validates the
method payload again, executes editor work only from the Godot main thread and reports one of the
existing mutation states. It cannot grant itself a capability or settle its own Rust transaction.

### Use pure GDScript as the default bridge

The default package ships a pure-GDScript EditorPlugin and runtime probe because this control
plane is dominated by metadata and editor calls rather than CPU-heavy game logic. This choice
provides one portable add-on for all supported desktop hosts, avoids a native library inside the
editor and permits runtime capability shims for each exact Godot patch version.

The add-on speaks the existing bridge envelope and message kinds. New methods and typed payload
schemas are capability-negotiated inside that envelope. If an envelope field or replay rule must
change, the bridge protocol major version changes; the implementation does not silently relax
unknown-field rejection.

### Allow gdext only as an optional backend

If measurement or an unavailable GDScript API establishes a concrete need, an optional native
backend may directly use the upstream `godot` crate. The project will not implement its own
GDExtension FFI, API generator, Variant ABI or object-lifetime binding.

The optional backend has these boundaries:

- It lives in a separate workspace/package and never becomes a dependency of the standalone
  server crates.
- It is disabled by default and distributed as separate per-platform add-on artifacts.
- It implements the same bridge method schemas and conformance tests as the GDScript backend.
- It uses an exact dependency pin. The first implementation spike evaluates `godot = "=0.5.5"`.
- Stable calls target the lowest required API level. Any use of the upstream experimental Godot
  API feature requires live-load tests against Godot 4.4.1, 4.5.1, 4.6.1 and 4.7.1 rather than an
  assumption of cross-minor compatibility.
- Native signing, notarization, crash recovery, license notices and package provenance are
  release gates for the optional artifact, not inherited from the static server package.

MPL-2.0 is accepted for this isolated optional dependency. It permits commercial closed-source
use and requires source availability for modifications to MPL-covered files, not for unrelated
project files. License obligations are recorded explicitly, but the license alone is not a reason
to reimplement the binding.

### Discovery reports executable reality

Post-0.1 profiles remain authorization ceilings, but MCP tool discovery returns only methods the
running build implements. A method that is implemented but temporarily unreachable may stay in
discovery with an explicit availability reason. A planned method with no handler is not advertised
as an executable tool.

This is a domain-contract change and therefore requires a new contract version. It does not alter
historical version 0.1 manifests or receipts.

## Why not make gdext the default

A gdext bridge would still need the standalone MCP server, bridge authentication, durable journal
and process ownership. Making it mandatory would add one native dynamic library for every target,
editor-process native failure modes and a signing/notarization matrix without removing those Rust
services.

The principal authoring operations also use editor-only APIs whose Rust exposure is explicitly
experimental. Pure GDScript does not make those Godot APIs stable, but it makes exact-version
adaptation a small runtime module rather than a separately compiled native artifact for each
incompatible surface.

## Why not stay headless-only

Headless canonical re-save is useful for import, validation and deterministic resource output, but
it cannot replace live selection, editor dirty-buffer awareness, EditorUndoRedoManager actions,
viewport capture or interaction with a running game. Complete authoring needs both batch and live
backends behind one semantic command model.

## Consequences

- The version 0.1 statement that no EditorPlugin ships remains correct for version 0.1 artifacts.
- Post-0.1 packaging adds a default GDScript add-on and exact-version editor/runtime integration
  tests.
- The current release-graph audit and qualification checks that require editor/runtime absence
  must be versioned or replaced before implementation can qualify.
- The current ban on gdext remains valid for the default server and GDScript package. A future
  native package gets a narrow package-specific license and dependency policy instead of weakening
  the default graph globally.
- The bridge protocol, Rust policy decisions and public tool schemas remain backend-neutral.
- Performance does not justify a native backend without a reproducible benchmark against the
  GDScript implementation.

## Native-backend activation criteria

The gdext backend may move beyond an isolated spike only when at least one condition is proven:

1. A required operation cannot be expressed through a stable-enough GDScript/EditorPlugin API on
   the exact supported matrix.
2. A representative 10,000-node or equivalent property snapshot misses its agreed latency or
   memory budget after GDScript profiling and bounded batching.
3. Image, recording or bulk Variant conversion consumes enough editor frame time to violate the
   runtime responsiveness gate.

The comparison records identical workloads, Godot binaries, payload limits and output digests.
Backend preference, language preference and star count are not activation evidence.

## Spike outcome (2026-10)

The isolated spike lives in `gdext-reference/`, its own Cargo workspace outside the
repository's: `godot = "=0.5.5"` with its `api-4-4` feature, the lowest API level the
matrix needs, no experimental Godot API, and no
committed `.gdextension` descriptor — its driver writes one into each scratch project. It
implements one bridge read natively, the runtime probe's scene-tree read, and describes
every node exactly as `runtime_probe.gd` does.

Measured on one Linux x86_64 host with 32 CPUs, describing a 10,000-node scene ten times
per implementation, headless; milliseconds as minimum / median / maximum. The three
snapshots were byte-identical on every build, and identical across builds.

| Godot | GDScript walk as it shipped (`pop_front`) | GDScript walk now shipped (cursor) | gdext |
| --- | --- | --- | --- |
| 4.4.1 | 68 / 69 / 70 | 34 / 35 / 42 | 56 / 58 / 64 |
| 4.5.1 | 66 / 68 / 71 | 30 / 30 / 37 | 52 / 53 / 61 |
| 4.6.1 | 344 / 709 / 1085 | 27 / 27 / 36 | 50 / 50 / 58 |
| 4.7.1 | 697 / 705 / 855 | 39 / 79 / 86 | 62 / 67 / 119 |

Holding one snapshot took about 11 MB of engine static memory from GDScript and about
14.5 MB from gdext. Encoding a 320×240 capture as PNG and base64 took 1.7–4.3 ms; those are
engine calls from either language.

Profiling found the slow case: the probe's breadth-first walk used `Array.pop_front()`,
which shifts the whole pending array on every step, so the walk was quadratic in the node
count and became an order of magnitude slower on 4.6.1 and later. The shipped walks — the
probe's tree and UI reads and the add-on's scene snapshot — now advance a cursor instead.

Live load: the extension loaded and ran in every build when the project ran headless. With
the extension in the project, the 4.5.1, 4.6.1 and 4.7.1 **editors** abort (SIGABRT) during
an import and when an editor is opened and quit, with an `api-4-4` build and with an
`api-4-7` build alike, reloadable or not; the 4.4.1 editor does not. The same project without the
extension imports cleanly on every build.

Against the activation criteria:

1. Every required operation is expressed through GDScript on the whole matrix; none needs
   the native backend.
2. After profiling, the GDScript walk is faster than the native one on every build and uses
   less memory; there is no latency or memory budget it misses.
3. Capture encoding is engine work either way; the native backend removes no frame time.

None is met, and the native backend adds an editor-process failure mode on three of the
four supported builds. **The native backend stays absent.** The default package is
unchanged and does not need it; the spike stays as the reference lane, built only by the
manual `gdext-reference-lane` workflow.

## Invalidation conditions

Revisit this decision if Godot publishes a stable out-of-process editor-control API, if the
GDScript add-on cannot satisfy the acceptance matrix after measured optimization, if gdext reaches
a materially stronger stable editor API, or if maintaining two conforming bridge backends costs
more than the demonstrated native benefit.
