# 0009 — JWT uses the aws-lc-rs backend

**Status:** accepted; supersedes ADR 0007's residual-advisory decision. Still in force at
`jsonwebtoken 11.1.0` (2026-10-13): the same two features, the same backend, and
`cargo tree --workspace --all-features -i rsa` matches no package. The versions below are
the ones this decision was made against.

## Decision

Build `jsonwebtoken 10.4.0` with `default-features = false` and exactly the
`use_pem` and `aws_lc_rs` features. Do not enable its `rust_crypto` backend.

This removes `rsa 0.9.10` from the dependency graph instead of hiding its
RUSTSEC-2023-0071 advisory. `deny.toml` gains no ignore, the advisory database
remains current, and `cargo deny check advisories` must pass because the affected
package is absent.

The plan's line-24 prohibition of OpenSSL and native-tls remains unchanged.
`aws_lc_rs` is a JWT cryptography backend, not a change to the HTTP TLS stack;
`reqwest` remains configured for rustls only.

## Dependency and advisory

Before this decision, the locked dependency path was:

```text
rsa 0.9.10
└── jsonwebtoken 10.4.0
    └── godot-mcp-server
```

RUSTSEC-2023-0071 describes the Marvin Attack against non-constant-time RSA
private-key operations. Its upstream advisory has `patched = []`, so there is no
safe `rsa` upgrade available within the `rust_crypto` backend. Keeping that
backend would leave the required advisory gate red; ignoring the advisory would
only conceal the vulnerable package. Selecting `aws_lc_rs` removes the package
and its affected implementation from the graph.

`aws-lc-rs 1.17.3` and `aws-lc-sys 0.43.0` were already in the locked graph before
this decision, reached through `rustls 0.23.42`, `quinn-proto 0.11.16` and
`rustls-webpki 0.103.13`, which arrive via `reqwest 0.13.4`,
`hyper-rustls 0.27.9`, `tokio-rustls 0.26.4` and `rustls-platform-verifier 0.7.0`.
Selecting `aws_lc_rs` therefore introduces no new C-compiled dependency; it points
`jsonwebtoken` at a provider the workspace already built. `cargo tree --offline -p
godot-mcp-server --features auth --edges normal -i rsa` no longer resolves any
package named `rsa`.

## Why this remains self-contained on musl

Because `aws-lc-sys 0.43.0` was already in the locked graph, it was already
compiled into every build of this workspace: the previous candidate's CI Linux
build log shows `Compiling aws-lc-sys v0.43.0` three times. That same candidate
archived `godot-mcp` and `godot-mcp-supervisor`, and both executables reported
`statically linked` with a `readelf -d` `NEEDED` count of 0. The self-contained
property was therefore already demonstrated with `aws-lc-sys` statically linked,
before this decision changed anything.

The presence of the `cmake` Rust build dependency in `aws-lc-sys` does not mean
the `cmake` program is always invoked. The builder selection in
`aws-lc-sys/builder/main.rs:653-739` first determines whether bindgen is required
and whether the target has supported pre-generated bindings. For supported musl
targets with a working C compiler, `is_bindgen_required()` is false and the
builder selects `CcBuilder`; `CmakeBuilder` is the fallback rather than the
primary path.

The `aws-lc-sys` supported-platform table includes both
`x86_64_unknown_linux_musl` and `aarch64_unknown_linux_musl`. On the local
`x86_64-unknown-linux-musl` release build, with the `cmake` program deliberately
absent from `PATH`, the build completed through `CcBuilder`. Its object code was
linked into the executable rather than loaded from a shared library.

The resulting binary was checked independently:

```text
file       -> static-pie linked
ldd        -> statically linked
readelf -d -> NEEDED entries: 0
```

The release archive must preserve the same property for both shipped
executables, `godot-mcp` and `godot-mcp-supervisor`. A successful compile alone
is insufficient; the distribution gate checks the archived binaries themselves.

This local evidence covers Linux musl. The macOS and MSVC build paths cannot be
executed on this host and remain subject to their native CI legs; this ADR does
not claim that local Linux verification proves those platforms.

## Algorithm verification

The configured asymmetric allowlist remains exactly `{RS256, PS256, ES256}`.
Changing a cryptography backend therefore requires real end-to-end signature
checks for all three paths, not only policy tests that admit their names.

The auth tests create genuine signed JWTs from test-only private keys, publish
the matching public JWK metadata, and pass each token through the complete proxy
hop, header policy, exact-`kid` selection, JWK metadata, signature, and claims
pipeline. The RSA fixtures and signing code remain behind
`#[cfg(any(test, feature = "test-hooks"))]`; no private key enters a default
release.

## Relationship to ADR 0007

ADR 0007 accepted a visible red advisory only because the shipped resource
server held no RSA private key and performed verification only. This decision
supersedes that acceptance: `rsa 0.9.10` is no longer present, so release safety
no longer depends on arguing that its vulnerable operation is unreachable.

ADR 0007 remains in the repository as a historical record of the prior threat
analysis. Its public-key-only resource-server invariant and
`crates/godot-mcp-server/tests/adr_0007_private_key_guard.rs` remain active as
defense in depth. Token issuance, private-key ingestion, RSA decryption, or
shipping `test-hooks` would still expand the server's security boundary and must
fail mechanically. The guard is no longer an exception that permits a vulnerable
dependency or a red advisory gate.

## Consequences

* `Cargo.lock` removes `rsa 0.9.10` and its now-unused dependency branch. It adds no
  package: the entire aws-related delta is one added line, `"aws-lc-rs"` inside
  `jsonwebtoken 10.4.0`'s `dependencies` list, which is a single new dependency edge
  to an already-present package.
* The advisory gate becomes green by dependency removal, never by changing
  `deny.toml`, suppressing RUSTSEC-2023-0071, or pinning stale vulnerability data.
* Release builds require a C compiler for `CcBuilder`, but supported musl targets
  with pre-generated bindings do not require bindgen or the `cmake` program.
* OpenSSL, native-tls, and the `reqwest` TLS configuration remain untouched.
* Any future aws-lc update must rerun the RSA/EC signature tests, advisory checks,
  musl release build, and archived-binary dynamic-dependency checks.

## Invalidation condition

Reopen this decision if an update reintroduces `rsa`, selects `CmakeBuilder` on a
supported release target without a provisioned and reviewed toolchain, adds a
dynamic `NEEDED` dependency to either archived executable, or makes any of
RS256, PS256, or ES256 fail its genuine end-to-end verification path. Replacing
one cryptography implementation with another advisory ignore is not an allowed
remediation.
