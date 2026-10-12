# Project leases

Only one `godot-mcp` process may mutate a project at a time. That exclusion is held
against the project's **operating-system object identity**, not against the path you
typed.

## Why identity rather than path

A path is a name, and names are not stable. The same directory can be reached
through a different spelling, a different case, a bind mount or a junction; a
different directory can be put behind the same spelling between the moment you look
and the moment you write. A lease keyed on text would be defeated by both.

So the lease is keyed on the identity of the opened objects: the project root and its
`project.godot`, each identified by the stable object identity the platform reports
for the handle actually held — the volume and file identifiers on Windows, the device
and inode on Unix.

The consequences are the ones you want:

* Two different spellings of the same directory converge to **one** lease, so a
  second client cannot get a second lease by writing the path differently.
* A spelling that is later re-pointed at a **different** object is detected and
  refused, because the retarget is a change of identity even though the text did not
  change.
* Canonical display paths are kept for messages only. They are never the authority.

## Symlinks are refused outright

If any component of the project root or the `project.godot` path is a symbolic link,
acquisition is refused before the lease is taken — even when the link currently
resolves to the same object. A link's target can change between the check and the
write, and no amount of re-checking closes that window.

Supported directory junctions and bind mounts are the deliberate exception: they
converge to the same lease when they name the same object, and the alias evidence is
recorded at acquisition and re-verified before every effect. A changed alias target is
refused.

## Revalidation before every effect

Holding a handle is not the same as knowing the handle still names what you leased.
The recorded identity and the alias evidence are re-derived and compared immediately
before each effect. A mismatch aborts before anything is written, rather than being
detected afterwards.

Handles are opened non-inheritable. A child process — a Godot run, an import helper —
never inherits the authority the server holds.

## Where the lease and the state live

The server discovers the nearest `project.godot` upward from the path you give it, and
everything it keeps for that project lives in one private `.godot-mcp` directory
directly inside the **discovered root**: the lock, the journal, the analysis index, the
audit log and the launch scratch. The directory is opened relative to the held root
handle, so starting the server through the root, a subdirectory, a relative path or a
lexical alias reaches the same lock and the same journal. A second server through any
of those spellings is refused with `conflict` before it opens a journal or creates a
directory, and every Godot launch receives the discovered root as its `--path`.

Earlier builds derived that directory from the path you typed, so a server started
from a subdirectory could have left a second `.godot-mcp` directory inside the
project. At startup every such directory is found and inspected before any write is
admitted:

* its lock is taken with this project's key, so a still-running older server for the
  same project is a `conflict`;
* if exactly one state directory holds open transactions and it is an older one, those
  transactions are reconciled in place with the normal recovery rules;
* if more than one state directory holds open or quarantined transactions, nothing is
  reconciled and writes stay blocked until you resolve them, because choosing one could
  discard the only record of an effect; and
* the selection and the inventory are written to `state-root-migration.json` in the
  canonical `.godot-mcp` directory.

## Same process, different processes

These are two separate layers and it is worth knowing which one you are hitting:

* Within one process, requests for the same lease key are serialized by the server's
  own registry.
* Across processes, the held operating-system lock is the authority. That is what
  makes exclusion hold when the other process is not yours.

A second process that tries to take a held lease gets a `conflict`, immediately,
rather than blocking indefinitely.

## Generations

A lease acquisition establishes a **project generation**. It appears in idempotency
identities and in artifact bindings, so a result produced under an older generation is
never served as if it were current. See [Artifacts](artifacts.md) and
[Recovery](recovery.md).

## Writes are compare-and-swap

A durable write never overwrites blindly.

* **Creating** a file stages the content privately and publishes it with a
  non-replacing rename. If something appeared at the destination in the meantime, the
  publication fails rather than replacing it.
* **Replacing** a file binds both the opened target's identity and its current bytes.
  If either changed, the write is reported stale and nothing is published. Where the
  platform offers an atomic exchange it is used, so an intervening replacement is
  either rejected before publication or restored without clobbering the other actor's
  object. Where it does not, the write fails closed instead of risking the clobber.

A stale outcome is a `conflict`. It means the file changed under you and your write
was not applied — re-read and decide, do not retry blindly.

## What this does not protect against

The lease bounds what *this server* does. It does not stop you, your editor, or any
other program on the machine from writing the same files at the same time. If the
Godot editor has the project open and is writing to it, the lease does not know and
cannot arbitrate.

See [Recovery](recovery.md) for what happens when a process dies holding a lease, and
the [threat model](../security/threat-model.md) for the file boundary in full.
