# Artifacts

An operation whose result is a block of bytes rather than a value returns it as an MCP
resource with an **opaque identifier**, and only that way. This page describes that
contract.

Captures of a managed game session or of the connected editor, and the outputs of a
managed export, are the artifacts this build produces: a screenshot is one PNG, a
recording is one animated PNG and an export output is one file of the exported build (see
the section at the end, [Managed sessions](managed-sessions.md),
[Editor bridge](editor-bridge.md#captures) and [Exports](exports.md)).

## The identifier is the whole address

An artifact is read through `godot-mcp://artifacts/{artifact_id}`. The identifier is
opaque: it is not a file name, it does not encode a location, and it is not
concatenated with anything to form a path. There is no tool, resource or CLI flag
that returns where the bytes live on disk.

That is a deliberate boundary. A returned path is a capability: whoever holds it can
usually read it again later, as anyone, without passing any of the checks the
resource read applies. Keeping the address opaque means every read goes
through those checks.

## Every read is bound

A read succeeds only when all of these still hold:

* the artifact belongs to the **project generation** it was produced under. A new
  generation invalidates it rather than serving stale bytes under a live name;
* the requesting **principal** is the one the artifact is bound to, in whichever mode
  the transport is using;
* the artifact is within its **retention window**;
* the requested range is within the **size and range limits**.

An artifact is bound to the principal that asked for it and to the project, not to one
MCP session. Over stdio the transport has one local principal, so a restarted stdio server
still serves an artifact to the same client until its retention ends. Over authenticated
HTTP every session opened with the same credential is the same principal: a client that
reconnects between a capture and its read, as one does after the session idle timeout,
still reads it. In `trusted_local` the principal comes from the launch secret, so a client
of another launch is another principal and reads nothing the previous launch produced. In
`proxied_oidc` the principal is the configured `(iss, sub)`, which outlives a restart, so
after a restart that principal still reads what it produced, within the retention window,
as over stdio.

A failure here is a typed outcome, never a partial read. Over stdio it is `not_found` for
an identifier that no longer resolves and `permission_denied` for a binding mismatch.
Over HTTP every refusal is the same `not_found`, so a refusal does not reveal whether the
artifact exists.

`resources/list` names only the artifacts the caller could read: the ones bound to its
principal, and none that has expired.

## Transport parity

An artifact read behaves the same over stdio and over authenticated HTTP: same
identifier, same range semantics, same limits, same chunking. A range the contract admits
returns the same bytes over both, and a range past the end is refused over both. What
differs is only the binding described above. The acceptance suite exports through the
same server over each transport and compares the reads.

## Retention

Artifacts are transient by design. Read what you need within the retention window; do
not treat an identifier as a durable handle to store and reuse later. When retention expires the identifier stops resolving, which
is the intended behaviour rather than a bug to work around.

An expired artifact gives its bytes and its share of the store's budget back: the next
artifact registered removes everything that expired by then, and a server restart removes
it from disk. Artifacts a restart keeps hold their share of the budget again. Recovery
evidence — what an export a killed server left behind had written — is not an artifact:
it is never served and retention does not remove it (see [Exports](exports.md)).

## What produces an artifact

`runtime.screenshot.capture` and `runtime.recording.capture` register their capture in
the server's artifact store and answer its opaque URI with the media type, byte length,
frame count, pixel size and SHA-256. The bytes are kept under the server's private state
directory, never in the project, and are checked against that length and SHA-256 before
the artifact becomes readable; nothing about a capture is committed until it is.

* The artifact is bound to the project generation and to the principal that asked for
  the capture.
* It stays readable for 15 minutes; a server restart keeps the artifacts still inside
  that window whose bytes still match their SHA-256, and removes the rest.
* The store holds at most 256 MiB of artifacts at once. A capture that does not fit is
  refused, never truncated.
* One read returns at most `limits.max_output_bytes`; read a larger artifact in byte
  ranges. A `resources/read` whose `_meta` holds
  `{"io.godot-mcp/artifactRange": {"start": 0, "endInclusive": 1023}}` returns bytes 0
  to 1023, both included. A range that ends past the artifact's last byte, or is longer
  than `limits.max_output_bytes`, is refused with `invalid_input`.

`editor.screenshot.capture` registers what the connected editor shows the same way: the
server reads the PNG back from the editor in bounded chunks and checks it against the
length and SHA-256 the editor reported before the artifact becomes readable.

`runtime.image.compare` registers its capture, and its difference image when pixels differ,
the same way. `test.scenario.run` registers every screenshot and comparison its steps take,
and its report as `application/json`; when the report cannot be kept, the answer still
carries it, with `artifact: null` and the reason in `artifact_error`.

`project.export` registers every output file of a successful export the same way, as
`application/octet-stream` with its size and SHA-256, once the export process has ended;
`project.export.status` lists them. An export's outputs register together or not at all,
and an export larger than the store's 256 MiB is failed rather than truncated.

A capture needs a windowed game session or an editor with a window: a headless one
answers `unavailable` rather than an empty or synthesized image. What a capture shows is whatever the engine
drew into its root viewport; it promises nothing about fidelity beyond that, and frames
from different Godot builds can differ.

See [Profiles and tool outcomes](profiles-and-outcomes.md) for how availability is
reported, and the [threat model](../security/threat-model.md) for why the identifier
is opaque.
