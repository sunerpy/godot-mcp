# 0015 — A quarantined editor change is settled by the editor's late answer

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

- **The bridge keeps what it stopped waiting for.** When a mutation's caller gives up, at the
  deadline or because the session ended, the server keeps the request if its payload names the
  server's `transaction_id`, at most 64 requests. Giving up, delivering an answer and losing the
  session take the request under one lock, so an answer that meets the deadline reaches either
  the caller or the kept request, never neither.
- **The add-on reports what it could not deliver.** Add-on 0.4.1 keeps the answer to a mutation
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

## Consequences

- An unanswered editor change no longer leaves a project read-only for good while the editor
  that runs it is alive: it settles when the editor answers, on the same session, on a later
  one, or on the next server for the project.
- The journal schema does not change: the receipt is the existing `editor_runtime` proof, bound
  through the transaction ID the request's payload already carried.
- A second authority can leave the quarantined state: besides the operator's repair permit, a
  late editor answer bound to its transaction. It writes the same proof-backed transition, never
  a transition without a receipt.
- Which session carries the answer follows the bridge's silence rule, which this decision does
  not change: the server closes a session that stays silent for 30 seconds while it awaits no
  request, and a server status the add-on has not taken is such a request
  ([Editor bridge protocol](../../protocol/bridge-protocol.md)). Measured on Linux x86_64 under
  Xvfb: with `deadline_ms` 1000, a run that held Godot 4.4.1's main thread for 34 s, and another
  for 50 s, kept its session, because the status the quarantine changed was outstanding, and
  its answer settled it there. With `deadline_ms` 29000 and the main thread held for 40 s, each
  locked build, 4.4.1, 4.5.1, 4.6.1, 4.7.1 and 4.7.2, lost the session to the silence rule,
  dialled again when the run ended, and settled the run from its report 42.8 to 43.1 s after the
  call.
- An add-on older than 0.4.1 sends no report, so only an answer on the same session settles a
  change made through it.
- An editor that exits before it answers sends nothing; that change stays quarantined until an
  operator resolves it.

## Alternatives considered

- **Treat a timeout as not started.** Rejected: the editor may have saved files, and replaying
  an `unresolved` change could apply it twice ([ADR 0006](0006-three-state-transactions.md)).
- **Bind a report by idempotency ID alone.** Rejected: the same idempotency ID may name
  requests of different tools, and the journal's transaction ID is already in every mutation's
  payload, so binding needs nothing new.
- **Keep the session open until the answer comes.** Rejected: a run can hold the editor's main
  thread indefinitely, and a session that never ends hides a lost editor.
