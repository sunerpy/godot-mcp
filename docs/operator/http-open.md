# HTTP: `open`

`open` serves MCP over HTTP on any address, for a development network: the Godot editor on
one machine and the agent on another, a container next to its host, a second computer at the
same desk. It asks for no credential, or for one fixed token, so a client needs the URL and
nothing else. [ADR 0014](../contributor/adr/0014-open-http-mode.md) records why the mode
exists and what it gives up.

Use it on a network whose machines are yours. Anyone who can reach the port can use every tool
of the profile it serves. Beyond such a network, keep the server on loopback with
[`trusted_local`](http-trusted-local.md), or put a reverse proxy that terminates TLS in front
of it with [`proxied_oidc`](http-proxied-oidc.md).

## Start it

One flag starts it, without a configuration file:

```sh
godot-mcp serve --project /path/to/my-game --profile full --http 0.0.0.0:8765
```

`--http <ADDR>` takes the address and port to bind: `0.0.0.0:8765` or `[::]:8765` for every
interface, one address of this machine, or `127.0.0.1:8765` for this host only. It cannot be
combined with `--config` or `--workspace`, and the address must be an IP address and a port.
The server prints on stderr where to connect:

```text
godot-mcp serve: HTTP (open) listening on 0.0.0.0:8765, every interface
godot-mcp serve: this host: http://127.0.0.1:8765/mcp
godot-mcp serve: other hosts: http://192.0.2.10:8765/mcp
godot-mcp serve: warning: no token; anyone who can reach this port can use every tool of the full profile
```

The `other hosts` line names the address this machine reaches the rest of the network from.
When the machine has no such route the line reads
`http://<this host's LAN address>:8765/mcp`; fill in the address its network settings show. A
listener bound to one address prints that address's URL; one bound to loopback prints no
warning, since only this machine reaches it. With `--allow-scripts`, and no token, a second
warning says that anyone who can reach the port can also run GDScript with your privileges.

## Require a token

`--http-token-env <VAR>` names an environment variable holding a token. The server reads it
once, at start: a variable that is unset or empty refuses the launch, and every request must
then present `Authorization: Bearer <token>`. A request without it, or with another value, is
answered HTTP 401, and the start-up warning is not printed.

```sh
export GODOT_MCP_LAN_TOKEN='<redacted: a token you chose>'
godot-mcp serve --project /path/to/my-game --profile full \
  --http 0.0.0.0:8765 --http-token-env GODOT_MCP_LAN_TOKEN
```

The variable's name is yours to choose, and the launch exits with status 2, naming it, when it
is unset or holds only spaces. As with stdio, a launch without `--config` reads no other
`GODOT_MCP_` variable; `--profile`, `--allow-scripts` and the other flags of `serve` apply as
usual.

The token travels in clear over HTTP. It keeps out the other devices on a network, not someone
who can watch its traffic.

## Configure it in a file

The same mode from `--config`, with every optional key set:

<!-- docs-check:config expect="accept" -->
```toml
transport = "http"

[http]
bind = "0.0.0.0:8765"
mode = "open"
host = "192.0.2.10:8765"
origin = "http://192.0.2.10:8765"

[http.token.env]
var = "GODOT_MCP_LAN_TOKEN"
```

* `bind` is any address and port.
* `token` is optional. It is an environment variable (`[http.token.env]`) or a file
  (`[http.token.file]` with `path`) holding the token, never the token itself. A token file
  must be a regular file you own that no other user can access, reached through no symbolic
  link; line breaks at its end are ignored.
* `host` is optional. Without it any `Host` is accepted; with it only that exact authority,
  port included, and anything else is answered HTTP 403.
* `origin` is optional. Without it a request that carries an `Origin` header is answered HTTP
  403; with it only that exact origin passes.
* `session_idle_timeout_ms` and `session_lifetime_ms` work as in the other modes
  ([Session timing](configuration.md#session-timing)).

The other modes' keys are refused here, and this mode's `token` is refused in theirs:

<!-- docs-check:config expect="reject" -->
```toml
transport = "http"

[http]
bind = "0.0.0.0:8765"
mode = "open"

[http.local_secret.env]
var = "GODOT_MCP_LAUNCH_SECRET"
```

`GODOT_MCP_HTTP_TOKEN_ENV` and `GODOT_MCP_HTTP_TOKEN_FILE` set the token from the environment
alongside `--config`, as `GODOT_MCP_HTTP_MODE=open` sets the mode.

## What it refuses

* **An `Origin` it was not given.** A web page can send a request to any address its browser
  reaches, LAN addresses included, and the browser attaches the page's `Origin`. MCP clients
  that are not web pages send none. A client that runs in a browser needs its origin named in
  `origin`.
* **Forwarded headers and a query string**, answered HTTP 400 as in the other modes. The
  listener does not trust a proxy it was not told about, and a credential never travels in a
  URL.
* **Every other mode's credential.** It publishes no launch secret and reads no proxy hop.

## Who a request is

The listener cannot tell its callers apart, so it does not pretend to: it serves every request
as one principal named after the launch. Sessions, tasks and artifacts stay bound to it as they
are to the launch secret's principal under `trusted_local`, so a client that reconnects reads
the artifacts it captured. A restart makes a new principal. `health.get` reports
`http.mode` as `open`, and the journal and the audit log record the mode, never the token.

## Connect a client

On this host, dial the loopback URL; on another host, dial the `other hosts` URL. A client that
takes a URL needs nothing else when no token is set:

```json
{
  "mcpServers": {
    "godot-mcp": {
      "type": "http",
      "url": "http://192.0.2.10:8765/mcp"
    }
  }
}
```

With a token, the client presents it in its headers:

```json
{
  "mcpServers": {
    "godot-mcp": {
      "type": "http",
      "url": "http://192.0.2.10:8765/mcp",
      "headers": { "Authorization": "Bearer <redacted: the token>" }
    }
  }
}
```

The keys a client uses for the URL and the headers differ between clients; the server needs
only the URL and, when a token is set, the `Authorization` header. A request from the command
line:

```sh
curl --silent --show-error \
  --header 'authorization: Bearer <redacted: the token>' \
  --header 'content-type: application/json' \
  --header 'accept: application/json, text/event-stream' \
  --data '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"example","version":"0.1.0"}}}' \
  http://192.0.2.10:8765/mcp
```

## What stays the same

The editor bridge and the runtime probe stay on loopback; the server reaches them on its own
machine. `serve` still defaults to stdio, so nothing listens unless `--http` or a configuration
file asks for it. The same kernel answers stdio and every HTTP mode, so a tool answers the
same way over each.
