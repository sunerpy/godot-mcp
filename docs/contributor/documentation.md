# Documentation rules

`docs/` is verified mechanically. Prose about a tool name, a configuration key, a protocol
constant or a Godot version rots silently, so none of it is trusted.

Run the checker before sending a documentation change:

```sh
cargo +1.96.0 run -p godot-mcp-cli --features test-hooks --bin docs-check
```

It exits `0` only on a clean tree and prints one line per finding, each naming the page,
the line, the rule and what to do.

## Two mechanisms

**Generated pages** are rendered from their canonical source and byte-compared. Editing one
by hand fails the check. Regenerate instead:

```sh
cargo +1.96.0 run -p godot-mcp-cli --features test-hooks --bin docs-check -- --write
```

**Every other page** is checked against the canonical owner of each fact it mentions. There
is no list of approved names in the checker to edit: the inventory is derived from the
capability manifests, the enums' own schemas, the bridge wire schema, the production
configuration loader and the Godot releases lock. Changing what is allowed means changing
the code, the schema or the lock.

## The rules

| Rule | What it catches |
| --- | --- |
| `generated-page` | A generated page that no longer matches its canonical rendering |
| `stale-name` | A back-ticked dotted name that is not a canonical tool, configuration key, error code, plane or capability |
| `stale-env-var` | A `GODOT_MCP_`-prefixed name the real configuration loader does not recognize |
| `stale-protocol-name` | A bridge kind, envelope field, mutation status or HTTP constant that does not match its owner |
| `stale-resource-uri` | A `godot-mcp://` URI outside the canonical resource set |
| `config-example` | A configuration example with no expected verdict, or one the production loader disagrees with |
| `broken-link` | A relative link that resolves to nothing |
| `required-statement` | A page missing an honesty-critical statement |
| `runnable-block` | A malformed executable block |
| `overstated-containment` | Describing this project's isolation in stronger terms than it provides |
| `forbidden-affordance` | Naming a bypass that does not exist |
| `unauthenticated-public-example` | A request example reaching a non-loopback endpoint with no credential shown |
| `non-loopback-bind` | An example binding the unspecified address |
| `example-host` | A host that is neither loopback nor under the reserved example domain |
| `unredacted-credential-example` | An assignment whose key names a credential and whose value is not redacted |
| `credential-in-docs` | A credential-shaped value anywhere in the tree |
| `opaque-artifact-only` | An artifact addressed as a path instead of an opaque resource |
| `godot-version-alias` | A major-minor Godot alias instead of an exact patch version |
| `service-model-claim` | Advertising a shared or hosted deployment model |
| `in-process-tls` | Documenting TLS termination inside this process |
| `loopback-as-identity` | Treating a loopback address as identity or authorization |
| `unsupported-promise` | Promising behaviour the release does not implement |
| `unverified-tool-count` | Repeating an unverified third-party tool count |
| `lint-self-check` | The lint's own exemptions were widened into a bypass |
| `secret-scanner-self-check` | The credential scanner would report clean without detecting anything |

## Writing a configuration example

Every TOML example must declare the verdict it expects, and the checker gets that verdict
from the production loader — not from a copy of its rules:

````markdown
<!-- docs-check:config expect="accept" -->
```toml
transport = "stdio"
```
````

Use `expect="reject"` for an example that demonstrates a refusal. An unannotated TOML block
is a finding, because an unchecked example is exactly the kind that drifts.

Accepted examples also define which configuration keys the `stale-name` rule recognizes, so
documenting a key means showing it in an accepted example.

## Writing an executable block

The quickstart's steps are executed verbatim by the test suite, in order, in a clean
temporary environment. Mark the block, declare every variable it expands, and write the
commands exactly as a reader should run them:

````markdown
<!-- docs-script:begin name="stdio-smoke" -->
<!-- docs-script:var PROJECT = the project root -->
```sh
ls "$PROJECT"
```
<!-- docs-script:end -->
````

Verbatim means verbatim: nothing rewrites a command before running it. A step that expands
a variable the block never declared is an error rather than something the harness fills in,
which is what keeps the page and the test honest about each other.

## Examples use reserved names only

Hosts are loopback or under the reserved `example.invalid` domain. Credential values are
redaction placeholders or configured indirections. A request example that reaches a
non-loopback endpoint must show the credential it presents — an example that appears to
work without authentication teaches the wrong thing even when the real server would refuse
it.

## Honesty rules and their two exemptions

Some findings exist to stop the documentation from overstating what this release delivers.
Both exemptions are narrow and verified rather than trusted, because a free-text opt-out
would make the lint decorative:

* `<!-- lint:quoted-term -->` permits a containment term only on the threat-model page, and
  only when every back-ticked span containing it is a substring of the engine's own posture
  constant. A marker on a sentence that is not part of that constant still fails.
* `<!-- lint:absence-claim -->` permits naming a non-existent affordance only on a line that
  also states, in a recognized phrasing, that it does not exist. It must be on the **same
  line** as the term.

The lint proves both exemptions still reject their abuse on every run, so widening them
fails in production rather than only under `cargo test`.

## Required statements

A few pages must carry a specific statement, because dropping it would leave the page
implying something untrue — that the editor plane ships, that the version matrix has been
observed on real runners, that all three archives were built. The list, with a reason for
each, lives next to the checker.

If you find one of these statements awkward, do not delete it. Change the situation it
describes, then change the requirement.
