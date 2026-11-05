@tool
extends RefCounted
## The editor bridge's handler set: which bridge methods exist, their reads,
## and the live scene snapshot every read and mutation shares.
##
## Reads never change editor state: they open no scene, select nothing, mark
## nothing unsaved and add nothing to any undo history. Mutations are in
## `mutation_handlers.gd`.

const VariantCodec = preload("variant_codec.gd")
const TypeCatalog = preload("type_catalog.gd")
const Mutations = preload("mutation_handlers.gd")
const NodeReader = preload("node_reader.gd")
const Capture = preload("capture.gd")
const Preferences = preload("preferences.gd")
const Shortcuts = preload("shortcuts.gd")
const EditorConfig = preload("editor_config.gd")
const EditorCamera = preload("editor_camera.gd")
const EditorDialogs = preload("editor_dialogs.gd")
const EditorMonitors = preload("editor_monitors.gd")
const ResourcePreview = preload("resource_preview.gd")
const ScriptTabs = preload("script_tabs.gd")
const SceneReader = preload("scene_reader.gd")

const PLUGIN_VERSION := "0.4.0"
const MAX_NODES := 2000
const MAX_PROPERTIES_PER_NODE := 256
const READS := [
	"bridge.describe_session",
	"bridge.ping",
	"bridge.server_status",
	"editor.camera.read",
	"editor.capture.chunk",
	"editor.capture.viewport",
	"editor.dialogs.read",
	"editor.node",
	"editor.performance.read",
	"editor.preferences.read",
	"editor.preview.resource",
	"editor.scene.read",
	"editor.scene.tree",
	"editor.state",
	"editor.types.class",
	"editor.types.search",
]
const SKIPPED_USAGE := PROPERTY_USAGE_CATEGORY | PROPERTY_USAGE_GROUP | PROPERTY_USAGE_SUBGROUP
## Frames the editor gets to show a scene it was asked to show.
const SHOW_FRAMES := 30

var plugin: EditorPlugin = null
var mutations = null
var captures = null
var previews = null
## The plugin's log capture, which serves `editor.log.read` while it runs.
var editor_log = null
## The plugin's panel state, which takes `bridge.server_status`.
var panel_state = null
## Script tabs this plugin proved are their script's, by path: the tab's
## CodeEdit, held weakly. Before Godot 4.7 the editor cannot say whether a tab
## is unsaved, but a tab seen here still says whether it is at its saved
## version. A closed tab's CodeEdit is freed, so it is never mistaken for a
## later one.
var script_tabs: Dictionary = {}
## Undo-history version recorded when each history's scene was last known to
## match its file: when it was first seen open, and after every save.
var saved_versions: Dictionary = {}
## Whether an editor.restart was asked for and has not ended in a decline.
var restart_pending := false
## How many restarts the editor declined, and the question it asked last
## ({"title", "text"}), which was cancelled.
var restart_declines := 0
var restart_question = null


func _init(owner_plugin: EditorPlugin = null) -> void:
	plugin = owner_plugin
	mutations = Mutations.new(self)
	captures = Capture.new()
	previews = ResourcePreview.new()


func methods() -> Array:
	var all: Array = READS.duplicate()
	for method in Mutations.METHODS:
		if supports(method):
			all.append(method)
	for method in ["editor.log.read", "editor.shortcuts.read"]:
		if supports(method):
			all.append(method)
	all.sort()
	return all


func supports(method: String) -> bool:
	if method == "editor.log.read":
		return log_capture()
	if method == "editor.scene.close":
		return scene_close()
	if method == "editor.shortcuts.read" or method == "editor.shortcuts.set":
		return shortcuts()
	if method == "editor.camera.set":
		return EditorCamera.can_set()
	if method == "editor.script.close":
		return ScriptTabs.can_close()
	return READS.has(method) or Mutations.METHODS.has(method)


## Whether this editor can close a scene from a plugin: false on a Godot
## before 4.5.
static func scene_close() -> bool:
	return EditorInterface.has_method("close_scene")


## Whether this editor names its shortcuts to a plugin: false on a Godot
## before 4.6.
static func shortcuts() -> bool:
	return EditorConfig.has_shortcuts()


## Whether the editor's log is being captured: false on a Godot before 4.5.
func log_capture() -> bool:
	return editor_log != null and editor_log.capturing()


## Record that the editor declined a restart it was asked for: it asked
## `question`, which was cancelled.
func restart_declined(question: Dictionary) -> void:
	restart_pending = false
	restart_declines += 1
	restart_question = question


