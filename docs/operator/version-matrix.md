<!-- docs-generated: version-matrix -->
<!-- Regenerate with: cargo run -p godot-mcp-cli --features test-hooks --bin docs-check -- --write -->
# Godot compatibility matrix

This page is generated from `tests/fixtures/godot-releases.lock.json`, the sole source of every Godot
version, download URL and expected digest in this repository. Editing it by hand fails the docs
check; change the canonical source and regenerate instead.

Every entry is an exact patch version. A major-minor alias is never a lock
entry and never appears in CI, in configuration or in these pages, because an
alias silently re-points at a different build.

Minimum supported version: `4.4.0`.

## What this matrix does and does not claim

The matrix below is **defined**: the CI workflows derive their job list, their
download URLs and their expected digests from this lock, and a test asserts no
workflow restates a version by hand. Whether a given row has been **observed
green on a real runner** is a separate fact, recorded per observation and not
implied by presence here. For the tree this page was generated from, every
editor row and every template archive below was exercised by the real-client
matrix (`scripts/real_client/platform_matrix.py`) on a real runner of its
platform: Linux x86_64, an Apple silicon Mac and a Windows Server 2022 desktop
session; see [Process boundaries](process-boundaries.md#what-has-been-executed-where).
An observation belongs to the tree it was made on, so
a row is not observed for a later tree until the matrix runs there again.


## Releases

### `4.4.1`

* Git tag: `4.4.1-stable`
* Channel: `stable`
* Published: `2025-03-26T09:19:36Z`
* Release page: https://github.com/godotengine/godot/releases/tag/4.4.1-stable

| Platform | Kind | Artifact | Bytes | SHA-256 | Digest provenance |
| --- | --- | --- | --- | --- | --- |
| `linux-x86_64` | `editor` | `Godot_v4.4.1-stable_linux.x86_64.zip` | 62203199 | `d6e382fb531019f85630c1f485a561a0d20c4a2344b6c3847735cfee7da812aa` | `sha512_sums_txt_chain` |
| `macos-universal` | `editor` | `Godot_v4.4.1-stable_macos.universal.zip` | 132839010 | `6c670596d21afb7917c739d0b85b10b4ac6a44051ffa2a672f90bc7da5062864` | `sha512_sums_txt_chain` |
| `windows-x86_64` | `editor` | `Godot_v4.4.1-stable_win64.exe.zip` | 69065928 | `1c729738f42e43036a7147838aa9fa56d62101cf90884f315e1ab525e8e69d61` | `sha512_sums_txt_chain` |
| `all-platforms` | `export_templates` | `Godot_v4.4.1-stable_export_templates.tpz` | 1206040900 | `7a8d14ade489fd4d22f178193021fe8a876a9e51068ed4dde26dac3ae4c59a88` | `sha512_sums_txt_chain` |

### `4.5.1`

* Git tag: `4.5.1-stable`
* Channel: `stable`
* Published: `2025-10-15T15:26:55Z`
* Release page: https://github.com/godotengine/godot/releases/tag/4.5.1-stable

| Platform | Kind | Artifact | Bytes | SHA-256 | Digest provenance |
| --- | --- | --- | --- | --- | --- |
| `linux-x86_64` | `editor` | `Godot_v4.5.1-stable_linux.x86_64.zip` | 69572744 | `02ec53d1cc7dbb9cc6355393c61b9ab43d1244751a124f10248a4802830788cd` | `github_release_asset_digest` |
| `macos-universal` | `editor` | `Godot_v4.5.1-stable_macos.universal.zip` | 160646527 | `65c27959d02aaacfc131ec7ecb90179ba8045200cb02982bf2be96d117010b8a` | `github_release_asset_digest` |
| `windows-x86_64` | `editor` | `Godot_v4.5.1-stable_win64.exe.zip` | 76685794 | `defccc78669e644861b4247626b01ae362cd9f23975edf19c8bfd2eb1f6a1783` | `github_release_asset_digest` |
| `all-platforms` | `export_templates` | `Godot_v4.5.1-stable_export_templates.tpz` | 1355592535 | `1998af37f1387684e2c211cdb483daf492fc64dc6b12096bddcdca25b6910c86` | `github_release_asset_digest` |

### `4.6.1`

* Git tag: `4.6.1-stable`
* Channel: `stable`
* Published: `2026-02-16T20:26:38Z`
* Release page: https://github.com/godotengine/godot/releases/tag/4.6.1-stable

| Platform | Kind | Artifact | Bytes | SHA-256 | Digest provenance |
| --- | --- | --- | --- | --- | --- |
| `linux-x86_64` | `editor` | `Godot_v4.6.1-stable_linux.x86_64.zip` | 71799550 | `cecd0cb6b55e931318a9d7237dc4197d69ea914966787a454808523626e2789f` | `github_release_asset_digest` |
| `macos-universal` | `editor` | `Godot_v4.6.1-stable_macos.universal.zip` | 161125030 | `f43613ad72ab1cc9ef14383545f4de344844451a5c9c5f4ea4a923f1f34f91e4` | `github_release_asset_digest` |
| `windows-x86_64` | `editor` | `Godot_v4.6.1-stable_win64.exe.zip` | 79805408 | `088da0457e2d1f7f8a45e6d4605a42b756ea33b9ccd75059bdbc01bb31c2b40d` | `github_release_asset_digest` |
| `all-platforms` | `export_templates` | `Godot_v4.6.1-stable_export_templates.tpz` | 1249771228 | `e6d372afd4fdfaae9571eb5e3568afcd96ce6db9a569244034154faf0ac69875` | `github_release_asset_digest` |

### `4.7.1`

* Git tag: `4.7.1-stable`
* Channel: `stable`
* Published: `2026-07-14T18:03:10Z`
* Release page: https://github.com/godotengine/godot/releases/tag/4.7.1-stable

| Platform | Kind | Artifact | Bytes | SHA-256 | Digest provenance |
| --- | --- | --- | --- | --- | --- |
| `linux-x86_64` | `editor` | `Godot_v4.7.1-stable_linux.x86_64.zip` | 76056717 | `c7ff14fd28472c8d4f193043de30278dcf7e5241a1dcf7566b02e27addaa33ba` | `github_release_asset_digest` |
| `macos-universal` | `editor` | `Godot_v4.7.1-stable_macos.universal.zip` | 166939858 | `897cb7f9799796c717ae75f31446aed883dc92b1d6c3b33d893cc7843fff2fa9` | `github_release_asset_digest` |
| `windows-x86_64` | `editor` | `Godot_v4.7.1-stable_win64.exe.zip` | 84198557 | `c7a289051eaefb460b0106b60e9cd5bee0ef55fd102dcb2bed1eb356cf3d90a1` | `github_release_asset_digest` |
| `all-platforms` | `export_templates` | `Godot_v4.7.1-stable_export_templates.tpz` | 1280486955 | `86409db6200b6f8fd3230989c2d2002851f3dd18acf11d7bdbafddf5a0dd0f72` | `github_release_asset_digest` |

## Digest provenance kinds

* `github_release_asset_digest`: SHA-256 published by the official godotengine/godot GitHub release as the asset digest.
* `sha512_sums_txt_chain`: The official release's SHA512-SUMS.txt entry was verified against the downloaded bytes, and the SHA-256 recorded here was computed from those same verified bytes (the 4.4.1 release predates per-asset digests).

## Required platforms

* `linux-x86_64`
* `macos-universal`
* `windows-x86_64`
* `all-platforms`
