# Configuration

Configuration is loaded in four layers, each overriding the one before it:

1. built-in defaults,
2. a strict TOML file,
3. recognized `GODOT_MCP_`-prefixed environment variables,
4. typed command-line overrides.

The loader is strict in both directions. An unknown TOML key is a parse error, and
an unknown `GODOT_MCP_`-prefixed environment variable is a rejection rather than
something quietly ignored — a typo in a variable name must not silently leave the
server running with a different setting than you intended.

Validate any file before you rely on it:

```sh
godot-mcp check-config --config ./godot-mcp.toml
```

It exits `0` when the configuration is valid and `2` when it is rejected, printing
the sanitized reason and its error code to `stderr`.

## What reads a configuration file in this release

`check-config --config <path>` validates a file without starting the server.
`serve --config <path>` loads the same strict contract and selects its transport.
Without `--config`, `serve` preserves the local stdio default, starts no listener
and creates no HTTP credential. `--project` and `--profile` remain explicit CLI
authority and override those two fields from the file. `--workspace <dir>` replaces
`--project` to start in bootstrap state over a workspace directory; it is
available with the stdio transport only. See
[Profiles and tool outcomes](profiles-and-outcomes.md#authoring).

## Secrets are never stored inline

There is no configuration key that takes a credential value. A secret is always an
indirection resolved at the point of use — either an environment variable name or
the path of a protected file:

<!-- docs-check:config expect="accept" -->
```toml
transport = "http"

[http]
bind = "127.0.0.1:8765"
mode = "trusted_local"
host = "127.0.0.1:8765"
origin = "http://127.0.0.1:8765"

[http.local_secret.env]
var = "GODOT_MCP_LAUNCH_SECRET"
```

An inline string cannot deserialize into that position at all, so pasting a value
there is a parse error rather than a working configuration with a credential in it.
Resolved secret material is wrapped in a type whose `Debug` and `Display` are always
`[REDACTED]`, so it cannot reach a log line by accident.

## The default: local stdio

The shipped example is the whole configuration a local stdio deployment needs:

<!-- docs-check:config expect="accept" -->
```toml
transport = "stdio"
```

Under `stdio` an `[http]` section is not merely unnecessary, it is rejected. A
configuration that names both a stdio transport and an HTTP listener is ambiguous
about which surface is live, and an ambiguous security posture fails closed:

<!-- docs-check:config expect="reject" -->
```toml
transport = "stdio"

[http]
bind = "127.0.0.1:8765"
mode = "trusted_local"
host = "127.0.0.1:8765"
origin = "http://127.0.0.1:8765"
```

## A complete file

Every non-HTTP key, in one file the production loader accepts:

<!-- docs-check:config expect="accept" -->
```toml
project_root = "/srv/projects/my-game"
godot_binary = "/opt/godot/godot"
profile = "core"
transport = "stdio"
disposable = false

[bridge]
endpoint = "127.0.0.1:0"

[log_sink.file]
path = "/var/log/godot-mcp/server.log"

[process]
boundary = "cgroup"
trust = "trusted"

[limits]
deadline_ms = 30000
max_output_bytes = 8388608
max_body_bytes = 1048576
max_queue = 64
```

`endpoint = "127.0.0.1:0"` is the loopback form the loader requires. Read
[`[bridge]`](#bridge) before you set it to anything else: the key is validated but
has no runtime effect in this release.

## Keys

### Top level

| Key | Meaning |
| --- | --- |
| `project_root` | Candidate project directory. Secure ownership is established separately, at lease acquisition. |
| `godot_binary` | Godot executable to use for controlled-engine operations. |
| `profile` | Capability profile to expose. `serve` takes this from `--profile`, not from this key. |
| `transport` | `stdio` (default) or `http`. `serve --config` starts the selected transport; HTTP remains loopback-only and authenticated. |
| `disposable` | Reserved server-wide switch. `false` is the only runnable value in this release; `serve` refuses `true` rather than silently loading the original project or claiming that a filtered copy confines untrusted code. |
| `log_sink` | `stderr` (default) or a rolling file. |

The engine crate contains operation-scoped disposable-copy machinery, but `serve` does not
yet route every engine operation through it. Consequently `disposable = true` fails startup
closed. A filtered copy only redirects project-tree mutations; it does not restrict
absolute-path reads or writes, network access, or arbitrary child-process behavior. See the
[threat model](../security/threat-model.md) for the complete security posture.

### `[http]`

Present only when the final transport is `http`. See
[HTTP: `trusted_local`](http-trusted-local.md) and
[HTTP: `proxied_oidc`](http-proxied-oidc.md).

The `proxied_oidc` shape, which the loader also accepts:

<!-- docs-check:config expect="accept" -->
```toml
transport = "http"

[http]
bind = "127.0.0.1:8765"
mode = "proxied_oidc"
host = "godot-mcp.example.invalid"
origin = "https://godot-mcp.example.invalid"
external_resource_url = "https://godot-mcp.example.invalid/mcp"
allowed_algorithms = ["RS256"]
required_scopes = ["godot.read"]
required_capabilities = ["read"]

[http.proxy_hop.credential_file]
path = "/etc/godot-mcp/proxy-hop.cred"

[http.principal]
issuer = "https://issuer.example.invalid"
subject = "operator@example.invalid"
```

| Key | Meaning |
| --- | --- |
| `http.bind` | Listener address. Must be loopback. |
| `http.mode` | `trusted_local` or `proxied_oidc`. |
| `http.host` | The exact expected `Host` authority. |
| `http.origin` | The exact expected `Origin`. |
| `http.local_secret` | `trusted_local` only, and **required** there: the secret indirection. Validated but never consumed by the production `serve` path — see [`http.local_secret`](#httplocal_secret) before you set it. |
| `http.proxy_hop` | `proxied_oidc` only: the authenticated proxy-hop identity. |
| `http.principal` | `proxied_oidc` only: the one frozen issuer/subject pair. |
| `http.external_resource_url` | `proxied_oidc` only: the externally visible HTTPS URL. |
| `http.allowed_algorithms` | `proxied_oidc` only: a nonempty subset of `RS256`, `PS256`, and `ES256`. Omission defaults to `RS256` only. |
| `http.required_scopes` | `proxied_oidc` only: nonempty RFC 6749 scope tokens required on every bearer token. Omission defaults to `godot.read`. |
| `http.required_capabilities` | `proxied_oidc` only: nonempty capabilities required by the served profile. Omission defaults to `read`. |

The schema also retains the socket-path proxy-hop form for the frozen transport
contract. The shipped backend currently accepts requests over TCP, so it cannot
prove that a TCP request arrived through that filesystem socket. `check-config`
validates the shape, but `serve` refuses it at startup; use the credential-file
form with mode `0600` for the shipped TCP listener.

The mode-specific keys are mutually exclusive. Supplying a `trusted_local` secret
under `proxied_oidc`, or a principal under `trusted_local`, is rejected rather than
ignored — a key that looks configured but has no effect is a security trap.

#### `http.local_secret`

**This key is required under `trusted_local`, and the served credential does not come
from it.** That is the same half-exposed shape as [`[bridge]`](#bridge), so here is the
whole truth:

* the loader **requires** it under `trusted_local` (a missing one is
  `local_secret_missing`), **forbids** it under `proxied_oidc`, and validates the
  indirection's shape, so `check-config` accepts or rejects a file on it;
* it has exactly one runtime consequence, and it is at load time, not at serve time:
  the `env` form's variable name is the one `GODOT_MCP_`-prefixed variable exempted
  from the unknown-variable rejection described under
  [Environment variables](#environment-variables);
* the `serve` path never resolves it. `HttpTransportOptions::from_config` reads
  `http.bind`, `http.mode`, `http.host`, `http.origin` and the `proxied_oidc` keys and
  nothing else, so neither the named variable nor the named file is ever read;
* the bearer secret a `trusted_local` request must present is **published by the
  server**, not supplied by you: each launch draws 256 bits from the OS CSPRNG and
  writes them to `<project>/.godot-mcp/http-launch-secret` with owner-only
  permissions, replacing any file a previous launch left. See
  [HTTP: `trusted_local`](http-trusted-local.md).

It is retained rather than removed because the configuration schema is frozen, and
removing a required key from a frozen schema is itself a breaking change. Treat it as
reserved: point it at any protected indirection whose shape the loader accepts, and do
not expect a client to authenticate with the value behind it. Substituting a
per-launch server-published secret for an operator-supplied static one is deliberate —
it rotates on every restart and its file mode is enforced by the server rather than
trusted from the filesystem.

### `[bridge]`

| Key | Meaning |
| --- | --- |
| `bridge.endpoint` | Reserved. Validated as loopback, but not read by the production `serve` path, so it has no runtime effect in this release. |

**This key currently has no effect, and that is worth stating plainly**, because the
section above says a key that looks configured but does nothing is a trap. It is exactly
that shape today, so here is the whole truth:

* the loader parses it and rejects a non-loopback address, and `check-config` accepts and
  reports it — so a configuration file carrying it is valid;
* the `serve` path never reads it. Under the `editor` and `full` profiles the editor
  bridge listens on an ephemeral loopback port it chooses itself and announces through the
  private rendezvous file in the project's state directory, which is how the `godot_mcp`
  add-on finds it (see [Editor bridge](editor-bridge.md));
* consequently, setting it changes no behaviour.

The language server and debug adapter are not configured either. Under the `runtime`
and `full` profiles a managed editor session hosts them on loopback ports the server
allocates per session, and `EndpointRegistry::connect` dials one only through that
editor's own launch receipt, after the listener is attributed to the editor process (see
[Managed sessions](managed-sessions.md)). Other profiles attach no LSP/DAP boundary: it
reports `available = false` and refuses every exchange rather than claim readiness
because the adapter code is compiled in.

It is retained rather than removed because the wire contract it belongs to is frozen and
shipped (the codec and `schemas/bridge/protocol-v1.json`), and removing a key from a
frozen configuration schema is itself a breaking change. Treat it as reserved, not as a
switch. See [Editor bridge](editor-bridge.md) and
[ADR 0010](../contributor/adr/0010-complete-development-bridge-strategy.md).

### `[limits]`

| Key | Meaning |
| --- | --- |
| `limits.deadline_ms` | Scheduler queue-admission deadline and engine-process operation ceiling; HTTP also applies it while reading a request body. |
| `limits.max_output_bytes` | Maximum serialized tool response and artifact-range response size. |
| `limits.max_body_bytes` | Maximum serialized tool-argument size; HTTP also rejects a larger wire body before dispatch. |
| `limits.max_queue` | Scheduler queue bound and shared in-flight request bound across stdio and HTTP. |

All four keys are read by the production `serve` path, so none is reserved or
validation-only at key granularity. `limits.deadline_ms` is enforced as the
scheduler queue-admission deadline and as the HTTP request-body read deadline.
For the process-backed `project.import.run`, `project.run`, `project.test`, and
`script.check` tools, the effective process deadline is the smaller of the call's `deadline_ms`
argument and `limits.deadline_ms`; a caller can request a shorter deadline but
cannot expand the configured operation budget. The default of 30 s is short for a first
import of a project with many assets — a committed test framework's fonts, for example,
took longer than that in the real-client acceptance runs — so raise `limits.deadline_ms`
for such projects and pass the call's `deadline_ms`. The scheduler operation deadline
does not yet time-limit other admitted, non-process work, and
`shutdown_budget_ms` does not yet drive the scheduler's shutdown drain. On stdio
shutdown, the `rmcp` service's quitting path carries cancellation so an in-flight
request observes cancellation instead of being dropped mid-effect; it does not
invoke that drain.

Queue priority is assigned by the server from the operation family; it is never
accepted from tool arguments or transport input. Caller-blocking discovery,
analysis, inspection, language-service and diagnostic reads are `interactive`;
ordinary project/editor/runtime mutations are `normal`; and project import plus
destructive project-wide export are `bulk`. The `operator` class is reserved but
unused: local `transaction repair` is a proof-producing recovery path and does not
enter this scheduler. A queued lower class is promoted after at most 16 higher-class
admissions overtake it, so a steady stream of interactive work cannot starve bulk
work indefinitely.

Every limit must be greater than zero. A zero limit would mean "unbounded" in
practice, so it is rejected:

<!-- docs-check:config expect="reject" -->
```toml
transport = "stdio"

[limits]
deadline_ms = 0
```

### `[process]`

Which containment boundary every Godot launch is created in, and whether the project's
code is trusted. See [Process boundaries](process-boundaries.md) for what each boundary
guarantees.

| Key | Meaning |
| --- | --- |
| `process.boundary` | `weak`, `cgroup`, `adapter` or `job_object`. Unset, the host's own: the weak locator on Linux and macOS, a Job Object on Windows. `cgroup` is Linux-only and `job_object` Windows-only. |
| `process.trust` | `trusted` (default) or `untrusted`. An untrusted project is never given the weak locator or a delegated cgroup; it needs `adapter`, or a Job Object on Windows. |
| `process.adapter.program` | Absolute path of the container or virtual-machine adapter. Exactly when `process.boundary` is `adapter`. |
| `process.adapter.root` | Absolute directory the adapter keeps its boundaries under. Exactly when `process.boundary` is `adapter`. |

`cgroup` uses a cgroup v2 sub-tree delegated to the cgroup the server runs in. Start the
server inside one, for example by having the MCP client run
`systemd-run --user --scope -p Delegate=yes --quiet godot-mcp serve --stdio ...`: the
server moves itself into its own child cgroup there and creates every launch's cgroup
beside it, with process, memory and CPU limits. A server not started in a delegated
cgroup starts no Godot process and says why in `health.get`. The cgroup limits and
terminates every launch but cannot keep a member from moving itself out of it, so it is
weak containment, for trusted projects only (see
[Process boundaries](process-boundaries.md#untrusted-a-strong-boundary-or-nothing)).

An adapter boundary names its adapter, and only it may:

<!-- docs-check:config expect="accept" -->
```toml
transport = "stdio"

[process]
boundary = "adapter"
trust = "untrusted"

[process.adapter]
program = "/usr/local/bin/godot-mcp-container"
root = "/var/lib/godot-mcp/boundaries"
```

The repository's own adapter for Linux, and the cgroup delegation it needs, are
described under
[the reference namespace adapter](process-boundaries.md#the-reference-namespace-adapter-on-linux).

<!-- docs-check:config expect="reject" -->
```toml
transport = "stdio"

[process]
boundary = "adapter"
```

## Environment variables

Each recognized variable maps to exactly one key, using the same names in upper
snake case under the `GODOT_MCP_` prefix — for example `GODOT_MCP_TRANSPORT`,
`GODOT_MCP_PROFILE`, `GODOT_MCP_HTTP_BIND`, `GODOT_MCP_HTTP_MODE`,
`GODOT_MCP_LOG_SINK`, `GODOT_MCP_DEADLINE_MS`, `GODOT_MCP_PROCESS_BOUNDARY`,
`GODOT_MCP_PROCESS_TRUST`. The adapter's two paths are read from the file only. OIDC
policy lists use
comma-separated values in `GODOT_MCP_HTTP_ALLOWED_ALGORITHMS`,
`GODOT_MCP_HTTP_REQUIRED_SCOPES`, and
`GODOT_MCP_HTTP_REQUIRED_CAPABILITIES`.

Secret indirections have one variable per form, and naming both is an error rather
than a silent precedence rule: `GODOT_MCP_HTTP_LOCAL_SECRET_ENV` names the variable
holding the secret, `GODOT_MCP_HTTP_LOCAL_SECRET_FILE` names a protected file
holding it.

An override that only half-configures HTTP is rejected as incomplete, listing what
is missing. There is no partial HTTP listener.

## Invariants enforced before serving

These are not defaults you can relax; configuration has no key that would let you.

* **The listener is loopback.** A non-loopback `http.bind` is rejected with a
  permission-denied error. External reachability is a reverse proxy's job, never
  this process's.
* **The bridge endpoint is loopback.** Same rule, same reason. Validation is all it
  does today: no listener is started from it — see [`[bridge]`](#bridge).
* **A `trusted_local` secret indirection is mandatory but unconsumed.** The loader
  refuses the mode without one, and the `serve` path never resolves it; the presented
  credential is the server's own per-launch secret — see
  [`http.local_secret`](#httplocal_secret).
* **Stdio serving requires separate stdout and stderr.** Before stdio protocol
  startup, `serve` compares the process stderr object with protocol stdout for every
  diagnostics sink and refuses merged streams (including a shared PTY). This also
  applies with `[log_sink.file]`, because panic and fallback failure reporting still
  write to process stderr. A configured file sink must additionally be a regular file
  whose object identity differs from stdout. Non-protocol commands such as
  `check-config` may share terminal descriptors and do not make this comparison.
* **TLS is not configured here.** There is no certificate or key setting, because
  this process does not terminate TLS. See
  [HTTP: `proxied_oidc`](http-proxied-oidc.md).
* **An `Origin` must carry no path, query or fragment**, and an issuer or external
  resource URL must be absolute HTTPS. A loose match would let a near-miss
  authority through.

## See also

* [Troubleshooting](troubleshooting.md) for the rejection you actually hit.
* [Threat model](../security/threat-model.md) for what each invariant defends.
