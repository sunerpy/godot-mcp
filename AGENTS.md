# AGENTS.md — godot-mcp contributor contract

godot-mcp is a local-first, single-user Model Context Protocol server for Godot
projects. One static Rust process runs beside a project: it analyses GDScript,
scenes and resources without an engine, changes text resources through journaled
transactions, and starts, drives, captures and debugs Godot games and editors it
owns. Clients talk to it over stdio, or over authenticated loopback HTTP.

This file is the canonical instruction source for coding agents. `CLAUDE.md`
imports it; do not create a second agent guide. Behaviour that changes with the
code belongs in [`docs/`](docs/README.md), not here.

## Navigate with CodeGraph first

A local `.codegraph/` index (never committed) answers most "where is X / how does
Y reach Z" questions faster than reading files. Verify it before the first query:

```bash
codegraph status . --json
codegraph explore "how a managed session is stopped" -p .
codegraph search "SessionManager" -p .
codegraph node "SessionManager::stop" -p .
```

Lifecycle commands take an optional positional path; research commands take one
query plus `-p/--path`. Do not initialise or rebuild an index unless `status`
says so.

## Hard invariants

1. **Fail closed.** An outcome the server cannot prove is `uncertain`, not
   success. A transaction interrupted by a crash is reconciled as proven
   pre-state, proven post-state or a write-blocking quarantine before anything is
   served. Never turn an unprovable state into a pass, and never retry an effect
   whose outcome is unknown.
2. **Protocol purity.** On stdio, stdout carries MCP frames and nothing else.
   Logs and diagnostics go to stderr.
3. **No unauthenticated listener.** HTTP serves only `trusted_local` (a launch
   secret on loopback) or `proxied_oidc` (a proxy hop plus one end-user
   principal). Do not add a network surface without authentication.
4. **Authority comes from held handles, not path text.** Project files are read
   and written through the descriptor-relative filesystem layer in
   `godot-mcp-domain`; symlink components, retargeted junctions and paths outside
   the project fail before any effect.
5. **Exact dependencies.** Every direct dependency is pinned with `=`. TLS is
   rustls only; OpenSSL and native-tls must not enter the graph. The native gdext
   bridge (crate `godot`, `.gdextension`) never reaches a shipping manifest,
   binary, archive or SBOM — the release-graph audit checks all of them.
6. **Godot versions come from the lock.** Every Godot version, download URL and
   digest is derived from `tests/fixtures/godot-releases.lock.json`. No workflow,
   script or Makefile restates one; `ci_matrix_lock` fails the build if one does.
7. **Frozen contracts evolve by the rules.** The `schemas/` tree, the bridge
   protocol v1 envelope and the tool argument shapes change only as
   [Schema evolution](docs/protocol/schema-evolution.md) allows. A tool's
   advertised schema and its handler must agree on every case.
8. **No manual releases or version edits.** Release Please owns the workspace
   version, `Cargo.lock`'s workspace entries, the manifest and the tags.

## Workspace ownership

| Crate                 | Owns                                                                        |
| --------------------- | --------------------------------------------------------------------------- |
| `godot-mcp-domain`    | contracts, error taxonomy, typed Godot values, config, filesystem authority |
| `godot-mcp-analysis`  | static GDScript / scene / resource analysis and the semantic graph          |
| `godot-mcp-engine`    | controlled Godot execution, LSP/DAP adapters, supervision, containment      |
| `godot-mcp-bridge`    | the editor-bridge protocol, its codec and the shipped GDScript addon        |
| `godot-mcp-server`    | MCP kernel, policy, journal, scheduler, sessions, HTTP, diagnostics         |
| `godot-mcp-cli`       | the `godot-mcp` binary, packaging, release gates, operator repair           |
| `qualification-tools` | build/test-only qualification tooling; never shipped                        |

`godot-mcp-domain`, `-analysis` and `-engine` never depend on `-server` or
`-cli`; a test reads the manifests to enforce it. See
[Architecture](docs/contributor/architecture.md).

## Test first

Write the failing test first and read why it fails; make it pass with the
smallest change; refactor with it green. Never delete, skip, weaken or rename a
test to make a change pass. Tests that need a maintainer-only input (`.omo/`) are
`#[ignore]`d on clean checkouts with that reason, not passed vacuously.

## Change-to-proof matrix

| Change                               | Minimum focused proof                                                       | Documentation                                   |
| ------------------------------------ | --------------------------------------------------------------------------- | ----------------------------------------------- |
| tool arguments or schemas            | the tool's schema/handler agreement test; `tool_schemas`; `tool_catalog`    | generated tool inventory (`docs-check --write`) |
| analysis (GDScript, scenes, graph)   | the analysis crate's suites for the changed parser or query                 | the operator page that describes the result     |
| engine execution, sessions, captures | the engine and server session suites; the platform-specific variant         | `managed-sessions.md`, `process-boundaries.md`  |
| journal, recovery, quarantine        | restart and crash-stage suites; reconciliation cases                        | `recovery.md`                                   |
| HTTP and authentication              | `http_*` and `oidc_resource_server` suites; negative cases first            | `http-*.md`, `threat-model.md`                  |
| packaging and release                | `release_package`, `release_graph_audit`, `ci_matrix_lock`; `make dist`     | `packaging.md`                                  |
| workflows and scripts                | `actionlint`, `shellcheck`, `ci_matrix_lock`                                | `docs/contributor/workflow.md`                  |
| documentation                        | `cargo +1.96.0 run -p godot-mcp-cli --features test-hooks --bin docs-check` | the canonical page, not a summary of it         |

## Documentation rules

- `docs/` is checked mechanically by `docs-check`; generated pages are
  regenerated with `docs-check -- --write`, never edited by hand.
- The English README (`README.md`) and the Chinese one (`docs/readme/README.zh-CN.md`)
  keep the same structure, commands and claims.
- The website's words live in `docs/site/` (English at the root, Chinese under
  `zh/`), published at <https://firlab.app/godot-mcp/>; `docs/site/README.md`
  has the writing rules.
- Every number in the docs is measured, and says what it was measured on. Say
  "not measured" rather than estimate.

## Validation

```bash
make check      # fmt-check, clippy, workspace tests, locked build, cargo-deny
make ci         # check + SBOM + the Godot fixture entry point
```

CI (`.github/workflows/ci.yml`) observes every release-gate check in the
catalogue (`make release-gate-specs`) and judges them with the release-gates
harness; the required check is `CI Success`.

## Git, PR and release discipline

- Preserve dirty work and other worktrees; never reset, clean, stash or delete
  state you did not create.
- Every change reaches `main` through a pull request, squash-merged at the head
  that passed CI (`gh pr merge --squash --match-head-commit <sha>`), never by a
  direct push — even when the token could bypass the ruleset.
- PR titles are English Conventional Commits; the squash subject is the PR title
  and Release Please reads it (`feat` → minor, `fix` → patch while `0.x`). Do not
  add AI or co-author trailers.
- Do not commit, push, merge or change GitHub settings unless asked.
- Releases are same-run and draft-until-verified: exact-SHA `CI Success`, three
  native archives each smoke-tested against the locked Godot build, checksums,
  attestations and a remote byte check before the draft is published.
