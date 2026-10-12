Release-candidate fixture for the Todo-35 release-gates harness.

What this is
------------

A stand-in candidate worktree. It carries exactly the two things the harness
reads out of a candidate:

* `ci/observations/` — what each CI job observed. The committed file records one
  `executed`, exit-code-`0` observation for every required check the harness
  derives from `tests/fixtures/godot-releases.lock.json`, so it exercises the
  passing path end to end.
* `ci/oidc/openid-configuration.json` — the deterministic local OIDC/JWKS
  declaration. It creates no key material; it names the four JWKs Todo 29 already
  committed under `crates/godot-mcp-server/tests/fixtures/auth`.

What this is NOT
----------------

These observations are fixture data, not evidence that any of those checks ran on
this machine. The Godot version matrix, the GPU/display job, the Docker
isolation check and the macOS/Windows packaged smokes have never executed on the
development host: it has no real Godot install, no GPU/display session, no
container runtime, no MSVC toolchain and no macOS toolchain. Their real execution
belongs to the runners the workflows in `.github/workflows/` target.

What is genuinely proved locally is the harness's own logic: that a complete set
of passing observations yields a schema-valid non-qualifying document, and that a
missing, skipped, unavailable, command-mismatched or nonzero observation becomes
an explicit failed entry rather than a pass or a silent skip. The negative cases
are built in-test (`crates/godot-mcp-cli/tests/ci_matrix_lock.rs`) from copies of
this fixture.

Regenerating the observations
-----------------------------

The observation set is derived from the catalogue, so it goes stale on purpose
whenever the lock changes:

    make release-gate-specs

and rewrite `ci/observations/all-required-checks-passed.json` with one `executed`
record per listed check, using `command` verbatim and the `source_commit` /
`source_tree` values `qa/tasks/35.json` passes. Packaged-smoke fixture records also
carry the matching digest from `tests/fixtures/release-archives/SHA256SUMS.txt` so
the tests can prove receipt-to-qualification archive provenance.
