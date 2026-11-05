@tool
extends RefCounted
## `editor.restart`: restart the editor, as Project > Reload Current Project
## does, once nothing the editor holds can be lost (ADR 0013).
##
## The restart itself is EditorInterface.restart_editor(false), called from a
## helper node (`restarter.gd`) once the answer to this request has left the
## socket: the editor unloads its plugins inside that call, so none of the
## plugin's code may be on the stack. Before it is asked:
##
## * a restart already under way, a running game, a visible dialog, a scene
##   that was never saved, or a scene or script this editor reports unsaved
##   (unless `save` is set) refuses the restart, with nothing changed;
## * with `save` the open scenes the server listed are saved first, through
##   the same path as editor.scene.save, and on Godot 4.7.1 the script tabs
##   too, each proven saved before the restart is asked for;
## * an editor started with `--headless` is refused: Godot does not pass that
##   option on to the editor it restarts, which then finds no display and
##   exits (measured on Godot 4.4.1 to 4.7.1). An editor started with
##   `--display-driver headless` restarts headless, since that option is passed
##   on. Only Linux tells a plugin which options its editor started with, so
##   elsewhere a headless editor is refused too.
##
## Whatever the editor still finds unsaved when it is asked (a script tab an
## older build cannot report, a resource changed in the inspector) makes it ask
## a question rather than restart; the helper node cancels that question, so
## nothing is saved or discarded, and reports the decline through
## editor.state's `restart`.

const Restarter = preload("restarter.gd")
const EditorDialogs = preload("editor_dialogs.gd")

const MAX_NAMED := 8


static func _ok(payload: Dictionary) -> Dictionary:
	return {"ok": true, "payload": payload, "mutation_status": "committed"}


static func _fail(kind: String, message: String, status: String = "not_started") -> Dictionary:
	return {"ok": false, "error_kind": kind, "message": message, "mutation_status": status}


static func _godot() -> String:
	var version: Dictionary = Engine.get_version_info()
	return "%d.%d.%d" % [int(version.get("major", 0)), int(version.get("minor", 0)), int(version.get("patch", 0))]


static func _named(paths: Array) -> String:
	var shown: Array = paths.slice(0, MAX_NAMED)
	var text := ", ".join(shown)
	if paths.size() > shown.size():
		text += " and %d more" % (paths.size() - shown.size())
	return text


## The options this process was started with, or null where the system does
## not tell (anywhere but Linux).
static func launch_arguments():
	var file := FileAccess.open("/proc/self/cmdline", FileAccess.READ)
	if file == null:
		return null
	var raw: PackedByteArray = file.get_buffer(65536)
	file.close()
	if raw.is_empty():
		return null
	var arguments: Array = []
	var start := 0
	for index in raw.size():
		if raw[index] == 0:
			arguments.append(raw.slice(start, index).get_string_from_utf8())
			start = index + 1
	if start < raw.size():
		arguments.append(raw.slice(start).get_string_from_utf8())
	return arguments


## Why a restart of this editor would not come back, or "".
static func _not_restartable() -> String:
	if DisplayServer.get_name() != "headless":
		return ""
	var arguments = launch_arguments()
	if arguments == null:
		return "this editor runs headless, and this system does not tell whether it was started with --headless, which Godot does not pass on to the editor it restarts (measured on Godot 4.4.1 to 4.7.1): the restarted editor would find no display and exit. Nothing was restarted."
	if arguments.has("--headless"):
		return "this editor was started with --headless, which Godot does not pass on to the editor it restarts (measured on Godot 4.4.1 to 4.7.1): the restarted editor would find no display and exit. Start the editor with --display-driver headless instead, which a restart keeps. Nothing was restarted."
	return ""


