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

`reconcile` reports the transition the proof would allow and records nothing; `compensate`,
`terminate` and `acknowledge-safe-state` record one transition each, and only when the server
re-derives the proof itself and it holds. Start with `reconcile`, or with `--dry-run` on the
operation you intend. The [recovery reference](../../operator/recovery.md) describes every proof
and refusal.

## One server per project

A project is held by one server at a time, through a lease on the project's own directory. A
second server for the same project refuses to start rather than share the journal; one that starts
after the first has gone takes the lease over and reconciles whatever the first left.
