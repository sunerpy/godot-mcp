# 0003 — There is no arbitrary-script-execution tool

**Status:** accepted.

## Decision

`unsafe_script` is a reserved identifier and nothing more. There is no capability variant
for it, no handler, no profile membership, no configuration option, and no presence in the
shipping binary. A request naming it is refused exactly as an unknown tool is.

## Why

Several comparable tools expose an "execute this GDScript" or "run this editor script"
entry point. It is genuinely useful, and it is genuinely the wrong trade for this project.

**It collapses the reviewable surface to one entry point.** Every other decision in this
repository — capability profiles, effect kinds, proof-backed transactions, containment
boundaries — exists so that what the server can do is enumerable and each action is
accountable. A tool that runs arbitrary code makes all of that advisory: the profile no
longer describes what can happen, and the transaction journal records "a script ran"
rather than what it did.

**Loading a project already runs code, and that is exactly the argument for restraint,
not against it.** A `@tool` script executes in the editor and a `@tool` autoload runs as
soon as the project loads, so a Godot project's own code is going to execute. Because that
baseline exists and cannot be removed, the marginal risk of an *additional* explicit
execution path is not "it's already unsafe, so why not" — it is that the existing risk is
bounded by what the project's own code does, while an execution tool is bounded by what
any caller can compose.

**An unsafe path that exists gets used.** A feature behind a flag is a feature, and the
deployments that most need the restriction are the ones most likely to turn it on to get
past an obstacle.

## What replaces it

Outcome-oriented tools. Instead of a caller writing a script to answer a question, the
question is a tool with a schema, a capability, a plane and a typed result. Where an
answer genuinely requires the engine, a supervised Godot invocation does it — inside a
containment boundary, with a durable receipt.

Where no sound answer exists, the outcome is `uncertain`. That is the honest alternative
to letting a caller work around the gap with a script.

## Consequences

* Some things a script could do are not expressible. That is accepted; a missing capability
  is a visible, discussable gap, while an execution tool hides every gap behind the same
  unaudited door.
* The tool surface grows by deliberate addition, which is slower and reviewable.

## Enforcement

The absence is structural and is proven rather than asserted. Every profile is enumerated
in full and none contains an execution tool; the kernel refuses the reserved name and a
set of execution-shaped names; strict schema validation rejects unknown fields so a
parameter cannot smuggle a script in; and a live client's negative calls are exercised in
tests. A byte-level scan of the shipping artifact is a supplementary check, not the
argument.
