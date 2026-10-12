# Threat model

This page states what `godot-mcp` defends, what it explicitly does not, and who owns
the risk that is left. It is written to be usable when deciding whether to point this
server at a project you did not write.

## Posture in one paragraph

`godot-mcp` is a local-first, single-user server. It runs as your account, on your
machine, with your access to your files. Its guarantees are about **accountability and
containment of mutation**: which project it may change, that a change either happened or
did not, that a process it started can always be accounted for, and that every request
that crosses a transport is authenticated. It provides no confinement of code execution
and does not claim to.

<!-- lint:quoted-term --> The engine layer states this in its own shipped words, and no page may describe it in stronger terms: `not a code sandbox`, `mutation containment`.

That phrasing is not modesty. Loading a Godot project **is** arbitrary code execution: a
`@tool` script runs inside the editor, a `@tool` autoload runs as soon as the project
loads, and an import implies editor mode. Godot's own documentation notes the editor
carries no protection against misuse of `@tool` scripts. So the code runs, and the only
useful questions are what it may change and what still bounds it.

## Trust boundaries

Four boundaries. Each is stated with what it enforces, what it does not, and the
residual risk you keep.

### 1. The file boundary

**Enforces.** Authority comes from held descriptors, not from path text. The project
root and its `project.godot` are identified by the stable operating-system object
identity of the handles actually held. Two spellings of the same object converge to one
lease; a spelling later re-pointed at a different object is refused. Any symbolic-link
component is refused outright, even when it currently resolves to the same object,
because a link target can change between the check and the write. Supported junctions
and bind mounts are the one exception, and their alias evidence is recorded at
acquisition and re-verified before every effect. Handles are non-inheritable, so a child
process never receives the server's authority. Writes are compare-and-swap: a create
publishes with a non-replacing rename, a replace binds the target's identity *and* its
bytes and fails closed where the platform offers no atomic exchange.

**Does not enforce.** Anything outside the leased project. The server runs as your
account and can read and write whatever your account can. It does not restrict the
filesystem reachable by Godot or by project code it starts.

**Residual risk.** You, your editor, or any other program can write the same files
concurrently; the lease cannot arbitrate what it does not mediate. An absolute-path write
performed by project code lands wherever your account can write. See
[Project leases](../operator/project-leases.md).

### 2. The process boundary

**Enforces.** Every Godot process is created inside a containment boundary whose
identity is durably recorded **before** the process exists, so no crash phase can leave a
process that nothing durable knows about. The operator chooses the boundary and declares
whether the project is trusted in `[process]`; nothing falls back to a weaker boundary
than the one chosen. Untrusted execution requires a strong boundary
— a Windows Job Object with no-breakaway and kill-on-close, a Linux cgroup v2 sub-tree,
or a configured container or virtual-machine adapter — and **fails closed** when none is
available. Trusted execution may use a documented weak boundary whose identity is a
five-part tuple (process id, start time, process group id, a 256-bit nonce, the control
endpoint) plus the control socket's inode; reconciliation demands an exact match on all
of it. A bare process group id is never identity and never proof, because process ids are
reused and a group says nothing about which program is in it. A process group is a
membership and cleanup mechanism and nothing more.

**Does not enforce.** What the process executes, reads, or connects to. A boundary of
this kind bounds membership and resources; it is not a restriction on code.

