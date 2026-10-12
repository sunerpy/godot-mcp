# Troubleshooting

Symptom first. Each entry names what you actually see, then what it means.

## Startup and configuration

### `check-config: rejected [invalid_input]: unknown godot-mcp environment variable: ...`

Something in your environment starts with `GODOT_MCP_` and is not a recognized key. The
loader treats that prefix as its own namespace and refuses an unknown name rather than
ignoring it, because a typo would otherwise leave you running with a different setting
than you think.

Unset the variable, or rename your own helper variables so they do not use that prefix.
This commonly catches a shell helper someone named after the product under that prefix;
pick a name outside it instead.

### `check-config: rejected [invalid_input]: [http] is forbidden when the final transport is stdio`

The file names `transport = "stdio"` and also carries an `[http]` section. Which surface
is live would be ambiguous, so it is refused. Remove the section, or set
`transport = "http"` deliberately.

### `check-config: rejected [permission_denied]: http bind address ... is not loopback`

The listener may only bind loopback. This is not adjustable. If a client cannot reach
the host, put a reverse proxy in front and use
[`proxied_oidc`](http-proxied-oidc.md).

### `check-config: rejected [invalid_input]: http override is incomplete; missing: [...]`

An environment or command-line override started configuring HTTP without finishing it.
There is no partial listener; supply the named keys.

### `godot-mcp serve: diagnostics failed: ... invalid diagnostics sink: ...`

`serve` opened the configured file sink and rejected the object before starting the
protocol. A FIFO or character device reports `godot-mcp serve: diagnostics failed:
diagnostics could not be installed: invalid diagnostics sink: the object is not a
regular file`. A regular file that is the same kernel object as the serving process's
stdout reports `godot-mcp serve: diagnostics failed: diagnostics could not be installed:
invalid diagnostics sink: protocol stdout is not a diagnostics sink`.

`check-config` cannot detect either condition. A path name does not identify the object
that will be opened, and stdout depends on how the parent starts `serve`. Use `stderr`
or an ordinary regular file that is distinct from the serving process's stdout.

### `godot-mcp serve: startup failed: ...`

The lease could not be acquired or the journal could not be opened. Common causes:

* another `godot-mcp` process already holds this project — the exclusion is by object
  identity, so a different spelling of the same directory is the same project;
* a component of the project root or `project.godot` path is a symbolic link, which is
  refused outright;
* `project.godot` is not present at or above the path you passed.

See [Project leases](project-leases.md).

## Requests

### A tool is listed but returns `unavailable`

Membership and availability are different things. The tool belongs to the profile you
selected; the plane behind it cannot be reached right now.

The editor plane is available only while a Godot editor running the `godot_mcp`
add-on is connected: install it with `editor.addon.install`, enable it, open the
project in the editor, and check `editor.status.get`. See
[Editor bridge](editor-bridge.md).

The runtime plane answers for a running managed game session: start one with
`project.run.start` and pass its session ID. A command to a session that has ended
answers `unavailable`; `project.run.status` shows its state and exit facts, and
`project.run.logs` its output. Captures also need a windowed session, which needs a
display in the server's environment. Debug commands answer for a running editor session
started with `editor.session.start`; an editor that did not start lists why in
`project.run.logs` for its session. A listener on a usual Godot port is never adopted:
only the ports the server allocated to its own editor session, held by that editor's
process, are dialled. See [Managed sessions](managed-sessions.md).

`project.export` answers `unavailable` when exactly the resolved build's export templates
are not installed: the message names the label, such as `4.7.1.stable`, and the template
file the preset needs. Install that build's templates where Godot's Export Template
Manager puts them; `export.presets.list` shows, per preset, whether they were found. A
preset with a secret written into it is refused with `invalid_input` naming the options.
See [Exports](exports.md).

This is distinct from the controlled process boundary delivered by Todos 13-17.
`project.run`, `project.import.run` and the other engine-plane tools can launch a real
Godot process. For an engine-plane availability failure, check that a Godot binary is
configured and, when the operation needs a display, that a display is genuinely
available.

### An analysis answers `uncertain`

The analyser could not prove the answer and declined to guess. GDScript is dynamically
typed and paths can be built at runtime, so this is a real outcome and not an internal
error. Narrow the question, or use a controlled engine operation that observes the real
behaviour. Do not read `uncertain` as "probably fine".

### A write returns `conflict`

Either the file changed under you — a compare-and-swap write bound the target's identity
and bytes, and one of them moved — or an idempotency identity was reused in a different
context. Nothing was written in either case.

Re-read and decide. A blind retry is how you overwrite somebody else's change. See
[Recovery](recovery.md).

### A request returns `permission_denied` over HTTP

The credential, the `Host` or the `Origin` did not match exactly. Under
`trusted_local`, note that the secret is per launch: a secret from a previous run of
the server is not accepted. Under `proxied_oidc`, both the proxy-hop credential **and**
the end-user token must be present and the token's issuer and subject must equal the
configured principal exactly.

### A long request seems to hang

Check the configured deadline. A per-operation wall-clock deadline applies, and an
expired one returns `timeout`. A cancelled request resolves through the service's own
cancellation path, so an in-flight effect observes cancellation rather than being
abandoned mid-write.

## After a crash

### `godot-mcp serve: write gate closed: ...`

A transaction could not be reconciled to a proven state, so it is quarantined and
writes are refused. Reads still work. This does not clear itself.

Diagnose it, then resolve it deliberately:

```sh
godot-mcp transaction repair \
  --project /srv/projects/my-game \
  --transaction-id 00000000-0000-0000-0000-000000000301 \
  --operation reconcile \
  --proof-file ./proof.json \
  --dry-run
```

Read the report before recording anything. `reconcile` writes nothing. See
[Recovery](recovery.md) for the four operations and what each proves.

### `terminate applies only to a process_launch transaction, not ...`

`terminate` is meaningful only for a supervised launch. For a file effect, the
transitions are `compensate` (undo from a proven pre-state) or
`acknowledge-safe-state` (record a proven post-state).

### Repair refuses with a proof mismatch

The proof file did not match what the command independently re-derived. That is the
check working. The command re-derives every field itself, for the transaction's own
recorded effect kind, against the current project object and generation; for a
composite transaction it also re-validates the linked launch sub-proof, and a supplied
proof file cannot suppress that.

Regenerate the proof from the current state rather than editing it to match.

### Writes stay blocked and reconciliation mentions a survivor

A process from a previous run is still accounted for and may still be writing to this
project. The server will not mutate while that is true. Identify it by the recorded
identity and use `--operation terminate`, which acts only on a proven, still-contained
survivor. See [Process boundaries](process-boundaries.md).

## Diagnostics

Diagnostics go to `stderr` or to a rolling file, never to stdout. Increase verbosity
with the standard tracing environment filter. Secret material is wrapped in types whose
rendering is always redacted, and evidence records carry an output hash and byte count
rather than raw output, so raising verbosity does not start leaking payloads.
