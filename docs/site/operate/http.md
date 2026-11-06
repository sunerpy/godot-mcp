---
description: When to serve godot-mcp over HTTP, how its three modes accept a request, and how long a session lasts.
---

# HTTP

stdio is the default and the arrangement to prefer: one client, one process, no socket and no
credential. Turn on HTTP when a second process has to reach a server that is already running, on
this machine or on another machine of your development network.

## Three modes

| Mode | For | A request is accepted when |
| --- | --- | --- |
| `trusted_local` | Another local process run by the same user | It carries the per-launch secret, and its `Host` and `Origin` match the configured ones exactly |
| `open` | A client on another machine of a development network whose machines are yours | It carries the token, when one is set, and no `Origin` other than the configured one |
| `proxied_oidc` | A client on another host, through a reverse proxy on this one | It arrives through the proxy hop with the hop's own credential, carrying a valid token of the one configured end user |

`trusted_local` and `proxied_oidc` bind a loopback address and authenticate every request:
reaching the socket proves nothing about who is calling. `open` binds the address you give it, so
anyone who can reach the port can use every tool of the profile it serves, unless a token is set.

## `trusted_local`

The secret is created fresh for every launch and handed over through an environment variable or a
protected file; it is never written into the configuration:

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

Start the server with `godot-mcp serve --project /path/to/my-game --config /path/to/godot-mcp.toml`;
the MCP endpoint is `/mcp` on the bind address. A non-loopback `bind` is refused by the loader, so
no configuration of this mode is reachable from another host. The
[`trusted_local` reference](../../operator/http-trusted-local.md) covers how the secret is produced
and scoped.

## `open`

One flag starts it, with no configuration file:

```sh
godot-mcp serve --project /path/to/my-game --profile full --http 0.0.0.0:8765
```

`0.0.0.0:8765` binds every interface; one address of this machine, or `127.0.0.1:8765`, narrows it.
The server prints on stderr the URL to dial from this host and from the others, and a warning
while no token is set. A client that takes a URL needs nothing else:

```json
{
  "mcpServers": {
    "godot-mcp": {
      "type": "http",
      "url": "http://127.0.0.1:8765/mcp"
    }
  }
}
```

On another machine, put the `other hosts` URL from the server's output in `url`.

`--http-token-env <VAR>` names an environment variable that holds a token: every request must then
present `Authorization: Bearer <token>`, and one without it is answered HTTP 401. A request that
carries an `Origin` header, which a web page in a browser does, is answered HTTP 403 unless that
origin is configured. The token travels in clear over HTTP, so it keeps out the other devices on
the network, not someone who can watch its traffic. The
[`open` reference](../../operator/http-open.md) has the configuration file, the `Host` and
`Origin` keys and every refusal.

## `proxied_oidc`

The reverse proxy terminates TLS and authenticates the user; the server, still on loopback,
accepts only requests that carry the proxy hop's credential and validates the user's token itself
against the issuer, audience, scopes and signing keys you configure. Token validation refuses
unsigned tokens, unexpected algorithms, keys a token tries to bring with it and every private or
redirected key address. The [`proxied_oidc` reference](../../operator/http-proxied-oidc.md) has the
whole configuration and the residual risk you own.

## Sessions

A client opens an MCP session with its first request and names the session in every request after
it. Two clocks end a session, whichever runs out first:

| Key in `[http]` | Ends a session | When the key is not set |
| --- | --- | --- |
| `session_idle_timeout_ms` | That has received and sent nothing for this many milliseconds | One hour |
| `session_lifetime_ms` | This many milliseconds after it opened, however busy it is | `deadline_ms` × `max_queue` from `[limits]`: 32 minutes with the default limits |

The server then answers HTTP 404 for the session, and the client opens a new one, as the MCP
specification requires. The captures and exports of the old session stay readable from the new one,
because they belong to the client's identity rather than to one session: the launch secret under
`trusted_local`, the launch itself under `open`, the configured user under `proxied_oidc`. They stay
readable until their retention ends, 15 minutes by default. A restart under `trusted_local` or `open`
makes a new identity, so a client of the new launch cannot read what the previous launch captured.

With the defaults the lifetime runs out first. For sessions that last an hour, set both keys:

<!-- docs-check:config expect="accept" -->
```toml
transport = "http"

[http]
bind = "127.0.0.1:8765"
mode = "trusted_local"
host = "127.0.0.1:8765"
origin = "http://127.0.0.1:8765"
session_idle_timeout_ms = 3600000
session_lifetime_ms = 3600000

[http.local_secret.env]
var = "GODOT_MCP_LAUNCH_SECRET"
```

A longer lifetime does not extend a credential: every request still proves its secret or token
before the session is looked up. The [configuration reference](../../operator/configuration.md#session-timing)
has both keys.

godot-mcp 0.4.0 and earlier have no `open` mode: every HTTP listener binds loopback.

godot-mcp 0.3.0 and earlier close a session after five minutes without a message, and let only
the session that produced an artifact read it. A client that reconnected in between could not read
its own capture, and the refusal answered `-32601` (method not found), which some clients report as
the server not supporting resource reads.

## Parity

HTTP calls the same kernel as stdio: the same tools, the same profiles, the same journal and the
same answers. Anything an HTTP client can do, a stdio client of the same server can do, and the
other way round.
