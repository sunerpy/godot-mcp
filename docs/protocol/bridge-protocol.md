# Editor bridge protocol

The wire contract between the Rust server and a GDScript editor plug-in is frozen and
ships in the archive as `schemas/bridge/protocol-v1.json`, alongside its GDScript codec.

The plug-in that would speak it is not part of this release. The protocol is documented
here because it is finished, self-contained, and versioned — a future plug-in is an
implementation task against a fixed contract rather than a redesign. See
[Editor bridge](../operator/editor-bridge.md).

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