func is_mutation(method: String) -> bool:
	return Mutations.METHODS.has(method)


static func ok(payload: Dictionary, status: String = "not_started") -> Dictionary:
	return {"ok": true, "payload": payload, "mutation_status": status}


static func fail(kind: String, message: String, status: String = "not_started") -> Dictionary:
	return {"ok": false, "error_kind": kind, "message": message, "mutation_status": status}


func dispatch(method: String, payload: Dictionary, context: Dictionary) -> Dictionary:
	match method:
		"bridge.describe_session":
			return ok(describe())
		"bridge.ping":
			return ok({"pong": true})
		"bridge.server_status":
			if panel_state == null:
				return fail("unavailable", "the godot_mcp panel is not running")
			var refusal: String = panel_state.accept_server_status(payload)
			if refusal != "":
				return fail("invalid_input", refusal)
			return ok({"accepted": true})
		"editor.state":
			return ok(state())
		"editor.scene.tree":
			return scene_tree(payload)
		"editor.types.class":
			var described: Dictionary = TypeCatalog.describe(str(payload.get("class", "")))
			if described.get("ok", false):
				return ok(described["payload"])
			return fail(described["error_kind"], described["message"])
		"editor.node":
			return NodeReader.node(self, payload)
		"editor.scene.read":
			return SceneReader.read(self, payload)
		"editor.types.search":
			return NodeReader.search(payload)
		"editor.preferences.read":
			return Preferences.read(payload)
		"editor.shortcuts.read":
			return Shortcuts.read(payload)
		"editor.capture.viewport":
			return await captures.viewport(payload)
		"editor.capture.chunk":
			return captures.chunk(payload)
		"editor.camera.read":
			return EditorCamera.read(payload, main_screen())
		"editor.dialogs.read":
			return EditorDialogs.read(payload)
		"editor.performance.read":
			return EditorMonitors.read(payload)
		"editor.preview.resource":
			return await previews.preview(payload, captures)
		"editor.log.read":
			if not log_capture():
				return fail("unsupported", "this editor does not capture its log; log capture needs Godot 4.5 or later")
			return ok(editor_log.read(payload))
		_:
			return await mutations.dispatch(method, payload, context)


func describe() -> Dictionary:
	var version: Dictionary = Engine.get_version_info()
	return {
		"plugin_version": PLUGIN_VERSION,
		"godot_version": str(version.get("string", "")),
		"godot": {
			"major": int(version.get("major", 0)),
			"minor": int(version.get("minor", 0)),
			"patch": int(version.get("patch", 0)),
			"status": str(version.get("status", "")),
		},
		"methods": methods(),
		"headless": DisplayServer.get_name() == "headless",
		"unsaved_scenes_api": EditorInterface.has_method("get_unsaved_scenes"),
		"log_capture": log_capture(),
		"scene_close": scene_close(),
		"shortcuts": shortcuts(),
		"camera_set": EditorCamera.can_set(),
		"script_close": ScriptTabs.can_close(),
		"pid": OS.get_process_id(),
	}


# --- scenes, histories and unsaved state -------------------------------------


func history_of(root: Node) -> UndoRedo:
	var manager: EditorUndoRedoManager = EditorInterface.get_editor_undo_redo()
	return manager.get_history_undo_redo(manager.get_object_history_id(root))


func history_id_of(root: Node) -> int:
	return EditorInterface.get_editor_undo_redo().get_object_history_id(root)


## Record that the edited scene matches its file at its current history version.
## Any other open scene is not recorded: the editor names a scene's history
## only while that scene is the edited one.
func mark_saved(root: Node) -> void:
	if root != null and root == EditorInterface.get_edited_scene_root():
		saved_versions[history_id_of(root)] = history_of(root).get_version()


## The root of the open scene at `path` as the editor holds it, or null.
## Godot 4.4 lists no open scene's root but the edited one's.
func held_root(path: String) -> Node:
	var edited: Node = EditorInterface.get_edited_scene_root()
	if edited != null and edited.scene_file_path == path:
		return edited
	if EditorInterface.has_method("get_open_scene_roots"):
		for root in EditorInterface.call("get_open_scene_roots"):
			if root != null and root.scene_file_path == path:
				return root
	return null


## Whether the file at `path` holds the open scene `root` as the editor holds
## it, by their semantic digests.
func holds(path: String, root: Node) -> bool:
	if root == null:
		return false
	var disk: Dictionary = file_snapshot(path)
	return not disk.is_empty() and str(disk.get("digest", "")) == edit_digest(root)