static func request(handlers, mutations, payload: Dictionary) -> Dictionary:
	var save := bool(payload.get("save", false))
	var scenes = payload.get("scenes", [])
	if typeof(scenes) != TYPE_ARRAY:
		return _fail("invalid_input", "`scenes` lists the open scenes to save")
	if handlers.restart_pending:
		return _fail("conflict", "a restart of this editor is already under way")
	var refusal := _not_restartable()
	if refusal != "":
		return _fail("unsupported", refusal)
	if EditorInterface.is_playing_scene():
		return _fail("conflict", "the editor is running the project, and a restart would first ask whether to stop it; stop it with editor.play.stop, then restart. Nothing was restarted.")
	var shown: Array = EditorDialogs.visible_dialogs()
	if not shown.is_empty():
		return _fail("conflict", "the editor shows a dialog (\"%s\"); answer it with editor.dialog.respond first. Nothing was restarted." % shown[0].title)
	var state: Dictionary = handlers.state()
	var edited: Node = EditorInterface.get_edited_scene_root()
	if edited != null and edited.scene_file_path == "":
		return _fail("conflict", "the scene being edited was never saved, and a restart would ask where to save it; save it in the editor first. Nothing was restarted.")
	var unsaved_scenes: Array = []
	var unknown_scenes: Array = []
	for entry in state["open_scenes"]:
		var path: String = str(entry.get("path", ""))
		if path == "":
			continue
		if entry.get("unsaved") == true:
			unsaved_scenes.append(path)
		elif entry.get("unsaved") == null:
			unknown_scenes.append(path)
	var unsaved_scripts: Array = []
	for entry in state["open_scripts"]:
		if entry.get("unsaved") == true:
			unsaved_scripts.append(str(entry.get("path", "")))
	if not save:
		var unsaved: Array = unsaved_scenes + unsaved_scripts
		if not unsaved.is_empty():
			return _fail("conflict", "%s hold changes their files do not, and a restart would ask what to do with them; save them first, or pass `save: true` to save them before the restart. Nothing was restarted." % _named(unsaved))
	var saved: Array = []
	var also_saved: Array = []
	var scripts_saved: Array = []
	if save:
		var script_editor = EditorInterface.get_script_editor()
		var saves_scripts: bool = script_editor != null and script_editor.has_method("save_all_scripts") and script_editor.has_method("get_unsaved_files")
		if not unsaved_scripts.is_empty() and not saves_scripts:
			return _fail("unsupported", "%s hold edits their files do not, and this editor (Godot %s) cannot save a script tab from a plugin: saving one needs ScriptEditor.save_all_scripts of Godot 4.7 or later. Save them in the editor, then restart. Nothing was restarted." % [_named(unsaved_scripts), _godot()])
		if not unsaved_scenes.is_empty() or not unknown_scenes.is_empty():
			var written: Dictionary = await mutations._save({"scenes": scenes, "all": true})
			if not written.get("ok", false):
				return written
			saved = written["payload"].get("saved", [])
			also_saved = written["payload"].get("also_saved", [])
		if saves_scripts and not Array(script_editor.call("get_unsaved_files")).is_empty():
			var unsaved_before: Array = Array(script_editor.call("get_unsaved_files"))
			script_editor.call("save_all_scripts")
			var still: Array = Array(script_editor.call("get_unsaved_files"))
			if not still.is_empty():
				return _fail("unavailable", "the editor was asked to save its script tabs, but %s still hold edits their files do not, so whether each was written is not known; nothing was restarted" % _named(still), "unresolved")
			scripts_saved = unsaved_before
	var client = handlers.plugin.get_node_or_null("GodotMcpBridge") if handlers.plugin != null else null
	var restarter := Node.new()
	restarter.name = "GodotMcpRestarter"
	restarter.set_script(Restarter)
	restarter.set("handlers_ref", weakref(handlers))
	restarter.set("client_ref", weakref(client) if client != null else null)
	EditorInterface.get_base_control().add_child(restarter)
	handlers.restart_pending = true
	return _ok({
		"requested": true,
		"pid": OS.get_process_id(),
		"saved": saved,
		"also_saved": also_saved,
		"scripts_saved": scripts_saved,
		"action": "godot-mcp: editor.restart",
	})
