# Editor bridge protocol

The wire contract between the Rust server and a GDScript editor plug-in is frozen and
ships in the archive as `schemas/bridge/protocol-v1.json`, alongside its GDScript codec.

The `godot_mcp` editor add-on speaks it, and so does the runtime probe of a managed game
session, each over its own authenticated loopback connection. The protocol did not change
when they shipped: everything they added travels inside the envelope as request methods
and payloads. See [Editor bridge](../operator/editor-bridge.md) and
[Managed sessions](../operator/managed-sessions.md).

## Framing

One message per WebSocket text frame, in both directions. The codec is
self-implemented rather than taken from a dependency, so the frame handling on both
sides of the bridge is exactly the frozen subset the protocol needs.

Control frames remain WebSocket-layer messages rather than bridge envelopes. A received
Ping is answered immediately with a Pong carrying the identical application data unless
a Close was received first. Received Pong frames are consumed without a reply. A valid
received Close is answered once with a Close carrying the identical payload; the session
then refuses every later envelope and Pong write. The endpoint does not originate
scheduled Ping frames; bridge liveness uses the explicit `bridge:heartbeat` envelope.

### RFC 6455 receiver obligations

The receiver does not claim a general-purpose WebSocket implementation. Its fixed peer
is Godot 4.7.1 `WebSocketPeer`, and the table below records every RFC 6455 frame-receiver
obligation as enforced, contractually excluded, or not applicable.

| Receiver obligation | Status | Bridge behavior |
| --- | --- | --- |
| Client-to-server masking (§5.1, §5.3) | Enforced | Every client frame must be masked; server frames must be unmasked. |
| RSV bits and negotiated extensions (§5.2, §5.8) | Enforced / not applicable | Any RSV bit is refused. The bridge negotiates no extension, so extension-defined frame interpretation does not apply. |
| Known opcodes (§5.2, §5.6) | Enforced | Text, Close, Ping, and Pong are accepted. Binary and every other non-contract opcode are refused. |
| Canonical payload lengths (§5.2) | Enforced | Non-minimal extended lengths and the reserved high bit of a 64-bit length are refused. A declared payload over 4 MiB is refused from the header before payload allocation. |
| Fragmented-message reassembly (§5.4) | Contractually excluded | FIN-clear frames and continuation opcodes are refused. Each bridge JSON document is exactly one `send_text` message from the sole peer described below. |
| Control frames during a fragmented message (§5.4) | Not applicable | No fragmented message enters a reassembly state; standalone control frames are still handled immediately. |
| Text UTF-8 (§5.6, §8.1) | Enforced | A text-frame payload that is not valid UTF-8 is refused before envelope parsing. |
| Control-frame finality and 125-byte limit (§5.5) | Enforced | FIN-clear control frames and control payloads over 125 bytes are refused. |
| Close payload and closing handshake (§5.5.1, §7.4) | Enforced | Empty payloads are accepted; otherwise the two-byte status code and UTF-8 reason are validated. A valid received Close is answered once with the identical payload, then no later data or Pong frame is sent on that session. |
| Ping and Pong (§5.5.2–§5.5.3) | Enforced | Ping receives an immediate Pong with identical application data unless Close arrived first; Pong is consumed without reply. Scheduled server Ping is optional and is not used because `bridge:heartbeat` supplies liveness. |

