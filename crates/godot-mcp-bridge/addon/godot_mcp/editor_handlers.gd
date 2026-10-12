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

const PLUGIN_VERSION := "0.2.0"
const MAX_NODES := 2000
const MAX_PROPERTIES_PER_NODE := 256
const READS := [
	"bridge.describe_session",
	"bridge.ping",
	"editor.scene.tree",
	"editor.state",
	"editor.types.class",
]
const SKIPPED_USAGE := PROPERTY_USAGE_CATEGORY | PROPERTY_USAGE_GROUP | PROPERTY_USAGE_SUBGROUP

var plugin: EditorPlugin = null
var mutations = null
## Undo-history version recorded when each history's scene was last known to
## match its file: when it was first seen open, and after every save.
var saved_versions: Dictionary = {}


func _init(owner_plugin: EditorPlugin = null) -> void:
	plugin = owner_plugin
	mutations = Mutations.new(self)


func methods() -> Array:
	var all: Array = READS.duplicate()
	all.append_array(Mutations.METHODS)
	all.sort()
	return all


func supports(method: String) -> bool:
	return READS.has(method) or Mutations.METHODS.has(method)


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
		"editor.state":
			return ok(state())
		"editor.scene.tree":
			return scene_tree(payload)
		"editor.types.class":
			var described: Dictionary = TypeCatalog.describe(str(payload.get("class", "")))
			if described.get("ok", false):
				return ok(described["payload"])
			return fail(described["error_kind"], described["message"])
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
	}


# --- scenes, histories and unsaved state -------------------------------------


func history_of(root: Node) -> UndoRedo:
	var manager: EditorUndoRedoManager = EditorInterface.get_editor_undo_redo()
	return manager.get_history_undo_redo(manager.get_object_history_id(root))


func history_id_of(root: Node) -> int:
	return EditorInterface.get_editor_undo_redo().get_object_history_id(root)


## Record that the edited scene matches its file at its current history version.
func mark_saved(root: Node) -> void:
	if root != null:
		saved_versions[history_id_of(root)] = history_of(root).get_version()


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
	return {
		"open_scenes": open_scenes,
		"open_scripts": open_scripts,
		"edited_scene": edited.scene_file_path if edited != null else null,
		"selection": selection,
		"history": history,
		"playing": EditorInterface.is_playing_scene(),
	}


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


## The semantic tree of the scene `root` as the editor holds it: owned nodes
## with their class, script, instanced scene, groups and changed stored
## properties, and the persistent signal connections between them, plus a
## digest of all of it. Equal digests mean equal scene content.
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
	var body := {"nodes": nodes, "connections": connections}
	return {
		"nodes": nodes,
		"connections": connections,
		"truncated": owned.size() >= MAX_NODES,
		"digest": JSON.stringify(body, "", true, true).sha256_text(),
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