## The open scenes of `paths` a save just made is not proven to have saved.
## The editor announces a scene saved, and EditorInterface.save_scene()
## answers OK, even when the file could not be written. So a scene counts as
## saved when the editor itself reports it saved, which Godot 4.7 does, or
## when its file holds the scene as the editor holds it. On Godot 4.4 each
## open scene but the edited one is shown in turn to be read, and the edited
## one shown again after. Every proven scene is marked saved.
func not_saved(paths: Array) -> Array:
	var reported: bool = EditorInterface.has_method("get_unsaved_scenes")
	var still_unsaved: Array = Array(EditorInterface.call("get_unsaved_scenes")) if reported else []
	var listed: bool = EditorInterface.has_method("get_open_scene_roots")
	var edited: Node = EditorInterface.get_edited_scene_root()
	var edited_path: String = edited.scene_file_path if edited != null else ""
	var missing: Array = []
	for path in paths:
		var root: Node = held_root(path)
		if root == null and not listed and EditorInterface.get_open_scenes().has(path):
			root = await show_scene(path)
		var proven: bool = (reported and not still_unsaved.has(path)) or holds(path, root)
		if proven and root != null:
			mark_saved(root)
		elif not proven:
			missing.append(path)
	if edited_path != "":
		await show_scene(edited_path)
	return missing


## Make the open scene at `path` the edited one, and answer its root once the
## editor shows it, or null when it does not within SHOW_FRAMES frames. The
## editor ignores a request to show a scene while it still settles the last
## one it showed, so the request is repeated each frame.
func show_scene(path: String) -> Node:
	var tree: SceneTree = EditorInterface.get_base_control().get_tree()
	for frame in SHOW_FRAMES:
		var edited: Node = EditorInterface.get_edited_scene_root()
		if edited != null and edited.scene_file_path == path:
			return edited
		EditorInterface.open_scene_from_path(path)
		await tree.process_frame
	var shown: Node = EditorInterface.get_edited_scene_root()
	return shown if shown != null and shown.scene_file_path == path else null


## Whether the edited scene `root` holds changes its file does not: true or
## false, or null when this build cannot tell (a scene that was already edited
## before this plugin started, on a build without get_unsaved_scenes()).
func unsaved(root: Node):
	if root == null:
		return null
	if EditorInterface.has_method("get_unsaved_scenes"):
		return EditorInterface.call("get_unsaved_scenes").has(root.scene_file_path)
	var id: int = history_id_of(root)
	if not saved_versions.has(id):
		return null
	return history_of(root).get_version() != int(saved_versions[id])


## Remember that `code` is the tab showing the script at `path`.
func remember_tab(path: String, code: CodeEdit) -> void:
	script_tabs[path] = weakref(code)


## Whether the tab last proved to show `script` holds unsaved edits: false
## when it is still open, shows the script's source and is at its saved
## version; true when it moved past its saved version; null when no such tab
## is known.
func known_tab_unsaved(script):
	var known = script_tabs.get(script.resource_path)
	var code = known.get_ref() if known != null else null
	if not (code is CodeEdit):
		script_tabs.erase(script.resource_path)
		return null
	if code.get_version() != code.get_saved_version():
		return true
	return null if code.text != script.source_code else false


