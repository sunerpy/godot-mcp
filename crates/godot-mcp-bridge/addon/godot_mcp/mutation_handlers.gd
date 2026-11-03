@tool
extends RefCounted
## Editor mutations: selection, opening and closing a scene, opening a
## script, the main screen shown, semantic scene batches and live edits as one
## undoable action, saving, undo/redo, project settings, the editor's own
## settings and shortcuts, file-system scans, running the project, and
## reloading this plugin.
##
## A scene batch is the same typed command model the headless helper applies
## (`add_node`, `set_property`, `connect_signal`, ...). It is applied to the
## scene the editor holds, never to its file:
##
## * the scene must be open, its file must still hash to what the server
##   checked, and the editor must not hold changes the file does not — a
##   divergent buffer is refused, never overwritten;
## * every operation is applied immediately while each primitive change records
##   how to redo and how to undo it; the first operation that cannot be applied
##   undoes everything recorded so far, in reverse, and nothing is committed;
## * on success one named EditorUndoRedoManager action carrying all recorded
##   steps is committed without executing them again, and the scene is saved;
##   if saving fails the action is undone again.
##
## Node references are paths relative to the scene root or node handles the
## server bound to paths of the file the server checked; each handle is bound to
## its node object before the first operation, so it follows that node through
## renames and moves and never names a node created later at the same path.

const VariantCodec = preload("variant_codec.gd")
const ProjectSettingsApply = preload("project_settings_apply.gd")
const Filesystem = preload("filesystem.gd")
const EditingMethods = preload("editing_methods.gd")
const Play = preload("play.gd")
const Preferences = preload("preferences.gd")
const Shortcuts = preload("shortcuts.gd")

const HANDLE_PREFIX := "nh1:"
const MAX_OPERATIONS := 128
const MAX_SELECTION := 256
## Frames the editor gets to settle after the selection is released and again
## after a reload, before the next step.
const SETTLE_FRAMES := 3
const METHODS := [
	"editor.addon.reload",
	"editor.filesystem.scan",
	"editor.history.redo",
	"editor.history.undo",
	"editor.main_screen.set",
	"editor.play.start",
	"editor.play.stop",
	"editor.preferences.set",
	"editor.scene.batch",
	"editor.scene.close",
	"editor.scene.edit",
	"editor.scene.open",
	"editor.scene.save",
	"editor.script.open",
	"editor.selection.set",
	"editor.settings.apply",
	"editor.shortcuts.set",
]

## The owning handler set, held weakly: it owns this object, and a strong
## reference back would keep both alive after the plugin is disabled.
var _owner: WeakRef = null
var _bound: Dictionary = {}
var _steps: Array = []
## What each `call_method` of the batch being applied returned.
var _results: Array = []
var _files = Filesystem.new()
var _play = Play.new()


func _init(owner = null) -> void:
	_owner = weakref(owner) if owner != null else null


func _handlers():
	return _owner.get_ref() if _owner != null else null


static func ok(payload: Dictionary) -> Dictionary:
	return {"ok": true, "payload": payload, "mutation_status": "committed"}


static func fail(kind: String, message: String, status: String = "not_started") -> Dictionary:
	return {"ok": false, "error_kind": kind, "message": message, "mutation_status": status}


static func refused(reason: String, index: int, detail: String) -> Dictionary:
	return fail("invalid_input", "%s at operation %d (%s)" % [reason, index, detail.replace("\n", " ")])


func dispatch(method: String, payload: Dictionary, context: Dictionary) -> Dictionary:
	match method:
		"editor.filesystem.scan":
			return await _files.scan(_handlers(), payload, context)
		"editor.script.open":
			return _files.open_script(_handlers(), payload)
		"editor.play.start":
			return await _play.start(_handlers(), payload)
		"editor.play.stop":
			return await _play.stop(payload)
		"editor.selection.set":
			return _select(payload)
		"editor.scene.open":
			return await _open(payload)
		"editor.main_screen.set":
			return await _main_screen(payload)
		"editor.scene.batch":
			return await _batch(payload)
		"editor.scene.edit":
			return _edit(payload)
		"editor.scene.save":
			return await _save(payload)
		"editor.scene.close":
			return await _close(payload)
		"editor.preferences.set":
			return await Preferences.change(payload)
		"editor.shortcuts.set":
			return await Shortcuts.change(payload)
		"editor.history.undo":
			return _history(payload, true)
		"editor.history.redo":
			return _history(payload, false)
		"editor.addon.reload":
			return _reload()
		"editor.settings.apply":
			return ProjectSettingsApply.apply(payload)
	return fail("unsupported", "the editor bridge does not implement %s" % method)


## `editor.main_screen.set`: show the 2D, 3D or Script main screen, and
## answer once it is shown.
func _main_screen(payload: Dictionary) -> Dictionary:
	var screen := str(payload.get("screen", ""))
	if not ["2D", "3D", "Script"].has(screen):
		return fail("invalid_input", "`screen` is 2D, 3D or Script")
	var handlers = _handlers()
	EditorInterface.set_main_screen_editor(screen)
	var tree: SceneTree = EditorInterface.get_base_control().get_tree()
	for frame in SETTLE_FRAMES:
		if handlers.main_screen() == screen:
			break
		await tree.process_frame
	var shown: String = handlers.main_screen()
	if shown != screen:
		return fail("unavailable", "the editor was asked for the %s main screen but shows %s" % [screen, shown], "committed")
	return ok({"main_screen": shown, "action": "godot-mcp: editor.main_screen.set"})


# --- node references ---------------------------------------------------------


