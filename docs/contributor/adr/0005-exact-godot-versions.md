# 0005 — Only exact Godot patch versions, from one lock

**Status:** accepted.

## Decision

Every Godot version this project references is an exact patch version, and all of them come
from one committed lock: `tests/fixtures/godot-releases.lock.json`. Each entry carries the
official release tag, publication timestamp, per-platform artifact names, download URLs,
byte sizes and SHA-256 digests, plus a named provenance for how each digest was obtained.

A major-minor alias is never a lock entry, and no version, URL or digest is restated by hand
anywhere — not in CI, not in a script, not in documentation. The compatibility matrix page
is generated from the lock.

## Why exact versions

**An alias silently re-points.** "Works on 4.x" means whatever that alias resolved to on the
day someone ran it. A test that passed against one build and a bug report filed against
another are not comparable, and neither is reproducible later.

**A digest is the only real identity.** Verifying a download against a locked digest turns
"we tested this version" into "we tested these bytes". Without it, an artifact could be
re-published under the same name and every past result would silently refer to something
else.

**Provenance differs between releases and should be visible.** Older releases predate
per-asset digests, so their SHA-256 was derived from bytes verified against the release's
own checksum file, while newer ones use the published asset digest directly. Recording
*which* method produced each digest means a reader can judge the strength of each entry
rather than assuming they are uniform.

## Why one source

Restating a version in a second place creates two truths, and the second one goes stale
silently. So CI derives its job matrix, its download URLs and its expected digests from the
lock, and a test asserts that no workflow restates a locked value.

That test is the load-bearing part. Without it, "everything derives from the lock" is an
intention rather than a property.

## Defined versus observed

These are different claims and the documentation keeps them apart.

* **Defined** — the lock lists the version and CI has a job for it, derived from the lock.
* **Observed** — a real runner executed that job against those exact bytes and it passed.

Presence in the matrix is the first claim only. The second is recorded per release-gate
observation, bound to the exact source commit, source tree and, where an artifact was
exercised, that artifact's digest — so a pass against a different commit, tree or artifact
is rejected exactly like a missing one.

At the time of writing, no row has been executed against a real Godot installation on the
authoring host, and the generated matrix says so on its face rather than leaving a reader to
assume otherwise.

## Consequences

* Adding support for a new Godot release means adding a lock entry with its official digest.
  There is no way to "just try" a version, which is the intended friction.
* A release whose digest cannot be established with a named provenance does not go in the
  lock.
* The documentation cannot mention a major-minor Godot alias; the documentation checker
  rejects one.
