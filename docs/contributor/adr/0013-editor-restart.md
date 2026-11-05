# 0013 — The editor is restarted only when nothing it holds can be lost

**Status:** accepted for contract 0.4.0. It takes up the restart that
[ADR 0011](0011-live-editor-operability.md) left to the person using the editor, for the
2026-11-05 goal of business parity, which asks for restarting and reloading the project.

## Context

ADR 0011 left restarting the editor out of contract 0.3.0 for two reasons: Godot's own restart
first asks whether to save each scene with unsaved changes, a question only the person using the
editor can answer, and Godot 4.4.1's restart does not pass `--headless` on to the editor it
starts. A setting whose answer says `restart_required` still needed someone at the editor, and so
did an editor that had to reload a project after a change outside it.

`EditorInterface.restart_editor(save)` is the public way a plugin restarts the editor: with
`save` it saves every open scene first, then it does what Project > Reload Current Project does.
Both reasons were measured again on Godot 4.4.1, 4.5.1, 4.6.1 and 4.7.1, official Linux x86_64
builds, with a probe plugin in a headless editor (and a windowed one under Xvfb):

- **It asks before it discards.** `restart_editor(false)` with an unsaved scene opens a
  ConfirmationDialog, Don't Save, Cancel and Save & Close, and restarts nothing; pressing Cancel
  closes it and the editor keeps running. With an unsaved script tab, even `restart_editor(true)`
  opens the same kind of question, Save & Reload: it saves scenes, not scripts. Its source asks
  the same way, on every build, about resources changed in its global history and about anything
  another plugin reports unsaved; from Godot 4.5.1 on about a running game, which 4.4.1 stops
  without asking; and on 4.7.1 about an export-template download. So the call itself never
  discards, but it can stop at a question nobody in an automated session answers.
- **No build passes `--headless` on.** An editor started with `--headless` restarted into one
  that found no display and exited with "Unable to create DisplayServer, all display drivers
  failed", on all four builds, not only 4.4.1. The editor passes on the options Godot marks
  forwardable, and `--display-driver headless --audio-driver Dummy` is among them: an editor
  started that way restarted, headless, as a new process on all four builds. A windowed editor
  restarted with a window on all four.
- **The call unloads its caller.** The restart unloads every editor plugin inside the call. The
  probe plugin calling it from its own code crashed Godot 4.4.1, 4.5.1 and 4.6.1 and made 4.7.1
  report a freed instance; called from a node of its own outside the plugin, it returned
  cleanly on all four.

## Decision

`editor.restart` is offered, in the `editor` and `full` profiles, within these rules.

- **Refused while anything known is unsaved.** An open scene or script the editor reports
  unsaved refuses the restart with `conflict`, naming it, unless `save` is set. With `save` the
  open scenes are saved first through the same path as `editor.scene.save`, each proven saved by
  the editor's report or by its file, and on Godot 4.7.1, which gives a plugin
  `ScriptEditor.save_all_scripts` and `get_unsaved_files`, the script tabs too; an unsaved
  script tab on 4.4.1, 4.5.1 and 4.6.1 refuses a saving restart with `unsupported`. A scene that
  was never saved, a running game and a visible dialog refuse it as well.
- **Never discards.** What the editor still finds unsaved when it is asked, a script tab an older
  build cannot report or a resource changed in the inspector, makes it ask instead of restart.
  The add-on cancels every dialog the restart opened, so the question saves and discards
  nothing, and the call answers `conflict` quoting the question and naming what `save` wrote
  before it.
- **Refused where it would not come back.** An editor started with `--headless` is refused with
  `unsupported` on every build. Only Linux tells a plugin the options its process started with,
  through `/proc/self/cmdline`, so a headless editor elsewhere is refused too. An editor this
  server runs as a managed session (`editor.session.start`) is refused with `conflict`: the
  restarted editor would be a new process outside that session's process boundary. The add-on
  reports its process id when it connects, and the session manager names the session running
  it.
- **Called from outside the plugin, once the answer left.** The add-on answers the request,
  committed, and a helper node of its own calls `restart_editor(false)` once the answer has left
  the socket, so none of the plugin's code is on the stack when the editor unloads it.
- **Proven by a new process connecting.** The call answers success only when the add-on of a
  different editor process connects within `timeoutMs` (120000 by default, 1000 to 600000);
  otherwise `uncertain`, naming the process that was asked and whether it is still connected.
  The restart is journaled, covering the whole project when `save` is set, so a lost answer to
  a saving restart blocks writes until it is resolved.
- **Nothing else meanwhile.** From the request until the restarted editor connects, the server
  sends the editor no other change, admits no exclusive headless run, and lets no headless writer
  touch the project's files: the editor that exits may still write them, and the restarted one
  is loading them.

## Consequences

- The add-on 0.4.0 serves the restart, and its session description carries the process id. An
  add-on that does not report it answers `unavailable`, since whether it is a managed session
  cannot be told.
- An agent can restart an editor it did not start only when that editor has a display, or was
  started with `--display-driver headless`; the live suite starts its restarting editors that way.
- A restart's answer gives the previous and the new process id and how long the wait took. The
  restarted editor reopens the scene that was being edited, as Godot's own reload does.

Not decided here: restarting into the project manager, restarting an editor on Windows or macOS
(not measured), and answering a question the editor asks before it restarts, which stays with the
person using it.
