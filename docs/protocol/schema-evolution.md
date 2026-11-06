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

### Contract 0.3.0

Contract 0.3.0 adds seventeen tools — `editor.node.get`, `editor.types.search`,
`editor.log.get`, `editor.preferences.get`, `editor.shortcuts.get`, `editor.scene.edit`,
`editor.scene.save`, `editor.scene.close`, `editor.main_screen.set`,
`editor.screenshot.capture`, `editor.filesystem.scan`, `editor.script.open`,
`editor.preferences.set`, `editor.shortcuts.set`, `editor.play.start`, `editor.play.stop` and
`runtime.performance.get` — the artifacts
resource in the `editor` profile, the batch operation `call_method`, and the `results` field
of every batch answer. The 0.2.0 manifests stay frozen in `schemas/domain/profiles/v0.2/`.

Three behaviours narrow while a live editor is connected, so they are listed with the
release rather than hidden in it:

* `project.settings.patch` is carried out by the editor, which writes `project.godot` with
  Godot's own writer and keeps no comments;
* every other headless write to `project.godot` is refused with `conflict`;
* `project.import.run` and `project.uid.update` are refused with `conflict`.

An artifact a 0.2.0 server recorded is unreadable to a 0.3.0 server and is discarded when
it starts; an artifact stays readable for 15 minutes in any case.

### Contract 0.4.0

Contract 0.4.0 adds thirty-nine tools and widens six requests. The 0.3.0 manifests stay
frozen in `schemas/domain/profiles/v0.3/`.

* Seven project-wide static views, in every profile: `project.dependencies.audit`,
  `project.metrics.get`, `project.search`, `project.signals.get`, `project.uid.get`,
  `scene.exports.get` and `scene.nodes.find`.
* Eleven managed-session tools, in the `runtime` and `full` profiles: `runtime.properties.set`,
  `runtime.properties.batch.get`, `runtime.wait`, `runtime.sample`, `runtime.nodes.find`,
  `runtime.navigation.path.get`, `runtime.node.move`, `runtime.ui.press`,
  `input.recording.start`, `input.recording.stop` and `input.replay`. A game session whose probe
  predates them answers `unsupported` for each.
* Three test tools for a managed game, in the `runtime` and `full` profiles:
  `runtime.time_scale.set`, `runtime.image.compare` and `test.scenario.run`. The runtime probe
  gains four methods for them; a game session whose probe predates them answers `unsupported`.
* Three tools that run caller-written GDScript
  ([ADR 0012](../contributor/adr/0012-declared-script-execution.md)): `script.run` in a
  supervised headless Godot and `editor.script.run` in the connected editor, in the `editor`
  and `full` profiles, and `runtime.script.run` in a managed game, in the `runtime` and `full`
  profiles. Each is listed and refused with `permission_denied` unless the operator turned on
  `allow_scripts` for the launch. The editor bridge and the runtime probe gain the methods
  `editor.script.run` and `runtime.script.run`, which a server lists in a session's
  capabilities only while the switch is on; add-on 0.4.0 and probe 0.4.0 implement them.
* Three engine reads, in the `runtime` and `full` profiles: `scene.read` and `resource.read`
  call the reading methods of a scene's or a resource's objects in a supervised headless Godot,
  and `shader.check` compiles a shader there and answers its mode and uniforms.
* `editor.scene.read`, in the `editor` and `full` profiles, makes the calls `scene.read`
  makes on the scene the connected editor is editing, unsaved edits included. Add-on 0.4.0 serves
  it with the bridge method `editor.scene.read`; with an add-on that predates it the tool answers
  `unsupported` and says to update it.
* `project.batch.apply`, in the `editor` and `full` profiles, applies a batch to each of up to
  16 scenes and resources in one Godot run and publishes every file in one transaction.
* Two Android device tools, in the `runtime` and `full` profiles: `android.devices.list`
  lists the devices the configured SDK's adb sees, and `android.deploy` installs and starts
  the APK of an Android export on one of them, proving each phase.
