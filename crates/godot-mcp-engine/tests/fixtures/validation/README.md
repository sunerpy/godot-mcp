# Todo 15 validation fixtures

Everything here is committed, so the acceptance suite
(`tests/validation_import_test.rs`) never needs a real Godot installation and
never touches the network.

## `binaries/`

Fake Godot builds. **Not real Godot** — each one reproduces exactly the
observable command-line contract Todo 15 depends on, with the engine-source
citations recorded in the file's own header comment.

| fixture | what it models |
| --- | --- |
| `godot-validation` | a 4.4+ editor build that honours `--check-only --script`, `--import` and `-s <framework runner>` |
| `godot-silent-ignore` | a build that **silently ignores** every flag it is handed while printing a plausible `started` banner and exiting 0 |

`godot-silent-ignore` is the load-bearing fixture. Todo 13 observed the real
behaviour directly (an export-template build answers `--editor` with exit 0 and
empty output), and the official command-line reference states that "unknown
command line arguments have no effect whatsoever. The engine will **not** warn
you when using a command line argument that doesn't exist with a given build
type." So "the process started" and "the process exited 0" prove nothing, and the
fixture exists to prove the classifier agrees.

Each fixture has a `unix/*.sh` and a `windows/*.cmd` variant. **Only the `.sh`
variants have actually been executed**: this repository's host is Linux and there
is no Windows runner, so the `.cmd` variants are committed behavioural twins that
remain unverified on Windows.

## `projects/`

Godot project fixtures. Per-project engine behaviour is declared in
`fake-engine.conf`, read by the fake engine — never by the Rust code.

| project | shape |
| --- | --- |
| `gut` | GUT installed; clean + deliberately broken script; import succeeds; test run passes 7/7 |
| `gut-failing` | GUT installed; test run reports 2 failures and exits 1 |
| `gut-timeout` | GUT installed; the run deliberately outlives a short deadline and produces no summary |
| `gdunit4-failing` | gdUnit4 installed; 1 failure, 1 skipped, exits **100** (gdUnit4 never uses exit 1) |
| `import-failure` | `art/sprite.png` is not a PNG, so the import fails on a named resource |
| `no-framework` | no `addons/` directory at all |

The framework addons are committed **marker sets**, not vendored copies of GUT or
gdUnit4: exactly the files a real installation has that detection keys on
(`addons/gut/plugin.cfg`, `addons/gut/gut_cmdln.gd`,
`addons/gdUnit4/plugin.cfg`, `addons/gdUnit4/bin/GdUnitCmdTool.gd`). That is the
whole point of the `no-framework` fixture too: the engine plane detects a
pre-existing setup and refuses with `unsupported` when there is none. It never
adds a framework to anyone's project.

The `.out` files are the frameworks' own end-of-run summary text, in the exact
shape their source code produces — GUT's `Totals` block (labels padded to 18
columns, values right-aligned in 5, `0` rendered as the literal `none`, and
`Failing Tests` **absent entirely** on a clean run) and gdUnit4's
`Overall Summary:` line (` N test cases | N errors | N failures | N flaky | N
skipped | N orphans |`). Parsing real framework text is what makes the reported
counts evidence rather than decoration.

The acceptance suite always copies a project into a scratch directory before
running, because an import legitimately writes `.godot/` into the project it
imports. The committed fixtures are never mutated.
