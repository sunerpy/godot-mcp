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

### Contract 0.6.0

Contract 0.6.0 adds two tools, a project file format and a configuration key
([ADR 0016](../contributor/adr/0016-project-commands.md)). The 0.5.0 manifests stay frozen in
`schemas/domain/profiles/v0.5/`. The same release corrects schemas that disagreed with their
checks and writes each tool's schema with only the definitions it uses.

* `editor.commands.list` and `editor.commands.call`, in the `editor` and `full` profiles, list and
  call the commands a project declares in `godot_mcp_commands.json`. The list reads the file and
  runs nothing; the call checks its arguments against the command's declared schema and is
  carried out by the connected editor. See [Project commands](../operator/project-commands.md).
* The declaration's format, version 1, is published as
  `schemas/domain/project-commands-v1.schema.json`.
* The JSON Schema subset that checks a command's arguments, `godot_mcp_domain::jsonschema`, gains
  `maximum`, the inclusive upper bound beside `minimum`.
* The configuration key `project_commands` and the variable `GODOT_MCP_PROJECT_COMMANDS`. Unset,
  project commands are on for stdio, `trusted_local` and `open`, and off for `proxied_oidc`.
* The editor bridge gains the method `editor.commands.call`, which a server lists in a session's
  capabilities only while project commands are on; add-on 0.5.0 implements it. With an add-on that
  predates it the tool answers `unsupported` and says to update it.

All of them are additions: a 0.5.0 client loses nothing, and a request it could send means what
it meant. A 0.5.0 server refuses a configuration file with `project_commands`, as it refuses every
unknown key, and ignores a `godot_mcp_commands.json`, which is an ordinary project file to it.

#### 0.6.0 corrections

Release 0.6.0 also corrects places where a tool's advertised schema and its own check
disagreed. No data-transfer object, error variant or capability manifest changes shape for
them.

* In `scene.batch.apply`, `scene.create`, `editor.scene.edit` and `project.batch.apply`, the
  `method` of `connect_signal` and `disconnect_signal` was advertised as `call_method`'s list of
  editing methods, so a client that checks its arguments against `tools/list` could not send a
  callback such as `_on_body_entered`, which the server takes. It is now a name like any other;
  `call_method` keeps the list.
* The length limit on a name, a node path or a property path counts characters, as Unicode
  scalar values, the unit `maxLength` counts in, in `scene.batch.apply`, `scene.create`,
  `resource.batch.apply`, `resource.create`, `project.batch.apply`, `editor.scene.edit`,
  `scene.read`, `resource.read`, `editor.scene.read`, `editor.node.get`,
  `editor.selection.set`, `project.settings.patch` and `project.create`. Their checks counted
  UTF-8 bytes and refused a value of multi-byte characters that fit its limit in characters;
  such a value is now taken. The limits themselves are unchanged.
* Their schemas state the rules those checks already applied: the length limits they lacked,
  no control character (U+0000 to U+001F and U+007F to U+009F) where the check refuses one, the
  shape of a relative node path and of a node handle, the extension a scene or script path
  needs, the characters an input action's name cannot hold, and that a project's name is not
  white space alone.

No request a 0.5.0 server took is refused.

#### 0.6.0 schema writing

Release 0.6.0 also changes how the tool schemas are written and nothing they decide: the
schemas take and refuse exactly the requests they did before.

* A tool's `$defs` holds only the definitions its schema refers to, directly or through another
  definition. A tool that takes only plain values no longer carries the definitions of a
  written value, a resource reference or a `call_method` argument, and one that writes values
  no longer carries the plain value's.
* A tool that takes operations, `scene.batch.apply`, `scene.create`, `resource.batch.apply`,
  `resource.create`, `project.batch.apply` and `editor.scene.edit`, defines in its own `$defs`
  each operation field schema it repeats: the node path of `node`, `parent`, `from` and `to`,
  the property path of `property`, the scene path of `scene`, the name of `name`, `type`,
  `signal`, `group` and `class`, a signal's callback `method`, `call_method`'s editing `method`,
  `arguments` and `repeat`, and a sub-resource's `properties`. Every field that held the schema
  holds a `$ref` to it with nothing beside it, so a client reads the same field either way; a
  field schema a tool holds once stays where it is.