* Eight live-editor tools, in the `editor` and `full` profiles: `editor.camera.get`,
  `editor.camera.set`, `editor.script.close`, `editor.dialogs.get`, `editor.dialog.respond`,
  `editor.restart` ([ADR 0013](../contributor/adr/0013-editor-restart.md)),
  `editor.performance.get` and `editor.resource.preview`. Add-on 0.4.0 serves a bridge method for
  each and adds three optional fields to its session description, its process id and whether it
  can move a 3D camera and close a script tab, and a `restart` record to the editor state it
  reports. With an add-on that predates them each tool answers `unsupported` and says to update
  it, and on a Godot build without the API a tool needs it answers `unsupported` naming the build.
* `project.files.list` accepts an optional `glob`.
* `project.export` accepts `kind` (`project`, `pack` or `patch`), `packFormat` and
  `patches`, and exports Android presets as APKs, which a 0.3.0 server refused as
  `unsupported`.
* `project.settings.patch` accepts `set_setting` and `clear_setting` for any setting the typed
  operations do not own, and `project.settings.get` accepts `settings` and `prefix` to select
  entries.
* A batch's `set_property`, `clear_property` and `set_subresource` take a property path
  (`position:x`, `material:albedo_color`) as well as a name.
* A `new_resource` argument may name any resource class or global script class except
  `Script`, `GDExtension` and the classes that inherit them, and may carry initial properties.
* `call_method` takes `repeat`, and its table of editing calls lists more classes: the
  animation tree's state machine and blend nodes, `VisualShader`, `TileSet`,
  `TileSetAtlasSource`, `MeshLibrary`, the navigation resources and `Control`.

All of them are additions: a 0.3.0 client loses nothing, and a request it could send means
what it meant. One answer changes: a write whose value the engine does not keep, such as a
player's `bus` naming a bus the project's layout lacks, which Godot replaces with `Master`, is
refused as `value_not_kept` and changes nothing, where a 0.3.0 server answered success for a
file that held the other value.

### Contract 0.5.0

Contract 0.5.0 adds one tool, recipe prompts and an HTTP mode. The 0.4.0 manifests stay frozen
in `schemas/domain/profiles/v0.4/`.

* `scene.recipe.apply`, in the `editor` and `full` profiles, applies a recipe by name: its
  `recipe` is an object with one key, the recipe's name, holding the recipe's parameters, and
  the server expands it into the operations of one `scene.batch.apply` batch, journaled and
  replayed under the recipe tool's own name, or, for a scene open in the connected editor, one
  undoable action named `godot-mcp: scene.recipe.apply`. Its schema lists every recipe's
  parameters, and its answer adds `recipe`, the name and every parameter value used, and
  `operations`. The theme recipe makes a resource rather than nodes, and the tool refuses it
  and names `resource.create`. See [Recipes](../operator/recipes.md).
* `prompts/list` adds one prompt per recipe in the `editor` and `full` profiles,
  `godot-mcp.recipe.<name>`, generated from the same registry; `godot-mcp.profile-guide` stays
  in every profile. Prompts are not part of the profile manifests.
* `http.mode` takes `open` ([ADR 0014](../contributor/adr/0014-open-http-mode.md)), with the
  optional key `http.token` and the variables `GODOT_MCP_HTTP_TOKEN_ENV` and
  `GODOT_MCP_HTTP_TOKEN_FILE`; under it `http.host` and `http.origin` are optional, and any
  `http.bind` is accepted. A journal record's `transport_mode` takes `open`, and `health.get`
  reports `http.mode` as `open` for such a listener.

All of them are additions: a 0.4.0 client loses nothing, and a request it could send means what
it meant. A 0.4.0 server refuses a configuration file with `mode = "open"`, as it refuses every
unknown value.

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

Adding a request method is compatible. Methods travel inside the envelope, a peer lists the
methods it implements when the session opens, and the server never sends one the peer did
not list: a tool whose method an older add-on lacks answers `unsupported`.

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
