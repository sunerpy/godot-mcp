---
description: How the server settles a change that a crash interrupted, and how to resolve a quarantine.
---

# Recovery

A change to the project either happened or it did not, and after a crash the server has to work
out which. It does that before it serves anything, every time it starts.

## Three outcomes, and no fourth

| State | Means |
| --- | --- |
| `committed` | The change is proven to have landed |
| `compensated` | The change was undone from a proven earlier state; the project is as it was |
| `quarantined` | Neither could be proven; writes are blocked until the transaction is resolved |

There is no "assume it worked": a state the server cannot prove becomes quarantine, a loud,
blocking condition rather than a warning that is easy to miss.

## At startup

The server takes the project's lease, opens the journal and settles every transaction a previous
process left open, then prints one line per outcome and the state of the write gate on stderr:

```console
godot-mcp serve: startup reconciliation compensated=1
godot-mcp serve: startup reconciliation committed=2
godot-mcp serve: write gate open
```

With a transaction quarantined the gate stays closed: reads still work, writes are refused with
the reason, and `health.get` reports it. Godot processes a previous server left running — a game,
an editor, an export — are ended and recorded at the same time.

## When the editor answers late

A change made through the editor is also quarantined when its answer does not come back in time:
a run that outlasted its `deadline_ms`, a save during a long import. The editor still finishes
what it started and answers, on the same connection or, when that connection ended, on the next
one it opens, even to a restarted server. The server then ends the transaction the way the
answer says, `committed`, or `compensated` when the change never started, and writes reopen. An
answer that cannot say what happened leaves the quarantine in place.

Keep the editor open until it answers. An add-on installed by godot-mcp 0.5.0 or earlier answers
only on the same connection; `editor.addon.update` brings it up to date.

An editor that crashed or was closed before it answered sends nothing. Once that editor has ended,
stop the server and run `godot-mcp transaction repair` with `--operation acknowledge-safe-state`
and a `writer-ended` proof: it accepts the project's files as they are, and a retry of the request
still answers `uncertain`. The command refuses while that editor still runs, and when the server
could not identify the editor's process, as on Windows; the
[recovery reference](../../operator/recovery.md#an-editor-that-ended-before-it-answered) has the
steps. godot-mcp 0.5.0 and earlier keep such a change quarantined for good.

## Resolving a quarantine

Resolving a quarantine is a deliberate local action, through a command on the machine itself; no
tool and no transport reaches it:

```sh
godot-mcp transaction repair \
  --project /path/to/my-game \
  --transaction-id 00000000-0000-0000-0000-000000000301 \
  --operation reconcile \
  --proof-file ./proof.json \
  --dry-run
```

`reconcile` reports the transition the proof would allow and records no transition; `compensate`,
`terminate` and `acknowledge-safe-state` record one transition each, and only when the server
re-derives the proof itself and it holds. Start with `reconcile`, or with `--dry-run` on the
operation you intend. The [recovery reference](../../operator/recovery.md) describes every proof
and refusal.

## One server per project

A project is held by one server at a time, through a lease on the project's own directory. A
second server for the same project refuses to start rather than share the journal; one that starts
after the first has gone takes the lease over and reconciles whatever the first left.
