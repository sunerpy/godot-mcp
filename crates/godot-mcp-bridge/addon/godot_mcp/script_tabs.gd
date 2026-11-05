@tool
extends RefCounted
## `editor.script.close`: close the script editor's tab of one script.
##
## ScriptEditor.close_file() exists from Godot 4.7 on; it reloads a tab that
## holds unsaved edits from its file before closing it, which discards them.
## So a tab with unsaved edits is refused, before anything changes, and only
## a build that also says which tabs are unsaved (get_unsaved_files(), from
## Godot 4.7 on too) closes tabs at all.


static func _ok(payload: Dictionary) -> Dictionary:
	return {"ok": true, "payload": payload, "mutation_status": "committed"}


static func _fail(kind: String, message: String, status: String = "not_started") -> Dictionary:
	return {"ok": false, "error_kind": kind, "message": message, "mutation_status": status}


## Whether this editor can close a script tab from a plugin.
static func can_close() -> bool:
	return ClassDB.class_has_method("ScriptEditor", "close_file") and ClassDB.class_has_method("ScriptEditor", "get_unsaved_files")


static func _open_scripts(script_editor) -> Array:
	var paths: Array = []
	for script in script_editor.get_open_scripts():
		var path: String = script.resource_path
		if path.begins_with("res://") and not path.contains("::"):
			paths.append(path)
	return paths


static func close(handlers, payload: Dictionary) -> Dictionary:
	if not can_close():
		var version: Dictionary = Engine.get_version_info()
		return _fail("unsupported", "this editor (Godot %d.%d.%d) cannot close a script tab from a plugin: closing needs ScriptEditor.close_file of Godot 4.7 or later. Nothing was changed." % [int(version.get("major", 0)), int(version.get("minor", 0)), int(version.get("patch", 0))])
	var path := str(payload.get("path", ""))
	if not path.begins_with("res://") or path.get_extension() != "gd":
		return _fail("invalid_input", "`%s` is not a .gd script in the project" % path)
	var script_editor = EditorInterface.get_script_editor()
	if script_editor == null or not _open_scripts(script_editor).has(path):
		return _fail("not_found", "%s is not open in the script editor" % path)
	if Array(script_editor.call("get_unsaved_files")).has(path):
		return _fail("conflict", "%s holds edits its file does not, which closing would discard; save them in the editor, or undo them, then close it. Nothing was closed" % path)
	var closed: int = script_editor.call("close_file", path)
	var still := _open_scripts(script_editor)
	if closed != OK or still.has(path):
		if still.has(path):
			return _fail("unavailable", "the editor did not close %s (%s); nothing was changed" % [path, error_string(closed)])
		return _fail("unavailable", "the editor answered %s while closing %s, which is no longer open" % [error_string(closed), path], "unresolved")
	if handlers != null:
		handlers.script_tabs.erase(path)
	return _ok({
		"closed": path,
		"open_scripts": still,
		"action": "godot-mcp: editor.script.close",
	})