func _node(root: Node, reference) -> Node:
	var spelled: String = str(reference)
	if spelled.begins_with(HANDLE_PREFIX):
		var bound = _bound.get(spelled)
		if not is_instance_valid(bound):
			return null
		if bound != root and not root.is_ancestor_of(bound):
			return null
		return bound
	if spelled == "" or spelled == ".":
		return root
	if spelled.begins_with("/") or spelled.contains(".."):
		return null
	return root.get_node_or_null(NodePath(spelled))


static func _owned(root: Node, node: Node) -> bool:
	return node == root or node.owner == root


static func _collect_owned(node: Node, root: Node, into: Array) -> void:
	if node.owner == root:
		into.append(node)
	for child in node.get_children():
		_collect_owned(child, root, into)


static func _map_owned(original: Node, copy: Node, owned: Array, into: Array) -> void:
	if owned.has(original):
		into.append(copy)
	for child in mini(original.get_child_count(), copy.get_child_count()):
		_map_owned(original.get_child(child), copy.get_child(child), owned, into)


static func _is_node_class(name: String) -> bool:
	return ClassDB.class_exists(name) and ClassDB.can_instantiate(name) \
		and (name == "Node" or ClassDB.is_parent_class(name, "Node"))


static func _is_resource_class(name: String) -> bool:
	return ClassDB.class_exists(name) and ClassDB.can_instantiate(name) \
		and (name == "Resource" or ClassDB.is_parent_class(name, "Resource"))


static func _has_property(object: Object, property: String) -> bool:
	for entry in object.get_property_list():
		if entry["name"] == property:
			return true
	return false


# --- recorded primitive steps ------------------------------------------------


## Run one primitive change now and record how to redo and undo it. Each call
## is [object, method, arguments].
func _record(do_calls: Array, undo_calls: Array, do_refs: Array = [], undo_refs: Array = []) -> void:
	for call in do_calls:
		call[0].callv(call[1], call[2])
	_steps.append({"do": do_calls, "undo": undo_calls, "do_refs": do_refs, "undo_refs": undo_refs})


func _rollback() -> void:
	for index in range(_steps.size() - 1, -1, -1):
		for call in _steps[index]["undo"]:
			call[0].callv(call[1], call[2])
	for step in _steps:
		for created in step["do_refs"]:
			if is_instance_valid(created) and created.get_parent() == null:
				created.free()
	_steps = []


func _commit(root: Node, label: String) -> void:
	var manager: EditorUndoRedoManager = EditorInterface.get_editor_undo_redo()
	manager.create_action(label, UndoRedo.MERGE_DISABLE, root)
	for step in _steps:
		for call in step["do"]:
			manager.callv("add_do_method", [call[0], call[1]] + call[2])
	for index in range(_steps.size() - 1, -1, -1):
		for call in _steps[index]["undo"]:
			manager.callv("add_undo_method", [call[0], call[1]] + call[2])
	for step in _steps:
		for created in step["do_refs"]:
			manager.add_do_reference(created)
		for removed in step["undo_refs"]:
			manager.add_undo_reference(removed)
	manager.commit_action(false)
	_steps = []


# --- selection and scenes ------------------------------------------------------


func _select(payload: Dictionary) -> Dictionary:
	var found: Dictionary = _handlers().open_scene_root(str(payload.get("scene", "")))
	if not found.get("ok", false):
		return found
	var root: Node = found["root"]
	var references = payload.get("nodes", [])
	if typeof(references) != TYPE_ARRAY or references.size() > MAX_SELECTION:
		return fail("invalid_input", "`nodes` must be an array of at most %d node paths" % MAX_SELECTION)
	var nodes: Array = []
	for reference in references:
		var node: Node = _node(root, reference)
		if node == null or not _owned(root, node):
			return fail("not_found", "%s names no node of %s" % [str(reference), root.scene_file_path])
		nodes.append(node)
	var selection: EditorSelection = EditorInterface.get_selection()
	selection.clear()
	for node in nodes:
		selection.add_node(node)
	var selected: Array = []
	for node in selection.get_selected_nodes():
		selected.append(_handlers().node_path(root, node))
	return ok({"scene": root.scene_file_path, "selection": selected})


func _open(payload: Dictionary) -> Dictionary:
	var path: String = str(payload.get("path", ""))
	if not path.begins_with("res://") or not (path.ends_with(".tscn") or path.ends_with(".scn")):
		return fail("invalid_input", "`path` must name a res:// scene")
	if not ResourceLoader.exists(path):
		return fail("not_found", "%s does not exist" % path)
	var was_open: bool = EditorInterface.get_open_scenes().has(path)
	EditorInterface.open_scene_from_path(path)
	var root: Node = EditorInterface.get_edited_scene_root()
	if root == null or root.scene_file_path != path:
		return fail("unavailable", "the editor did not make %s the current scene" % path, "unresolved")
	var reloaded := false
	if was_open and bool(payload.get("reload", false)):
		# Reloading discards what the editor holds, so it is refused unless the
		# editor holds exactly what the file does.
		var disk: Dictionary = _handlers().file_snapshot(path)
		if disk.is_empty() or _handlers().edit_digest(root) != disk["digest"]:
			return fail("conflict", "the editor holds changes to %s that its file does not; save or revert them first" % path)
		# Godot 4.6.1 crashes when a scene is reloaded while the editor
		# selection holds its nodes, so the selection is released first and
		# the editor gets a few frames before and after the reload; then the
		# same node paths are selected again in the reloaded scene.
		var selected: Array = _selected_paths(root)
		if not selected.is_empty():
			EditorInterface.get_selection().clear()
			await _settle()
		EditorInterface.reload_scene_from_path(path)
		root = EditorInterface.get_edited_scene_root()
		if root == null or root.scene_file_path != path:
			return fail("unavailable", "the editor did not reload %s" % path, "unresolved")
		reloaded = true
		if not selected.is_empty():
			await _settle()
			root = EditorInterface.get_edited_scene_root()
			if root == null or root.scene_file_path != path:
				return fail("unavailable", "the editor did not keep %s open after reloading it" % path, "unresolved")
			_select_paths(root, selected)
	if not was_open or reloaded:
		# Freshly loaded from its file: the scene and the file agree.
		_handlers().mark_saved(root)
	return ok({
		"edited_scene": path,
		"reloaded": reloaded,
		"open_scenes": Array(EditorInterface.get_open_scenes()),
	})


