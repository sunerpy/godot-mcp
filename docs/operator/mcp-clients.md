# MCP clients

The server is a plain MCP peer. Any client that can launch a process and speak
newline-delimited JSON-RPC over its stdin and stdout can use it, and that is the
configuration to prefer: it needs no port, no credential and no network.

Which editor or IDE you use is irrelevant. The client connects to the `godot-mcp`
process, not to the Godot editor.

## stdio (recommended)

Most clients accept a command and arguments. The shape is always the same:

```json
{
  "mcpServers": {
    "godot-mcp": {
      "command": "/opt/godot-mcp/bin/godot-mcp",
      "args": ["serve", "--stdio", "--project", "/srv/projects/my-game", "--profile", "core"]
    }
  }
}
```

Choose the profile deliberately. `core` is read-only; the others add write, run and
export capabilities. See [Profiles and tool outcomes](profiles-and-outcomes.md).

If the client lets you pass environment variables, remember that every
`GODOT_MCP_`-prefixed variable is configuration and an unrecognized one is a
rejection:

```json
{
  "mcpServers": {
    "godot-mcp": {
      "command": "/opt/godot-mcp/bin/godot-mcp",
      "args": ["serve", "--stdio", "--project", "/srv/projects/my-game"],
      "env": { "GODOT_MCP_PROFILE": "core", "GODOT_MCP_LOG_SINK": "stderr" }
    }
  }
}
```

## Agent Skills

Each tool describes itself, but not how tools combine into a job. The Agent Skills godot-mcp
carries do: `godot-mcp skill install` copies them into the skill directories of the agents you
use, and an agent loads one when a task needs it. Install them on the machine the agent runs
on, which over HTTP may not be the machine with the project. [Agent Skills](skills.md) lists
the targets, the commands and which target each client uses.

## HTTP on loopback

Use HTTP only when a second local process genuinely has to reach the same running
server. It is off by default. A `trusted_local` listener is always loopback, and every
request is authenticated — reaching the socket proves nothing about who is calling.

With `trusted_local`, the client presents the per-launch secret:

```sh
curl --silent --show-error \
  --header 'authorization: Bearer <redacted: the per-launch secret>' \
  --header 'content-type: application/json' \
  --data '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"example","version":"0.1.0"}}}' \
  http://127.0.0.1:8765/mcp
```

The path is `http-protocol:/mcp`. See [HTTP: `trusted_local`](http-trusted-local.md)
for how the secret is produced and scoped.

## HTTP on a development network

When the client runs on another machine of a network whose machines are yours — the agent
on one computer, the Godot project on another — start the `open` mode instead. It binds the
address you give and asks for no credential, or for one fixed token:

```sh
godot-mcp serve --project /srv/projects/my-game --profile full --http 0.0.0.0:8765
```

The server prints the URL another host dials. A client that takes a URL needs nothing else
when no token is set:

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

Anyone who can reach the port can use every tool of the profile. With
`--http-token-env <VAR>` every request must present `Authorization: Bearer <token>`. See
[HTTP: `open`](http-open.md) for the token, the `Host` and `Origin` checks and what the mode
gives up.

## Through a same-host reverse proxy

When a client beyond a network you trust has to reach the server, put a reverse proxy
in front. The proxy terminates TLS, authenticates the end user, and forwards to the
loopback backend with its own proxy-hop credential. The backend still binds loopback
only; nothing about this arrangement exposes the listener.

The configuration below is illustrative and uses reserved example names throughout.
Every credential value is redacted; substitute your own out-of-band.

```nginx
server {
  listen 443 ssl;
  server_name godot-mcp.example.invalid;

  ssl_certificate     /etc/ssl/godot-mcp.example.invalid.pem;   # <redacted>
  ssl_certificate_key /etc/ssl/godot-mcp.example.invalid.key;   # <redacted>

  location /mcp {
    proxy_pass http://127.0.0.1:8765/mcp;
    proxy_set_header Host godot-mcp.example.invalid;
    proxy_set_header Origin https://godot-mcp.example.invalid;
    proxy_set_header x-godot-mcp-proxy-hop "<redacted: the proxy-hop credential>";
    proxy_set_header Authorization $http_authorization;
  }
}
```

Two headers matter and neither is optional:

* `http-protocol:x-godot-mcp-proxy-hop` proves the request came through *your*
  proxy rather than from anything else that can open a loopback socket.
* the end user's bearer token is what identifies the person, and the backend accepts
  exactly one frozen issuer/subject pair.

A client then talks to the proxy, not to the backend:

```sh
curl --silent --show-error \
  --header 'authorization: Bearer <redacted: the end-user OIDC token>' \
  --header 'content-type: application/json' \
  --data '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}' \
  https://godot-mcp.example.invalid/mcp
```

See [HTTP: `proxied_oidc`](http-proxied-oidc.md) for the full contract, including
what the backend does when either half of that pair is missing.

## What a client should expect

* `initialize` is answered by the shared kernel; `serverInfo` names the server,
  `godot-mcp-server`, and its version, the one the first line of `godot-mcp version`
  prints.
* The server speaks MCP revisions 2024-11-05 through 2025-11-25, over the
  `initialize` handshake. A client that asks for a later revision is answered with
  2025-11-25.
* The 2026-07-28 revision's per-request lifecycle is not served. `server/discover` is
  answered "method not found" (`-32601`), as a server that predates it answers, so a
  client that probes falls back to `initialize`. A tool, resource or prompt request
  that carries its client context in its own `_meta` instead is refused as
  `unsupported` (`-32600`) on stdio and over HTTP, before the kernel sees it, so over
  HTTP none is served outside a session that `initialize` opened. Measured over stdio
  on Linux x86_64 with the official MCP Python SDK 2.3.0, whose default `auto` mode
  probes first: the client fell back to `initialize` and negotiated 2025-11-25.
  Pinned to 2026-07-28, it could not connect.
* `tools/list` and `resources/list` reflect the selected profile exactly. A tool is
  listed because it is in the profile, not because it happens to be reachable right
  now.
* `prompts/list` holds `godot-mcp.profile-guide` in every profile and, in `editor` and
  `full`, one `godot-mcp.recipe.<name>` prompt per recipe, each with the recipe's
  parameters, a call template and the operations it becomes ([Recipes](recipes.md)).
* A failure is a typed outcome, not free text. The vocabulary is in
  [Outcome taxonomy](../protocol/error-taxonomy.md).
* A long operation reports progress and honours cancellation; a cancelled request
  observes cancellation rather than being dropped mid-effect.
