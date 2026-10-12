# Licences and provenance

`godot-mcp` is MIT-licensed. The full text ships in every release archive as
`licenses/LICENSE-MIT`.

Third-party terms are recorded in `THIRD-PARTY-NOTICES.md`, which also ships in the
archive. This page summarizes what that file records and why.

## Direct third-party terms

| Component | Licence | Relationship |
| --- | --- | --- |
| `rmcp` (exactly `2.2.0`) | Apache-2.0 | Runtime dependency: the MCP protocol implementation |
| `tree-sitter-gdscript` (exactly `6.1.0`) | MIT | Runtime dependency: the GDScript grammar |
| `codegraph-rust` | MIT | Behaviour and fixtures adapted; **not** a dependency |

The `codegraph-rust` entry is the one that needs explaining. The static-analysis layer
adapts that project's behaviour — its GDScript node-kind vocabulary, its
`project.godot` and scene/resource section vocabularies, and its cross-document Godot
relationship model — but no upstream file is copied verbatim, and nothing in this
workspace links to or reads from that checkout at build or run time. The notices file
lists, file by file, which behaviour was adapted from which upstream source, so the
provenance is auditable rather than asserted.

## The bill of materials is derived, not written

Each archive carries `sbom/godot-mcp.cdx.json`, a CycloneDX document derived from the
committed dependency lock. It reflects the graph that was actually locked, not an
ambient environment at packaging time — a hand-maintained component list would drift
from the lock the moment anything changed.

## What is deliberately absent from the graph

* **The native Godot bridge.** Crate `godot` and the `gdext` family are absent from
  everything that ships, and their absence is checked across the workspace dependency
  tables, every member's feature tables, the dependency lock, the executable's
  linked-crate closure, every archive entry, each archive's embedded manifest and the
  SBOM components. The reasoning is in
  [ADR 0001](adr/0001-gdext-reference-only.md); the licence dimension matters too,
  because that project is under a file-level copyleft that a first release should not
  take on without a deliberate decision.
* **OpenSSL and native-tls.** All TLS goes through rustls, which keeps a C toolchain
  and its own licence surface out of the graph entirely.

## Pinning

Every direct dependency is pinned with an exact version, and only the approved features
are enabled. A version range would mean the licence set could change without any change
to this repository, which is exactly the situation a provenance record exists to
prevent.

The toolchain and the two audit tools are recorded in a committed lock with their source
URL, package checksum and installed executable digest, so a build can be attributed to
specific tool binaries rather than to whatever happened to be installed.

## Enforcement

```sh
cargo +1.96.0 deny check bans licenses sources
```

This is the gate: it enforces the bans, the accepted licence set and the accepted
sources. Note that the full `cargo deny check` also consults an external advisory feed
and can fail for reasons unrelated to this repository, so the three named subcommands
are what the gate runs.

## Adding a dependency

1. Check the licence is in the accepted set.
2. Pin the exact version and enable only the features you need.
3. Add its notice to `THIRD-PARTY-NOTICES.md` if its terms require one.
4. Run the gate above and confirm the release-graph audit still passes.
