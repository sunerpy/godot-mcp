# HTTP: `proxied_oidc`

`proxied_oidc` is the only supported way to serve a request that originates off this
host. It does not change where the backend listens: the backend still binds loopback
only. What changes is who is allowed to hand it a request, and how the end user is
identified.

Two independent facts must both hold before a request is dispatched. Either one
missing is a refusal, not a downgrade.

1. **The request came through your proxy.** The proxy presents an authenticated
   proxy-hop credential. This is what distinguishes your reverse proxy from any
   other local process that can open a loopback socket.
2. **The end user is the one configured principal.** The bearer token is validated
   and must carry exactly the configured `(issuer, subject)` pair. One principal is
   the whole allowance — this is a single-user server, and there is no key that turns
   it into anything else.

## Where TLS lives

The proxy terminates TLS. This process has no certificate setting, no key setting
and no TLS listener, and there is no configuration that would give it one. That is
deliberate: TLS termination, certificate rotation and the public name are the
proxy's job, and keeping them out of this process removes a whole class of
misconfiguration from it.

## Configuration

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

The shipped backend receives requests over loopback TCP, so use a protected
credential file:

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

The configuration schema retains the socket-path proxy-hop form for its frozen
contract, but a TCP listener cannot derive Unix-socket or named-pipe provenance
from an HTTP request. `serve` therefore refuses that form at startup rather than
trusting a client-supplied marker. A future socket-backed listener may activate it
only when the transport itself can establish the peer provenance.

`external_resource_url` is the externally visible HTTPS URL clients use. It must be
absolute HTTPS and must not be a loopback or unspecified address — a public resource
identifier that resolves to loopback would be meaningless to the client resolving
it.

The three policy lists are optional only because omission has narrow, documented
defaults: `allowed_algorithms = ["RS256"]`, `required_scopes = ["godot.read"]`, and
`required_capabilities = ["read"]`. An explicit empty list, duplicate value, invalid
scope token, unknown algorithm, or unknown capability is rejected at configuration
load. Algorithms can only narrow the hard asymmetric set `{RS256, PS256, ES256}`.
Required capabilities must be granted by the profile selected by `serve`; an
incompatible profile is rejected before the listener binds.

Incomplete is refused. A `proxied_oidc` section without a proxy-hop credential, a
principal, or the external resource URL is rejected with the missing field named:

<!-- docs-check:config expect="reject" -->
```toml
transport = "http"

[http]
bind = "127.0.0.1:8765"
mode = "proxied_oidc"
host = "godot-mcp.example.invalid"
origin = "https://godot-mcp.example.invalid"
external_resource_url = "https://godot-mcp.example.invalid/mcp"

[http.principal]
issuer = "https://issuer.example.invalid"
subject = "operator@example.invalid"
```

## The proxy hop

The header the backend reads is `http-protocol:x-godot-mcp-proxy-hop`. Your proxy
sets it; nothing else legitimately does.

```nginx
location /mcp {
  proxy_pass http://127.0.0.1:8765/mcp;
  proxy_set_header Host godot-mcp.example.invalid;
  proxy_set_header Origin https://godot-mcp.example.invalid;
  proxy_set_header x-godot-mcp-proxy-hop "<redacted: the proxy-hop credential>";
  proxy_set_header Authorization $http_authorization;
}
```

Strip any inbound copy of that header at the edge. A client must never be able to
supply it.

## Token validation

The order is fixed and fails closed at the first step that does not hold.

1. **Header policy first.** The algorithm must be one of the configured
   asymmetric-only allowlist. `none` and every HMAC algorithm are refused. A token
   carrying `crit`, `jku`, `x5u`, an embedded `jwk` or an `x5c` chain is refused
   before anything else happens — a token must never influence which key or URL is
   trusted.
2. **Keys come from server-side configuration only.** The key set is fetched from
   the configured location, never from anything the token names. The token's `kid`
   is used purely as a lookup constraint, and a duplicate, missing or ambiguous
   `kid` is a refusal rather than a guess.
3. **Key metadata must agree with the token.** The key's type, curve, algorithm,
   use and permitted operations are checked against the already-validated header.
4. **Then the signature is verified.**
5. **Only then are claims read.** The `(issuer, subject)` pair must equal the
   configured principal exactly, the audience must name `external_resource_url`,
   and every configured scope must be present.

Outbound key fetches ignore ambient proxy environment variables and connect to each
vetted address for the configured host while keeping the original hostname, SNI and
certificate verification. An environment variable must not be able to redirect a
key fetch.

The active JWKS is refreshed every five minutes through that same proxy-free,
address-vetted, hostname/SNI-preserving path. A valid replacement is installed
atomically. A fetch failure or invalid replacement retains the last verified set;
it never installs partial key data and an unknown new `kid` remains rejected until a
later refresh succeeds.

## Refusal status and challenge

Missing hop identity, a malformed or invalid token, wrong issuer/subject/audience,
expired claims, and unknown keys return byte-identical `401 Unauthorized` responses.
Their `WWW-Authenticate` header uses the `Bearer` scheme, the `invalid_token` error,
and the resource metadata URL
`https://godot-mcp.example.invalid/.well-known/oauth-protected-resource`.

An otherwise valid token missing a configured scope returns the same constant body
with status `403` and `error="insufficient_scope"`. The metadata URL is always the
external HTTPS origin, never the loopback backend address.

## Making a request

Clients talk to the proxy:

```sh
curl --silent --show-error \
  --header 'authorization: Bearer <redacted: the end-user OIDC token>' \
  --header 'content-type: application/json' \
  --data '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}' \
  https://godot-mcp.example.invalid/mcp
```

## Residual risk you own

* Anything that can write the proxy-hop credential can impersonate the proxy. Treat
  its file or socket as a secret with the same care as a private key.
* The proxy is the only thing enforcing the public name and TLS. A misconfigured
  proxy is outside this process's control and this process cannot detect it.
* One principal means one person. Sharing that identity between people removes the
  only attribution the server has.

See the [threat model](../security/threat-model.md) for how these sit next to the
file and process boundaries.
