# HTTP: `trusted_local`

`trusted_local` is the simpler of the two HTTP modes. Use it when a second process
on the same machine has to reach a server that is already running, and stdio cannot
be shared between them.

It is not the default and not a step up from stdio. If one client is talking to the
server, stay on [stdio](quickstart-stdio.md): fewer moving parts, no credential, no
socket.

## What it is

* The listener binds a loopback address. A non-loopback bind is rejected by
  `check-config`, so there is no configuration in which this process is reachable
  from another host.
* Every request carries a **per-launch secret** scoped to the user who started the
  server. A new launch means a new secret; a secret from a previous launch is not
  accepted.
* The exact `Host` authority and `Origin` are configured and matched exactly. A
  near-miss authority is refused rather than normalized into a match, which is what
  stops a browser page on the same machine from driving the server.
* The MCP path is `http-protocol:/mcp`.

## What it is not

Reaching the loopback socket is not an authorization. Any local process running as
any user can connect to a loopback port; what distinguishes your client is the
secret it presents, and nothing else. The
[threat model](../security/threat-model.md) states this as a boundary rather than as
a caveat.

## Configuration

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

The secret is an indirection, never a value in the file. The loader requires one under
this mode and accepts two forms — an environment variable name, or the path of a
protected file:

<!-- docs-check:config expect="accept" -->
```toml
transport = "http"

[http]
bind = "127.0.0.1:8765"
mode = "trusted_local"
host = "127.0.0.1:8765"
origin = "http://127.0.0.1:8765"

[http.local_secret.file]
path = "/run/user/1000/godot-mcp/launch-secret"
```

Put the secret file in a directory only your account can read. The loader validates the
indirection's shape here; the `serve` path never resolves it, so nothing is read from
the variable or the file. See
[`http.local_secret`](configuration.md#httplocal_secret) for the whole truth about that
key before you plan around it.

The mode-specific keys of the other mode are forbidden here. A `principal` or a
`proxy_hop` under `trusted_local` is a rejection, not a no-op:

<!-- docs-check:config expect="reject" -->
```toml
transport = "http"

[http]
bind = "127.0.0.1:8765"
mode = "trusted_local"
host = "127.0.0.1:8765"
origin = "http://127.0.0.1:8765"

[http.local_secret.env]
var = "GODOT_MCP_LAUNCH_SECRET"

[http.principal]
issuer = "https://issuer.example.invalid"
subject = "operator@example.invalid"
```

## Making a request

Start the opt-in listener:

```sh
godot-mcp serve --config ./godot-mcp.toml --project /srv/projects/my-game
```

The server reports the launch-secret path on `stderr`; under the project it is
`.godot-mcp/http-launch-secret`. **The server publishes that secret itself**: each
launch draws 256 bits from the OS CSPRNG, writes them there owner-readable only, and
replaces whatever a previous launch left. The configured `local_secret` is an
indirection the frozen configuration contract requires; the serving path does not
resolve it and never adopts its value as the launch credential. Read the published
file through your client's protected credential mechanism and substitute it only at
request time.

```sh
curl --silent --show-error \
  --header 'authorization: Bearer <redacted: the per-launch secret>' \
  --header 'content-type: application/json' \
  --data '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}' \
  http://127.0.0.1:8765/mcp
```

A request without the secret, or with a stale one, is refused with
`permission_denied`. A request whose `Host` or `Origin` does not match exactly is
refused for the same reason.

## Parity with stdio

The same kernel serves both transports, so a tool answers identically over stdio
and over authenticated HTTP. That includes artifact reads: an artifact resource is
addressed the same way and is bound to the same project generation and principal
either way. See [Artifacts](artifacts.md).

## When you actually need more

If the client cannot run on this host, do not widen this mode. Put a same-host
reverse proxy in front and switch to
[`proxied_oidc`](http-proxied-oidc.md), which is the only supported way to serve a
request that originates off-host.
