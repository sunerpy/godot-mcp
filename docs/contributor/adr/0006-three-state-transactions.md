# 0006 — A durable mutation has exactly three terminal states

**Status:** accepted.

## Decision

A durable transaction ends in exactly one of `compensated`, `committed` or `quarantined`.
A fourth outcome is not representable in the type that records it.

Each transition is backed by a proof that is re-derived rather than trusted, and there is
no bypass: no force flag, and no operation that clears a quarantine without a proof.

## Why three, and why quarantine is one of them

The tempting design has two states and an implicit third: succeeded, failed, and "we're not
sure, assume it's fine". That third case is where data loss lives.

An unprovable state is therefore given a name, and that name **blocks writes**. Quarantine is
deliberately loud and deliberately inconvenient, because the alternative is a server that
keeps writing to a project whose state it cannot describe.

Making the fourth case unrepresentable — rather than merely discouraged — means no future code
path can introduce "assume it worked" by accident. It would not compile.

## Proofs are re-derived, not supplied

An operator repair takes a proof file, but the file is not the evidence. The command computes
every proof field itself, for the transaction's own recorded effect kind, against the current
project object and generation, and compares. A composite transaction's linked sub-proof is
re-validated independently, and the supplied file cannot suppress that check.

The reason is simple: a proof you accept on the operator's word is a comment, not a proof. The
file's job is to state an intent precisely enough to be checked.

## Three effect kinds, and no fourth

`file`, `process_launch`, `editor_runtime`.

Composite operations were the pressure point. A canonical text-resource write invokes a
supervised Godot helper *and* changes a file, which looks like it needs a fourth kind. It does
not: it is classified `file`, and its proof is a conjunction of a `process_launch` sub-proof for
the helper and a `file` sub-proof for the on-disk change. It commits only if both hold.

Adding a fourth kind would have meant a fourth proof form, a fourth recovery path, and a fourth
set of crash cases to reason about — for a case that composition already covers exactly.

## One journal

There is exactly one journal implementation, one schema, and one set of fsync phases. Later work
that needed fairness, backpressure and mode-aware idempotency reuses it and is barred from
adding persistence.

Recovery reasoning depends on there being a single place that knows what happened. Two journals
would mean reconciliation has to decide which one is right, and there is no principled way to do
that.

## Effects go through one gate

A durable effect is performed only through a permit-typed gateway whose permit type is private to
the layer owning the transaction contract. A new mutation path cannot skip journalling, because
without a permit it does not compile.

This is the difference between a convention and an invariant.

## Identity is the whole context

An idempotency identity binds the project's object identity and generation, the mode-specific
principal and session, the operation and tool family, the idempotency identifier, and the full
canonical request hash.

Reuse across a different tool, session, principal, project object or generation **conflicts**
rather than returning the other context's result. Returning it would be a cross-context
information leak wearing a cache hit's clothing.

## Consequences

* An operator sometimes has to intervene. That is the intended cost of never guessing.
* Recovery is auditable: every transition in the journal is backed by a proof that was
  independently re-derived at the time it was recorded.
* `conflict` is a normal, expected outcome, and clients must handle it rather than retrying
  blindly. See [Recovery](../../operator/recovery.md).
