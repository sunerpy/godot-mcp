@tool
extends RefCounted
## `editor.filesystem.scan` and `editor.script.open`.
##
## A scan has the editor rescan the project's files, import what changed and
## register new script classes, then reloads each open script tab whose file
## changed underneath it — but only a tab this plugin can prove is that
## script's, that holds no unsaved edit and whose buffer still matches the
## script it was loaded from. Any other tab is left as it is and reported.
## ScriptEditor.reload_open_files() is never used: it reloads every open file
## and can open a dialog nobody answers.
##
## Neither ever starts a program: a scan that would import with Blender or
## FBX2glTF is refused, and so is opening a script while the editor sends
## scripts to an external editor.

const MAX_DIAGNOSTICS := 50
## Frames the file system must stay idle before a scan counts as done.
const IDLE_FRAMES := 2
const EXTERNAL_EDITOR := "text_editor/external/use_external_editor"


static func _ok(payload: Dictionary) -> Dictionary:
	return {"ok": true, "payload": payload, "mutation_status": "committed"}


static func _fail(kind: String, message: String, status: String = "not_started") -> Dictionary:
	return {"ok": false, "error_kind": kind, "message": message, "mutation_status": status}


static func _now_ms() -> int:
	return int(Time.get_unix_time_from_system() * 1000.0)


static func _external_editor() -> bool:
	var settings := EditorInterface.get_editor_settings()
	return settings.has_setting(EXTERNAL_EDITOR) and bool(settings.get_setting(EXTERNAL_EDITOR))


## Why importing this project could start an external program, or "".
static func _external_importer() -> String:
	var settings := EditorInterface.get_editor_settings()
	for importer in [
		["blend", "filesystem/import/blender/enabled", "filesystem/import/blender/blender_path", "Blender"],
		["fbx", "filesystem/import/fbx2gltf/enabled", "filesystem/import/fbx/fbx2gltf_path", "FBX2glTF"],
	]:
		var enabled: bool = ProjectSettings.has_setting(importer[1]) and bool(ProjectSettings.get_setting(importer[1]))
		var program: String = str(settings.get_setting(importer[2])) if settings.has_setting(importer[2]) else ""
		if enabled and program != "" and _has_extension("res://", importer[0]):
			return "the project has .%s files and the editor imports them with %s (%s); a scan could start that program, which this tool never does. Import them in the editor, or clear %s." % [importer[0], importer[3], program, importer[2]]
	return ""


static func _has_extension(directory: String, extension: String) -> bool:
	var access := DirAccess.open(directory)
	if access == null:
		return false
	access.include_hidden = false
	for file in access.get_files():
		if file.get_extension().to_lower() == extension:
			return true
	for child in access.get_directories():
		if child.begins_with("."):
			continue
		if _has_extension(directory.path_join(child), extension):
			return true
	return false


## Wait until the file system has been idle for IDLE_FRAMES frames: no scan
## and, on Godot 4.7, no import. False when `deadline` (unix ms) passed first.
static func _settled(deadline: int) -> bool:
	var filesystem := EditorInterface.get_resource_filesystem()
	var tree: SceneTree = EditorInterface.get_base_control().get_tree()
	var idle := 0
	while idle < IDLE_FRAMES:
		if deadline > 0 and _now_ms() >= deadline:
			return false
		var busy: bool = filesystem.is_scanning()
		if filesystem.has_method("is_importing"):
			busy = busy or bool(filesystem.call("is_importing"))
		idle = 0 if busy else idle + 1
		await tree.process_frame
	return true


func scan(handlers, payload: Dictionary, context: Dictionary) -> Dictionary:
	var refused := _external_importer()
	if refused != "":
		return _fail("unavailable", refused)
	var deadline := int(context.get("deadline_unix_ms", 0))
	var logged_before: int = handlers.editor_log.last() if handlers.log_capture() else -1
	var filesystem := EditorInterface.get_resource_filesystem()
	filesystem.scan()
	if not await _settled(deadline):
		return _fail("unavailable", "the scan did not finish before its deadline; the editor may still be importing", "unresolved")
	var reimport: PackedStringArray = PackedStringArray()
	for path in payload.get("reimport", []):
		reimport.append(str(path))
	if not reimport.is_empty():
		filesystem.reimport_files(reimport)
		if not await _settled(deadline):
			return _fail("unavailable", "the reimport did not finish before its deadline; the editor may still be importing", "unresolved")
	var reloaded := _reload_scripts(handlers)
	var diagnostics = null
	if logged_before >= 0:
		diagnostics = handlers.editor_log.read({
			"after": logged_before,
			"kinds": ["error", "warning", "script", "shader"],
			"limit": MAX_DIAGNOSTICS,
		})["entries"]
	return _ok({
		"reloaded_scripts": reloaded["reloaded"],
		"stale_scripts": reloaded["stale"],
		"script_reload": reloaded["script_reload"],
		"diagnostics": diagnostics,
		"action": "godot-mcp: editor.filesystem.scan",
	})