func state() -> Dictionary:
	var edited: Node = EditorInterface.get_edited_scene_root()
	var open_scenes: Array = []
	var unsaved_paths: Array = []
	if EditorInterface.has_method("get_unsaved_scenes"):
		unsaved_paths = Array(EditorInterface.call("get_unsaved_scenes"))
	for path in EditorInterface.get_open_scenes():
		var entry := {"path": path, "edited": edited != null and edited.scene_file_path == path, "unsaved": null}
		if EditorInterface.has_method("get_unsaved_scenes"):
			entry["unsaved"] = unsaved_paths.has(path)
		elif entry["edited"]:
			entry["unsaved"] = unsaved(edited)
		open_scenes.append(entry)
	var selection: Array = []
	if edited != null:
		for node in EditorInterface.get_selection().get_selected_nodes():
			if node == edited or edited.is_ancestor_of(node):
				selection.append(node_path(edited, node))
	var open_scripts: Array = []
	var script_editor = EditorInterface.get_script_editor()
	if script_editor != null:
		var unsaved_files = null
		if script_editor.has_method("get_unsaved_files"):
			unsaved_files = Array(script_editor.call("get_unsaved_files"))
		for script in script_editor.get_open_scripts():
			var script_path: String = script.resource_path
			if not script_path.begins_with("res://") or script_path.contains("::"):
				continue
			var script_unsaved = null
			if unsaved_files != null:
				script_unsaved = unsaved_files.has(script_path)
			elif script.source_code != FileAccess.get_file_as_string(script_path):
				# This build cannot say whether the editor buffer is unsaved,
				# only that the script differs from its file.
				script_unsaved = true
			else:
				script_unsaved = known_tab_unsaved(script)
			open_scripts.append({"path": script_path, "unsaved": script_unsaved})
	var history = null
	if edited != null:
		var undo_redo: UndoRedo = history_of(edited)
		history = {
			"version": undo_redo.get_version(),
			"has_undo": undo_redo.has_undo(),
			"has_redo": undo_redo.has_redo(),
			"current_action": undo_redo.get_current_action_name(),
		}
	var filesystem: EditorFileSystem = EditorInterface.get_resource_filesystem()
	var playing_scene := EditorInterface.get_playing_scene()
	return {
		"open_scenes": open_scenes,
		"open_scripts": open_scripts,
		"edited_scene": edited.scene_file_path if edited != null else null,
		"selection": selection,
		"history": history,
		"playing": EditorInterface.is_playing_scene(),
		"playing_scene": playing_scene if playing_scene != "" else null,
		"filesystem": {
			"scanning": filesystem.is_scanning(),
			# Only Godot 4.7 says whether an import is running.
			"importing": filesystem.call("is_importing") if filesystem.has_method("is_importing") else null,
		},
		"main_screen": main_screen(),
		"log_capture": log_capture(),
		"scene_close": scene_close(),
		"shortcuts": shortcuts(),
		"restart": {"pending": restart_pending, "declined": restart_declines, "question": restart_question},
	}


## The main screen the editor shows, told by which screen is visible: `2D`,
## `3D`, `Script`, or `other` for any other screen (AssetLib, a plugin's).
static func main_screen() -> String:
	var viewport_2d = EditorInterface.get_editor_viewport_2d()
	if viewport_2d != null and viewport_2d.get_parent() is Control and viewport_2d.get_parent().is_visible_in_tree():
		return "2D"
	var viewport_3d = EditorInterface.get_editor_viewport_3d(0)
	if viewport_3d != null and viewport_3d.get_parent() is Control and viewport_3d.get_parent().is_visible_in_tree():
		return "3D"
	var script_editor = EditorInterface.get_script_editor()
	if script_editor != null and script_editor.is_visible_in_tree():
		return "Script"
	return "other"


static func node_path(root: Node, node: Node) -> String:
	return "." if node == root else String(root.get_path_to(node))


## The open scene at `path`, or the edited one when `path` is empty. Only the
## edited scene's live tree is reachable on every supported build, so another
## open scene answers not_found until it is made current.
func open_scene_root(path: String) -> Dictionary:
	var edited: Node = EditorInterface.get_edited_scene_root()
	if path == "":
		if edited == null:
			return fail("not_found", "no scene is being edited")
		return {"ok": true, "root": edited}
	if not EditorInterface.get_open_scenes().has(path):
		return fail("not_found", "%s is not open in the editor" % path)
	if edited == null or edited.scene_file_path != path:
		return fail("not_found", "%s is open but not the current scene; open it first" % path)
	return {"ok": true, "root": edited}


func scene_tree(payload: Dictionary) -> Dictionary:
	var found: Dictionary = open_scene_root(str(payload.get("scene", "")))
	if not found.get("ok", false):
		return found
	var root: Node = found["root"]
	var snapshot: Dictionary = scene_snapshot(root)
	snapshot["path"] = root.scene_file_path
	snapshot["unsaved"] = unsaved(root)
	var history: UndoRedo = history_of(root)
	snapshot["history"] = {"version": history.get_version(), "has_undo": history.has_undo(), "has_redo": history.has_redo()}
	return ok(snapshot)


# --- the live semantic snapshot ----------------------------------------------


static func _owned_nodes(root: Node) -> Array:
	var nodes: Array = [root]
	# A cursor rather than pop_front(), which would make a large scene's walk
	# quadratic.
	var pending: Array = [root]
	var cursor := 0
	while cursor < pending.size() and nodes.size() < MAX_NODES:
		var node: Node = pending[cursor]
		cursor += 1
		for child in node.get_children():
			if child.owner == root:
				nodes.append(child)
				pending.append(child)
			if nodes.size() >= MAX_NODES:
				break
	return nodes