## The open scenes, without the empty entry Godot 4.7 leaves once the last
## scene is closed.
static func _open_scenes() -> Array:
	return Array(EditorInterface.get_open_scenes()).filter(func(path): return path != "")


## `editor.scene.close`: close an open scene, as the editor's Close Scene does,
## unless it may hold a change its file does not: Godot discards such a change
## without asking, so the close is refused instead, before anything changes. A
## scene that is open but not the edited one is shown, closed, and the scene
## that was edited shown again.
func _close(payload: Dictionary) -> Dictionary:
	if not EditorInterface.has_method("close_scene"):
		return fail("unsupported", "this editor cannot close a scene from a plugin; closing needs Godot 4.5 or later")
	var handlers = _handlers()
	var edited: Node = EditorInterface.get_edited_scene_root()
	var edited_path: String = edited.scene_file_path if edited != null else ""
	var path := str(payload.get("scene", ""))
	if path == "":
		if edited == null:
			return fail("not_found", "no scene is being edited")
		if edited_path == "":
			return fail("conflict", "the scene being edited was never saved, and closing it would discard it; save it in the editor first")
		path = edited_path
	if not _open_scenes().has(path):
		return fail("not_found", "%s is not open in the editor" % path)
	var saved = _saved(handlers, path, handlers.held_root(path))
	if saved == false:
		return fail("conflict", "%s holds changes its file does not, which closing would discard; save them with editor.scene.save, or undo them, then close it. Nothing was closed" % path)
	if saved == null:
		return fail("conflict", "this editor cannot tell that %s holds no change its file does not; save it with editor.scene.save, then close it. Nothing was closed" % path)
	if path != edited_path:
		if edited != null and edited_path == "":
			return fail("conflict", "the scene being edited was never saved, so the editor could not show it again after closing %s; save it, or make %s the edited scene, then close it. Nothing was closed" % [path, path])
		if await handlers.show_scene(path) == null:
			return await _shown_again(handlers, edited_path, "the editor did not show %s, so it was not closed" % path)
	var closed = EditorInterface.call("close_scene")
	if _open_scenes().has(path):
		return await _shown_again(handlers, edited_path, "the editor did not close %s (%s)" % [path, error_string(closed)])
	if path != edited_path and _open_scenes().has(edited_path):
		await handlers.show_scene(edited_path)
	var now: Node = EditorInterface.get_edited_scene_root()
	return ok({
		"closed": path,
		"open_scenes": _open_scenes(),
		"edited_scene": now.scene_file_path if now != null else null,
		"action": "godot-mcp: editor.scene.close",
		"transaction_id": str(payload.get("transaction_id", "")),
	})


## Whether the open scene at `path`, whose root is `root`, holds no change its
## file does not: what Godot 4.7 reports for every open scene, or the edited
## scene's undo history; otherwise true when its file holds the scene as the
## editor does, and null when nothing tells. Before Godot 4.7 the editor names
## a scene's undo history only while that scene is the edited one.
func _saved(handlers, path: String, root: Node):
	if EditorInterface.has_method("get_unsaved_scenes"):
		return not Array(EditorInterface.call("get_unsaved_scenes")).has(path)
	if root != null and root == EditorInterface.get_edited_scene_root():
		var unsaved = handlers.unsaved(root)
		if unsaved != null:
			return not unsaved
	return true if handlers.holds(path, root) else null


## The answer to a close that did not happen: `why`, with nothing changed once
## the editor shows the scene it showed before, and unresolved when it cannot
## be brought back to it.
func _shown_again(handlers, edited_path: String, why: String) -> Dictionary:
	var now: Node = EditorInterface.get_edited_scene_root()
	var current: String = now.scene_file_path if now != null else ""
	if current != edited_path and edited_path != "":
		var shown: Node = await handlers.show_scene(edited_path)
		if shown != null:
			current = shown.scene_file_path
	if current == edited_path:
		return fail("unavailable", "%s; nothing was changed" % why)
	return fail("unavailable", "%s, and the editor now shows %s instead of %s" % [why, current if current != "" else "no scene", edited_path if edited_path != "" else "no scene"], "unresolved")


## The selected nodes of the scene `root`, as paths relative to it.
func _selected_paths(root: Node) -> Array:
	var paths: Array = []
	for node in EditorInterface.get_selection().get_selected_nodes():
		if node == root or root.is_ancestor_of(node):
			paths.append(root.get_path_to(node))
	return paths


func _select_paths(root: Node, paths: Array) -> void:
	var selection: EditorSelection = EditorInterface.get_selection()
	for path in paths:
		var node: Node = root.get_node_or_null(path)
		if node != null:
			selection.add_node(node)


func _settle() -> void:
	var tree: SceneTree = EditorInterface.get_base_control().get_tree()
	for frame in SETTLE_FRAMES:
		await tree.process_frame


