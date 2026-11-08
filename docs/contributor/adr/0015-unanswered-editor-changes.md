# 0015 — An unanswered editor change is settled by its late answer, or once its editor has ended

**Status:** accepted for release 0.6.0. It amends [ADR 0006](0006-three-state-transactions.md)
for editor changes and [ADR 0012](0012-declared-script-execution.md) for unanswered runs.

## Context

An editor change whose answer the server never received is unresolved: the editor may have
written project files, so a change whose scope names files or the whole project is
quarantined and every write is refused ([ADR 0006](0006-three-state-transactions.md)). The
server stops waiting at a request's deadline and a two-second grace, or when the session that
carries it ends, but the editor goes on with a change it has started, because GDScript cannot
be stopped part way.

Up to release 0.5.0 nothing could resolve such a quarantine. The startup reconciliation and
`godot-mcp transaction repair` both settle an `editor_runtime` transaction only from a bound
receipt, and a change whose answer was lost has none: the bridge dropped a request as soon as
its caller stopped waiting, so the answer that came later had nowhere to go. The recovery page
and ADR 0012 told the operator to resolve the quarantine, and no operation could. Since
`editor.script.run`, `editor.scene.save`, `editor.play.start`, `editor.filesystem.scan` and a
`project.settings.patch` carried out by the editor all take the whole project as their scope,
one run that outlasted its `deadline_ms` left the project read-only for good.

## Decision

The editor's late answer is the receipt the quarantine lacked, and it settles the transaction.
When the editor ended before it answered, an operator settles the transaction instead, from a
proof that the editor's process has ended.

### The late answer

- **The bridge keeps what it stopped waiting for.** When a mutation's caller gives up, at the
  deadline or because the session ended, the server keeps the request if its payload names the
  server's `transaction_id`, at most 64 requests. Giving up, delivering an answer and losing the
  session take the request under one lock, so an answer that meets the deadline reaches either
  the caller or the kept request, never neither. A report that arrives before the server is
  ready to settle anything waits for it, at most 16.