A tool that took no tagged value still carries no `$defs`, and every reference resolves in the
tool's own schema. Measured as
[How much each profile lists](../operator/profiles-and-outcomes.md#how-much-each-profile-lists)
describes, `tools/list` shrinks from 406,072 to 312,424 bytes in the `editor` profile, from
231,860 to 193,983 in `runtime` and from 605,686 to 474,161 in `full`, measured before the
project-command tools were added; `core`, whose tools take no tagged value, stays at 28,044.

### Contract 0.7.0

Contract 0.7.0 adds one tool and the fields that step, sample and replay a running game by
physics frame ([ADR 0017](../contributor/adr/0017-frame-stepping.md)). The 0.6.0 manifests stay
frozen in `schemas/domain/profiles/v0.6/`.

* `runtime.frames.step`, in the `runtime` and `full` profiles, advances a managed game session by
  exactly `frames` physics frames (0 to 600) and holds it paused or lets it run; its answer says
  which frames advanced, whether the game is held and why the step ended. See
  [Managed sessions](../operator/managed-sessions.md#stepping-by-physics-frames).
* `runtime.sample` takes the optional `cadence`: `interval`, the default and what it did before,
  or `physics_frame`, one sample at the start of every physics frame. Its `time_base` adds
  `cadence`, and its `interval_ms` is `null` under `physics_frame`.
* `input.replay` takes the optional `schedule`: `time`, the default and what it did before, or
  `physics_frame`; an event takes the optional `frame` (0 to 30000). The answer adds `schedule`,
  `first_physics_frame`, `deliveries`, `scheduled_frames` and `max_lag_frames`, the last two
  `null` under `time`, as `scheduled_ms` and `max_lag_ms` are under `physics_frame`.
* `input.recording.start` and `input.recording.stop` answer `first_physics_frame`, and each
  recorded event its `frame`.
* The runtime probe, version 0.5.0, implements the bridge method `runtime.frames.step`. With a
  probe that predates it, the step answers `unsupported`, and so do a sample and a replay that ask
  for `physics_frame`, before anything is journaled or sent.

The schema of a replayed event no longer requires `t_ms`, and its check requires the field of the
replay's schedule, which is `t_ms` under the default one: a request a 0.6.0 server took means what
it meant, and one it refused for a missing `t_ms` is still refused, by the check. A 0.6.0 client
loses nothing. A 0.6.0 server refuses `cadence`, `schedule` and `frame`, as it refuses every
unknown field, so a recording a 0.7.0 server answered replays there only without its events'
`frame`.

#### 0.7.0 corrections

Contract 0.7.0 also corrects further places where a tool's advertised schema and its own check
disagreed. For these, no data-transfer object, error variant or capability manifest changes
shape, and no field is added, renamed or retyped.

* The length limit counts characters, as Unicode scalar values, the unit `maxLength` counts in,
  in fields whose checks counted UTF-8 bytes: `editor.types.get` `class`, `editor.types.search`
  `query` and `base`, `editor.log.get` `epoch`, the `path` of `editor.script.close`,
  `editor.resource.preview`, `scene.read`, `resource.read` and `shader.check`, the `action` and
  `key` of `input.action.send`, `input.key.send` and the input steps of `input.sequence.send` and
  `test.scenario.run`, `runtime.performance.get` `monitors`, a text value of
  `project.settings.patch` `set_value`, `project.search` `query`, `project.signals.get` `signal`,
  `scene.exports.get` `node`, the `type`, `group`, `name`, `property` and `value` of
  `scene.nodes.find`, the `include` and `exclude` globs of `project.dependencies.audit`,
  `project.metrics.get`, `project.search` and `scene.nodes.find`, and `project.files.list` `glob`.
  A value of multi-byte characters that fits its limit in characters is now taken. The limits are
  unchanged; a read's `path` is measured whole, its `res://` prefix included, as its schema
  always measured it.
* These refuse a control character (U+0000 to U+001F and U+007F to U+009F), and their schemas
  say so: the class names, the epoch and the two paths of the editor tools above, and the action
  and key names of the input tools above and of `input.replay` events (`action`, `key` and
  `physical`). None could name what the field names: no engine or script class name holds one,
  the plugin names an epoch in hexadecimal digits, the server writes and runs no path that holds
  one, the project settings the server writes refuse one in an action name and Godot's input-map
  editor one below U+0020, and Godot's `OS.find_keycode_from_string` knows no key name holding
  one. A search's text, a setting's text value, a monitor name and the analysis tools' names and
  globs still take one.
* `editor.types.search` `base` and the `name` of `remove_autoload` and `remove_input_action` were
  advertised with `minLength` 1 while their checks took the empty string; the checks now refuse
  it.
* The schemas state rules their checks already applied: the shape of the `path` of
  `editor.script.close` and `editor.resource.preview` (segments neither empty, `.` nor `..`, and a
  `.gd` ending for a script), the project-relative spelling and the extension, in any letter
  case, of the `path` of `scene.read`, `resource.read` and `shader.check`, and the `uid://` form
  of `project.uid.get` `uid`.
* No pattern spells "any character" as `.`, which ECMA-262 reads as any character but a line
  terminator (`\n`, `\r`, U+2028 and U+2029). The project-relative path pattern of every engine
  path field, `project.test`'s selection and `debug.command`'s GDScript path name the
  characters they take instead. A strict client now takes a path holding U+2028 or U+2029, as
  the checks always did, and `debug.command` a path holding `\n` or `\r` too; it refuses a
  `project.test` selection with `,` or `;` after a line terminator, as the check always did.

Some requests a 0.6.0 server took are refused: a control character in the fields of the second
point, an empty `base` or removed name, and a `res://`-prefixed read path of 1,025 to 1,030
characters. Of these, only the control characters met the 0.6.0 schemas.

Measured as
[How much each profile lists](../operator/profiles-and-outcomes.md#how-much-each-profile-lists)
describes, the frame fields and the corrections together grow `tools/list` from 28,044 to
28,074 bytes in the `core` profile, from 315,484 to 316,607 in `editor`, from 193,983 to
198,347 in `runtime` and from 477,221 to 482,678 in `full`.

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