func _history(payload: Dictionary, undo: bool) -> Dictionary:
	var found: Dictionary = _handlers().open_scene_root(str(payload.get("scene", "")))
	if not found.get("ok", false):
		return found
	var root: Node = found["root"]
	var history: UndoRedo = _handlers().history_of(root)
	if undo and not history.has_undo():
		return fail("conflict", "%s has nothing to undo" % root.scene_file_path)
	if not undo and not history.has_redo():
		return fail("conflict", "%s has nothing to redo" % root.scene_file_path)
	var action: String = history.get_current_action_name()
	var stepped: Dictionary = _step_history(history, undo, true)
	if not stepped.get("ok", false):
		return stepped
	if not undo:
		action = history.get_current_action_name()
	var answer := {
		"scene": root.scene_file_path,
		"action": action,
		"version": history.get_version(),
		"has_undo": history.has_undo(),
		"has_redo": history.has_redo(),
		"digest": _handlers().edit_digest(root),
		"saved": false,
		"sha256": null,
	}
	if bool(payload.get("save", false)):
		if EditorInterface.save_scene() != OK:
			return fail("unavailable", "the %s was applied but %s could not be saved" % ["undo" if undo else "redo", root.scene_file_path], "unresolved")
		_handlers().mark_saved(root)
		answer["saved"] = true
		answer["sha256"] = FileAccess.get_sha256(root.scene_file_path)
	return ok(answer)


## Take one step back (`undo`) or forward through the current scene's history
## exactly the way the editor's own Undo and Redo do.
##
## EditorUndoRedoManager keeps undo and redo stacks of its own beside each
## history's UndoRedo, and scripts cannot call its undo()/redo(). Stepping a
## scene's UndoRedo directly leaves the two out of step: the editor then logs
## "Inconsistent redo history", misjudges whether the scene is saved, and can
## crash. The Scene menu's Undo and Redo entries run the manager's own step, as
## Ctrl+Z does, so that is the only path taken.
##
## The editor's step goes to the newest history with a step to take, which may
## be the global or the remote history rather than this scene's; unless the
## caller knows this scene's step is the newest (`confined` false), the step is
## refused, with nothing changed, while either of them has one.
func _step_history(history: UndoRedo, undo: bool, confined: bool) -> Dictionary:
	var verb: String = "undo" if undo else "redo"
	if confined:
		var other: String = _other_history_with_step(undo)
		if other != "":
			return fail("conflict", "the editor's %s history also has a step to %s, so the editor's own %s would not be confined to this scene; take that step in the editor first" % [other, verb, verb])
	var entry: Dictionary = _scene_menu_entry(undo)
	if entry.is_empty():
		return fail("unavailable", "this editor has no Scene menu entry that runs its own %s" % verb)
	var version: int = history.get_version()
	entry["menu"].id_pressed.emit(entry["id"])
	var stepped: int = history.get_version()
	if stepped == version:
		return fail("unavailable", "the editor did not %s" % verb)
	if stepped != (version - 1 if undo else version + 1):
		return fail("unavailable", "the editor's %s moved this scene's history from version %d to %d" % [verb, version, stepped], "unresolved")
	return {"ok": true}


## Which of the editor's other histories, global or remote, also has a step to
## take, or "" when neither does.
func _other_history_with_step(undo: bool) -> String:
	var manager: EditorUndoRedoManager = EditorInterface.get_editor_undo_redo()
	var printing: bool = Engine.print_error_messages
	# A history the editor never created answers null, after logging an error.
	Engine.print_error_messages = false
	var found: String = ""
	for other in [[EditorUndoRedoManager.GLOBAL_HISTORY, "global"], [EditorUndoRedoManager.REMOTE_HISTORY, "remote"]]:
		var history: UndoRedo = manager.get_history_undo_redo(other[0])
		if history != null and (history.has_undo() if undo else history.has_redo()):
			found = other[1]
			break
	Engine.print_error_messages = printing
	return found


## The editor's Scene menu entry that runs its own undo (`undo`) or redo, as
## {"menu": PopupMenu, "id": int}, or {} when this editor has none. The entry is
## the one carrying the built-in `ui_undo`/`ui_redo` shortcut in a menu the
## editor itself answers: that shortcut object, where Godot 4.6 and later can
## name it, or the shortcut named by the action's untranslated built-in display
## name, which every supported build gives it.
func _scene_menu_entry(undo: bool) -> Dictionary:
	var settings: EditorSettings = EditorInterface.get_editor_settings()
	var action: String = "ui_undo" if undo else "ui_redo"
	var wanted = settings.call("get_shortcut", action) if settings.has_method("get_shortcut") else null
	var display_name: String = "Undo" if undo else "Redo"
	var window: Window = EditorInterface.get_base_control().get_tree().root
	for menu in window.find_children("*", "PopupMenu", true, false):
		if not _answered_by_editor(menu):
			continue
		for index in menu.item_count:
			var shortcut: Shortcut = menu.get_item_shortcut(index)
			if shortcut != null and (shortcut == wanted or shortcut.resource_name == display_name):
				return {"menu": menu, "id": menu.get_item_id(index)}
	return {}


static func _answered_by_editor(menu: PopupMenu) -> bool:
	for connection in menu.get_signal_connection_list("id_pressed"):
		var target: Object = connection["callable"].get_object()
		if target != null and target.get_class() == "EditorNode":
			return true
	return false


func _reload() -> Dictionary:
	var plugin_directory: String = "godot_mcp"
	var reloader := Node.new()
	reloader.name = "GodotMcpReloader"
	reloader.set_script(preload("reloader.gd"))
	reloader.set("plugin_directory", plugin_directory)
	EditorInterface.get_base_control().add_child(reloader)
	return ok({"scheduled": true})


# --- scene batches ---------------------------------------------------------------


