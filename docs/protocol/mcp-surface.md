# MCP surface

The public surface is Model Context Protocol: tools, resources, and typed outcomes. The
same kernel serves every transport, so a tool answers identically wherever the request
came from.

## Transports

| Transport | Default | Reachability |
| --- | --- | --- |
| stdio | Yes | The launching process only |
| HTTP | No | Loopback, authenticated, optionally behind a same-host proxy |

Under stdio, framing is newline-delimited JSON-RPC on stdin and stdout. **stdout carries
protocol frames and nothing else**: diagnostics can only be routed to `stderr` or to a
bounded rolling file, so a log line cannot corrupt a frame.

Under HTTP the MCP path is `http-protocol:/mcp`, the listener binds loopback, and every
request is authenticated. See [HTTP: `trusted_local`](../operator/http-trusted-local.md)
and [HTTP: `proxied_oidc`](../operator/http-proxied-oidc.md).

## Discovery

`initialize` is answered by the shared kernel and identifies the server. After
initialization:

* `tools/list` returns exactly the selected profile's tools;
* `resources/list` returns exactly the selected profile's resources.

Both reflect **membership**, not current reachability. A tool that belongs to the profile
is listed even when the plane behind it cannot be reached, and a call then returns
`unavailable`. Hiding the tool instead would make the contract depend on the environment,
which is the property clients most need to be able to rely on.

The canonical, generated lists are in
[Profiles, tools and resources](tool-inventory.md).

## Requests are validated strictly

Every request shape is schema-validated with unknown fields rejected. An unrecognized
field is a rejection, not something ignored — a client that mis-spells a parameter must
learn that immediately rather than get a silently different operation.

Two consequences worth designing for:

* a request naming a tool outside the profile is refused the same way an unknown tool is;
* an operation that would exceed a configured limit — body bytes, output bytes, queue
  depth, deadline — is refused or reported as `timeout` rather than truncated silently.

## Outcomes

An outcome is typed. The generated vocabulary lives in
[Outcome taxonomy](error-taxonomy.md); the parts a client must handle are:

* one of ten error codes on failure, never free-form text as the only signal;
* provenance and a confidence level on a static answer, so `exact`, `inferred` and
  `unresolved` are distinguishable;
* `uncertain` when a sound answer could not be produced — the analyser declining to
  guess, not an internal error.

## Resources

Four resources are always present:

* `godot-mcp://resources/capabilities`
* `godot-mcp://resources/health`
* `godot-mcp://resources/project`
* `godot-mcp://resources/scheduler`

Profiles that can produce binary results add `godot-mcp://artifacts/{artifact_id}`, whose
identifier is opaque and whose every read is bound to project generation, principal,
session, retention and size limits. See [Artifacts](../operator/artifacts.md).

## Progress and cancellation

A long operation reports progress. Cancellation resolves through the service's own
quitting path, so an in-flight request observes cancellation rather than being dropped
mid-effect — which matters because an effect dropped mid-write is exactly the situation
the recovery contract exists to avoid. See [Recovery](../operator/recovery.md).

## Versioning

Two version numbers are reported by `godot-mcp version`: the CLI's own version and the
**server contract** version. The contract version is what changes when the public tool or
resource surface changes shape. See [Schema evolution](schema-evolution.md).
