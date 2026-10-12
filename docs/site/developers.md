# Contributing

This page is for working on godot-mcp itself: the layout of the repository, the checks a change
has to pass, and how releases and this site are made.

## The repository

godot-mcp is a Rust workspace; `rust-toolchain.toml` pins Rust 1.96.0 and every direct dependency
is pinned to an exact version.

| Path | Holds |
| --- | --- |
| `crates/godot-mcp-domain/` | Contracts, the error taxonomy, typed Godot values, configuration and the filesystem authority |
| `crates/godot-mcp-analysis/` | Static GDScript, scene and resource analysis and the semantic graph |
| `crates/godot-mcp-engine/` | Controlled Godot execution, the language server and debug adapter clients, supervision and containment |
| `crates/godot-mcp-bridge/` | The editor-bridge protocol, its codec and the shipped GDScript add-on |
| `crates/godot-mcp-server/` | The MCP kernel, policy, journal, scheduler, sessions, HTTP and diagnostics |
| `crates/godot-mcp-cli/` | The `godot-mcp` binary, packaging, the release gates and operator repair |
| `schemas/` | The JSON schemas of every contract the server publishes |
| `tests/fixtures/godot-releases.lock.json` | Every Godot version the project supports, with its official URLs and digests |
| `docs/` | The references; `docs/site/` is this site |

[`AGENTS.md`](../../AGENTS.md) holds the rules every change follows, for people and coding agents
alike: fail closed, keep stdout for MCP frames, no unauthenticated listener, exact dependencies,
Godot versions only from the lock.

## Build and test

```sh
git clone https://github.com/sunerpy/godot-mcp.git
cd godot-mcp
make hooks    # pre-commit formats, pre-push runs make lint test
make check    # format check, clippy, workspace tests, locked build, cargo-deny
```

Work test-first: write the failing test, read why it fails, then make it pass. The GDScript codec
harness in the bridge suite runs against the Godot build named by `GODOT_MCP_GODOT_BINARY` and
prints a `SKIP` line without one; `make godot-test` runs the engine compatibility suite against an
exact locked build, fetched and verified by its digest.

CI runs every check of the release-gate catalogue (`make release-gate-specs`) on its own runner —
the workspace checks, the Godot engine matrix, the display check, the packaged smokes on Linux,
macOS and Windows and the Docker clean-room isolation check — and joins the observations with the
release-gates harness, which fails when any required check is missing or red. The workspace suite
also runs natively on macOS and Windows. `CI Success` requires all of them.

## Pull requests and releases

- Pull request titles follow Conventional Commits, for example
  `fix(engine): stop a session whose state directory was removed`; Release Please reads them to
  choose the next version.
- Pull requests are squash-merged into `main`; nothing is pushed to `main` directly.
- Release Please keeps a release pull request open. Merging it tags the release commit; the same
  run waits for `CI Success` on that commit, builds the archive for each platform on that
  platform, smoke-tests it against the locked Godot build, attaches the archives with
  `SHA256SUMS.txt` and an attestation, verifies the remote bytes and only then makes the release
  public.

## This site

The pages of this site live in `docs/site/`, English at the root and Chinese under `zh/`, next to
the code they describe, so a change that alters what users see updates its page in the same pull
request. [`docs/site/README.md`](README.md) lists the rules for these pages. The site's theme and
build live in [sunerpy/firlab](https://github.com/sunerpy/firlab), which copies the pages in after
every change to `main`.