func _batch(payload: Dictionary) -> Dictionary:
	var path: String = str(payload.get("path", ""))
	var operations = payload.get("operations")
	var handles = payload.get("handles", {})
	var expected: String = str(payload.get("expected_sha256", ""))
	var label: String = str(payload.get("label", "godot-mcp: scene batch"))
	if not path.begins_with("res://") or not path.ends_with(".tscn"):
		return fail("invalid_input", "`path` must name a res:// text scene")
	if typeof(operations) != TYPE_ARRAY or operations.size() > MAX_OPERATIONS:
		return fail("invalid_input", "`operations` must be an array of at most %d operations" % MAX_OPERATIONS)
	if typeof(handles) != TYPE_DICTIONARY:
		return fail("invalid_input", "`handles` must be an object")
	if not EditorInterface.get_open_scenes().has(path):
		return fail("not_found", "%s is not open in the editor" % path)

	var root: Node = EditorInterface.get_edited_scene_root()
	if root == null or root.scene_file_path != path:
		EditorInterface.open_scene_from_path(path)
		root = EditorInterface.get_edited_scene_root()
		if root == null or root.scene_file_path != path:
			return fail("unavailable", "the editor did not make %s the current scene" % path)
	if FileAccess.get_sha256(path) != expected:
		return fail("conflict", "%s changed on disk after the server checked it" % path)
	var disk: Dictionary = _handlers().file_snapshot(path)
	if disk.is_empty():
		return fail("unavailable", "%s cannot be loaded from its file" % path)
	if _handlers().edit_digest(root) != disk["digest"]:
		return fail("conflict", "the editor holds changes to %s that its file does not; change the scene the editor holds with editor.scene.edit, or save it with editor.scene.save first" % path)

	_bound = {}
	for handle in handles.keys():
		var node: Node = _node(root, handles[handle])
		if node == null or not _owned(root, node):
			return fail("conflict", "the node handle %s names no node of %s as the editor holds it" % [handle, path])
		_bound[str(handle)] = node

	var applied: Dictionary = _apply_all(root, operations)
	if not applied.get("ok", false):
		return applied
	_commit(root, label)
	var history: UndoRedo = _handlers().history_of(root)
	var version: int = history.get_version()
	# EditorInterface.save_scene() answers OK even when the file could not be
	# written, so the save is proven by what the editor reports and the file.
	if EditorInterface.save_scene() != OK or not (await _handlers().not_saved([path])).is_empty():
		# The action just committed is the newest step of every history, so
		# the editor's own undo takes exactly it; the file must then still
		# hold the scene as it was.
		if _step_history(history, true, false).get("ok", false) and _handlers().holds(path, root):
			return fail("unavailable", "%s could not be saved; the change was undone, and the file holds the scene as before" % path)
		return fail("unavailable", "%s could not be saved, and whether its file still holds the scene as before is not known" % path, "unresolved")
	return ok({
		"path": path,
		"saved": true,
		"sha256": FileAccess.get_sha256(path),
		"action": label,
		"version": version,
		"digest": _handlers().edit_digest(root),
		"results": _results,
		"transaction_id": str(payload.get("transaction_id", "")),
	})


## Apply every operation to `root`, recording each step; the first that cannot
## be applied undoes everything recorded so far and is returned.
func _apply_all(root: Node, operations: Array) -> Dictionary:
	_steps = []
	_results = []
	for index in operations.size():
		var operation = operations[index]
		if typeof(operation) != TYPE_DICTIONARY or not operation.has("op"):
			_rollback()
			return refused("operation_not_an_object", index, "every operation needs an op")
		var applied: Dictionary = _apply(root, operation, index)
		if not applied.get("ok", false):
			_rollback()
			return applied
	if _steps.is_empty():
		return fail("invalid_input", "the batch changes nothing")
	return {"ok": true}


# --- live edits and saves ------------------------------------------------------


## `editor.scene.edit`: apply operations to the scene the editor holds, unsaved
## changes included, when it is still the scene the caller read. The whole edit
## is one undoable action and nothing is saved.
func _edit(payload: Dictionary) -> Dictionary:
	var scene: String = str(payload.get("scene", ""))
	var operations = payload.get("operations")
	var expected: String = str(payload.get("expected_digest", ""))
	var label: String = str(payload.get("label", "godot-mcp: editor.scene.edit"))
	if typeof(operations) != TYPE_ARRAY or operations.is_empty() or operations.size() > MAX_OPERATIONS:
		return fail("invalid_input", "`operations` must hold between 1 and %d operations" % MAX_OPERATIONS)
	for index in operations.size():
		var operation = operations[index]
		if typeof(operation) == TYPE_DICTIONARY:
			for field in ["parent", "node", "from", "to"]:
				if str(operation.get(field, "")).begins_with(HANDLE_PREFIX):
					return refused("node_handle", index, "a live edit names nodes by path")
	var root: Node = EditorInterface.get_edited_scene_root()
	if scene != "":
		if not scene.begins_with("res://") or not scene.ends_with(".tscn"):
			return fail("invalid_input", "`scene` must name a res:// text scene")
		if not EditorInterface.get_open_scenes().has(scene):
			return fail("not_found", "%s is not open in the editor" % scene)
		if root == null or root.scene_file_path != scene:
			EditorInterface.open_scene_from_path(scene)
			root = EditorInterface.get_edited_scene_root()
			if root == null or root.scene_file_path != scene:
				return fail("unavailable", "the editor did not make %s the current scene" % scene)
	elif root == null:
		return fail("not_found", "no scene is being edited")
	if _handlers().edit_digest(root) != expected:
		return fail("conflict", "%s as the editor holds it is no longer the scene whose digest was read; read it again" % root.scene_file_path)
	_bound = {}
	var applied: Dictionary = _apply_all(root, operations)
	if not applied.get("ok", false):
		return applied
	_commit(root, label)
	var history: UndoRedo = _handlers().history_of(root)
	return ok({
		"scene": root.scene_file_path,
		"action": label,
		"history_version": history.get_version(),
		"digest": _handlers().edit_digest(root),
		"unsaved": _handlers().unsaved(root),
		"results": _results,
		"transaction_id": str(payload.get("transaction_id", "")),
	})


