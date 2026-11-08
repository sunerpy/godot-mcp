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
| `quarantined` | Neither could be proven. Writes are blocked until it is resolved: by an operator, or, for an editor change, by the editor's late answer. |

A fourth outcome is not representable. In particular there is no "assume it worked"
and no "probably fine" — an unprovable state becomes quarantine, which is a
deliberately loud, blocking condition rather than a warning you can miss.

One kind of `committed` transaction proves no post-state: an editor mutation the
editor never answered, which an operator commits once that editor has ended
([An editor that ended before it answered](#an-editor-that-ended-before-it-answered)).
Its files are accepted as they are, and its result stays `uncertain`.

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

## A transaction that writes several files

A file transaction may write more than one file: `file.move` writes its destination and
removes its source, and `project.batch.apply` writes every file of its batch.

* Before the first write, every recorded pre-state is read again. A file that changed
  since the transaction opened stops it with nothing written; that file no longer holds
  its recorded pre-state, so the transaction is quarantined, as a single-file write whose
  file changed is.
* The files are then written in order. When a later write fails, for example on a full
  disk or a directory that cannot be written, the files already written are written back
  to their recorded bytes, newest first, each as a compare-and-swap against the bytes the
  transaction wrote, and a file the transaction created is removed again. The
  transaction then ends `compensated` once every file is proven back in its pre-state. A
  directory created for a new file may remain, empty.
* When a write-back fails, or the server stops part way, neither state is proven and the
  transaction is quarantined.

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
resolved. A closed gate is not a transient condition that clears itself: only proof
opens it, an operator's repair or, for an editor change, the answer the editor gives
later (see [Late editor answers](#late-editor-answers)).

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

**An interrupted deploy is uncertain.** A deploy the killed server was running touched no
project file, so the next server records its transaction as failed and `uncertain`: the
project holds its pre-state, and whether the device installed or started the app is
unknown. Check the device before deploying again. See
[Exports](exports.md#android-devices).

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
is resolved, like any other quarantined transaction, by the editor's late answer or by an
operator.

Some editor effects may write files nobody can list in advance: `editor.scene.save` and
`editor.play.start` save the open scenes with whatever external resources they changed,
`editor.filesystem.scan` imports and writes `.import`, `.uid` and cache files, and
`project.settings.patch` carried out by the editor may switch a plugin, whose own code then
runs. Each of these transactions records the files it names, each with its precondition,
and one more entry, `*`, that stands for the whole project. A lost answer quarantines it
like any unresolved effect that touched files, so writes stay blocked until the editor's
late answer settles it or an operator resolves it, and a committed one is followed by a full
rescan of the workspace.

Nothing in the shipped planes silently substitutes for it. A durable write on the
project plane is not undoable through the editor's history — its recovery story is the
three-state contract above, which is why that contract is strict.

## Late editor answers

The server stops waiting for an editor change at its deadline, two seconds later, or when
the editor's session ends, but the editor goes on with a change it has started: GDScript
cannot be stopped part way. When the editor finishes, it answers anyway, on the same session,
or, when that session ended meanwhile, in a report to the next session it opens, which may
be a later server's ([Editor bridge protocol](../protocol/bridge-protocol.md#late-mutation-answers)).
That answer is the bound receipt the quarantined transaction lacked, and it settles the
transaction:

| The editor's late answer | The transaction |
| --- | --- |
| `committed` | ends `committed`, with the answer as its receipt, and the outcome the answer reports: success, or the error the change ended with |
| `not_started` | ends `compensated`, with the editor's reason |
| `unresolved` | stays `quarantined`; the answer is recorded in the audit log |

An answer settles only the quarantined live-editor transaction whose transaction ID and
idempotency ID it names, and the audit log records it before the transaction changes. Writes
reopen as soon as no transaction is quarantined. A retry of the same request then answers
the settled outcome from the journal, and the editor is never asked to run it again. An
answer that arrives while the server is still writing the transaction's unresolved outcome
is held and applied as soon as that outcome is written.

An editor that exits before it answers sends nothing. Its change stays quarantined until an
operator settles it once that editor has ended
([An editor that ended before it answered](#an-editor-that-ended-before-it-answered)).

## Operator repair

When a transaction is quarantined, resolving it is a deliberate local action.

`godot-mcp transaction repair` is a command-line subcommand for the local owner. It is
registered in no tool or resource registry, has no capability mapping, and is reachable
from no transport — the only way to it is a shell on the machine.

It takes four operations, and each one requires a proof file in the canonical
repair-proof schema:

| Operation | Effect |
| --- | --- |
| `reconcile` | Re-derives and reports the would-be transition. Changes no transaction; a proven one is written to the audit log. |
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

### An editor that ended before it answered

An editor mutation that may write project files and that the editor never answered —
`editor.script.run` past its deadline, or an editor that exited or lost its connection
mid-request — is quarantined as an `editor_runtime` transaction: the editor may have
written project files, and it may still be writing them. Nothing the server can read
proves what it wrote. What it can prove is that the writer is gone.

When the add-on connects, the server records which editor process it is talking to:
the process ID and its start time, kept only when the platform reports start times
(Linux and macOS), the add-on reports the executable it runs (add-on 0.4.1 and later),
and that process really runs that executable. An unanswered request's record names that process. Once
the process has ended, `acknowledge-safe-state` with a `writer-ended` proof accepts
the project's files as they are, and the request's own result stays unknown:

```json
{
  "effect_kind": "editor_runtime",
  "transaction_id": "00000000-0000-0000-0000-000000000302",
  "project_object_identity": "project_…",
  "project_generation": 0,
  "bridge_reachable": false,
  "claimed_disposition": "writer-ended"
}
```

`project_object_identity` and `project_generation` are the `object_id` and
`generation` of the `project` object that `project.info.get` answers, and a wrong
value is refused with the value the held lease designates. An optional `editor_process` object
(`pid`, `start_time`) is checked against the record when it is present.

1. Close the editor, or wait for it to end. While the recorded process still runs,
   `acknowledge-safe-state` is refused and `reconcile` proves no disposition, both
   with `the editor that ran this request is still running (pid …): wait for it to answer, or close it`.
2. Stop the server. The command holds the project's lease while it works, so it is
   refused while a server owns the project.
3. Check the files the request could have changed. The command proves that nothing
   can still write for the request, not what was written.
4. Run `reconcile`. It reports `proven-disposition writer-ended` and changes no transaction.
5. Run `acknowledge-safe-state`:

   ```sh
   godot-mcp transaction repair \
     --project /srv/projects/my-game \
     --transaction-id 00000000-0000-0000-0000-000000000302 \
     --operation acknowledge-safe-state \
     --proof-file ./proof.json
   ```

   The record moves to `committed_fsynced` with the terminal outcome `uncertain`,
   and the report names the editor process that ended.
6. Start the server. Writes are served again. A replay of the request with the same
   `idempotencyId` and arguments still answers `uncertain`, from the journal, and is
   not sent to the editor again; like every editor mutation, it is answered only
   while an editor is connected.

The command re-derives the end of the editor itself, from the operating system's
process table, and the server's own gateway derives it again before it records the
transition. It is refused, and the transaction stays quarantined, when:

* the record names no editor process — the server could not prove the editor's
  process when it connected (Windows, an editor in another PID namespace such as a
  container or a Flatpak, an add-on older than 0.4.1, which does not report its
  executable), the editor
  answered without settling the request, or the record predates editor process
  records;
* the proof file claims a reachable bridge, a bridge receipt, a live state probe, or
  another editor process than the one recorded;
* the operation is `compensate`: the end of the editor proves no pre-state.

## Related

* [Project leases](project-leases.md) — the exclusion and compare-and-swap rules.
* [Process boundaries](process-boundaries.md) — how a launch becomes accountable.
* [Troubleshooting](troubleshooting.md) — the symptom-first index.
