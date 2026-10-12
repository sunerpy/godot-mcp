# Recovery

A durable mutation either happened or it did not, and after a crash the server has to
work out which. This page is the contract it holds itself to.

The short version: a transaction can end in exactly three states, every transition is
backed by a re-derived proof, and there is no bypass.

## Three terminal states, and no fourth

| State | Meaning |
| --- | --- |
| `compensated` | The effect was undone from a proven pre-state. The project is as it was. |
| `committed` | The effect is proven to have landed in its post-state. |
| `quarantined` | Neither could be proven. Writes are blocked until an operator resolves it. |

A fourth outcome is not representable. In particular there is no "assume it worked"
and no "probably fine" — an unprovable state becomes quarantine, which is a
deliberately loud, blocking condition rather than a warning you can miss.

## Three effect kinds

Every transaction records which kind of effect it made, and the proof form follows from
that:

* `file` — an on-disk mutation, proven by a pre/post content hash or by a
  create/delete tombstone;
* `process_launch` — a supervised process, proven by the durable launch receipt plus
  the terminal result (exit status, signal, and post-exit emptiness of the boundary);
* `editor_runtime` — an effect applied through the editor bridge.

Some operations are composite. A canonical text-resource or resource-identifier write
is always classified `file`, never `editor_runtime`, even though it invokes a
supervised Godot helper: its proof is a conjunction of a `process_launch` sub-proof for
the helper and a `file` sub-proof for the on-disk change. The transaction commits only
if **both** hold — its own hash is valid *and* the linked launch proof shows a clean
terminal state. A composite never introduces a fourth proof kind or a second journal.

## What happens at startup

Before the first request is served, the server acquires the lease, opens the journal
and reconciles everything a previous process left open. It reports one line per outcome
on `stderr`, then the write-gate status:

```console
godot-mcp serve: startup reconciliation compensated=1
godot-mcp serve: startup reconciliation committed=2
godot-mcp serve: write gate open
```

If a transaction could not be resolved, the gate is closed instead and the reason is
printed. Read requests still work; writes are refused until the quarantine is
resolved. A closed gate is not a transient condition that clears itself.

## Survivors from a previous run

A Godot process can outlive the server that started it — a `SIGKILL` gives the server
no chance to clean up.

Reconciliation uses the durable launch receipt to enumerate the recorded boundary and
identify survivors by exact identity: process id, start time, process group id, nonce
and endpoint, plus the control socket's inode. A partial match is not a match. A
process that merely happens to have the same process id after a reboot is not the
survivor and is not touched.

An **unresolved survivor blocks mutation**. The server will not write while it cannot
account for a process that may still be writing to the same project.

## Managed sessions and held input

A managed game or editor session is a supervised launch whose boundary identity is
durable before its start answers. When a server stops normally it stops its sessions
first. When it is killed, the sessions keep running, and the next server for the project
terminates every one of them during startup — before it serves anything — from that
durable identity, archives their output, and records each as `reconciled` when its
boundary was proven empty or `quarantined` when it was not. See
[Managed sessions](managed-sessions.md).

**An interrupted export registers nothing.** An export the killed server was running is
terminated the same way; none of its output becomes an artifact. What it had written is
kept in the server's private state as recovery evidence: `project.export.status` reports
how many bytes are retained, the bytes are never served, and artifact retention never
removes them. See [Exports](exports.md).

**Synthetic input is released without the server.** A game's probe releases everything
it holds the moment it loses its server connection, and quits 15 s later; a hold also
ends by itself when its `holdMs` passes, and a stop or a cancelled sequence releases at
once. So a killed server cannot leave a key, button or action stuck down.

**No project setting or autoload is installed for a session.** The probe is passed to
Godot on the command line, so there is nothing to restore after a crash, and the
displaced-autoload fact in self-diagnostics stays `not_required`.

## Conflicts

A `conflict` outcome always means: something changed under you, and nothing was
written.

* A compare-and-swap write whose target's identity or bytes changed is reported stale.
  Re-read and decide; do not retry blindly, because a blind retry is how you overwrite
  someone else's change.
* An idempotency identity reused in a **different** context — a different tool family,
  session, principal, project object or generation — conflicts rather than returning
  the other context's result. Returning it would be a cross-context information leak
  disguised as a cache hit.

The identity that must match is the whole of: the project's object identity and
generation, the mode-specific principal and session, the operation and tool family, the
idempotency identifier, and the full canonical request hash.

An exact same-context replay returns the recorded transaction's `status` and
`transaction_id`, not the typed proof from the original effect. The replay projection
never carries the typed proof regardless of phase; a proof, when one was recorded,
remains in the journal record. The projection is deliberately small and stable:
`terminal_reason`, when present, becomes `reason` alongside `status` and
`transaction_id`.

## Undo semantics

Undoable mutation belongs to the editor plane and goes through Godot's own editor undo
system, so that a change this server makes appears in the editor's undo history like any
other edit. With the `godot_mcp` editor add-on connected, a `scene.batch.apply` on a scene
open in the editor is one undo action there, and `editor.undo` / `editor.redo` move through
that history; see [Editor bridge](editor-bridge.md). An editor change whose answer was lost
is unresolved: it is never resent, and one that may have saved a file blocks writes until it
is resolved, like any other quarantined transaction.

Nothing in the shipped planes silently substitutes for it. A durable write on the
project plane is not undoable through the editor's history — its recovery story is the
three-state contract above, which is why that contract is strict.

## Operator repair

When a transaction is quarantined, resolving it is a deliberate local action.

`godot-mcp transaction repair` is a command-line subcommand for the local owner. It is
registered in no tool or resource registry, has no capability mapping, and is reachable
from no transport — the only way to it is a shell on the machine.

It takes four operations, and each one requires a proof file in the canonical
repair-proof schema:

| Operation | Effect |
| --- | --- |
| `reconcile` | Re-derives and reports the would-be transition. Records nothing. |
| `compensate` | Undoes the effect from a proven pre-state. |
| `terminate` | Terminates a proven, still-contained survivor. `process_launch` only. |
| `acknowledge-safe-state` | Records a proven post-state as committed. |

Start with `reconcile`, or with `--dry-run` on the operation you intend, and read what
it says before you record anything.

The command re-derives every proof field itself, for the transaction's own recorded
effect kind, against the current project object and generation. It journals exactly one
proof-backed transition, and only if the proof holds. For a composite transaction it
independently re-validates the linked sub-proof; a supplied proof file cannot suppress
that check.

A `process_launch` proof is re-derived from live process identity (process ID and start
time) and from the boundary's own record of its members, which only the weak locator
keeps in a form this command can enumerate on its own. A quarantined launch from a
delegated cgroup, a container adapter or a Windows Job Object is therefore refused here
rather than approximated; the next server for the project reconciles those launches at
its start, through the boundary itself.

<!-- lint:absence-claim --> There is no bypass: `--force` is not a recognized argument and produces a parse error, and no operation clears a quarantine without a proof.

```sh
godot-mcp transaction repair \
  --project /srv/projects/my-game \
  --transaction-id 00000000-0000-0000-0000-000000000301 \
  --operation reconcile \
  --proof-file ./proof.json \
  --dry-run
```

Every refusal — an unproven state, a mismatched proof, the wrong project object or
generation, a transaction that is not quarantined, an I/O error while re-deriving —
exits non-zero and leaves the transaction exactly as it was.

## Related

* [Project leases](project-leases.md) — the exclusion and compare-and-swap rules.
* [Process boundaries](process-boundaries.md) — how a launch becomes accountable.
* [Troubleshooting](troubleshooting.md) — the symptom-first index.