## `editor.scene.save`: save the declared open scenes when each file still
## holds the bytes the server checked, and report every file the editor wrote,
## including resources it saved with them.
func _save(payload: Dictionary) -> Dictionary:
	var scenes = payload.get("scenes")
	var all: bool = bool(payload.get("all", false))
	if typeof(scenes) != TYPE_ARRAY or scenes.is_empty():
		return fail("invalid_input", "`scenes` must name the scenes to save")
	var open: Array = Array(EditorInterface.get_open_scenes())
	var declared: Array = []
	for entry in scenes:
		if typeof(entry) != TYPE_DICTIONARY:
			return fail("invalid_input", "every scene to save must be an object")
		var path: String = str(entry.get("path", ""))
		if not path.begins_with("res://"):
			return fail("invalid_input", "a scene that was never saved has no file; save it in the editor first")
		if not open.has(path):
			return fail("not_found", "%s is not open in the editor" % path)
		var sha = entry.get("sha256")
		if sha == null:
			if FileAccess.file_exists(path):
				return fail("conflict", "%s was created on disk after the server checked it" % path)
		elif not FileAccess.file_exists(path) or FileAccess.get_sha256(path) != str(sha):
			return fail("conflict", "%s changed on disk after the server checked it" % path)
		declared.append(path)
	if all:
		var listed: Array = open.duplicate()
		listed.sort()
		var named: Array = declared.duplicate()
		named.sort()
		if listed != named:
			return fail("conflict", "the open scenes changed after the server listed them")
	elif declared.size() != 1:
		return fail("invalid_input", "one scene is saved unless `all` is set")

	var root: Node = EditorInterface.get_edited_scene_root()
	if not all and (root == null or root.scene_file_path != declared[0]):
		EditorInterface.open_scene_from_path(declared[0])
		root = EditorInterface.get_edited_scene_root()
		if root == null or root.scene_file_path != declared[0]:
			return fail("unavailable", "the editor did not make %s the current scene" % declared[0])
	# The editor announces every scene it tried to save, written or not; only
	# a resource it reports saved was written.
	var written: Array = []
	var plugin: EditorPlugin = _handlers().plugin
	var on_resource := func(resource: Resource) -> void:
		if resource != null and resource.resource_path.begins_with("res://"):
			written.append(resource.resource_path)
	plugin.resource_saved.connect(on_resource)
	var error: int = OK
	if all:
		EditorInterface.save_all_scenes()
	else:
		error = EditorInterface.save_scene()
	plugin.resource_saved.disconnect(on_resource)
	if error != OK:
		return fail("unavailable", "%s could not be saved (%s); whether it was written is not known" % [declared[0], error_string(error)], "unresolved")
	var unproven: Array = await _handlers().not_saved(declared)
	if not unproven.is_empty():
		return fail("unavailable", "%s may not have been saved: the editor does not report it saved and its file does not hold it as the editor does, so whether it was written is not known" % ", ".join(unproven), "unresolved")
	root = EditorInterface.get_edited_scene_root()

	var saved: Array = []
	for path in declared:
		saved.append({"path": path, "sha256": FileAccess.get_sha256(path)})
	var also: Array = []
	for path in written:
		if not declared.has(path) and not also.any(func(entry): return entry["path"] == path):
			also.append({"path": path})
	return ok({
		"saved": saved,
		"also_saved": also,
		"action": "godot-mcp: editor.scene.save",
		"digest": _handlers().edit_digest(root) if root != null else "saved",
		"transaction_id": str(payload.get("transaction_id", "")),
	})


