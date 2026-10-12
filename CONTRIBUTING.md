# Contributing to godot-mcp

Thank you for improving godot-mcp. The project values small, test-first changes
whose behaviour is proved rather than described, and a server that fails closed
when it cannot prove an outcome.

## Before you start

1. Read [`AGENTS.md`](AGENTS.md) — the hard invariants and the change-to-proof
   matrix apply to every contributor, human or agent.
2. Read the [contributor workflow](docs/contributor/workflow.md) and the
   [architecture](docs/contributor/architecture.md) before moving code between
   crates.
3. For a change to a tool's arguments, a schema under `schemas/`, the bridge
   protocol or the release pipeline, open an issue or a draft pull request first
   so the compatibility boundary is explicit.

By participating, you agree to follow the [Code of Conduct](CODE_OF_CONDUCT.md).
Report vulnerabilities privately as described in [`SECURITY.md`](SECURITY.md),
not in a public issue.

## Development setup

The toolchain is pinned in `rust-toolchain.toml` (Rust 1.96.0) and every direct
dependency is pinned exactly. The licence and advisory gate needs the
`cargo-deny` version recorded in `tooling.lock.json`, and `make fmt` and
`make fmt-check` need oxfmt at the version the `Makefile` pins
(`npm install --global oxfmt@0.64.0`) for the Markdown, YAML and JSON at the top
level and under `.github/`.

```bash
git clone https://github.com/sunerpy/godot-mcp.git
cd godot-mcp
make hooks      # optional: pre-commit runs make fmt, pre-push runs make lint test
make check
```

The GDScript codec harness in the bridge suite runs against the Godot build named
by `GODOT_MCP_GODOT_BINARY` and prints a `SKIP` line when it is unset. The engine
compatibility suite runs through `make godot-test` against an exact locked build,
fetched and verified by digest (the URLs and digests come from `make ci-matrix`).

## Work on a change

- Start with the narrowest test that demonstrates the missing behaviour, and
  check that it fails for the right reason.
- Keep analysis, engine execution, protocol and presentation in their owning
  crates; the layering is enforced by a test.
- Add the negative cases: a refused request must leave no effect behind.
- Update the canonical documentation in the same change, and run `docs-check`:

  ```bash
  cargo +1.96.0 run -p godot-mcp-cli --features test-hooks --bin docs-check
  ```

## Commit and pull request conventions

Pull request titles are English Conventional Commits, because the squash commit
takes the title and Release Please reads it:

```text
feat(server): add the scene diff tool
fix(engine): stop a session whose state directory was removed
```

`feat` raises the minor version and `fix` the patch version while the project is
`0.x`; a breaking change uses `feat!` or a `BREAKING CHANGE:` footer. Keep the
subject imperative, without a trailing period, and do not add AI or co-author
trailers.

A pull request states the problem and its evidence, the behaviour chosen, the
compatibility impact, the validation actually run and the documentation updated.
The template asks for each of them.

## Validation

While iterating, run the affected crate's suites. Before asking for review:

```bash
make check
git diff --check
```

CI then runs every check of the release-gate catalogue on Linux, the packaged
smokes on Linux, macOS and Windows, and the workspace suite natively on macOS and
Windows. The required check is `CI Success`.

## Releases

Do not edit the workspace version, the release manifest, the scaffold version,
tags or release assets by hand. Merging the release pull request that Release
Please opens is the release: the workflow builds each archive on its own
platform, smoke-tests it against the locked Godot build, attests and verifies the
assets, and only then publishes the draft.