static func _default_value(node: Node, property: String):
	var script = node.get_script()
	if script != null:
		var scripted = script.get_property_default_value(property)
		if scripted != null:
			return scripted
	return ClassDB.class_get_property_default_value(node.get_class(), property)


## A read-only spelling of a stored property value: a contract tagged value, or
## for a resource embedded in the scene its class and its own changed values.
static func _describe_value(value, depth: int = 0):
	if value is Resource and (value.resource_path == "" or value.resource_path.contains("::")):
		var properties: Dictionary = {}
		if depth < 2:
			for property in value.get_property_list():
				var usage: int = int(property.get("usage", 0))
				if not (usage & PROPERTY_USAGE_STORAGE) or (usage & SKIPPED_USAGE):
					continue
				var name: String = property["name"]
				if name in ["resource_local_to_scene", "resource_name", "resource_path", "script"]:
					continue
				var current = value.get(name)
				if current == ClassDB.class_get_property_default_value(value.get_class(), name):
					continue
				properties[name] = _describe_value(current, depth + 1)
		return {"type": "subresource", "value": {"class": value.get_class(), "properties": properties}}
	var encoded: Dictionary = VariantCodec.encode(value)
	if encoded["ok"]:
		return encoded["value"]
	return {"type": "unrepresentable", "value": {"variant_type": type_string(typeof(value))}}


static func _stored_properties(node: Node) -> Dictionary:
	var properties: Dictionary = {}
	for property in node.get_property_list():
		var usage: int = int(property.get("usage", 0))
		if not (usage & PROPERTY_USAGE_STORAGE) or (usage & SKIPPED_USAGE):
			continue
		var name: String = property["name"]
		if name == "script" or properties.size() >= MAX_PROPERTIES_PER_NODE:
			continue
		var value = node.get(name)
		if value == _default_value(node, name):
			continue
		properties[name] = _describe_value(value)
	return properties


## The semantic digest of the scene `root` as the editor holds it, hashed in
## one pass over every owned node: its path, class, script, instanced scene and
## groups, every changed stored property with embedded sub-resources to any
## depth, and the persistent signal connections between the scene's nodes. No
## limit applies, so a change anywhere in the scene changes the digest; equal
## digests mean equal scene content. It is what editor.scene.edit compares
## `expected_digest` with, and what every answer reports as `digest`.
static func edit_digest(root: Node) -> String:
	var hashing := HashingContext.new()
	hashing.start(HashingContext.HASH_SHA256)
	var visited: Dictionary = {}
	var connections: Array = []
	# A cursor rather than pop_front(), which would make the walk quadratic.
	var pending: Array = [root]
	var cursor := 0
	while cursor < pending.size():
		var node: Node = pending[cursor]
		cursor += 1
		for child in node.get_children():
			if child.owner == root:
				pending.append(child)
		var script = node.get_script()
		var groups: Array = []
		for group in node.get_groups():
			if not String(group).begins_with("_"):
				groups.append(String(group))
		groups.sort()
		_digest_text(hashing, "node", node_path(root, node))
		_digest_text(hashing, "class", node.get_class())
		_digest_text(hashing, "script", script.resource_path if script != null else "")
		_digest_text(hashing, "instance", node.scene_file_path if node != root else "")
		_digest_text(hashing, "groups", ",".join(groups))
		for property in node.get_property_list():
			var usage: int = int(property.get("usage", 0))
			if not (usage & PROPERTY_USAGE_STORAGE) or (usage & SKIPPED_USAGE):
				continue
			var name: String = property["name"]
			if name == "script":
				continue
			var value = node.get(name)
			if value == _default_value(node, name):
				continue
			_digest_text(hashing, "property", name)
			_digest_value(hashing, value, visited)
		for connection in node.get_incoming_connections():
			var source = connection["signal"].get_object()
			if not (int(connection["flags"]) & CONNECT_PERSIST) or not (source is Node):
				continue
			if source != root and not root.is_ancestor_of(source):
				continue
			connections.append("%s|%s|%s|%s" % [
				String(connection["signal"].get_name()),
				node_path(root, source),
				node_path(root, node),
				String(connection["callable"].get_method()),
			])
	connections.sort()
	for connection in connections:
		_digest_text(hashing, "connection", connection)
	return hashing.finish().hex_encode()


