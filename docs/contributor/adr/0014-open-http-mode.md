# 0014 — An open HTTP mode for development networks

**Status:** accepted for release 0.5.0. It supersedes [ADR 0002](0002-loopback-only-http.md)
in part, for the opt-in `open` mode only, and amends
[ADR 0012](0012-declared-script-execution.md).

## Context

ADR 0002 keeps every HTTP listener on loopback: a client on another machine reaches the
server only through a reverse proxy that authenticates the user. That is the right shape for
a server exposed beyond the people who run it, and it is more setup than a development
network needs: the Godot editor on one machine and the agent on another, a container next to
the host, a second computer at the same desk.

The project owner asked on 2026-11-07 for HTTP that binds `0.0.0.0`, so a server can be used
across a local network, with light security: it is used in development, and convenience comes
first.

## Decision

A third HTTP mode, `http.mode = "open"`, started from a configuration file or with
`godot-mcp serve --http <ADDR>`:

- **It binds any address**: `0.0.0.0` or `::` for every interface, one address of the
  machine, or loopback.
- **It asks for no credential, or for one fixed token.** No launch secret is published. When
  a token is configured (`http.token`, an environment-variable or file indirection, or
  `--http-token-env <VAR>`), it is read once at start, a token that is missing or empty
  refuses the launch, and every request presents `Authorization: Bearer <token>`, compared in
  constant time. Without one, a request needs no credential.
- **It accepts any `Host`** unless `http.host` names the one to accept, matched exactly.
- **It refuses what a web page or a proxy sends.** A request carrying an `Origin` header is
  refused unless it equals `http.origin`. Forwarded headers and a query string are refused as
  in the other modes.
- **It serves every request as one principal named after the launch.** Sessions, tasks and
  artifacts stay bound to it as they are to the launch secret's principal under
  `trusted_local`. The journal, the audit log and `health.get` record the mode as `open`.
- **It says where to connect, and warns.** At start it prints the URL a client on this host
  dials and, for a wildcard bind, the one another host dials. When the port is reachable from
  other hosts and no token is set, it warns that anyone who can reach the port can use every
  tool of the profile, and says so again for script execution when that is on.
- **It accepts script execution.** `allow_scripts` is accepted with the open mode (the
  amendment to ADR 0012 below).

What does not change: `trusted_local` and `proxied_oidc` keep their loopback-only bind and
their credentials; the editor bridge and the runtime probe stay on loopback; this process
still terminates no TLS; and `serve` still defaults to stdio, so nothing listens unless asked.

## Why

**The proxy is the cost the owner asked to drop.** On a development network the machines that
can reach the port are the developer's own, and ADR 0002's proxy, certificate and principal
are setup with no one to protect against. The mode exists for that case and is named for
what it is.

**The Origin refusal stays because it costs MCP clients nothing.** A web page the developer
visits can send requests to any address the browser reaches, LAN addresses included, and the
browser attaches the page's `Origin` to every cross-origin request. MCP clients do not send
one. Refusing every request that carries an `Origin` the operator did not name removes the one
attacker that does not need to be on the network, without asking anything of a real client.

**The token is optional, not mandatory.** A mandatory token adds a step to every launch in the
case the mode was asked for. A shared network (an office, a café, a conference) is a different
case, and the token is one flag away; the start-up warning names the risk when it is off.

**A launch principal, not the token's identity.** The server cannot tell callers apart, so it
does not pretend to: one principal per launch keeps the session and artifact bindings intact
without putting the token into identities, the journal or the audit log.

## What ADR 0002 keeps and loses

- **Superseded, for the open mode only:** "the backend binds a loopback address in every
  mode" and "every HTTP mode authenticates". An open listener without a token authenticates
  nobody, by the operator's choice.
- **Kept:** `trusted_local` and `proxied_oidc` bind loopback only, a non-loopback bind is still
  refused for them by configuration validation and again before the socket is bound, and TLS
  still lives in a proxy.
- **Kept, and still true of the open mode:** reaching the socket is never treated as identity.
  The open mode does not trust who connected; it asks for no identity, or for the shared token.

## Amendment to ADR 0012

ADR 0012 refuses `allow_scripts` with `proxied_oidc`, so a script never runs for a caller the
server only knows through a proxy. That refusal stays. The open mode accepts `allow_scripts`:
the operator who started the server decided who can reach it, and the start-up warning says
that anyone who can reach the port can then run code as the user.

## Consequences

- With no token, anyone who can reach the port can use every tool of the profile: read the
  project, change it, run it and, with `--allow-scripts`, run code with the user's privileges.
  That is like sharing the project directory with write access to everyone on the network.
- The token travels in clear over HTTP. Anyone who can see the traffic can read it, so it
  keeps out the other devices on a network, not someone watching it. Beyond a development
  network, use `trusted_local` behind a proxy that terminates TLS (ADR 0002).
- The documentation lint no longer refuses an example that binds the unspecified address; it
  refuses one that dials it (`unspecified-destination`), since no client can connect to
  `0.0.0.0` or `[::]`.

## Alternatives rejected

**Keep loopback and document SSH port forwarding.** It works and stays secure, and it is the
per-session setup the owner asked to be rid of. It remains available: `trusted_local` is
unchanged.

**A mandatory token.** Rejected above: one more step on every launch for the case the mode
exists for.

**Accept any `Origin` as well.** That would let any page the developer visits drive the server
from inside the browser.

**TLS in this process.** Rejected for the reasons in ADR 0002, which still hold.

## Enforcement

- The configuration loader accepts the open mode on any address, with or without a token,
  Host or Origin, refuses the other modes' credentials in it and its token in theirs, and
  keeps the bridge on loopback (`crates/godot-mcp-domain/tests/config_layering.rs`;
  `crates/godot-mcp-cli/tests/check_config.rs`).
- The listener serves a wildcard bind without a credential, admits the token and nothing else
  when one is set, refuses an `Origin`, forwarded headers and a query string before dispatch,
  and only the open mode binds beyond loopback (`crates/godot-mcp-server/tests/http_open.rs`).
  A module self-audit pins that every bind is checked for loopback before the socket is
  bound, with the open mode as the only exemption (`crates/godot-mcp-server/src/http.rs`).
- `serve --http` prints where to connect and its warnings, requires the token it names, and
  is refused with `--config`, `--workspace` or an unset token variable
  (`crates/godot-mcp-cli/tests/serve_http_open.rs`, `crates/godot-mcp-cli/src/serve_banner.rs`).
