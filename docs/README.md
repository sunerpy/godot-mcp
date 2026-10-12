# godot-mcp documentation

`godot-mcp` is a local-first, single-user Model Context Protocol server for Godot
projects. It runs as one static Rust process next to your project. Your MCP client
talks to that process over stdio; it does not talk to the Godot editor, and it does
not care which editor or IDE you use.

The website, <https://firlab.app/godot-mcp/>, is the user guide in English and
Chinese; its pages live in [`site/`](site/README.md). The Chinese landing page is
[README.zh-CN.md](readme/README.zh-CN.md).

Start here:

* [Quickstart: local stdio](operator/quickstart-stdio.md) — install, configure and
  make a real request, in a few commands. Every command on that page is executed
  verbatim by the test suite.
* [Configuration](operator/configuration.md) — the layered loader, every
  recognized key, and the invariants that make a configuration fail closed.
* [Profiles and tool outcomes](operator/profiles-and-outcomes.md) — what a profile
  selects, and what an answer means.
* [Troubleshooting](operator/troubleshooting.md) — symptom-first, with the exact
  diagnosis for each.

## Operator guides

| Page | Subject |
| --- | --- |
| [Quickstart: local stdio](operator/quickstart-stdio.md) | The default deployment |
| [Configuration](operator/configuration.md) | Layered loading, keys, invariants |
| [MCP clients](operator/mcp-clients.md) | Client wiring examples |
| [Profiles and tool outcomes](operator/profiles-and-outcomes.md) | Membership, availability, uncertainty |
| [HTTP: `trusted_local`](operator/http-trusted-local.md) | Launch-secret-authenticated loopback HTTP |
| [HTTP: `proxied_oidc`](operator/http-proxied-oidc.md) | Reverse proxy hop plus one end-user principal |
| [Artifacts](operator/artifacts.md) | Opaque artifact resources |
| [Project leases](operator/project-leases.md) | OS-object identity and exclusion |
| [Process boundaries](operator/process-boundaries.md) | Weak trusted, strong untrusted |
| [Recovery](operator/recovery.md) | Restart survivors, quarantine, operator repair |
| [Editor bridge](operator/editor-bridge.md) | What ships, and what does not |
| [Managed sessions](operator/managed-sessions.md) | Game and editor sessions, input, captures, debugging |
| [Exports](operator/exports.md) | Presets, exact templates, managed exports and their artifacts |
| [Godot compatibility matrix](operator/version-matrix.md) | Exact locked versions |
| [Packaging](operator/packaging.md) | Archive contents and verification |
| [Troubleshooting](operator/troubleshooting.md) | Symptom-first diagnosis |

## Security

| Page | Subject |
| --- | --- |
| [Threat model](security/threat-model.md) | Trust boundaries and residual risk |

## Protocol

| Page | Subject |
| --- | --- |
| [MCP surface](protocol/mcp-surface.md) | Transports, discovery, request shape |
| [Profiles, tools and resources](protocol/tool-inventory.md) | Generated inventory |
| [Outcome taxonomy](protocol/error-taxonomy.md) | Generated vocabulary |
| [Editor bridge protocol](protocol/bridge-protocol.md) | The frozen wire envelope |
| [Schema evolution](protocol/schema-evolution.md) | How a contract may change |

## Contributing

| Page | Subject |
| --- | --- |
| [Workflow](contributor/workflow.md) | Test-first development and the gates |
| [Architecture](contributor/architecture.md) | Crates, layers, boundaries |
| [Complete development plan](contributor/complete-game-development.md) | Approved post-0.1 repair, authoring, runtime and export implementation plan |
| [Documentation rules](contributor/documentation.md) | How these pages are verified |
| [Licences and provenance](contributor/licenses.md) | Third-party terms |
| [Decision records](contributor/adr/README.md) | Why things are the way they are |
