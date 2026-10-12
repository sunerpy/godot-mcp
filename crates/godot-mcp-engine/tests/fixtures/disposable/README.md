# Todo 16 disposable-execution fixtures

Committed fixtures for `tests/disposable_execution.rs`. Nothing here needs a real
Godot installation and nothing touches the network.

## This is mutation containment, **not** a code sandbox

Read this before adding an assertion to the suite. The disposable-copy posture
redirects *which project tree* an untrusted Godot run is allowed to mutate. It
does **not** confine what that process can do:

* an absolute-path write outside the copy is **not** prevented by the copy;
* a network connection is **not** prevented by the copy;
* reading a file the user's account can read is **not** prevented by the copy.

Confining those is the containment boundary's job (Todo 14: a Windows Job
Object, a Linux cgroup v2 sub-tree, or a configured container/VM adapter), and
even a strong boundary is a resource/membership boundary — still not a code
sandbox. The fixtures below therefore only ever demonstrate what is actually
true: the *source* tree is untouched, the excluded material is absent from the
copy, and the copy is discarded afterwards.

## Why an untrusted posture is the default at all

Godot's own documentation states that a `@tool` script "makes it execute in the
editor" and warns that "the editor does not include protections for potential
misuse of `@tool` scripts", with "Modifications in the editor \[being\]
permanent, with no undo/redo possible"
(<https://docs.godotengine.org/en/stable/tutorials/plugins/running_code_in_the_editor.html>).
A `@tool` autoload therefore runs project code as soon as the project is loaded,
and `--import` implies `--editor`. So "just importing a project" is arbitrary
code execution on the operator's machine. The fixture project below carries a
`@tool` script and a `@tool` autoload precisely so that fact is visible in the
committed material rather than only in prose.

## `projects/untrusted/`

A project that is simultaneously *secret-bearing* and *mutating*:

| path | why it is here |
| --- | --- |
| `project.godot` | the project marker, plus a `@tool` autoload registration |
| `scripts/player.gd` | a `@tool` script — proof that loading executes code |
| `scripts/game_state.gd` | the `@tool` autoload's backing script |
| `main.tscn` | ordinary project content that must survive the copy |
| `.env` | secret material, excluded by name |
| `.env.production` | secret material, excluded by prefix |
| `private.pem` | secret material, excluded by suffix |
| `credentials.json` | secret material, excluded by name |
| `export_presets.cfg` | Godot's own export config; stores keystore/store passwords |
| `.godot/project.godot` | the Todo 8 trap: the engine writes its **own** copy of `project.godot` inside its cache. It is never source. |
| `.godot/uid_cache.bin` | engine-generated cache, excluded |
| `dot-git/config` | stands in for `.git/config` (see below) |
| `dot-git/HEAD` | stands in for `.git/HEAD` |
| `fake-engine.conf` | per-scenario behaviour for the fake engine |

### The `dot-git/` rename

Git refuses to track any path containing a `.git` component, so the VCS-metadata
case cannot be committed under its real name. The suite installs the fixture into
a scratch directory and renames `dot-git/` to `.git/` there. That rename is test
scaffolding; the production exclusion policy only ever sees the real `.git` name.

## `binaries/`

`godot-untrusted.{sh,cmd}` is a fake Godot 4.4+ editor build. It reproduces only
the observable contract this suite depends on:

* `--version` prints a 4.x version string and exits 0.
* `--import` implies `--editor`/`--quit` in a real engine; here it writes
  `.godot/uid_cache.bin` (the artifact the import is asked to produce), then
  behaves like a hostile-but-honest project: it mutates the tree it was pointed
  at, deletes a source file, writes a marker one level above the project, and
  records which of the excluded files it could see. Every one of those writes is
  *relative to `--path`*, which is exactly the class of mutation a disposable
  copy contains.
* Unknown flags are ignored without a diagnostic, as the official command-line
  reference documents.

`fake-engine.conf` keys:

```
import_sleep_ms=<integer>   # outlive a short deadline deliberately
```

The Windows `.cmd` twin is committed for parity but has never been executed here
(this host is Linux); it is not evidence of Windows behaviour.
