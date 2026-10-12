# Packaging

A release is one archive per target. It contains the self-contained target executables
plus the assets and metadata needed to verify them.

## Contents

```console
godot-mcp-0.1.0-x86_64-unknown-linux-musl/
├── MANIFEST.json
├── bin/godot-mcp
├── bin/godot-mcp-supervisor
├── bridge/bridge_codec.gd
├── examples/godot-mcp.toml
├── licenses/LICENSE-MIT
├── licenses/THIRD-PARTY-NOTICES.md
├── sbom/godot-mcp.cdx.json
└── schemas/...
```

* `MANIFEST.json` records, for every payload file, its path, byte size, SHA-256 and
  mode, plus the version, target, build profile and the declared executable.
* `sbom/godot-mcp.cdx.json` is a CycloneDX software bill of materials derived from the
  committed dependency lock, not from an ambient environment.
* `schemas/` is the complete schema tree, so a client can validate against exactly the
  contract this build implements.
* `examples/godot-mcp.toml` is a valid configuration containing no credential.

There is no dynamic library, no `.gdextension` descriptor and no native bridge in an
archive. The release-graph audit checks the workspace dependency tables, every member's
feature tables, the dependency lock, the executable's linked-crate closure, every
archive entry, each archive's embedded payload manifest and the SBOM components. A
native Godot bridge crate anywhere in any of those fails the audit.

Test-only binaries cannot be in a release archive by construction — they are gated
behind a feature that a release build does not enable — and an archive that carried one
anyway would be reported.

The Linux executables use `x86_64-unknown-linux-musl` and are statically linked, so they
do not require the target machine's glibc, libm or libgcc runtime. Windows builds use the
MSVC ABI with `crt-static`; they retain only Windows system API imports rather than a
redistributable `VCRUNTIME140.dll`. macOS follows the platform convention and links only
what the system provides: libSystem and libiconv, and for `godot-mcp` also the Security,
CoreFoundation and CoreServices frameworks.

## Verify what you downloaded

```sh
godot-mcp self-check --package /opt/godot-mcp
```

`self-check` re-derives every payload digest and size from the manifest, requires the
notices, the SBOM and the example configuration to be present, requires `bin/` to hold
exactly the declared executable, and refuses a payload file containing a credential
marker. It exits `0` when clean, `1` on any failure, and `2` when the binary is not
running from an extracted package.

The published `SHA256SUMS.txt` covers the archives themselves, in the format
`sha256sum -c` expects.

## Reproducibility

The release build is checked by building the published target for the host platform twice
from clean and comparing the two executables byte for byte. The check reports the actual result rather than
asserting a property, so a divergence is visible instead of assumed away.

## Platform coverage: what has actually been built

This matters more than the target list, so it is stated plainly: only the Linux archive has been built and verified on this host.
`make dist` builds only the host platform's published target, so each archive is built
natively on its own platform: the Linux archive on Linux, the macOS archive on an Apple
silicon Mac, the Windows archive on Windows.

| Target | How it is checked |
| --- | --- |
| `x86_64-unknown-linux-musl` | Statically linked; `packaged-smoke` runs the archive's own executables against the locked Linux Godot build |
| `aarch64-apple-darwin` | `otool -L` lists only system libraries and frameworks; `packaged-smoke` runs against the locked macOS Godot build |
| `x86_64-pc-windows-msvc` | The import tables of both executables name no CRT DLL, confirming `crt-static`; `packaged-smoke` runs against the locked Windows Godot build |

CI builds and smoke-tests all three archives on every change (`.github/workflows/ci.yml`),
and the release workflow (`.github/workflows/release.yml`) rebuilds them from the release
tag, smoke-tests each one on its own platform, and only then attaches it to the draft
release. The real-client matrix — every check, against every exact Godot build the lock
names — has run against the executables extracted from these archives on Linux, on a Mac
in a logged-in Aqua session and on a Windows desktop session. Where each process boundary
ran is recorded in [Process boundaries](process-boundaries.md#what-has-been-executed-where).

Each provenance attestation is scoped to its packaging host: its `subject` contains the
archive built there with `built_on_this_host: true`, while targets without a local
toolchain appear under `not_built_here`. Across the three native attestations, every
target is the `subject` exactly once and appears under `not_built_here` in the other two.
Nothing is fabricated, and no archive set contains a placeholder.

The reproducibility result above is Linux-only: the macOS and Windows archives establish
packaged execution, not byte-identical builds.

## Godot versions

The compatibility matrix is derived from a committed lock of exact patch versions with
their official download URLs and digests. Nothing in CI or in these pages restates a
version by hand, and a major-minor alias is never a lock entry — an alias silently
re-points at a different build. See the
[compatibility matrix](version-matrix.md), including what it does and does not claim to
have observed.

## Installing into a project

The archive's project-facing assets are installed with `assets install`; see
[Editor bridge](editor-bridge.md) for what they are and for the ownership rules.