## Reload every open script tab whose file changed, when the tab is provably
## that script's, holds no unsaved edit and still shows the script's source.
func _reload_scripts(handlers) -> Dictionary:
	if _external_editor():
		# Switching tabs would open the external program.
		return {"reloaded": [], "stale": [], "script_reload": "skipped_external_editor"}
	var script_editor := EditorInterface.get_script_editor()
	var screen: String = handlers.main_screen()
	var previous = script_editor.get_current_script()
	var unsaved_files = null
	if script_editor.has_method("get_unsaved_files"):
		unsaved_files = Array(script_editor.call("get_unsaved_files"))
	var reloaded: Array = []
	var stale: Array = []
	var switched := false
	for script in script_editor.get_open_scripts():
		var path: String = script.resource_path
		if not path.begins_with("res://") or path.contains("::"):
			continue
		var on_disk := FileAccess.get_file_as_string(path)
		if script.source_code == on_disk:
			continue
		EditorInterface.edit_script(script, -1, 0, false)
		switched = true
		var current = script_editor.get_current_editor()
		var code = current.get_base_editor() if current != null else null
		var reason := ""
		if script_editor.get_current_script() != script or not (code is CodeEdit):
			reason = "unverified_tab"
		elif unsaved_files != null and unsaved_files.has(path):
			reason = "unsaved"
		elif unsaved_files == null and code.get_version() != code.get_saved_version():
			reason = "unsaved"
		elif code.text != script.source_code:
			reason = "unverified_tab"
		if reason != "":
			stale.append({"path": path, "reason": reason})
			continue
		script.source_code = on_disk
		script.reload(true)
		code.text = on_disk
		code.clear_undo_history()
		code.tag_saved_version()
		handlers.remember_tab(path, code)
		reloaded.append(path)
	if switched:
		if previous != null:
			EditorInterface.edit_script(previous, -1, 0, false)
		if handlers.main_screen() != screen and ["2D", "3D", "Script"].has(screen):
			EditorInterface.set_main_screen_editor(screen)
	return {"reloaded": reloaded, "stale": stale, "script_reload": "done"}


func open_script(handlers, payload: Dictionary) -> Dictionary:
	if _external_editor():
		return _fail("unavailable", "the editor opens scripts in an external program (%s); editor.script.open uses the built-in script editor only and starts no program" % EXTERNAL_EDITOR)
	var path := str(payload.get("path", ""))
	if not path.begins_with("res://") or path.get_extension() != "gd":
		return _fail("invalid_input", "`%s` is not a .gd script in the project" % path)
	if not FileAccess.file_exists(path):
		return _fail("not_found", "%s does not exist" % path)
	var script = ResourceLoader.load(path, "Script")
	if not (script is Script):
		return _fail("invalid_input", "%s does not load as a script" % path)
	var line := maxi(int(payload.get("line", 1)), 1)
	var column := maxi(int(payload.get("column", 1)), 1)
	# edit_script counts lines from 1 on every build; Godot 4.4 and 4.5 ignore
	# its column and 4.6 and later count it from 1. The caret is placed again
	# on the tab once it is provably this script's.
	EditorInterface.edit_script(script, line, column, true)
	var script_editor := EditorInterface.get_script_editor()
	var caret = null
	var current = script_editor.get_current_editor()
	if script_editor.get_current_script() == script and current != null and current.get_base_editor() is CodeEdit:
		var code: CodeEdit = current.get_base_editor()
		code.set_caret_line(line - 1)
		code.set_caret_column(column - 1)
		if code.text == script.source_code:
			handlers.remember_tab(path, code)
		caret = {"line": code.get_caret_line() + 1, "column": code.get_caret_column() + 1}
	return _ok({
		"path": path,
		"line": line,
		"column": column,
		"caret": caret,
		"action": "godot-mcp: editor.script.open",
	})
