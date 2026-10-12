# 0007 — RUSTSEC-2023-0071 is not exploitable in the resource-server path

**Status:** superseded by [ADR 0009](0009-aws-lc-rs-jwt-backend.md); the
public-key-only invariant and its mechanical guard remain active.

## Supersession

ADR 0009 removed `rsa 0.9.10` by selecting `jsonwebtoken`'s `aws_lc_rs`
backend. The residual-advisory acceptance and red-gate consequences below are
therefore historical and no longer authorize the release state. This record is
retained because its threat analysis explains why the prior package was not
exploitable through the shipped path, and because its private-key guard remains
useful defense in depth for a resource server that must never issue tokens.

## Decision

The original decision kept `rsa 0.9.10` through `jsonwebtoken 10.4.0` while recording
RUSTSEC-2023-0071 (the Marvin Attack) as a visible, reviewed residual advisory.
The shipped server only verifies signatures with public keys; it neither owns an
RSA private key nor performs RSA signing or decryption.

This decision does **not** make the advisory gate green. We do not add a
`cargo-deny` ignore and do not pin the advisory database before CVSS 4.0 support.
`cargo deny check advisories` therefore continues to fail on this advisory until
a safe dependency upgrade exists or maintainers approve another explicit policy.

## Dependency and advisory

The locked dependency path is:

```text
rsa 0.9.10
└── jsonwebtoken 10.4.0
    └── godot-mcp-server
```

RUSTSEC-2023-0071 reports private-key recovery through observable timing leakage
from non-constant-time RSA private-key operations. The advisory states that no
safe upgrade is available.

## Why the shipped path is not exploitable

The conclusion depends on all of the following independently checked facts:

1. The production authentication modules contain no `EncodingKey` use. The only
   four source-tree matches are in `auth/testkeys.rs`, which is reachable solely
   through `#[cfg(any(test, feature = "test-hooks"))]` in `auth/mod.rs`.
2. The production RSA path constructs `DecodingKey` values from public JWK
   modulus/exponent components and calls `jsonwebtoken::crypto::verify`. It does
   not call an RSA signing or decryption API and does not ingest private-key
   material.
3. A default-feature release build contains zero printable DER-like key blocks
   matching `^MI[A-Za-z0-9+/]{60,}` and zero printable `PRIVATE KEY` strings.
   The private test fixtures are therefore absent from the shipped binary.
4. The server is a resource server: callers present tokens, configured JWKS
   supplies public verification keys, and the server verifies signatures. The
   Marvin Attack requires a private-key holder performing observable RSA signing
   or decryption operations; no such operation exists in this production path.

These facts establish absence of the vulnerable operation, not a claim that
`rsa 0.9.10` is generally safe.

## Invalidation condition

This decision is immediately invalid if any production path introduces an RSA
private key or any RSA signing or decryption operation. That includes enabling
`testkeys`/`test-hooks` in a release, accepting private PEM/DER/JWK material,
issuing tokens, signing artifacts, decrypting ciphertext, or adopting a
dependency path that performs a private-key operation on the server's behalf.

When any such change is proposed, the change must stop before release, rerun the
advisory and binary-content checks, and receive a fresh security assessment. This
ADR may not be cited as an exception after its public-key-only assumption fails.

That condition is enforced mechanically, not remembered. The test suite
`crates/godot-mcp-server/tests/adr_0007_private_key_guard.rs` fails the build when
any file under `crates/*/src` acquires an RSA private key or a signing or
decryption call, when `auth/testkeys.rs` loses its
`#[cfg(any(test, feature = "test-hooks"))]` gate, when a default-feature closure
reaches `test-hooks`, or when the parsed JWK shape gains a private member. Its
failure text names the file and line, the assumption above that the change breaks,
and the fact that the correct response is a fresh assessment of
RUSTSEC-2023-0071 — never a relaxed guard. A reader who wants to know whether the
reasoning on this page still holds can therefore run that test instead of
re-deriving it; an invariant that lives only in prose is not an invariant.

## Consequences

* The vulnerability remains visible in every advisory run; a red advisory gate
  is preferable to a green gate that silently excludes current vulnerability data.
* Dependency updates must continue to look for a constant-time RSA implementation
  or a `jsonwebtoken` release that removes the vulnerable path.
* Reviewers have one explicit trigger for reopening this decision: any RSA
  private-key operation in production.