func _apply(root: Node, op: Dictionary, index: int) -> Dictionary:
	var kind: String = str(op.get("op", ""))
	match kind:
		"add_node", "instance_scene":
			var parent: Node = _node(root, op.get("parent", "."))
			if parent == null or not _owned(root, parent):
				return refused("parent_not_found", index, str(op.get("parent", ".")))
			var name: String = str(op.get("name", ""))
			if name == "" or name.validate_node_name() != name:
				return refused("invalid_node_name", index, name)
			if parent.has_node(NodePath(name)):
				return refused("node_exists", index, name)
			var child: Node = null
			if op.has("scene"):
				var scene: PackedScene = ResourceLoader.load(str(op["scene"]), "PackedScene")
				if scene == null:
					return refused("scene_not_loadable", index, str(op["scene"]))
				child = scene.instantiate(PackedScene.GEN_EDIT_STATE_INSTANCE)
				if child == null:
					return refused("scene_not_instantiable", index, str(op["scene"]))
			else:
				var node_type: String = str(op.get("type", "Node"))
				if not _is_node_class(node_type):
					return refused("unknown_node_type", index, node_type)
				child = ClassDB.instantiate(node_type)
			child.name = name
			_record([[parent, "add_child", [child]], [child, "set_owner", [root]]],
				[[parent, "remove_child", [child]]], [child], [])
		"remove_node":
			var node: Node = _node(root, op.get("node", ""))
			if node == null or node == root or not _owned(root, node):
				return refused("node_not_found", index, str(op.get("node", "")))
			var parent: Node = node.get_parent()
			var owned: Array = []
			_collect_owned(node, root, owned)
			var undo_calls: Array = [[parent, "add_child", [node]], [parent, "move_child", [node, node.get_index()]]]
			for descendant in owned:
				undo_calls.append([descendant, "set_owner", [root]])
			_record([[parent, "remove_child", [node]]], undo_calls, [], [node])
		"rename_node":
			var node: Node = _node(root, op.get("node", ""))
			var name: String = str(op.get("name", ""))
			if node == null or not _owned(root, node):
				return refused("node_not_found", index, str(op.get("node", "")))
			if name == "" or name.validate_node_name() != name:
				return refused("invalid_node_name", index, name)
			if node != root and node.get_parent().has_node(NodePath(name)):
				return refused("node_exists", index, name)
			_record([[node, "set_name", [name]]], [[node, "set_name", [String(node.name)]]])
		"move_node":
			var node: Node = _node(root, op.get("node", ""))
			var parent: Node = _node(root, op.get("parent", "."))
			if node == null or node == root or not _owned(root, node):
				return refused("node_not_found", index, str(op.get("node", "")))
			if parent == null or not _owned(root, parent) or parent == node or node.is_ancestor_of(parent):
				return refused("invalid_parent", index, str(op.get("parent", ".")))
			if parent != node.get_parent() and parent.has_node(NodePath(String(node.name))):
				return refused("node_exists", index, String(node.name))
			var old_parent: Node = node.get_parent()
			var owned: Array = []
			_collect_owned(node, root, owned)
			var do_calls: Array = [[old_parent, "remove_child", [node]], [parent, "add_child", [node]]]
			var undo_calls: Array = [[parent, "remove_child", [node]], [old_parent, "add_child", [node]],
				[old_parent, "move_child", [node, node.get_index()]]]
			for descendant in owned:
				do_calls.append([descendant, "set_owner", [root]])
				undo_calls.append([descendant, "set_owner", [root]])
			if op.has("index"):
				var position: int = clampi(int(op["index"]), 0, parent.get_child_count() - (1 if parent == old_parent else 0))
				do_calls.append([parent, "move_child", [node, position]])
			_record(do_calls, undo_calls)
		"duplicate_node":
			var node: Node = _node(root, op.get("node", ""))
			var name: String = str(op.get("name", ""))
			if node == null or node == root or not _owned(root, node):
				return refused("node_not_found", index, str(op.get("node", "")))
			if name == "" or name.validate_node_name() != name:
				return refused("invalid_node_name", index, name)
			if node.get_parent().has_node(NodePath(name)):
				return refused("node_exists", index, name)
			var owned: Array = []
			_collect_owned(node, root, owned)
			var copy: Node = node.duplicate()
			copy.name = name
			var parent: Node = node.get_parent()
			var copied: Array = []
			_map_owned(node, copy, owned, copied)
			var do_calls: Array = [[parent, "add_child", [copy]]]
			for descendant in copied:
				do_calls.append([descendant, "set_owner", [root]])
			_record(do_calls, [[parent, "remove_child", [copy]]], [copy], [])
		"set_property", "clear_property":
			var node: Node = _node(root, op.get("node", "."))
			if node == null or not _owned(root, node):
				return refused("node_not_found", index, str(op.get("node", ".")))
			return _set_property(node, op, index, kind == "clear_property")
		"attach_script", "detach_script":
			var node: Node = _node(root, op.get("node", "."))
			if node == null or not _owned(root, node):
				return refused("node_not_found", index, str(op.get("node", ".")))
			var script = null
			if kind == "attach_script":
				script = ResourceLoader.load(str(op.get("script", "")), "Script")
				if script == null:
					return refused("script_not_loadable", index, str(op.get("script", "")))
				var base: String = String(script.get_instance_base_type())
				if base != "" and node.get_class() != base and not ClassDB.is_parent_class(node.get_class(), base):
					return refused("script_base_mismatch", index, base)
			_record([[node, "set_script", [script]]], [[node, "set_script", [node.get_script()]]])
		"replace_instance":
			var node: Node = _node(root, op.get("node", ""))
			if node == null or node == root or node.owner != root or node.scene_file_path == "":
				return refused("not_an_instance", index, str(op.get("node", "")))
			var scene: PackedScene = ResourceLoader.load(str(op.get("scene", "")), "PackedScene")
			if scene == null:
				return refused("scene_not_loadable", index, str(op.get("scene", "")))
			var replacement: Node = scene.instantiate(PackedScene.GEN_EDIT_STATE_INSTANCE)
			if replacement == null:
				return refused("scene_not_instantiable", index, str(op.get("scene", "")))
			var parent: Node = node.get_parent()
			var position: int = node.get_index()
			replacement.name = node.name
			var owned: Array = []
			_collect_owned(node, root, owned)
			var undo_calls: Array = [[parent, "remove_child", [replacement]], [parent, "add_child", [node]],
				[parent, "move_child", [node, position]]]
			for descendant in owned:
				undo_calls.append([descendant, "set_owner", [root]])
			_record([[parent, "remove_child", [node]], [parent, "add_child", [replacement]],
				[parent, "move_child", [replacement, position]], [replacement, "set_owner", [root]]],
				undo_calls, [replacement], [node])
		"connect_signal", "disconnect_signal":
			var source: Node = _node(root, op.get("from", "."))
			var target: Node = _node(root, op.get("to", "."))
			var signal_name: String = str(op.get("signal", ""))
			var method: String = str(op.get("method", ""))
			if source == null or not _owned(root, source):
				return refused("node_not_found", index, str(op.get("from", ".")))
			if target == null or not _owned(root, target):
				return refused("node_not_found", index, str(op.get("to", ".")))
			if not source.has_signal(signal_name):
				return refused("unknown_signal", index, signal_name)
			if method == "" or not (target.has_method(method) \
					or (target.get_script() != null and target.get_script().has_method(method))):
				return refused("unknown_method", index, method)
			var callable := Callable(target, method)
			if kind == "connect_signal":
				if source.is_connected(signal_name, callable):
					return refused("already_connected", index, signal_name)
				_record([[source, "connect", [signal_name, callable, Object.CONNECT_PERSIST]]],
					[[source, "disconnect", [signal_name, callable]]])
			else:
				if not source.is_connected(signal_name, callable):
					return refused("not_connected", index, signal_name)
				_record([[source, "disconnect", [signal_name, callable]]],
					[[source, "connect", [signal_name, callable, Object.CONNECT_PERSIST]]])
		"add_group", "remove_group":
			var node: Node = _node(root, op.get("node", "."))
			var group: String = str(op.get("group", ""))
			if node == null or not _owned(root, node):
				return refused("node_not_found", index, str(op.get("node", ".")))
			if group == "":
				return refused("invalid_group", index, group)
			if kind == "add_group":
				if not node.is_in_group(group):
					_record([[node, "add_to_group", [group, true]]], [[node, "remove_from_group", [group]]])
			else:
				if not node.is_in_group(group):
					return refused("not_in_group", index, group)
				_record([[node, "remove_from_group", [group]]], [[node, "add_to_group", [group, true]]])
		"set_subresource":
			var node: Node = _node(root, op.get("node", "."))
			if node == null or not _owned(root, node):
				return refused("node_not_found", index, str(op.get("node", ".")))
			return _set_subresource(node, op, index)
		"call_method":
			var node: Node = _node(root, op.get("node", "."))
			if node == null or not _owned(root, node):
				return refused("node_not_found", index, str(op.get("node", ".")))
			return _call_method(root, node, op, index)
		_:
			return refused("unknown_operation", index, kind)
	return {"ok": true}