- **The add-on reports what it could not deliver.** Add-on 0.5.0 keeps the answer to a mutation
  it could not deliver, or delivered after the request's deadline, and records a queued mutation
  dropped with a lost connection as `not_started`: at most 16 answers that say what happened. It
  reports each in a mutation.late_answer event, at once when a session is described and to
  every session described afterwards
  ([Editor bridge protocol](../../protocol/bridge-protocol.md#late-mutation-answers)).
- **A report binds exactly or not at all.** An answer on the same session binds by correlation.
  A report naming this server's session must match a kept request by message ID, idempotency
  ID, method and transaction ID, and binds once. A report naming an earlier server's session is
  checked by the journal alone.
- **The journal settles only what the answer names.** The gateway settles the quarantined
  `editor_runtime` transaction of the live-editor boundary whose transaction ID and idempotency
  ID the answer names: `committed` ends it `committed` with the answer as its receipt and the
  outcome the answer reports, `not_started` ends it `compensated`, and `unresolved` leaves it
  quarantined. The audit log records the answer before the transaction changes, and the
  transition is the one `repair_transition` already journals for an operator's repair. An answer
  for a transaction whose outcome is still being written is held, at most 16, and applied as
  soon as that outcome is written.
- **A settled transaction replays its outcome.** A retry of the same request answers the
  settled outcome from the journal; the editor is never asked to run it again.

### An editor that ended

- **The server records which process it sent the request to.** Add-on 0.5.0 also reports the
  executable it runs in its session description. The server keeps the editor's process ID and
  start time only when the platform reports start times (Linux and macOS), the add-on named an
  executable, and the process runs that same file; the start time is read on both sides of the
  executable check, so an ID that changes hands meanwhile is recorded for neither process. The
  `editor_runtime` proof of a request that reached the editor and was never answered names that
  process, in a new optional `editor_process` field of journal schema v1.
- **The end of the writer is the proof.** `transaction repair --operation
  acknowledge-safe-state` accepts a `writer-ended` proof for such a transaction once the recorded
  process has ended: no process has its ID, or the one that has it started at another time. A
  live process whose start time cannot be read proves nothing. The command and the gateway
  each derive the end again from the process table; `reconcile` reports it and changes no
  transaction; `compensate` is refused, because the end of the editor proves no pre-state.
- **The files are accepted; the result stays unknown.** The transaction ends
  `committed_fsynced` with the terminal outcome `uncertain`. Writes reopen, and a retry of the
  request answers `uncertain` from the journal, never running it again.
- **Nothing else qualifies.** A transaction whose proof records an answer, names no process, or
  whose proof file names another process is refused, and so is any transaction while its
  editor still runs.

## Consequences

- An unanswered editor change no longer leaves a project read-only for good: it settles when
  the editor answers, on the same session, on a later one, or on the next server for the
  project, and an operator can settle it once the editor has ended.
- The late answer needs no new journal field: the receipt is the existing `editor_runtime`
  proof, bound through the transaction ID the request's payload already carried. The
  `editor_process` field is optional, so records written before it are read as before; a server
  of release 0.5.0 or earlier cannot read a record that carries it, since it rejects unknown
  fields.
- A second authority can leave the quarantined state: besides the operator's repair permit, a
  late editor answer bound to its transaction. It writes the same proof-backed transition, never
  a transition without a receipt.
- One kind of `committed` transaction proves no post-state: an editor change settled after its
  editor ended. ADR 0006's three states keep their meaning for every other transaction, and
  this one carries the outcome `uncertain`, so nothing reads it as a success.
- Which session carries the answer follows the bridge's silence rule, which this decision does
  not change: the server closes a session that stays silent for 30 seconds while it awaits no
  request a caller waits on ([Editor bridge protocol](../../protocol/bridge-protocol.md)).
  Release 0.6.0 also counted a server status the add-on had not taken. Measured then on Linux
  x86_64 under Xvfb: with `deadline_ms` 1000, a run that held Godot 4.4.1's main thread for
  34 s, and another for 50 s, kept its session, because the status the quarantine changed was
  outstanding, and its answer settled it there; with `deadline_ms` 29000 and the main thread
  held for 40 s, Godot 4.4.1, 4.5.1, 4.6.1, 4.7.1 and 4.7.2 each lost the session and settled
  the run from its report 43.0 to 44.1 s after the call. An editor that never answered again
  kept its session the same way and was shown as connected, so the status no longer counts.
  Measured after that change, on the same machine with the main thread held for 40 s: with
  `deadline_ms` 29000 and with 1000, each of the five builds lost the session to the silence
  rule, dialled again when the run ended, and settled the run from its report 42.7 to 44.4 s
  after the call.
- An add-on older than 0.5.0 sends no report and names no executable, so only an answer on the
  same session settles a change made through it, and the operator's repair cannot.
- The repair is unavailable where the server cannot identify the editor's process: on Windows,
  for an editor in another PID namespace such as a container or a Flatpak, for a request
  the editor answered without settling it, and for a server that crashed before it recorded the
  outcome.

## Alternatives considered

- **Treat a timeout as not started.** Rejected: the editor may have saved files, and replaying
  an `unresolved` change could apply it twice ([ADR 0006](0006-three-state-transactions.md)).
- **Bind a report by idempotency ID alone.** Rejected: the same idempotency ID may name
  requests of different tools, and the journal's transaction ID is already in every mutation's
  payload, so binding needs nothing new.
- **Keep the session open until the answer comes.** Rejected: a run can hold the editor's main
  thread indefinitely, and a session that never ends hides a lost editor.
- **Settle an ended editor's change from the files alone.** Rejected: a change that covers the
  whole project records no post-state to compare with, so the files cannot prove the result;
  they can only be accepted as it, with the result left `uncertain`.
- **Let a process ID alone identify the editor.** Rejected: IDs are reused, so a later process
  would pass for the editor, or the editor for an ended one.
