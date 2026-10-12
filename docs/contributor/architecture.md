# Architecture

Six shipping crates in one workspace, layered strictly. Dependencies flow upward toward the
server; nothing flows back down.

```console
godot-mcp-cli        command line, transports, operator repair
godot-mcp-server     MCP kernel, policy, scheduling, HTTP, diagnostics
godot-mcp-bridge     editor-bridge protocol and codec
godot-mcp-engine     controlled Godot execution, supervision, containment
godot-mcp-analysis   static GDScript / scene / resource analysis
godot-mcp-domain     contracts, error taxonomy, config, leases, filesystem authority
```

A seventh member, `qualification-tools`, is build- and test-only and is excluded from the
shipping set.

## The boundary that is enforced

`godot-mcp-domain`, `godot-mcp-analysis` and `godot-mcp-engine` must never import
`godot-mcp-server` or `godot-mcp-cli`. This is checked by a test that reads the manifests,
so a forbidden edge fails a gate rather than being caught in review.

The reason is not tidiness. The lower layers must be usable, and testable, without any
knowledge of stdio, HTTP, MCP wiring or CLI presentation — and a single upward edge would
make the whole graph cyclic in practice.

## What each crate owns

### `godot-mcp-domain`

The versioned domain contracts, the ten-code error taxonomy, the provenance and confidence
model, typed Godot value shapes, the capability-profile manifests, the strict layered
configuration loader, and the filesystem authority layer.

That last one is worth knowing about before you touch anything near it: `sysfs` is the sole
shared internal filesystem contract, with per-platform backends. Authority comes from held
descriptors, never from path text. Object identity is the platform's stable handle
identity. Its private Windows module is one of the few places `unsafe` is permitted, and
every FFI block there carries its own local safety argument.

### `godot-mcp-analysis`

Static analysis: GDScript parsing, scene and resource parsing, the semantic graph,
references, callers, impact, dangling-reference auditing and incremental indexing.

Structurally headless. It cannot start a process.

### `godot-mcp-engine`

Controlled Godot execution: validation, import, test execution, canonical text-resource
writes, resource-identifier operations, LSP and DAP adapters, disposable execution, launch
supervision and containment boundaries.

Also structurally headless in the sense that matters: it needs no editor session and no
plug-in.

Unsafe code is denied here as well, except in three private platform modules with no safe
binding: the Windows Job Object backend, the Windows process observations (the process,
ToolHelp and IP helper APIs) and the macOS process observations (`sysctl` and `libproc`).
Every call there carries its own safety argument, and every macOS kernel structure is
asserted at compile time to have the SDK's size and field offsets.

### `godot-mcp-bridge`

The frozen editor-bridge envelope, its WebSocket codec, rendezvous authentication and the
mutation status semantics. Self-contained: it ships as a protocol asset even though this
release has no plug-in speaking it. See
[Editor bridge protocol](../protocol/bridge-protocol.md).

### `godot-mcp-server`

The shared MCP kernel: tool and resource registries, the capability-profile policy layer,
request validation, the durable transaction journal and scheduler, the effect gateway,
diagnostics, and the optional HTTP transport with its authentication modes.

The kernel's core is deliberately free of process spawning: the static and project planes
answer without any ability to launch Godot, which is checked rather than assumed.

### `godot-mcp-cli`

The `godot-mcp` binary — project discovery, configuration validation, stdio serving, asset
installation, package self-check and the local operator-repair command — plus the
non-shipping audit, packaging, documentation and fixture tools.

Every tool that must never reach a user's machine is gated behind the `test-hooks` feature,
which a release build does not enable. That is a construction, not a policy: a packaging or
documentation tool cannot end up inside the artifact it produces.

## Effects go through one gate

A durable effect is not performed by whoever wants it. It goes through a permit-typed
effect gateway, and the permit type is private to the layer that owns the transaction
contract. That is why a new mutation path cannot accidentally skip journalling: without a
permit, it does not compile.

The journal has exactly one implementation, one schema, and one set of fsync phases. A
second persistence mechanism is not an option — recovery reasoning depends on there being
one place that knows what happened.

## Three effect kinds, and no fourth

`file`, `process_launch`, `editor_runtime`. A composite operation composes existing proof
forms rather than inventing a fourth; a canonical text-resource write is a `file` effect
whose proof also requires the linked launch's clean terminal result. See
[Recovery](../operator/recovery.md).

## The operator-repair command is reachable from one place

`godot-mcp transaction repair` is a `clap` subcommand and nothing else. It has exactly one
call site in the whole workspace, is registered in no tool or resource registry, and has no
capability mapping — so no MCP, HTTP or network path reaches it. That property is worth
preserving deliberately if you touch the command surface.

## Diagnostics cannot reach stdout

The diagnostic sink type has exactly two variants, `stderr` and a bounded rolling file.
There is no stdout variant to select, so protocol framing on stdout is safe by
construction rather than by care. A configured file path that resolves to a known stdout
alias is rejected as well.

## Adding a capability

1. Add or extend the contract in `godot-mcp-domain`, including its schema.
2. Implement it in the layer that owns the plane.
3. Expose it through the kernel with its capability and profile mapping.
4. If it produces a durable effect, route it through the effect gateway — do not add a
   second write path.
5. Update the documentation, and regenerate the generated pages.
6. Run the gates in [Workflow](workflow.md).

## Approved post-0.1 direction

This page describes the shipping version 0.1 architecture. The approved implementation direction
for a complete authoring/runtime/export loop is the
[Complete Godot development implementation plan](complete-game-development.md), governed by
[ADR 0010](adr/0010-complete-development-bridge-strategy.md). Current operator behavior does not
change until each milestone ships with its contract and qualification evidence.
