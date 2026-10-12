# Schema evolution

Three schema families are versioned independently, because they change for different
reasons and are consumed by different parties.

| Family | Location | Consumed by |
| --- | --- | --- |
| Domain contract | `schemas/domain/` | MCP clients |
| Bridge protocol | `schemas/bridge/` | A Godot-side plug-in |
| Operational documents | `schemas/transactions/`, `schemas/gates/`, `schemas/qualification/` | Operators and release automation |

Every schema ships in the release archive, so a client can validate against exactly the
contract a given build implements rather than against a published-but-drifted copy.

## Two rules that apply everywhere

**Unknown fields are rejected.** Every request and every operational document is
validated with unknown fields refused. This is a compatibility decision as much as a
security one: it means an additive field cannot be silently ignored by an older peer that
would then behave differently than the sender assumed.

**A version means one thing.** The envelope's shape version and the sender's protocol
version are separate fields precisely because conflating them makes it impossible to say
which one moved.

## Domain contract

The contract version is reported by `godot-mcp version` as `server-contract`. It changes
whenever a data-transfer object, an error variant or a capability manifest changes shape.

The capability-profile algebra is the strongest invariant here: `full` is computed as
`core` ∪ `editor` ∪ `runtime` from the canonical manifests, so it cannot drift from its
parts by editing a list. A profile's membership is deliberately **not** adjusted to hide a
plane a deployment cannot reach — the contract stays fixed and the runtime answers
`unavailable`. Rewriting the manifest to hide the gap would make the contract depend on
the environment, which is the opposite of what a contract is for.

What counts as a breaking change:

* removing or renaming a tool, resource, error code, capability, plane or confidence
  level;
* narrowing an accepted request — a field becoming required, a value range shrinking;
* changing what an existing outcome means.

What is compatible:

* adding a tool or resource to a profile;
* adding an optional request field that older servers reject and newer ones accept;
* adding an error code, provided existing codes keep their meaning. Clients must treat an
  unknown code as a failure they do not specifically handle rather than as success.

## Bridge protocol

The major component of the sender's protocol version decides compatibility, and an
incompatible major is refused at the handshake rather than allowed to fail later on an
unrecognized field.

Breaking, in this family:

* adding an eleventh message kind — there are exactly ten and the count is part of the
  contract;
* making a `null`-permitted field non-nullable, or omitting a field instead of sending it
  as `null`;
* changing a mutation status's replay semantics.

Because every field is present on every message, there is no "add an optional field"
change here: adding a field changes the key set, and therefore the envelope shape
version.

## Operational documents

The transaction journal, its repair-proof schema, the release-gate document and the
qualification documents are strict and hash-bound. They are not a public API, and they
are not something to hand-edit.

Two properties are load-bearing:

* **A proof is re-derived, never trusted.** The repair command computes every proof field
  itself for the transaction's own recorded effect kind and, for a composite transaction,
  independently re-validates the linked sub-proof. A supplied document cannot suppress a
  check.
* **Evidence is bound to an identity.** A release-gate entry binds the exact tested source
  commit, source tree and, where an artifact was exercised, that artifact's digest. A
  passing check against a different commit, tree or artifact is rejected exactly like a
  missing one, which is what closes the replayed-evidence gap.

## Godot versions

Version compatibility is not a schema, but it follows the same principle: exact patch
versions only, from one committed lock, with official digests. A major-minor alias is
never a lock entry, because an alias silently re-points at a different build and would
make a passing check meaningless. See the
[compatibility matrix](../operator/version-matrix.md).

## Deprecation

A breaking change to the domain contract bumps the contract version and is listed in the
release notes with the replacement named. A tool is not removed and re-added under a new
name in the same release; the old name is refused with `unsupported` and the message names
the replacement, so a client gets a diagnosis rather than a mystery.