**Residual risk.** Cleanup covers descendants that remain **within** the boundary. Code
that deliberately detaches out of it is outside the guarantee, and containing that needs
stronger external isolation than this provides. A delegated cgroup is only as strong as
the delegation: any process of the same user can write to a cgroup delegated to that user.
The Windows Job Object is the Windows default and has run on real Windows hosts: the
launch-protocol and session tests in a Windows Server 2022 container, and every
real-client check on a Windows Server 2022 desktop session. A Job Object is
kill-on-close, so a server that is killed takes its sessions with it. See
[Process boundaries](../operator/process-boundaries.md#what-has-been-executed-where).

### 3. The plug-in and asset boundary

**Enforces.** The release archive carries exactly one GDScript asset — the bridge
protocol codec — plus the bridge wire schema and an example configuration. Installation
writes only into one owned subdirectory, refuses an occupied path on first install, backs
up a file you modified before replacing it, and on removal deliberately keeps a file whose
contents no longer match the install manifest. A payload path outside the install root is
rejected by construction. No archive contains a dynamic library or a native extension
descriptor, and the release-graph audit checks the workspace dependency tables, every
member's feature tables, the dependency lock, the executable's linked-crate closure, every
archive entry, each archive's embedded manifest and the SBOM components for one.

**Does not enforce.** Anything about other plug-ins already in your project. If your
project has its own `@tool` code, that code runs when the project is loaded, and this
boundary has no bearing on it.

**Residual risk.** The `godot_mcp` editor add-on runs inside the editor with the editor's
own privileges, as every editor plug-in does. It listens on nothing: the server listens on
loopback, and the add-on dials it and must present the per-launch secret from the private
rendezvous file before one request is served. Any process running as your user can read that
file, so the bridge is no stronger than your user account. Its mutations are the typed scene
commands only; it exposes no arbitrary method call. See
[Editor bridge](../operator/editor-bridge.md).

### 4. The HTTP boundary

**Enforces.** HTTP is off by default. The backend binds loopback only, in every mode,
with no configuration that changes it. The exact `Host` authority and `Origin` are
matched exactly rather than normalized, which is what stops a local browser page from
driving the server. Under `trusted_local`, a per-launch, user-scoped secret authenticates
each request. Under `proxied_oidc`, two independent facts must both hold: an authenticated
proxy-hop credential proving the request came through your proxy, and a validated bearer
token whose issuer and subject equal the one configured principal exactly. Token handling
fails closed in a fixed order — forbidden header policy first (asymmetric algorithms only,
never `none` or HMAC; `crit`, `jku`, `x5u`, embedded `jwk` and `x5c` refused), then keys
from server-side configuration only with `kid` used purely as a lookup constraint and any
ambiguity refused, then key-metadata agreement, then signature verification, and only then
claims. No token value is ever authority over which URL, key or algorithm is trusted.
Outbound key fetches ignore ambient proxy environment variables while keeping the original
hostname, SNI and certificate verification.

**Does not enforce.** TLS. This process terminates none and has no certificate or key
setting. External reachability, the public name and certificate lifecycle belong to a
same-host reverse proxy.

**Residual risk.** A misconfigured proxy is invisible to this process. Anything able to
write the proxy-hop credential can impersonate your proxy, so treat it like a private
key. One principal means one person; sharing that identity destroys the only attribution
the server has.

### 5. The managed-session boundary

**Enforces.** A managed game session runs the server's own probe script, passed to Godot
on the command line; the project is not modified for it. The probe dials a loopback bridge
private to that one session and must present the session's secret from its private
rendezvous file before one request is served. It answers typed reads, typed synthetic input
(actions, keys, mouse) and captures, and nothing else: there is no arbitrary method call
and no expression evaluation. Every hold expires by itself, and the probe releases
everything it holds when it is stopped, when a sequence is cancelled and when it loses its
server, so a killed server cannot leave input held. A managed editor session hosts the
language server and debug adapter on loopback ports the server allocates; an endpoint is
dialled only when the editor's launch receipt still holds — its process ID and start time,
the binary that process runs, the project root and its generation — and the operating
system's socket table shows the listener is held by that very process. A connect alone is
never taken as ownership. The debug commands expose no expression evaluation, and answers
name project files by `res://` path, never by their absolute location.

**Does not enforce.** What the project's own code does once a game or an editor runs it:
it runs with your user's privileges, as it would if you ran it yourself, and a game the
debugger runs inherits the server's display.

**Residual risk.** Any process running as your user can read the private rendezvous files
and the socket table and can connect to loopback ports, so these sessions are no stronger
than your user account. Between the listener check and the connect, a process of the same
user could in principle replace the listener; every connect repeats both checks, which
narrows but does not close that window. A capture contains whatever the game shows. See
[Managed sessions](../operator/managed-sessions.md).

An export is a managed session too. It runs the project through the resolved build's own
installed templates only, writes into private storage rather than the project, and its
outputs are served only as artifacts bound to the session that asked for them. A preset
with a password, passphrase, token, secret or key written into `export_presets.cfg` is
refused before anything runs, and only the option names are ever reported. What an
exported build contains is whatever the project contains, and its smoke run executes it
with your user's privileges. See [Exports](../operator/exports.md).

## A loopback address is never an identity

Stated separately because it is the mistake most worth avoiding. Any local process, running
as any user, can connect to a loopback port. Arrival on that socket tells you nothing at
all about who sent the request. Authentication is what distinguishes your client, and it is
required in every HTTP mode — there is no mode in which reaching the socket is sufficient.

The same reasoning applies to the bridge endpoint, which is loopback-only *and*
rendezvous-authenticated through a private file with a constant-time comparison.

## What is deliberately absent

* **Arbitrary script execution.** `unsafe_script` is a reserved identifier with no
  capability, no handler, no profile membership, no configuration option and no presence in
  the shipping binary. This is the single largest deliberate difference from tools that
  expose an "execute this GDScript" entry point.
* **A force or bypass path.** A quarantined transaction is resolved only by a
  proof-backed transition; the repair command re-derives every proof field itself and
  independently re-validates a composite transaction's linked sub-proof.
* **Raw artifact locations.** An artifact is addressable only as an opaque
  `godot-mcp://artifacts/{artifact_id}` resource, bound to project generation, principal,
  session, retention and size limits. A returned path would be a durable capability that
  bypasses every one of those checks.
* **In-process TLS.** No certificate setting exists, so no misconfiguration of one is
  possible here.
* **A shared or hosted deployment model.** This is one user, one machine, one project
  lease at a time. It is not built for, and must not be operated as, a service serving
  multiple people.

## Secret handling

Configuration stores indirections only — an environment variable name or a protected file
path. An inline credential cannot deserialize into that position, so pasting one is a parse
error rather than a working configuration containing a secret. Resolved secret material is
wrapped in types whose `Debug` and `Display` are always redacted. Evidence records carry a
command, an exit code, an output byte count and an output hash, never raw output. Panic
output goes through a redacting hook and formats neither payloads nor source locations.
Diagnostics can only reach `stderr` or a bounded rolling file; a sink resolving to a stdout
alias is refused.

## Supply chain

Every direct dependency is pinned to an exact version. TLS is rustls only; OpenSSL and
native-tls are absent from the graph. The toolchain and the two audit tools are recorded in
a committed lock with their source URL, package checksum and installed executable digest. A
software bill of materials is derived from the committed dependency lock rather than from an
ambient environment, and ships in the archive.

## What is not verified on the authoring host

Stated so it is not mistaken for a claim:

* The Windows Job Object containment path is executed on Windows hosts, never here;
  see [Process boundaries](../operator/process-boundaries.md#what-has-been-executed-where).
* The macOS and Windows release archives have never been built here.
* No Godot compatibility row has been executed against a real Godot installation here; see
  the [compatibility matrix](../operator/version-matrix.md).

Each of these is a required external check before a release, and none of them is implied by
anything on this page.

## Reporting a problem

Report security issues privately to the project's maintainers rather than in a public
tracker, and include the exact version from `godot-mcp version` and the configuration with
every secret indirection left as a name rather than a value.
