@tool
extends RefCounted
## `editor.play.start` and `editor.play.stop`: run the project in the editor,
## as its Play buttons do, and stop it.
##
## Before it runs a scene the editor saves the open scenes, so a start names
## every open scene with the bytes the server saw, and answers which files the
## editor saved. The game is the editor's own child process. A start never
## waits on a dialog: it is refused when the editor would ask which scene to
## run, or to save a scene first.

## Frames a started game gets to come up before the start is answered; one
## that ended by then is reported.
const START_FRAMES := 3
## How long a stop waits for the game to end.
const STOP_MS := 5000


static func _ok(payload: Dictionary) -> Dictionary:
	return {"ok": true, "payload": payload, "mutation_status": "committed"}


static func _fail(kind: String, message: String, status: String = "not_started") -> Dictionary:
	return {"ok": false, "error_kind": kind, "message": message, "mutation_status": status}


## Why the open scenes are not the ones `payload` declares with the bytes the
## server saw, or "".
static func _declared_mismatch(payload: Dictionary) -> String:
	var scenes = payload.get("scenes")
	if typeof(scenes) != TYPE_ARRAY:
		return "`scenes` must list the open scenes"
	var declared: Array = []
	for entry in scenes:
		if typeof(entry) != TYPE_DICTIONARY:
			return "every open scene must be an object"
		var path: String = str(entry.get("path", ""))
		var sha = entry.get("sha256")
		if sha == null:
			if FileAccess.file_exists(path):
				return "%s was created on disk after the server checked it" % path
		elif not FileAccess.file_exists(path) or FileAccess.get_sha256(path) != str(sha):
			return "%s changed on disk after the server checked it" % path
		declared.append(path)
	var open: Array = []
	for path in EditorInterface.get_open_scenes():
		if path.begins_with("res://"):
			open.append(path)
	open.sort()
	declared.sort()
	if open != declared:
		return "the open scenes changed after the server listed them"
	return ""


## Why the editor cannot run `scene` without asking anyone, or "".
static func _unrunnable(scene: String) -> String:
	match scene:
		"main":
			var main := str(ProjectSettings.get_setting("application/run/main_scene", ""))
			if main == "":
				return "the project has no main scene (application/run/main_scene), and the editor would ask for one; name the `scene` to run"
			if not ResourceLoader.exists(main):
				return "the main scene %s does not exist" % main
		"current":
			var root := EditorInterface.get_edited_scene_root()
			if root == null:
				return "no scene is being edited"
			if root.scene_file_path == "":
				return "the scene being edited was never saved, and the editor would ask where to save it"
		_:
			if not scene.begins_with("res://") or not FileAccess.file_exists(scene):
				return "%s does not exist" % scene
	return ""


func start(handlers, payload: Dictionary) -> Dictionary:
	if DisplayServer.get_name() == "headless":
		return _fail("unavailable", "this editor runs headless, and a game it starts needs a window; run the editor with one, or run the game in a managed session with project.run.start")
	if EditorInterface.is_playing_scene():
		return _fail("conflict", "the editor already runs %s; stop it with editor.play.stop first" % EditorInterface.get_playing_scene())
	var mismatch := _declared_mismatch(payload)
	if mismatch != "":
		return _fail("conflict", mismatch)
	var scene := str(payload.get("scene", "main"))
	var unrunnable := _unrunnable(scene)
	if unrunnable != "":
		return _fail("invalid_input" if scene != "main" or unrunnable.contains("no main scene") else "not_found", unrunnable)

	# The editor announces every scene it tries to save, written or not, and
	# reports a resource saved only once it is written.
	var announced: Array = []
	var written: Array = []
	var plugin: EditorPlugin = handlers.plugin
	var on_scene := func(path: String) -> void:
		if not announced.has(path):
			announced.append(path)
	var on_resource := func(resource: Resource) -> void:
		if resource != null and resource.resource_path.begins_with("res://") and not written.has(resource.resource_path):
			written.append(resource.resource_path)
	plugin.scene_saved.connect(on_scene)
	plugin.resource_saved.connect(on_resource)
	match scene:
		"main":
			EditorInterface.play_main_scene()
		"current":
			EditorInterface.play_current_scene()
		_:
			EditorInterface.play_custom_scene(scene)
	plugin.scene_saved.disconnect(on_scene)
	plugin.resource_saved.disconnect(on_resource)

	var tree: SceneTree = EditorInterface.get_base_control().get_tree()
	for frame in START_FRAMES:
		await tree.process_frame
	var open: Array = Array(EditorInterface.get_open_scenes())
	var scenes: Array = announced.filter(func(path): return open.has(path))
	var unproven: Array = await handlers.not_saved(scenes)
	if not unproven.is_empty():
		if EditorInterface.is_playing_scene():
			EditorInterface.stop_playing_scene()
		return _fail("unavailable", "the editor ran the project without proving it saved %s, so the game was stopped; whether those files were written is not known" % ", ".join(unproven), "unresolved")
	var saved: Array = []
	for path in scenes:
		saved.append({"path": path, "sha256": FileAccess.get_sha256(path)})
	var also: Array = []
	for path in written:
		if not open.has(path):
			also.append({"path": path, "sha256": FileAccess.get_sha256(path) if FileAccess.file_exists(path) else null})
	if not EditorInterface.is_playing_scene():
		var files := ", ".join((scenes + also.map(func(entry): return entry["path"])).map(func(path): return str(path)))
		return _fail("unavailable", "the game ended as soon as it started; editor.log.get shows why. The editor saved: %s" % (files if files != "" else "nothing"), "committed")
	return _ok({
		"playing": true,
		"scene": EditorInterface.get_playing_scene(),
		"saved": saved,
		"also_saved": also,
		"action": "godot-mcp: editor.play.start",
		"transaction_id": str(payload.get("transaction_id", "")),
	})


func stop(payload: Dictionary) -> Dictionary:
	var was_playing := EditorInterface.is_playing_scene()
	if was_playing:
		EditorInterface.stop_playing_scene()
		var tree: SceneTree = EditorInterface.get_base_control().get_tree()
		var deadline := Time.get_ticks_msec() + STOP_MS
		while EditorInterface.is_playing_scene():
			if Time.get_ticks_msec() >= deadline:
				return _fail("unavailable", "the game did not stop within %d seconds" % (STOP_MS / 1000), "unresolved")
			await tree.process_frame
	return _ok({
		"was_playing": was_playing,
		"playing": false,
		"action": "godot-mcp: editor.play.stop",
		"transaction_id": str(payload.get("transaction_id", "")),
	})
