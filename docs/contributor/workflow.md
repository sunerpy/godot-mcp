# Workflow

Development here is test-first and gate-driven. The gates are not advisory: a change that
does not pass them is not finished.

## The gates

Every command pins the exact toolchain. The floating `stable` alias is never used, because
a gate whose meaning changes when a new compiler ships is not a gate.

```sh
cargo +1.96.0 fmt --all -- --check
cargo +1.96.0 clippy --workspace --all-targets --all-features -- -D warnings
cargo +1.96.0 test --workspace --all-features
cargo +1.96.0 build --workspace --all-features --locked
cargo +1.96.0 deny check bans licenses sources
```

`make fmt-check`, `make lint`, `make test`, `make build` and `make deny` wrap the same
commands, and `make check` runs all five; `make fmt` rewrites the formatting instead of
checking it. Warnings are denied, so a warning is a failure. `make fmt-check` also runs
oxfmt, at the version the `Makefile` pins (`OXFMT_VERSION`), over the Markdown, YAML and
JSON at the top level and under `.github/`; `.oxfmtignore` keeps it out of the trees that
tests, digests and the documentation checks read byte for byte.

## CI

`.github/workflows/ci.yml` runs on every pull request and every push to `main`. It
derives the release-gate catalogue from the lock (`make release-gate-specs`), runs each
check through `scripts/ci-observe.sh` on its own runner — the workspace gates, the
Godot engine matrix, the display check, the packaged smokes on Linux, macOS and Windows
and the Docker clean-room isolation check — and joins every observation to the
catalogue with the release-gates harness, which fails when any required check is
missing, skipped or red. The workspace suite also runs natively on macOS and Windows.
`CI Success`, the one required check, waits for all of them; the coverage job reports
without gating.

## Test-first, and what that means concretely

1. **Write the failing test first**, and check *why* it fails. A test that fails for the
   wrong reason proves nothing, so read the failure before writing any implementation.
2. **Make it pass with the smallest change** that actually addresses the cause.
3. **Refactor with the test staying green.**

Two rules that are not negotiable:

* An existing test is never deleted, skipped, weakened or renamed to make a change pass. A
  regression is fixed in the implementation.
* A test never asserts a fabricated success. If something cannot be verified in this
  environment, the test says so — see below.

## Evidence

Each task records a Red and a Green artifact: the exact commands, their **actually
observed** exit codes, and a digest of the captured output. An exit code in an evidence
file is one that was observed during that run, never one that was expected.

This is the point where honesty is cheapest to keep and most expensive to lose. An
evidence file asserting a pass that did not happen makes every other evidence file
worthless.

## Unverifiable things are stated, not assumed

Some properties cannot be verified on a Linux development host with no MSVC toolchain, no
macOS toolchain and no Godot installation. The convention is to make the gap an
**observable state** rather than a comment nobody reads:

* Windows process containment cannot run on this host. It runs on Windows hosts, and
  what ran where is recorded in
  [Process boundaries](../operator/process-boundaries.md#what-has-been-executed-where),
  never inferred from a Linux pass.
* The macOS and Windows archive paths are implemented and were never executed here. The
  packager records a target with no toolchain as *not built* rather than fabricating an
  archive.
* No Godot compatibility row has run against a real Godot installation here. The generated
  matrix says so on its face.
* A QA descriptor's assertions are reviewed by a person, never machine-evaluated. `qa-task`
  decides exit codes only, so a passing task is narrower than its assertions read. See
  [What a passing task does and does not prove](#what-a-passing-task-does-and-does-not-prove).

If you add a capability you cannot verify, add the honest status alongside it.

## QA task descriptors

Each task owns one descriptor under `qa/tasks/`, listing its exact commands, each command's
expected exit code, an evidence path, and its assertions. The single normative entry point is:

```sh
RUSTUP_TOOLCHAIN=1.96.0 make qa-task TASK=36
```

It fails closed on a missing descriptor, a wrong task number, an empty command or assertion
array, an evidence path that names a repo-relative file which does not exist, or an
exit-code mismatch. Only the owning task may change its own descriptor, and only in the
same change that alters the command.

### What a passing task does and does not prove

The runner decides exit codes, and only exit codes.
**The runner never evaluates a descriptor's assertions.**
They are prose reviewed by a person; the runner checks only that the list is
non-empty and holds no blank entry, then compares each command's actual exit code against
its recorded `expected_exit`. So a task exiting 0 means every command exited as recorded; it
does not mean any assertion was confirmed. An assertion that is simply false will not be
caught mechanically, only in review. Interpreting assertions, or migrating all 37
descriptors to a structured matcher format, was considered and deliberately not done at this
stage; this paragraph is the disclosure that stands in its place, and `docs-check` pins it.

Evidence is verified as far as its shape allows, which is not uniformly:

* A path prefixed with the literal `<attemptDir>/` placeholder names a file in the harness
  attempt directory, which lives outside this repository. The runner resolves it and
  requires it to exist only when `QA_ATTEMPT_DIR` names an existing directory; otherwise it
  prints `[UNCHECKED]` and says why, rather than passing it silently. All 37 committed
  descriptors use this shape, so on a plain local run every evidence path reports
  `[UNCHECKED]`.
* Any other value is read as a repo-relative path and must exist, or the task fails.
* `<attemptDir>` is the only placeholder recognised. Any other angle-bracket token in an
  evidence path is rejected rather than guessed at.

The runner checks that the evidence file exists. It does not check that the file's contents
correspond to the run that just happened.

## Documentation is verified, not reviewed

`docs/` is checked mechanically. Before you send a change that touches it:

```sh
cargo +1.96.0 run -p godot-mcp-cli --features test-hooks --bin docs-check
```

Generated pages are regenerated rather than edited:

```sh
cargo +1.96.0 run -p godot-mcp-cli --features test-hooks --bin docs-check -- --write
```

The rules, and why each exists, are in [Documentation rules](documentation.md).

## Dependencies

* Every direct dependency is pinned with an exact version. A range is not acceptable.
* Only the approved features are enabled.
* TLS is rustls only. OpenSSL and native-tls must not enter the graph.
* The native Godot bridge is not a dependency of anything that ships; see
  [ADR 0001](adr/0001-gdext-reference-only.md).

`cargo deny check bans licenses sources` enforces the bans, licences and sources. Note that
the full `cargo deny check` including the advisory database depends on an external feed and
can fail for reasons unrelated to this repository; the three named subcommands are the
gate.

## Pull requests and releases

Every change reaches `main` through a pull request that is squash-merged once `CI Success`
has passed on its head. The squash commit takes the pull request's title, which is an
English Conventional Commit (`feat(server): …`, `fix(engine): …`), because Release Please
reads it to choose the next version. If a change alters a documented behaviour, the
documentation change belongs in the same pull request — otherwise the docs check fails for
whoever lands next.

Release Please keeps a release pull request open. Merging it tags the release commit and
starts `.github/workflows/release.yml`, which waits for `CI Success` on that exact commit,
builds each archive on its own platform, smoke-tests it against the locked Godot build,
attests and uploads the archives, verifies the remote bytes and only then publishes the
draft release. Nobody edits the version, the changelog or a tag by hand.

Release Please picks the next version from the Conventional Commit types since the last
release: a `fix:` bumps the patch version (`0.1.0` to `0.1.1`), and before 1.0 a `feat:` bumps
the minor version. A fix ships as a patch release of its own rather than waiting for features.
The release pull request is opened with the workflow's `GITHUB_TOKEN`, so its CI runs stop at
`action_required` until a maintainer approves them; `CI Success` and the pull request title
check must pass on its head before it is merged.

## Architecture boundaries

The layering is enforced by a test, not by convention. Before moving code between crates,
read [Architecture](architecture.md).