func _set_property(node: Node, op: Dictionary, index: int, clear: bool) -> Dictionary:
	var property: String = str(op.get("property", ""))
	if property == "" or not _has_property(node, property):
		return refused("unknown_property", index, property)
	var value
	if clear:
		if node.property_can_revert(property):
			value = node.property_get_revert(property)
		else:
			value = ClassDB.class_get_property_default_value(node.get_class(), property)
	else:
		var decoded: Dictionary = VariantCodec.decode(op.get("value"))
		if not decoded["ok"]:
			return refused("invalid_value", index, decoded["reason"])
		value = decoded["value"]
	var before = node.get(property)
	node.set(property, value)
	var after = node.get(property)
	if not clear and typeof(value) != TYPE_NIL and typeof(after) != typeof(value) \
			and not (typeof(value) == TYPE_FLOAT and typeof(after) == TYPE_INT) \
			and not (typeof(value) == TYPE_INT and typeof(after) == TYPE_FLOAT) \
			and not (typeof(value) == TYPE_STRING and typeof(after) == TYPE_STRING_NAME) \
			and not (typeof(value) == TYPE_OBJECT):
		node.set(property, before)
		return refused("value_type_mismatch", index, property + " is " + type_string(typeof(after)))
	node.set(property, before)
	_record([[node, "set", [property, after]]], [[node, "set", [property, before]]])
	return {"ok": true}


## `call_method`: call a listed editing method on the node or on a resource
## the scene embeds, and record how to redo and undo it from the receiver's
## stored properties before and after the call.
func _call_method(root: Node, node: Node, op: Dictionary, index: int) -> Dictionary:
	var found: Dictionary = EditingMethods.receiver(node, op.get("resource"), root.scene_file_path)
	if not found.get("ok", false):
		return refused(found["reason"], index, found["detail"])
	var receiver: Object = found["object"]
	var method := str(op.get("method", ""))
	var listed := EditingMethods.listed_class(receiver, method)
	if listed == "":
		return refused("method_not_listed", index, "%s cannot be called on a %s" % [method, receiver.get_class()])
	var arguments: Array = []
	var given = op.get("arguments", [])
	if typeof(given) != TYPE_ARRAY or given.size() > EditingMethods.MAX_CALL_ARGUMENTS:
		return refused("invalid_arguments", index, "`arguments` lists at most %d values" % EditingMethods.MAX_CALL_ARGUMENTS)
	for argument in given:
		var decoded: Dictionary = EditingMethods.decode_argument(argument)
		if not decoded["ok"]:
			return refused("invalid_value", index, decoded["reason"])
		arguments.append(decoded["value"])
	var problem := EditingMethods.signature_problem(receiver, method, arguments)
	if problem != "":
		return refused("invalid_arguments", index, problem)
	var recorded: Dictionary = EditingMethods.call_recorded(receiver, listed, method, arguments)
	if not recorded["ok"]:
		if recorded["unresolved"]:
			return fail("unavailable", recorded["detail"], "unresolved")
		return refused("not_undoable", index, recorded["detail"])
	_steps.append({"do": recorded["do"], "undo": recorded["undo"], "do_refs": [], "undo_refs": []})
	_results.append({"index": index, "method": method, "value": EditingMethods.encode_result(recorded["value"])})
	return {"ok": true}


func _set_subresource(node: Node, op: Dictionary, index: int) -> Dictionary:
	var property: String = str(op.get("property", ""))
	var resource_class: String = str(op.get("class", ""))
	if property == "" or not _has_property(node, property):
		return refused("unknown_property", index, property)
	if not _is_resource_class(resource_class):
		return refused("unknown_resource_class", index, resource_class)
	var resource: Resource = ClassDB.instantiate(resource_class)
	var properties = op.get("properties", {})
	if typeof(properties) != TYPE_DICTIONARY:
		return refused("invalid_value", index, "properties must be an object")
	for key in properties.keys():
		if not _has_property(resource, str(key)):
			return refused("unknown_property", index, str(key))
		var decoded: Dictionary = VariantCodec.decode(properties[key])
		if not decoded["ok"]:
			return refused("invalid_value", index, decoded["reason"])
		resource.set(str(key), decoded["value"])
	var before = node.get(property)
	node.set(property, resource)
	if node.get(property) != resource:
		node.set(property, before)
		return refused("value_type_mismatch", index, property)
	node.set(property, before)
	_record([[node, "set", [property, resource]]], [[node, "set", [property, before]]])
	return {"ok": true}
