# Threat model

This page states what `godot-mcp` defends, what it explicitly does not, and who owns
the risk that is left. It is written to be usable when deciding whether to point this
server at a project you did not write.

## Posture in one paragraph

`godot-mcp` is a local-first, single-user server. It runs as your account, on your
machine, with your access to your files. Its guarantees are about **accountability and
containment of mutation**: which project it may change, that a change either happened or
did not, that a process it started can always be accounted for, and that every request
that crosses an authenticated transport is authenticated. The one transport that asks for
no credential, or for one shared token, is the `open` HTTP mode, which an operator starts by
name for a development network. It provides no confinement of code execution and does not
claim to.

<!-- lint:quoted-term --> The engine layer states this in its own shipped words, and no page may describe it in stronger terms: `not a code sandbox`, `mutation containment`.

That phrasing is not modesty. Loading a Godot project **is** arbitrary code execution: a
`@tool` script runs inside the editor, a `@tool` autoload runs as soon as the project
loads, an import implies editor mode, and a headless helper run starts every autoload,
`@tool` or not. Godot's own documentation notes the editor
carries no protection against misuse of `@tool` scripts. So the code runs, and the only
useful questions are what it may change and what still bounds it.

## Trust boundaries

Five boundaries. Each is stated with what it enforces, what it does not, and the
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