The fragmentation exclusion is an explicit peer-contract boundary, not an inference from
small test messages. In the pinned Godot 4.7.1 native implementation,
[`WebSocketPeer::send_text`](https://github.com/godotengine/godot/blob/4.7.1-stable/modules/websocket/websocket_peer.cpp)
routes through
[`WSLPeer::_send`](https://github.com/godotengine/godot/blob/4.7.1-stable/modules/websocket/wsl_peer.cpp)
to `wslay_event_queue_msg`. Godot's bundled
[wslay header](https://github.com/godotengine/godot/blob/4.7.1-stable/thirdparty/wslay/wslay/wslay.h)
documents that call as sending a message without fragmentation and exposes the distinct
`wslay_event_queue_fragmented_msg` API for fragmented sending. A loopback wire probe
against the exact locked Godot 4.7.1 binary also sends a 1 MiB `send_text` payload as one
`FIN=1`, text-opcode frame. Reassembly would therefore serve a non-contract peer while
changing the 4 MiB allocation limit from a header-only frame bound into an accumulated
message bound; this release refuses that shape instead.

## Rendezvous lifecycle

The server creates a new private rendezvous file for a path that does not exist and
removes its file during orderly shutdown. On platforms that provide atomic
directory-entry exchange, restart and explicit rotation safely replace an existing file
with a fresh per-launch secret.

Windows currently cannot perform that replacement while preserving the CAS safety
property: its atomic replacing rename could overwrite an actor that wins after the final
observation, and it has no atomic directory-entry exchange. On Windows, rotation and
startup over a crash-left rendezvous therefore refuse with
`WindowsExistingFileReplacementUnsupported` and leave the existing file unchanged.
The server's editor bridge does not hit this at startup: it holds the project's
exclusive lease while it starts, which proves that a rendezvous already on disk was left
by a server that stopped without its cleanup, so it removes that file and then creates
its own. The implementation does not fall back to an unsafe replacing rename.

## The envelope

Every message is the same JSON object with the same key set. **Every field is required
and present on every message**; a field that does not apply to a message kind is
explicitly `null` rather than omitted. That is what makes the canonical rendering of any
message have one byte layout on both sides, which in turn is what lets the two
implementations agree without negotiating.

Two version fields, deliberately distinct:

* `envelope:schema_version` — the shape of the envelope itself.
* `envelope:protocol_version` — the sender's bridge protocol version. Its major
  component decides compatibility.

## Canonical integer handling

The envelope has five unsigned-integer JSON paths and no floating-point envelope
field:

| JSON path | Message kinds |
| --- | --- |
| project → generation | `bridge:hello`, `bridge:capabilities` |
| `deadline_unix_ms` | `bridge:request` |
| progress → completed | `bridge:progress` |
| progress → total | `bridge:progress` |
| `sent_unix_ms` | `bridge:heartbeat` |

Godot's JSON parser represents every parsed JSON number as `TYPE_FLOAT`, and
GDScript's `TYPE_INT` is signed 64-bit. Neither type can represent the complete u64
range, so the codec does not attempt numeric restoration. During decode it locates
these five paths directly in the original JSON text, validates each token as a
canonical unsigned decimal no greater than `18446744073709551615`, and stores the
exact digits as a `String` in the decoded `Dictionary` before the parser's rounded
float can escape. During encode, only those same paths may turn a validated decimal
`String` back into an unquoted JSON number token. A future field named `generation`,
`completed`, or `total` elsewhere is not coerced.

This text-level rule covers every protocol-allowed u64, including values above
2^53 and `u64::MAX`, without converting the received value through any Godot numeric
type. Negative, fractional, out-of-range and duplicate integer-path values are
malformed and refused rather than rounded, truncated or sign-flipped. Canonical
decode-then-encode therefore preserves the original integer bytes, while envelopes
built inside GDScript may still supply an in-range nonnegative `TYPE_INT` at the same
paths.

## The ten message kinds

There are exactly ten and an eleventh is not representable:

| Kind | Purpose |
| --- | --- |
| `bridge:hello` | Opens a session and carries the authenticator |
| `bridge:capabilities` | Declares what the peer can do |
| `bridge:request` | Carries work |
| `bridge:result` | Successful completion |
| `bridge:error` | Failed completion |
| `bridge:progress` | Partial completion of work still running |
| `bridge:cancel` | Withdraws work that has not started |
| `bridge:heartbeat` | Proves liveness |
| `bridge:event` | Unsolicited notification |
| `bridge:shutdown` | Ends the session |

`envelope:origin` is `server` or `plugin`, so a message's sender is explicit rather than
inferred from the direction it arrived on.

## Work classification

`envelope:operation_class` is `read`, `mutation` or `control`, and `null` on a message
that carries no work. `read` never changes editor state. `mutation` does, and therefore
both a successful result and a failed mutation error must carry a mutation status.
On other errors the operation class and mutation status are both `null`. `control` is
protocol bookkeeping.

## Mutation status and replay

`envelope:mutation_status` is the field that makes retries safe:

| Status | Meaning | Replayable |
| --- | --- | --- |
| `mutation:not_started` | Nothing was applied | Yes |
| `mutation:committed` | The change was applied | No — it already happened |
| `mutation:unresolved` | The outcome could not be determined | **No** |

Only `mutation:not_started` is replayable. `mutation:unresolved` must be resolved, never
retried: replaying it risks applying the mutation a second time, and a second
application is exactly the failure a naive retry loop would cause.

The pairing is fail-closed: `bridge:error` with `operation_class: "mutation"` must carry
one of the three non-null statuses, and a non-null status is forbidden on every error
whose operation class is `null`. This lets a failed mutation distinguish a safe retry
from a committed or unresolved outcome without changing the fixed envelope key set.

## Authentication

`envelope:auth` is non-null only on `bridge:hello`. It carries the hex spelling of the
256-bit per-launch secret the server wrote to a private, owner-only rendezvous file, plus
a nonce that is single-use within a launch.

Three properties matter:

* the secret comes from the operating system's cryptographic random source, never from a
  hand-rolled generator;
* the rendezvous file is created owner-read-write only;
* the comparison is constant-time, so a mismatch leaks no timing information about how
  much of the secret was correct.

The bridge endpoint is loopback-only. Being able to open it is not authentication; the
handshake is. See the [threat model](../security/threat-model.md).

## Methods and liveness

A `bridge:request` names its method inside the envelope. When a session opens, the server
asks the peer to describe itself, and the peer answers with its version, its Godot version
and the exact list of methods it implements. The server sends no method the peer did not
list: a tool whose method an older add-on lacks answers `unsupported` before anything is
sent, naming the add-on's version. Adding a method is therefore a compatible change that
needs no new protocol version; an add-on and a server of different releases agree on the
methods both have.

An editor add-on's description also names its process ID (`pid`) and the executable it
runs (`executable`). The server keeps that process's identity — its ID and start time —
only when the operating system confirms both, and records it with any request the editor
then leaves unanswered, so that an operator can settle that request once the process has
ended ([Recovery](../operator/recovery.md#an-editor-that-ended-before-it-answered)). An
add-on that names no executable is served exactly as before, without that identity.

The server sends `bridge:heartbeat` every 5 seconds and closes a session that stays silent
for 30. A peer that is running a request whose `deadline_unix_ms` has not passed is not
silent: an editor runs one request at a time on its main thread and answers no heartbeat
until it is done, so a long scan is not mistaken for a lost editor. Once the deadline
passes without an answer the request is unresolved, and the silence rule applies again.

The server's own status request, method bridge.server_status
([The server's status](#the-servers-status)), is such a request too, and one not answered
within its 30 seconds is sent again while the add-on has not taken the latest
status, so a silent session stays open while that status is outstanding. A quarantine changes
the status. An editor that holds its main thread past a mutation's deadline therefore keeps its
session when the changed status was sent before the silence reached 30 seconds, and the answer
comes on that session; otherwise the session closes, and the answer comes in a report to the
next one ([Late mutation answers](#late-mutation-answers)).

## Late mutation answers

A server stops waiting for a mutation at the request's `deadline_unix_ms` plus a 2-second
grace, or when the session that carries it ends. The editor still runs a request it has
started, because GDScript cannot be stopped part way, so its answer can come after the
server gave up. That answer is the receipt the server's journal lacks, and the server binds
it to its request in one of two ways:

* a `bridge:result` or `bridge:error` that arrives on the same session after the server
  stopped waiting, correlated to the request;
* a `bridge:event` named mutation.late_answer, one for each answer the add-on kept: sent
  at once when a session is already described, and again to every session once it is
  described.

The server keeps each mutation request it stopped waiting for whose payload names the
server's `transaction_id`, at most 64, the oldest dropped first. The add-on keeps the answer
to a mutation it could not deliver, or delivered after the request's deadline, and records a
queued mutation dropped with a lost connection as `not_started`. It keeps only answers that
say what happened, `committed` or `not_started`, for a request whose payload names a
transaction: at most 16, the oldest dropped first.

The event's payload holds two objects. `request` names the request:

| Field | Value |
| --- | --- |
| `session_id` | The server session the request came on. |
| `message_id` | The request's message ID. |
| `idempotency_id` | The request's idempotency ID. |
| `method` | The request's method. |
| `transaction_id` | The `transaction_id` the request's payload named. |

`answer` says what happened:

| Field | Value |
| --- | --- |
| `mutation_status` | `committed` or `not_started`. |
| `ok` | Whether the request succeeded. |
| `action`, `digest` | On success, what the add-on recorded and its digest of the state it left, when it said; at most 256 characters each. |
| `error_kind`, `message` | On failure, the error category and its message, at most 2,048 characters. |

A report that names this server's session must match a kept request exactly, by message ID,
idempotency ID, method and transaction ID, and binds once: a repeated report, or one naming a
request this server never sent, is dropped. A report that names an earlier server's session is
handed on unverified, and only the journal can bind it, by transaction and idempotency ID. The
add-on sends its reports to every later session too; once the transaction is settled a report
changes nothing. No report makes the server send a request again. How the journal settles a
bound answer is in [Recovery](../operator/recovery.md#late-editor-answers).

An add-on older than 0.5.0 sends no report, so only an answer on the same session settles a
transaction there; a server that does not know the event ignores it, as it ignores every event
it does not know.

## The server's status

The server-status request, method bridge.server_status, is a `control` request that
tells an editor add-on what the server serves, for the add-on's panel to show. The
server's capabilities list the method on the editor bridge only, so an add-on can tell a
server that will send it from an older one that will not. Once the session is described,
the server sends it to an add-on that listed the method, and to no other peer: an add-on
that does not list it is served exactly as before, and the runtime probe of a managed
game is never sent it.

The payload holds:

| Field | Value |
| --- | --- |
| `server_version` | The server's version. |
| `profile` | The capability profile it serves. |
| `transport` | `stdio` or `http`. |
| `observed_at_unix_ms` | When the status was sampled. |
| `mutations_blocked` | Why the server blocks every mutation, or `null`. |
| `needs` | One entry per need the profile's tools have, with its `status` and `reason`: from the `health.get` component that answers for it (`component` is `project`, `godot`, `editor` or `runtime`), or for a managed editor session from the router's `lsp_dap` plane (`plane`). A reason is at most 1,024 characters. |
| `tools` | Each tool of the profile, at most 512, with its `name`, `title`, `needs` and `read_only`. |
| `tools_total`, `tools_truncated` | How many tools the profile has, and whether the list stops short of them. |

The server samples the status every 10 seconds while the session lasts and sends it again only
when it differs from the one the add-on last took, apart from `observed_at_unix_ms`. The add-on
answers with a result, or with `invalid_input` when the payload is not well formed, in which
case it keeps the last status it took; a refused status, or one not answered within 30 seconds,
is sent again with the next sample.

## Identity, project and deadlines

* `envelope:identity` is always present. A server fills its own version and the session
  identifier; a plug-in fills its plug-in version and the Godot version. The unfilled
  members are `null`, never omitted.
* `envelope:project` binds the message to a project when one applies.
* `envelope:deadline_unix_ms` carries the deadline, so a peer can decline work it cannot
  finish in time rather than starting it and being cancelled.
* `envelope:correlation_id` ties a result, error, progress or cancel back to its request;
  `envelope:idempotency_id` identifies retry-equivalent work.

## Compatibility

The major component of `envelope:protocol_version` decides compatibility. A peer
announcing an incompatible major is refused at the handshake rather than allowed to
proceed and fail later on a field it does not understand.

Adding a message kind, changing a field's meaning, or making a `null`-permitted field
required is a major change. See [Schema evolution](schema-evolution.md).
