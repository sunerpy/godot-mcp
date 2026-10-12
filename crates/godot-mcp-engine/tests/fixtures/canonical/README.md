# Todo 17 canonical-write fixtures

Everything here is committed, so the acceptance suite
(`tests/canonical_writes.rs`) never needs a real Godot installation and never
touches the network.

## `binaries/`

Fake Godot builds. **Not real Godot** — `godot-canonical` reproduces exactly the
observable contract Todo 17 depends on, with the engine-side reasoning recorded in
the file's own header comment:

```
--headless --path <project> --script res://godot_mcp_helpers/2/canonical_write.gd \
    -- --request <rel> --response <rel>
```

Everything after the bare `--` is what `OS.get_cmdline_user_args()` returns. The
fixture writes **only** inside `res://godot_mcp_helpers/2/staging/` plus the
response document it was told to write; it never touches the caller's target path,
because publishing onto the target is the Rust side's compare-and-swap under the
Todo 2 lease. Its exit statuses are the helper's own four: `0` ok, `64` usage,
`65` refused, `70` failed.

The fixture is also where the `uid://…` values come from. That placement is the
point: `ResourceUID.create_id_for_path()` is seeded by the engine with the path and
the project name, and `id_to_text()` owns the spelling, so the UID must be minted
on the engine side of the boundary. The Rust side never derives one, and this
fixture is what makes that assertable without a real Godot build.

Per-project behaviour is declared in `<project>/fake-engine.conf`, read by the
fixture — never by the Rust code:

| key | effect |
| --- | --- |
| `helper=ok` | run the helper normally |
| `helper=silent-ignore` | ignore `--script` entirely and exit 0 — the silent no-op an export-template build performs |
| `helper=no-response` | write the staged artifact but no response document |
| `helper=save-failed` | report `save_failed` and exit 70 |
| `helper=uid-conflict:<path>` | report a `uid_conflict` against `<path>` and exit 65 |
| `helper=partial-write` | write **half** the artifact and still exit 0 — an interrupted non-atomic producer |
| `helper=batch-refused` | refuse an `apply_batch` at its first operation (`node_not_found`) and exit 65 |
| `uid=<uid://…>` | the UID the engine "mints" for this project |
| `record=<absolute path>` | copy each helper request document to that path, so a test can read what reached the engine |

`helper=silent-ignore` and `helper=partial-write` are the load-bearing modes.
Todo 13 observed the real silent-ignore behaviour directly (an export-template
build answers `--editor` with exit 0 and empty output), and the official
command-line reference states that "unknown command line arguments have no effect
whatsoever. The engine will **not** warn you when using a command line argument
that doesn't exist with a given build type." So the helper's exit status can never
establish that the write happened, and `partial-write` proves the target never ends
up holding a half-written artifact.

Each fixture has a `unix/*.sh` and a `windows/*.cmd` variant. **Only the `.sh`
variant has actually been executed**: this repository's host is Linux and there is
no Windows runner, so the `.cmd` variant is a committed behavioural twin that
remains unverified on Windows. The `apply_batch` operation, `helper=batch-refused`
and `record=` exist only in the `.sh` variant: the server suites that use them
(`crates/godot-mcp-server/tests/semantic_authoring.rs`) run on Linux only.

## `projects/canonical/`

One Godot project fixture, deliberately minimal — the suite only needs a real
`project.godot` so the Todo 2 lease can be acquired and the project identity
revalidated.

| path | why it is here |
| --- | --- |
| `project.godot` | the lease marker |
| `fake-engine.conf` | per-project fixture behaviour |
| `scenes/existing.tscn` | an existing text scene, for update / UID registration / preimage-conflict cases |
| `data/existing.tres` | an existing text resource |
| `scenes/binary.scn` | a binary resource that must be refused, never rewritten as text |
| `art/binary.res` | the same refusal for the other binary extension |

The acceptance suite always copies the project into a scratch directory before
running, because a canonical write legitimately creates
`godot_mcp_helpers/2/` inside the project it writes. The committed fixtures are
never mutated.