**Residual risk.** The `godot_mcp` editor add-on runs inside the editor with the editor's own
privileges, as every editor plug-in does. It listens on nothing: the server listens on
loopback, and the add-on dials it and must present the per-launch secret from the private
rendezvous file before one request is served. Any process running as your user can read that
file, so the bridge is no stronger than your user account. The `editor`, `full` and `runtime`
profiles serve it; `runtime` lists only the two tools that run the project in the editor. Its
mutations are typed: scene commands, editing calls drawn from one fixed table of class methods
that change only their receiver's stored state, project settings, the editor's own settings and
shortcuts, saves, closing scenes and script tabs, captures and resource previews, navigation and
the editor's 3D cameras, rescans, answering dialogs, restarting the editor, and starting and
stopping the project. It exposes no arbitrary method call and evaluates no expression. The code it compiles
at runtime is its own logger, from a source constant it carries; only on a session whose
server offered `editor.script.run`, the caller's script; and only on a session whose server
offered `editor.commands.call`, a provider script the project declares. See
[Script execution](#script-execution) and [Project commands](#project-commands). A capture contains
whatever the editor shows, and its log whatever the editor and the project print. A game the
editor runs for `editor.play.start` is the editor's own child process, outside every boundary
this server holds, so it is refused for an untrusted project.

`editor.dialog.respond` presses one button of a dialog the editor shows, and the button does
whatever the editor does for it: it can save, discard or delete project files, or quit or restart
the editor. Nothing answers a dialog on its own; each press is one call that names the dialog and
the button, and it is journaled as covering the whole project, so a lost answer blocks writes
until it is resolved. `editor.restart` ends the editor's process, and Godot starts the restarted
editor as it exits, with the options it passes on and the old editor's environment; neither
editor is inside a boundary this server holds, as no editor you run is. It is refused for an
editor this server runs as a managed session, whose restarted process would leave that session's
boundary. It saves only with `save`, and it never discards: it is refused while the editor
reports an unsaved scene or script, and a question the editor still asks before restarting is
cancelled. From the request until the restarted editor connects, the server sends the editor no
other change and writes none of the project's files. See
[ADR 0013](../contributor/adr/0013-editor-restart.md).

`editor.preferences.set` and `editor.shortcuts.set` are the changes that reach past the
project: the editor writes them to your own editor settings file, which every project it opens
shares. A setting that names a program the editor starts, a path, a host, a port or a
credential, or that makes the editor rescan and import on its own, is refused, by name in the
server and again by name and property hint in the add-on; the ones that hold paths, hosts or
credentials are not read either. Every other setting can be changed, including ones that make
the editor save on its own, whose saves no transaction records. `editor.shortcuts.set` rebinds
the editor's shortcuts in the same file. It starts nothing itself, but a key it binds then
triggers the editor action it names whenever you press it, Run Project among them; every
binding a change made is in its answer, before and after.

The add-on's **Pause agent edits** switch makes the editor refuse, before they start, the
mutations the server sends it. It is a convenience for the person at the editor, not a security
boundary: it holds back only what passes through the editor, while the server's own file,
script, resource and asset writes go on; it lasts only as long as the editor and the add-on
run; and any process running as your user can still change the project's files, the add-on's
own among them. The panel that shows the bridge's activity lists each request's method, target
path and outcome, never its arguments, and copies no secret; the error messages **Copy
diagnostics** includes are the editor's own, and can name a node, a setting or a value the
request gave.
See [Editor bridge](../operator/editor-bridge.md).

### 4. The HTTP boundary

**Enforces.** HTTP is off by default. Under `trusted_local` and `proxied_oidc` the backend
binds loopback only, with no configuration that changes it, and the exact `Host` authority
and `Origin` are matched exactly rather than normalized, which is what stops a local browser
page from driving the server. Under `trusted_local`, a per-launch, user-scoped secret authenticates
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

Under `open` ([ADR 0014](../contributor/adr/0014-open-http-mode.md)) the backend binds the
address the operator gives, asks for no credential or for the one configured token, compared
in constant time, and serves every request as one principal named after the launch, so
sessions and artifacts stay bound to it. It accepts any `Host` unless one is configured, and
refuses before dispatch every request that carries an `Origin` it was not given, so a web
page the developer visits cannot drive it, along with forwarded headers and query strings.
The bridge endpoint and the game probe stay on loopback in this mode too.

**Does not enforce.** TLS. This process terminates none and has no certificate or key
setting. External reachability, the public name and certificate lifecycle belong to a
same-host reverse proxy. Under `open`, who can reach the port: without a token anyone who
can reach it can use every tool of the profile, and a token travels in clear over HTTP.

**Residual risk.** A misconfigured proxy is invisible to this process. Anything able to
write the proxy-hop credential can impersonate your proxy, so treat it like a private
key. One principal means one person; sharing that identity destroys the only attribution
the server has. An `open` listener on a network you do not control serves everyone on it;
the start-up warning says so when no token is set.

### 5. The managed-session boundary

**Enforces.** A managed game session runs the server's own probe script, passed to Godot
on the command line; the project is not modified for it. The probe dials a loopback bridge
private to that one session and must present the session's secret from its private
rendezvous file before one request is served. It answers typed reads, typed synthetic input
(actions, keys, mouse), captures and comparisons with a reference image, and nothing else:
there is no arbitrary method call and no expression evaluation, except the caller's script on a
session whose server offered `runtime.script.run` ([Script execution](#script-execution)). Every
hold expires by itself, and the probe releases
everything it holds when it is stopped, when a sequence is cancelled and when it loses its
server, so a killed server cannot leave input held. A managed editor session hosts the
language server and debug adapter on loopback ports the server allocates; an endpoint is
dialled only when the editor's launch receipt still holds — its process ID and start time,
the binary that process runs, the project root and its generation — and the operating
system's socket table shows the listener is held by that very process. A connect alone is
never taken as ownership. The debug commands expose no expression evaluation, and answers
name project files by `res://` path, never by their absolute location.

**Does not enforce.** What the project's own code does once a game or an editor runs it,
the scenes a test scenario's stress step instances included: it runs with your user's
privileges, as it would if you ran it yourself, and a game the debugger runs inherits the
server's display.

**Residual risk.** Any process running as your user can read the private rendezvous files
and the socket table and can connect to loopback ports, so these sessions are no stronger
than your user account. Between the listener check and the connect, a process of the same
user could in principle replace the listener; every connect repeats both checks, which
narrows but does not close that window. A capture contains whatever the game shows, and a
scenario's report whatever its steps read. A reference image is read through the held project,
never from `.godot`, `.godot-mcp` or `godot_mcp_helpers`, and goes only to the session's probe.
See [Managed sessions](../operator/managed-sessions.md).

An export is a managed session too. It runs the project through the resolved build's own
installed templates only, writes into private storage rather than the project, and its
outputs are served only as artifacts bound to the principal that asked for them. A preset
with a password, passphrase, token, secret or key written into `export_presets.cfg` is
refused before anything runs, and only the option names are ever reported. What an
exported build contains is whatever the project contains, and its smoke run executes it
with your user's privileges. See [Exports](../operator/exports.md).

An Android export runs the operator's configured SDK and JDK where they are and installs
nothing. Godot reads its Android settings from a private home the server writes, never
from yours, and the SDK it sees has a stand-in `adb` that exits with an error, so an export
cannot reach a device. A release export signs only with the keystore Godot's credentials
store names; the server reads whether those options are set and never their values. The
server's own debug keystore, in its private state, carries Android's well-known debug
password: anyone who can read that file can sign an update the device accepts over a
debug install, so it signs debug builds only. Godot writes an export's temporaries under
fixed names in the host's temporary directory, outside every private directory: the
server runs one export at a time, but another program of your user can read or replace
those files while an export runs.

`android.deploy` has an effect outside the project: it installs an app on a device and
starts it, with adb running as your user. It is a journaled mutation, refused before
anything is sent unless it names a finished Android export of the caller's own, and it
reports success only when the install, the launch and a running process were each proven;
an install whose outcome adb did not report is `uncertain` and is never retried. What the
app does on the device is whatever the project does. The adb server a call starts keeps
running after it, as adb's own process, and listens on `127.0.0.1:5037` (measured with
platform-tools 37.0.1), where any local process can reach it and through it every device
it has authorized. A real device has not been exercised; the deploy phases are checked
against a stand-in for adb.

## Script execution

Three tools run caller-written GDScript: `script.run` in a supervised headless Godot,
`editor.script.run` in the connected editor and `runtime.script.run` in a managed game
([ADR 0012](../contributor/adr/0012-declared-script-execution.md)). With
`editor.commands.call`, which runs code the project declares rather than code the caller writes
([Project commands](#project-commands)), they are the only execution surface, and the absence
audit proves it: no other advertised name denotes execution, and `allow_scripts` and
`project_commands` are the only keys that turn anything on.

**Enforces.** Nothing runs unless the operator turned the switch on for the launch
(`--allow-scripts`, or `allow_scripts = true` with `--config`); a call while it is off is
refused with `permission_denied` before anything is written or sent, and the editor add-on and
the runtime probe are not offered the methods, which they check too. The switch is refused
together with `http.mode = "proxied_oidc"`, so a script never runs for a caller the server only
knows through a proxy. It is accepted with `http.mode = "open"`, whose start-up warning says,
when no token is set, that anyone who can reach the port can then run code as you. Every run is a journaled transaction whose audit record carries digests,
never the source; its answer lists every project file that differs from before it; and an
outcome the server cannot prove stays `uncertain`. An unanswered editor run quarantines the
project, because the editor may still save files.

**Does not.** Confine the code. It runs with your privileges, inside the engine process the
tool names, and can read and write any file you can, start processes and open connections.
Only the headless run is ended by its deadline: code in the editor or a game goes on after the
server stops waiting, until it returns, the editor closes or the session stops. Nothing a
script does is undone, and what it does outside the project is not recorded.

**Residual risk.** While the switch is on, the capability profile no longer bounds what can
happen: treat `--allow-scripts` like handing the agent a shell in the project directory, and
leave it off for a project or a client you do not trust.

## Project commands

A project may declare commands in `godot_mcp_commands.json`, each implemented by a `@tool`
GDScript of the project's; `editor.commands.list` lists them and `editor.commands.call` has the
connected editor run one ([ADR 0016](../contributor/adr/0016-project-commands.md),
[Project commands](../operator/project-commands.md)).

**Enforces.** A listing reads the declaration and the provider scripts as files and runs
nothing. A call runs only a command the declaration names, through the provider script it names,
with arguments the command's declared schema accepts and the bridge carries exactly; every check
is made before anything is sent, against the files as they are at that moment, and a refused call
journals nothing. The caller chooses a command and its arguments, never code. The switch,
`project_commands`, is off for `proxied_oidc` unless the operator turns it on, and
`project_commands = false` turns it off for any transport; while it is off nothing is sent, and the
add-on, which is not offered the method, refuses it too. A call the editor began is a journaled
transaction over the whole project: its answer lists every project file that differs from before
it, and an unanswered call quarantines the project, because the editor may still be writing.

**Does not.** Confine the code. A command is the project's own code, run in the editor with your
privileges, and it can do anything a `@tool` script can: read and write any file you can, start
processes, open connections. Its declaration describes it and does not bound it: `mutates: false`
is not enforced, only reported when broken (`declaration_mismatch`), and nothing checks that a
description says what the code does. Code goes on after the server stops waiting, until it
returns or the editor closes, and nothing a command does is undone.

**Residual risk.** The switch is on by default for stdio, `trusted_local` and `open`, so whoever
can call the tools of the `editor` or `full` profile can run every declared command: the agent you
connected,
or, on an `open` listener without a token, anyone who can reach the port. For a project you did
not write, its declaration is part of what you trust, as its `@tool` scripts already are, which
the editor runs as it opens the project and its scenes; read it before you serve the project, or
set `project_commands = false`. The arguments are the caller's: a provider that hands one on to a
file path, a process or an expression runs what the caller sent, and the command's schema is the
only bound the server applies to it.

## A loopback address is never an identity

Stated separately because it is the mistake most worth avoiding. Any local process, running
as any user, can connect to a loopback port. Arrival on that socket tells you nothing at
all about who sent the request. Authentication is what distinguishes your client, and both
authenticated modes require it. The `open` mode does not treat reaching the socket as an
identity either: it asks for no identity, or only for the shared token, and says so when it
starts.

The same reasoning applies to the bridge endpoint, which is loopback-only *and*
rendezvous-authenticated through a private file with a constant-time comparison.

## What is deliberately absent

* **Script execution by default, or for a proxied caller.** The script tools run nothing until
  the operator allows them for a launch, never with `proxied_oidc`, and nothing else executes
  caller code; see [Script execution](#script-execution). `unsafe_script` stays a reserved
  identifier with no capability, no handler, no profile membership, no configuration option
  and no presence in the shipping binary.
* **Calls to project code the project does not declare, or for a proxied caller by default.**
  `editor.commands.call` runs only a command the project declares, and nothing for a
  `proxied_oidc` principal unless the operator turned `project_commands` on; see
  [Project commands](#project-commands).
* **A force or bypass path.** A quarantined transaction is resolved only by a
  proof-backed transition; the repair command re-derives every proof field itself and
  independently re-validates a composite transaction's linked sub-proof.
* **Raw artifact locations.** An artifact is addressable only as an opaque
  `godot-mcp://artifacts/{artifact_id}` resource, bound to project generation, principal,
  retention and size limits. A returned path would be a durable capability that
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
