# 0001 — The native Godot bridge is reference and test only

**Status:** superseded for post-0.1 work by [ADR 0010](0010-complete-development-bridge-strategy.md); this remains the historical first-release decision.

## Decision

The community `godot-rust/gdext` project — crate `godot`, verified at version `0.5.4`,
MPL-2.0, edition 2024 with a minimum supported Rust version of 1.94, exposing Godot API
levels through the 4.7.x series — is used for **reference and test only**.

It is not a first-release Cargo dependency, not a runtime dependency, and not a shipping
dependency. It is also not a replacement for the pure-GDScript bridge approach; the two
are different architectures, not two spellings of one.

If it exists in this repository at all, it exists as an isolated lane that ships nothing
and gates no release, pinned to exactly `=0.5.4`, and it must never use the project's
`experimental-godot-api` or `api-custom` features as a stability source.

## Why not ship it

Five reasons, each independently sufficient.

**Packaging and signing.** A native extension requires a compiled dynamic library per
operating system and architecture, plus a descriptor file. That multiplies the packaging
surface and pulls in code signing and notarization — where the pure-GDScript approach
needs neither. The first release ships one static executable per target and no dynamic
library at all.

**In-editor crash risk.** A native extension runs inside the editor process, and the
project's own documentation acknowledges that hot-reloading one can crash the editor.
This server's guarantees are about accountability of mutation: a crash in the middle of
an editor mutation is precisely the case the recovery contract exists to bound, and
choosing a mechanism that makes such crashes more likely works against it.

**The APIs we would want are unstable.** `EditorInterface` and
`EditorUndoRedoManager` — the two interfaces a native editor bridge would exist to use —
are exposed only behind an explicitly experimental feature. A first-release dependency on
an interface its own authors label unstable is not a defensible choice.

**Licence.** MPL-2.0 is a file-level copyleft. Taking that on is a decision that deserves
its own deliberation, not something to acquire incidentally while solving a different
problem.

**Toolchain coupling.** Edition 2024 with a minimum supported Rust version of 1.94 is
compatible with this workspace's pinned 1.96.0 today, but it adds a second constraint on
a pin that is otherwise ours to choose.

## What is not claimed

An upstream 4.7.x API-level label is a claim about generated bindings, not evidence of runtime behaviour. Runtime
compatibility with the exact versions in the
[compatibility matrix](../../operator/version-matrix.md) is something to prove with a real
live-load test, and no such prebuilt evidence exists at the time of writing. In
particular, 4.7.1 runtime compatibility is a thing to demonstrate, not a thing to assume.

## What a future decision would require

A default-off, exactly pinned native bridge is a distinct, high-risk architecture change.
It would need:

* its own decision record, separately reviewed rather than folded into this one;
* real live-load smoke tests against each exact version in the matrix —
  `4.4.1`, `4.5.1`, `4.6.1` and `4.7.1` — not an API-level label;
* a deliberate answer on the licence, the packaging surface and the signing story;
* a crash-safety argument that holds with a native extension in the editor process.

Until all of that exists, the answer stays no.

## Enforcement

The absence is checked, not trusted. The release-graph audit inspects the workspace
dependency table, every shipping member's dependency **and feature** tables, the
committed dependency lock, the shipping executable's linked-crate closure, every archive
entry, each archive's embedded payload manifest and the SBOM components. A native bridge
crate, an extension descriptor or a native library in any of those fails the audit.