## Feed one tagged, length-prefixed text into the digest, so no two different
## sequences of fields hash alike. An empty text adds only its prefix:
## HashingContext refuses an empty update with an engine error, and thousands
## of those per scene flood the Output panel until each one takes seconds.
static func _digest_text(hashing: HashingContext, tag: String, text: String) -> void:
	var bytes := text.to_utf8_buffer()
	hashing.update(("%s:%d:" % [tag, bytes.size()]).to_utf8_buffer())
	if not bytes.is_empty():
		hashing.update(bytes)


## Feed one stored value: an embedded sub-resource by its class, script and
## every changed stored property, once per resource however often it is
## referenced; a container item by item; anything else by its text form.
static func _digest_value(hashing: HashingContext, value, visited: Dictionary) -> void:
	if value is Resource and (value.resource_path == "" or value.resource_path.contains("::")):
		var id: int = value.get_instance_id()
		if visited.has(id):
			_digest_text(hashing, "seen", str(visited[id]))
			return
		visited[id] = visited.size()
		var script = value.get_script()
		_digest_text(hashing, "subresource", value.get_class())
		_digest_text(hashing, "script", script.resource_path if script != null else "")
		for property in value.get_property_list():
			var usage: int = int(property.get("usage", 0))
			if not (usage & PROPERTY_USAGE_STORAGE) or (usage & SKIPPED_USAGE):
				continue
			var name: String = property["name"]
			if name in ["resource_path", "script"]:
				continue
			var current = value.get(name)
			if current == ClassDB.class_get_property_default_value(value.get_class(), name):
				continue
			_digest_text(hashing, "property", name)
			_digest_value(hashing, current, visited)
		return
	if value is Resource:
		_digest_text(hashing, "resource", value.resource_path)
		return
	match typeof(value):
		TYPE_ARRAY:
			_digest_text(hashing, "array", str(value.size()))
			for item in value:
				_digest_value(hashing, item, visited)
		TYPE_DICTIONARY:
			_digest_text(hashing, "dictionary", str(value.size()))
			for key in value:
				_digest_value(hashing, key, visited)
				_digest_value(hashing, value[key], visited)
		TYPE_OBJECT:
			_digest_text(hashing, "object", value.get_class() if value != null else "")
		_:
			_digest_text(hashing, "value", var_to_str(value))


## The semantic tree of the scene `root` as the editor holds it: owned nodes
## with their class, script, instanced scene, groups and changed stored
## properties, and the persistent signal connections between them, plus the
## digest of the whole scene. The node list is bounded; the digest is not.
static func scene_snapshot(root: Node) -> Dictionary:
	var nodes: Array = []
	var connections: Array = []
	var owned: Array = _owned_nodes(root)
	for node in owned:
		var script = node.get_script()
		var groups: Array = []
		for group in node.get_groups():
			if not String(group).begins_with("_"):
				groups.append(String(group))
		groups.sort()
		nodes.append({
			"path": node_path(root, node),
			"type": node.get_class(),
			"script": script.resource_path if script != null and script.resource_path != "" else null,
			"instance": node.scene_file_path if node != root and node.scene_file_path != "" else null,
			"groups": groups,
			"properties": _stored_properties(node),
		})
		for connection in node.get_incoming_connections():
			var source = connection["signal"].get_object()
			if not (int(connection["flags"]) & CONNECT_PERSIST) or not (source is Node):
				continue
			if source != root and not root.is_ancestor_of(source):
				continue
			connections.append({
				"signal": String(connection["signal"].get_name()),
				"from": node_path(root, source),
				"to": node_path(root, node),
				"method": String(connection["callable"].get_method()),
			})
	connections.sort_custom(func(a, b): return JSON.stringify(a) < JSON.stringify(b))
	return {
		"nodes": nodes,
		"connections": connections,
		"truncated": owned.size() >= MAX_NODES,
		"digest": edit_digest(root),
	}


## The semantic snapshot of the scene file at `path` as it is on disk now.
static func file_snapshot(path: String) -> Dictionary:
	var packed = ResourceLoader.load(path, "PackedScene", ResourceLoader.CACHE_MODE_IGNORE)
	if packed == null:
		return {}
	var root: Node = packed.instantiate(PackedScene.GEN_EDIT_STATE_MAIN)
	if root == null:
		return {}
	var snapshot: Dictionary = scene_snapshot(root)
	root.free()
	return snapshot
