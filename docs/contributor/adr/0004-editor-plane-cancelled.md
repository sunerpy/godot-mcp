# 0004 — The editor plane was cancelled for the first release

**Status:** superseded for post-0.1 work by [ADR 0010](0010-complete-development-bridge-strategy.md). The work was **cancelled, not completed**, for the first release.

## Decision

The Godot editor plug-in and the editor plane it would serve are not part of the first
release. The frozen bridge protocol and its GDScript codec **are** shipped.

## What "editor" means here

The Godot **editor process**, not your IDE and not your MCP client. Clients always connect
to the standalone `godot-mcp` process over stdio; which editor a developer uses has never
been relevant. So "not tied to the editor" was never a claim about IDEs.

## The three planes

| Plane | Needs the Godot editor running | Needs a plug-in installed | What it does |
| --- | --- | --- | --- |
| Static analysis | No | No | Script, scene and resource parsing; the semantic graph; references, callers and impact; dangling-reference auditing; incremental indexing |
| Controlled engine | No | No | Headless validation, import and test execution; canonical text-resource writes; resource-identifier operations; language- and debug-server adapters; disposable execution |
| Editor bridge | Yes | Yes | Live selection, undoable editing through the editor's own undo system, viewport capture, runtime inspection and synthetic input |

The first two are structurally headless. The kernel code paths that answer them contain no
process spawn at all, which is checked rather than asserted.

## Why it was cancelled

**The prerequisites do not hold for the target users.** The editor plane needs the editor
running *and* a plug-in installed. Users of this server do not necessarily have the Godot
editor installed, and those who do do not necessarily want to keep it open while an
assistant works.

**The cost was six substantial pieces of work** — a main-thread dispatcher, a type
catalogue, editor state and selection reads, undoable mutations, capture and recording,
and runtime inspection with synthetic input — each needing a test pass against every exact
version in the compatibility matrix. That is a large investment in a plane most deployments
cannot reach.

**Half-delivering it would be worse than not delivering it.** A plane that appears
available and fails in unclear ways is more expensive for a user than one that says
plainly it is unavailable.

## What was kept, and why

The bridge protocol is finished and self-contained: a versioned envelope with exactly ten
message kinds, a self-implemented WebSocket codec, rendezvous authentication over a
private owner-only file with a constant-time comparison, and three-state mutation semantics
with explicit replay rules. Keeping it costs one small GDScript asset and a schema, and
throwing it away would mean redoing finished design work if the plane ever returns.

Its mutation-status semantics are also referenced by later work, so the definitions
continue to earn their place.

## Consequences, handled honestly

* **The profile algebra is not rewritten.** `full` remains `core` ∪ `editor` ∪ `runtime`,
  computed from the canonical manifests. Editing the manifests to hide the missing plane
  would make the contract depend on the deployment, which is the opposite of a contract.
* **Editor-plane tools answer `unavailable`.** They stay in `tools/list` with an honest
  runtime outcome. See
  [Profiles and tool outcomes](../../operator/profiles-and-outcomes.md).
* **Undo semantics are documented as designed, not as available.** Recorded so the intent
  survives; explicitly not presented as behaviour you can use. See
  [Editor bridge](../../operator/editor-bridge.md).
* **Downstream scope shrank rather than being faked.** Work that depended on the editor
  plane — cross-plane scheduling, the outcome router, capture artifacts in the HTTP
  transport — was reduced to the static and engine planes, with the editor branch
  implemented as an honest `unavailable` rather than a pretence.

None of the cancelled work is represented as complete anywhere: no evidence, no test pass
and no documentation claims it.

## What a return would require

The plug-in, a main-thread dispatcher, and a test pass against each exact version in the
[compatibility matrix](../../operator/version-matrix.md). The protocol it would speak is
already frozen, so it is an implementation task rather than a redesign.
