# godot-mcp bundled canonical-write helper, version 2.
#
# This file is SHIPPED WITH the server and pinned by SHA-256 in
# `src/canonical/helper.rs`. It is never generated at runtime and never fetched
# from the network: `helper::HelperScript::source()` returns exactly these bytes
# and a unit test asserts the pinned digest, so a tampered or substituted helper
# cannot be installed.
#
# It runs inside a supervised, disposable, exiting Godot subprocess launched
# through the Todo 14 launch protocol:
#
#   godot --headless --path <project> \
#         --script res://godot_mcp_helpers/2/canonical_write.gd \
#         -- --request <res-relative request> --response <res-relative response>
#
# Everything after `--` reaches this script through
# `OS.get_cmdline_user_args()`, which is the documented way to pass user
# arguments past the engine's own flag parser.
#
# Contract, deliberately narrow:
#
#   * The helper is the ONLY thing that builds engine-canonical `.tscn`/`.tres`
#     bytes and the ONLY thing that mints a `ResourceUID`. The Rust side never
#     synthesises resource text and never implements UID hashing.
#   * The helper writes ONLY inside `res://godot_mcp_helpers/2/staging/`. It
#     never touches the caller's target path. Publishing the staged artifact
#     onto the target is done by the Rust side, under the Todo 2 lease, as a
#     compare-and-swap against the recorded preimage.
#   * The helper refuses binary resources (`.scn`/`.res`) outright.
#   * A `batch` is a JSON array of typed operations the ENGINE applies to the
#     loaded scene tree or resource: nodes, properties (as tagged values),
#     scripts, instances, signal connections, groups and sub-resources. Every
#     operation is validated against the engine's own class database; the first
#     one that cannot be applied refuses the whole batch and nothing is staged.
#   * A batch may name nodes by handle. The Rust side resolves each handle to a
#     node path of the target as it is now (`handles`); the helper binds every
#     handle to that node OBJECT before the first operation, so a handle follows
#     its node through renames and moves and never names a node created later
#     at the same path. A handle whose node the batch removed names nothing.
#   * A headless save writes no `uid=` header, so an existing target's UID is
#     carried into the staged artifact and `uid://` references keep resolving.
#   * Exit 0 means "the staged artifact was written". It is NOT, on its own,
#     proof that the operation completed: the Rust side additionally requires a
#     clean-terminal `process_launch` proof AND a matching on-disk post-hash.

extends SceneTree

const HELPER_VERSION := "2"
const STAGING_PREFIX := "res://godot_mcp_helpers/2/staging/"
const TEXT_EXTENSIONS := ["tscn", "tres"]
const BINARY_EXTENSIONS := ["scn", "res"]

const HANDLE_PREFIX := "nh1:"

const EXIT_OK := 0
const EXIT_USAGE := 64
const EXIT_REFUSED := 65
const EXIT_FAILED := 70

# Node handle -> the node object it was bound to before the batch ran.
var _handles: Dictionary = {}


func _init() -> void:
	var args: PackedStringArray = OS.get_cmdline_user_args()
	var options: Dictionary = _parse_options(args)
	if not options.has("request") or not options.has("response"):
		printerr("canonical_write: --request and --response are both required")
		quit(EXIT_USAGE)
		return

	var request_path: String = _res(options["request"])
	var response_path: String = _res(options["response"])
	var request: Dictionary = _read_pairs(request_path)
	if request.is_empty():
		printerr("canonical_write: unreadable request ", request_path)
		quit(EXIT_USAGE)
		return

	var response: Dictionary = _run(request)
	var code: int = int(response.get("exit_code", EXIT_FAILED))
	response.erase("exit_code")
	response["helper_version"] = HELPER_VERSION
	if not _write_pairs(response_path, response):
		printerr("canonical_write: could not write the response document")
		quit(EXIT_FAILED)
		return
	quit(code)


func _run(request: Dictionary) -> Dictionary:
	var operation: String = str(request.get("operation", ""))
	var target: String = str(request.get("target", ""))
	var artifact: String = str(request.get("artifact", ""))

	if not artifact.begins_with("godot_mcp_helpers/"):
		return {"status": "refused", "reason": "artifact_outside_staging", "exit_code": EXIT_REFUSED}
	var artifact_res: String = _res(artifact)
	if not artifact_res.begins_with(STAGING_PREFIX):
		return {"status": "refused", "reason": "artifact_outside_staging", "exit_code": EXIT_REFUSED}

	if target != "":
		var extension: String = target.get_extension().to_lower()
		if BINARY_EXTENSIONS.has(extension):
			return {"status": "refused", "reason": "binary_resource", "exit_code": EXIT_REFUSED}
		if not TEXT_EXTENSIONS.has(extension):
			return {"status": "refused", "reason": "not_a_text_resource", "exit_code": EXIT_REFUSED}

	match operation:
		"create_resource":
			return _create_resource(request, target, artifact_res)
		"update_resource":
			return _update_resource(request, target, artifact_res)
		"apply_batch":
			return _apply_batch(request, target, artifact_res)
		"register_uid":
			return _register_uid(request, target, artifact_res)
		"query_uid":
			return _query_uid(request)
		_:
			return {"status": "refused", "reason": "unknown_operation", "exit_code": EXIT_USAGE}


# --- resource operations ---------------------------------------------------


func _create_resource(request: Dictionary, target: String, artifact_res: String) -> Dictionary:
	var target_res: String = _res(target)
	if FileAccess.file_exists(target_res):
		return {"status": "refused", "reason": "already_exists", "exit_code": EXIT_REFUSED}
	var batch: Dictionary = _parse_batch(request)
	if batch.has("refusal"):
		return batch["refusal"]

	var uid_id: int = _create_id_for(target_res)
	var saved: int = OK
	if target.get_extension().to_lower() == "tscn":
		var root_type: String = str(request.get("root_type", "Node"))
		if not _is_node_class(root_type):
			return _refuse("unknown_root_type", -1, root_type)
		var packed := PackedScene.new()
		var root := _build_tree(request)
		var applied: Dictionary = _apply_scene_ops(root, batch["ops"])
		if applied["status"] != "ok":
			root.free()
			return applied
		saved = packed.pack(root)
		if saved == OK:
			saved = ResourceSaver.save(packed, artifact_res)
		root.free()
	else:
		var resource_class: String = str(request.get("resource_class", "Resource"))
		if not _is_resource_class(resource_class):
			return _refuse("unknown_resource_class", -1, resource_class)
		var resource: Resource = ClassDB.instantiate(resource_class)
		resource.resource_name = str(request.get("resource_name", ""))
		var applied: Dictionary = _apply_resource_ops(resource, batch["ops"])
		if applied["status"] != "ok":
			return applied
		saved = ResourceSaver.save(resource, artifact_res)

	if saved != OK:
		return {"status": "failed", "reason": "save_failed", "exit_code": EXIT_FAILED}
	# A headless save writes no UID; the one minted above is put into the header
	# so the created file carries the UID this answer reports.
	if not _write_header_uid(artifact_res, ResourceUID.id_to_text(uid_id)):
		return {"status": "failed", "reason": "save_failed", "exit_code": EXIT_FAILED}
	return _ok(artifact_res, target_res, uid_id)


func _update_resource(request: Dictionary, target: String, artifact_res: String) -> Dictionary:
	var target_res: String = _res(target)
	var existing: Resource = ResourceLoader.load(target_res, "", ResourceLoader.CACHE_MODE_IGNORE)
	if existing == null:
		return {"status": "failed", "reason": "target_not_loadable", "exit_code": EXIT_FAILED}
	var new_name: String = str(request.get("resource_name", ""))
	if new_name != "":
		existing.resource_name = new_name
	var saved: int = ResourceSaver.save(existing, artifact_res)
	if saved != OK:
		return {"status": "failed", "reason": "save_failed", "exit_code": EXIT_FAILED}
	return _ok_carrying_uid(artifact_res, target_res)


# Apply a batch to an existing scene or resource and stage the result.
func _apply_batch(request: Dictionary, target: String, artifact_res: String) -> Dictionary:
	var target_res: String = _res(target)
	var batch: Dictionary = _parse_batch(request)
	if batch.has("refusal"):
		return batch["refusal"]
	var saved: int = OK
	if target.get_extension().to_lower() == "tscn":
		var packed: PackedScene = ResourceLoader.load(target_res, "", ResourceLoader.CACHE_MODE_IGNORE)
		if packed == null:
			return {"status": "failed", "reason": "target_not_loadable", "exit_code": EXIT_FAILED}
		# The edit state keeps instanced sub-scenes as instances, so packing again
		# saves a reference to each rather than inlining its nodes.
		var root: Node = packed.instantiate(PackedScene.GEN_EDIT_STATE_MAIN)
		if root == null:
			return {"status": "failed", "reason": "target_not_instantiable", "exit_code": EXIT_FAILED}
		var bound: Dictionary = _bind_handles(request, root)
		if bound["status"] != "ok":
			root.free()
			return bound
		var applied: Dictionary = _apply_scene_ops(root, batch["ops"])
		if applied["status"] != "ok":
			root.free()
			return applied
		var repacked := PackedScene.new()
		saved = repacked.pack(root)
		root.free()
		if saved == OK:
			saved = ResourceSaver.save(repacked, artifact_res)
	else:
		var resource: Resource = ResourceLoader.load(target_res, "", ResourceLoader.CACHE_MODE_IGNORE)
		if resource == null:
			return {"status": "failed", "reason": "target_not_loadable", "exit_code": EXIT_FAILED}
		var applied: Dictionary = _apply_resource_ops(resource, batch["ops"])
		if applied["status"] != "ok":
			return applied
		saved = ResourceSaver.save(resource, artifact_res)
	if saved != OK:
		return {"status": "failed", "reason": "save_failed", "exit_code": EXIT_FAILED}
	return _ok_carrying_uid(artifact_res, target_res)


# --- batch operations ------------------------------------------------------


func _refuse(reason: String, index: int, detail: String) -> Dictionary:
	return {
		"status": "refused",
		"reason": reason,
		"op_index": str(index),
		"detail": detail.replace("\n", " "),
		"exit_code": EXIT_REFUSED,
	}


func _parse_batch(request: Dictionary) -> Dictionary:
	var text: String = str(request.get("batch", "[]"))
	var parsed = JSON.parse_string(text)
	if typeof(parsed) != TYPE_ARRAY:
		return {"refusal": _refuse("batch_not_an_array", -1, "the batch must be a JSON array")}
	for operation in parsed:
		if typeof(operation) != TYPE_DICTIONARY or not operation.has("op"):
			return {"refusal": _refuse("operation_not_an_object", -1, "every operation needs an op")}
	return {"ops": parsed}


func _is_node_class(name: String) -> bool:
	return ClassDB.class_exists(name) and ClassDB.can_instantiate(name) \
		and (name == "Node" or ClassDB.is_parent_class(name, "Node"))


func _is_resource_class(name: String) -> bool:
	return ClassDB.class_exists(name) and ClassDB.can_instantiate(name) \
		and (name == "Resource" or ClassDB.is_parent_class(name, "Resource"))


# The node at `path` relative to the scene root; "." or "" is the root. A node
# handle names the node it was bound to, while that node is still in the scene.
func _node(root: Node, path) -> Node:
	var spelled: String = str(path)
	if spelled.begins_with(HANDLE_PREFIX):
		var bound = _handles.get(spelled)
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


# Bind every handle the request resolved to its node, before any operation runs.
func _bind_handles(request: Dictionary, root: Node) -> Dictionary:
	_handles = {}
	var parsed = JSON.parse_string(str(request.get("handles", "{}")))
	if typeof(parsed) != TYPE_DICTIONARY:
		return _refuse("handles_not_an_object", -1, "the handles must be a JSON object")
	for handle in parsed:
		var path: String = str(parsed[handle])
		var node: Node = null if path.begins_with(HANDLE_PREFIX) else _node(root, path)
		if node == null or not _owned(root, node):
			return _refuse("stale_handle", -1, str(handle))
		_handles[str(handle)] = node
	return {"status": "ok"}


# Whether `node` is saved as part of this scene rather than inside an instance.
func _owned(root: Node, node: Node) -> bool:
	return node == root or node.owner == root


func _set_owner_recursive(node: Node, root: Node, owned: Array) -> void:
	if owned.has(node):
		node.owner = root
	for child in node.get_children():
		_set_owner_recursive(child, root, owned)


func _collect_owned(node: Node, root: Node, into: Array) -> void:
	if node.owner == root:
		into.append(node)
	for child in node.get_children():
		_collect_owned(child, root, into)


func _apply_scene_ops(root: Node, ops: Array) -> Dictionary:
	for index in ops.size():
		var result: Dictionary = _apply_scene_op(root, ops[index], index)
		if result["status"] != "ok":
			return result
	return {"status": "ok"}


func _apply_scene_op(root: Node, op: Dictionary, index: int) -> Dictionary:
	var kind: String = str(op.get("op", ""))
	match kind:
		"add_node", "instance_scene":
			var parent: Node = _node(root, op.get("parent", "."))
			if parent == null or not _owned(root, parent):
				return _refuse("parent_not_found", index, str(op.get("parent", ".")))
			var name: String = str(op.get("name", ""))
			if name == "" or name.validate_node_name() != name:
				return _refuse("invalid_node_name", index, name)
			if parent.has_node(NodePath(name)):
				return _refuse("node_exists", index, name)
			var child: Node = null
			if op.has("scene"):
				var scene: PackedScene = ResourceLoader.load(str(op["scene"]), "PackedScene")
				if scene == null:
					return _refuse("scene_not_loadable", index, str(op["scene"]))
				child = scene.instantiate(PackedScene.GEN_EDIT_STATE_INSTANCE)
				if child == null:
					return _refuse("scene_not_instantiable", index, str(op["scene"]))
			else:
				var node_type: String = str(op.get("type", "Node"))
				if not _is_node_class(node_type):
					return _refuse("unknown_node_type", index, node_type)
				child = ClassDB.instantiate(node_type)
			child.name = name
			parent.add_child(child)
			child.owner = root
		"remove_node":
			var node: Node = _node(root, op.get("node", ""))
			if node == null or node == root or not _owned(root, node):
				return _refuse("node_not_found", index, str(op.get("node", "")))
			node.get_parent().remove_child(node)
			node.free()
		"rename_node":
			var node: Node = _node(root, op.get("node", ""))
			var name: String = str(op.get("name", ""))
			if node == null or not _owned(root, node):
				return _refuse("node_not_found", index, str(op.get("node", "")))
			if name == "" or name.validate_node_name() != name:
				return _refuse("invalid_node_name", index, name)
			if node != root and node.get_parent().has_node(NodePath(name)):
				return _refuse("node_exists", index, name)
			node.name = name
		"move_node":
			var node: Node = _node(root, op.get("node", ""))
			var parent: Node = _node(root, op.get("parent", "."))
			if node == null or node == root or not _owned(root, node):
				return _refuse("node_not_found", index, str(op.get("node", "")))
			if parent == null or not _owned(root, parent) or parent == node or node.is_ancestor_of(parent):
				return _refuse("invalid_parent", index, str(op.get("parent", ".")))
			if parent != node.get_parent() and parent.has_node(NodePath(String(node.name))):
				return _refuse("node_exists", index, String(node.name))
			var owned: Array = []
			_collect_owned(node, root, owned)
			node.get_parent().remove_child(node)
			parent.add_child(node)
			_set_owner_recursive(node, root, owned)
			if op.has("index"):
				parent.move_child(node, clampi(int(op["index"]), 0, parent.get_child_count() - 1))
		"duplicate_node":
			var node: Node = _node(root, op.get("node", ""))
			var name: String = str(op.get("name", ""))
			if node == null or node == root or not _owned(root, node):
				return _refuse("node_not_found", index, str(op.get("node", "")))
			if name == "" or name.validate_node_name() != name:
				return _refuse("invalid_node_name", index, name)
			if node.get_parent().has_node(NodePath(name)):
				return _refuse("node_exists", index, name)
			var owned: Array = []
			_collect_owned(node, root, owned)
			var copy: Node = node.duplicate()
			copy.name = name
			node.get_parent().add_child(copy)
			var copied: Array = []
			_map_owned(node, copy, owned, copied)
			_set_owner_recursive(copy, root, copied)
		"set_property", "clear_property":
			var node: Node = _node(root, op.get("node", "."))
			if node == null or not _owned(root, node):
				return _refuse("node_not_found", index, str(op.get("node", ".")))
			return _set_property(node, op, index, kind == "clear_property")
		"attach_script":
			var node: Node = _node(root, op.get("node", "."))
			if node == null or not _owned(root, node):
				return _refuse("node_not_found", index, str(op.get("node", ".")))
			var script: Script = ResourceLoader.load(str(op.get("script", "")), "Script")
			if script == null:
				return _refuse("script_not_loadable", index, str(op.get("script", "")))
			var base: String = String(script.get_instance_base_type())
			if base != "" and node.get_class() != base and not ClassDB.is_parent_class(node.get_class(), base):
				return _refuse("script_base_mismatch", index, base)
			node.set_script(script)
		"detach_script":
			var node: Node = _node(root, op.get("node", "."))
			if node == null or not _owned(root, node):
				return _refuse("node_not_found", index, str(op.get("node", ".")))
			node.set_script(null)
		"replace_instance":
			var node: Node = _node(root, op.get("node", ""))
			if node == null or node == root or node.owner != root or node.scene_file_path == "":
				return _refuse("not_an_instance", index, str(op.get("node", "")))
			var scene: PackedScene = ResourceLoader.load(str(op.get("scene", "")), "PackedScene")
			if scene == null:
				return _refuse("scene_not_loadable", index, str(op.get("scene", "")))
			var replacement: Node = scene.instantiate(PackedScene.GEN_EDIT_STATE_INSTANCE)
			var parent: Node = node.get_parent()
			var position: int = node.get_index()
			var name: StringName = node.name
			parent.remove_child(node)
			node.free()
			replacement.name = name
			parent.add_child(replacement)
			parent.move_child(replacement, position)
			replacement.owner = root
		"connect_signal", "disconnect_signal":
			var source: Node = _node(root, op.get("from", "."))
			var target: Node = _node(root, op.get("to", "."))
			var signal_name: String = str(op.get("signal", ""))
			var method: String = str(op.get("method", ""))
			if source == null or not _owned(root, source):
				return _refuse("node_not_found", index, str(op.get("from", ".")))
			if target == null or not _owned(root, target):
				return _refuse("node_not_found", index, str(op.get("to", ".")))
			if not source.has_signal(signal_name):
				return _refuse("unknown_signal", index, signal_name)
			if method == "" or not (target.has_method(method) \
					or (target.get_script() != null and target.get_script().has_method(method))):
				return _refuse("unknown_method", index, method)
			var callable := Callable(target, method)
			if kind == "connect_signal":
				if source.is_connected(signal_name, callable):
					return _refuse("already_connected", index, signal_name)
				source.connect(signal_name, callable, Object.CONNECT_PERSIST)
			else:
				if not source.is_connected(signal_name, callable):
					return _refuse("not_connected", index, signal_name)
				source.disconnect(signal_name, callable)
		"add_group", "remove_group":
			var node: Node = _node(root, op.get("node", "."))
			var group: String = str(op.get("group", ""))
			if node == null or not _owned(root, node):
				return _refuse("node_not_found", index, str(op.get("node", ".")))
			if group == "":
				return _refuse("invalid_group", index, group)
			if kind == "add_group":
				node.add_to_group(group, true)
			else:
				if not node.is_in_group(group):
					return _refuse("not_in_group", index, group)
				node.remove_from_group(group)
		"set_subresource":
			var node: Node = _node(root, op.get("node", "."))
			if node == null or not _owned(root, node):
				return _refuse("node_not_found", index, str(op.get("node", ".")))
			return _set_subresource(node, op, index)
		_:
			return _refuse("unknown_operation", index, kind)
	return {"status": "ok"}


# Map every node of `original` that `owned` lists onto its counterpart in `copy`.
func _map_owned(original: Node, copy: Node, owned: Array, into: Array) -> void:
	if owned.has(original):
		into.append(copy)
	var count: int = mini(original.get_child_count(), copy.get_child_count())
	for child in count:
		_map_owned(original.get_child(child), copy.get_child(child), owned, into)


func _apply_resource_ops(resource: Resource, ops: Array) -> Dictionary:
	for index in ops.size():
		var op: Dictionary = ops[index]
		var kind: String = str(op.get("op", ""))
		var result: Dictionary
		match kind:
			"set_property", "clear_property":
				result = _set_property(resource, op, index, kind == "clear_property")
			"set_subresource":
				result = _set_subresource(resource, op, index)
			_:
				result = _refuse("unknown_operation", index, kind)
		if result["status"] != "ok":
			return result
	return {"status": "ok"}


func _has_property(object: Object, property: String) -> bool:
	for entry in object.get_property_list():
		if entry["name"] == property:
			return true
	return false


func _set_property(object: Object, op: Dictionary, index: int, clear: bool) -> Dictionary:
	var property: String = str(op.get("property", ""))
	if property == "" or not _has_property(object, property):
		return _refuse("unknown_property", index, property)
	var value
	if clear:
		if object.property_can_revert(property):
			value = object.property_get_revert(property)
		else:
			value = ClassDB.class_get_property_default_value(object.get_class(), property)
	else:
		var decoded: Dictionary = _decode(op.get("value"))
		if not decoded["ok"]:
			return _refuse("invalid_value", index, decoded["reason"])
		value = decoded["value"]
	var before = object.get(property)
	object.set(property, value)
	var after = object.get(property)
	if not clear and typeof(value) != TYPE_NIL and typeof(after) != typeof(value) \
			and not (typeof(value) == TYPE_FLOAT and typeof(after) == TYPE_INT) \
			and not (typeof(value) == TYPE_INT and typeof(after) == TYPE_FLOAT) \
			and not (typeof(value) == TYPE_STRING and typeof(after) == TYPE_STRING_NAME) \
			and not (typeof(value) == TYPE_OBJECT):
		object.set(property, before)
		return _refuse("value_type_mismatch", index, property + " is " + type_string(typeof(after)))
	return {"status": "ok"}


func _set_subresource(object: Object, op: Dictionary, index: int) -> Dictionary:
	var property: String = str(op.get("property", ""))
	var resource_class: String = str(op.get("class", ""))
	if property == "" or not _has_property(object, property):
		return _refuse("unknown_property", index, property)
	if not _is_resource_class(resource_class):
		return _refuse("unknown_resource_class", index, resource_class)
	var resource: Resource = ClassDB.instantiate(resource_class)
	var properties = op.get("properties", {})
	if typeof(properties) != TYPE_DICTIONARY:
		return _refuse("invalid_value", index, "properties must be an object")
	for key in properties.keys():
		var nested: Dictionary = _set_property(resource, {"property": key, "value": properties[key]}, index, false)
		if nested["status"] != "ok":
			return nested
	object.set(property, resource)
	if object.get(property) != resource:
		return _refuse("value_type_mismatch", index, property)
	return {"status": "ok"}


# --- tagged values -----------------------------------------------------------
#
# The wire form is the domain contract's tagged value: {"type": ..., "value": ...},
# with the contract's own type spellings (`transform2_d`, `transform3_d`, ...).


func _bad(reason: String) -> Dictionary:
	return {"ok": false, "reason": reason}


func _good(value) -> Dictionary:
	return {"ok": true, "value": value}


func _number(value, key: String) -> float:
	return float(value.get(key, 0.0)) if typeof(value) == TYPE_DICTIONARY else 0.0


func _vec2(value) -> Vector2:
	return Vector2(_number(value, "x"), _number(value, "y"))


func _vec3(value) -> Vector3:
	return Vector3(_number(value, "x"), _number(value, "y"), _number(value, "z"))


func _vec4(value) -> Vector4:
	return Vector4(_number(value, "x"), _number(value, "y"), _number(value, "z"), _number(value, "w"))


func _decode(tagged) -> Dictionary:
	if typeof(tagged) != TYPE_DICTIONARY or not tagged.has("type"):
		return _bad("a value must be a tagged {type, value} object")
	var kind: String = str(tagged["type"])
	var value = tagged.get("value")
	match kind:
		"null":
			return _good(null)
		"bool":
			return _good(bool(value))
		"integer":
			return _good(int(value))
		"float":
			return _good(float(value))
		"string":
			return _good(str(value))
		"string_name":
			return _good(StringName(str(value)))
		"node_path":
			return _good(NodePath(str(value)))
		"vector2":
			return _good(_vec2(value))
		"vector2i":
			return _good(Vector2i(_vec2(value)))
		"rect2":
			return _good(Rect2(_vec2(value.get("position")), _vec2(value.get("size"))))
		"rect2i":
			return _good(Rect2i(Vector2i(_vec2(value.get("position"))), Vector2i(_vec2(value.get("size")))))
		"vector3":
			return _good(_vec3(value))
		"vector3i":
			return _good(Vector3i(_vec3(value)))
		"vector4":
			return _good(_vec4(value))
		"vector4i":
			return _good(Vector4i(_vec4(value)))
		"transform2_d":
			return _good(Transform2D(_vec2(value.get("x")), _vec2(value.get("y")), _vec2(value.get("origin"))))
		"plane":
			return _good(Plane(_vec3(value.get("normal")), float(value.get("distance", 0.0))))
		"quaternion":
			return _good(Quaternion(_number(value, "x"), _number(value, "y"), _number(value, "z"), _number(value, "w")))
		"aabb":
			return _good(AABB(_vec3(value.get("position")), _vec3(value.get("size"))))
		"basis":
			return _good(Basis(_vec3(value.get("x")), _vec3(value.get("y")), _vec3(value.get("z"))))
		"transform3_d":
			var basis = value.get("basis", {})
			return _good(Transform3D(Basis(_vec3(basis.get("x")), _vec3(basis.get("y")), _vec3(basis.get("z"))), _vec3(value.get("origin"))))
		"color":
			return _good(Color(_number(value, "red"), _number(value, "green"), _number(value, "blue"), _number(value, "alpha")))
		"array":
			if typeof(value) != TYPE_ARRAY:
				return _bad("an array value must be a JSON array")
			var items: Array = []
			for item in value:
				var decoded: Dictionary = _decode(item)
				if not decoded["ok"]:
					return decoded
				items.append(decoded["value"])
			return _good(items)
		"dictionary":
			if typeof(value) != TYPE_DICTIONARY:
				return _bad("a dictionary value must be a JSON object")
			var entries: Dictionary = {}
			for key in value.keys():
				var decoded: Dictionary = _decode(value[key])
				if not decoded["ok"]:
					return decoded
				entries[key] = decoded["value"]
			return _good(entries)
		"packed_byte_array":
			return _good(PackedByteArray(value))
		"packed_int32_array":
			return _good(PackedInt32Array(value))
		"packed_int64_array":
			return _good(PackedInt64Array(value))
		"packed_float32_array":
			return _good(PackedFloat32Array(value))
		"packed_float64_array":
			return _good(PackedFloat64Array(value))
		"packed_string_array":
			return _good(PackedStringArray(value))
		"packed_vector2_array":
			var points := PackedVector2Array()
			for item in value:
				points.append(_vec2(item))
			return _good(points)
		"packed_vector3_array":
			var points3 := PackedVector3Array()
			for item in value:
				points3.append(_vec3(item))
			return _good(points3)
		"packed_color_array":
			var colors := PackedColorArray()
			for item in value:
				colors.append(Color(_number(item, "red"), _number(item, "green"), _number(item, "blue"), _number(item, "alpha")))
			return _good(colors)
		"resource":
			var path: String = str(value.get("resource", "")) if typeof(value) == TYPE_DICTIONARY else ""
			if not path.begins_with("res://") or path.contains(".."):
				return _bad("a resource value names a res:// resource")
			var expected_type: String = str(value.get("resource_type", "Resource"))
			var resource: Resource = ResourceLoader.load(path)
			if resource == null:
				return _bad("the resource " + path + " cannot be loaded")
			if expected_type != "" and not resource.is_class(expected_type):
				return _bad(path + " is a " + resource.get_class() + ", not a " + expected_type)
			return _good(resource)
		_:
			return _bad("values of type " + kind + " cannot be written")


# --- ResourceUID operations ------------------------------------------------
#
# The UID value is ALWAYS produced by the engine singleton. There is no
# UID hashing anywhere in godot-mcp: `create_id_for_path` is seeded by the
# engine with the path and the project name, so the same path yields the same
# UID within a project, and `id_to_text` owns the `uid://` spelling.


func _register_uid(request: Dictionary, target: String, artifact_res: String) -> Dictionary:
	var target_res: String = _res(target)
	if not FileAccess.file_exists(target_res):
		return {"status": "refused", "reason": "target_missing", "exit_code": EXIT_REFUSED}

	var uid_id: int = _create_id_for(target_res)
	if ResourceUID.has_id(uid_id):
		var bound: String = ResourceUID.get_id_path(uid_id)
		if bound != target_res:
			return {
				"status": "refused",
				"reason": "uid_conflict",
				"uid": ResourceUID.id_to_text(uid_id),
				"existing_path": bound,
				"requested_path": target_res,
				"exit_code": EXIT_REFUSED,
			}
		ResourceUID.set_id(uid_id, target_res)
	else:
		ResourceUID.add_id(uid_id, target_res)

	var existing: Resource = ResourceLoader.load(target_res, "", ResourceLoader.CACHE_MODE_IGNORE)
	if existing == null:
		return {"status": "failed", "reason": "target_not_loadable", "exit_code": EXIT_FAILED}
	var saved: int = ResourceSaver.save(existing, artifact_res)
	if saved != OK:
		return {"status": "failed", "reason": "save_failed", "exit_code": EXIT_FAILED}
	if not _write_header_uid(artifact_res, ResourceUID.id_to_text(uid_id)):
		return {"status": "failed", "reason": "save_failed", "exit_code": EXIT_FAILED}
	return _ok(artifact_res, target_res, uid_id)


func _query_uid(request: Dictionary) -> Dictionary:
	var query: String = str(request.get("query", ""))
	if query == "":
		return {"status": "refused", "reason": "empty_query", "exit_code": EXIT_USAGE}
	var response := {"status": "ok", "exit_code": EXIT_OK, "query": query}
	if query.begins_with("uid://"):
		var uid_id: int = ResourceUID.text_to_id(query)
		response["uid"] = query
		response["registered"] = "true" if ResourceUID.has_id(uid_id) else "false"
		response["path"] = ResourceUID.get_id_path(uid_id) if ResourceUID.has_id(uid_id) else ""
	else:
		var path_res: String = _res(query)
		var uid_text: String = _uid_text_for(path_res)
		response["path"] = path_res
		response["uid"] = uid_text if uid_text.begins_with("uid://") else ""
		response["registered"] = "true" if uid_text.begins_with("uid://") else "false"
	return response


# --- helpers ---------------------------------------------------------------


# A new UID for `path_res`.
#
# Godot 4.5 and later seed the id from the path and the project
# (`ResourceUID.create_id_for_path`), so one path keeps one UID within a project.
# Godot 4.4 only offers a random `ResourceUID.create_id`; the conflict check in
# `_register_uid` applies to both.
func _create_id_for(path_res: String) -> int:
	if ResourceUID.has_method("create_id_for_path"):
		return int(ResourceUID.call("create_id_for_path", path_res))
	return ResourceUID.create_id()


# The `uid://` spelling of a resource's UID, or the path itself when it has none.
#
# `ResourceUID.path_to_uid` exists from Godot 4.5; Godot 4.4 offers the same fact
# through `ResourceLoader.get_resource_uid`. The method is probed rather than a
# version string compared, so an exact build is adapted by what it exposes.
func _uid_text_for(path_res: String) -> String:
	if ResourceUID.has_method("path_to_uid"):
		return str(ResourceUID.call("path_to_uid", path_res))
	var found: int = ResourceLoader.get_resource_uid(path_res)
	if found == ResourceUID.INVALID_ID:
		return path_res
	return ResourceUID.id_to_text(found)


# The `uid://` value in a text resource's header line, or "" when it has none.
func _header_uid(path_res: String) -> String:
	if not FileAccess.file_exists(path_res):
		return ""
	var file := FileAccess.open(path_res, FileAccess.READ)
	if file == null:
		return ""
	var header: String = file.get_line()
	file.close()
	var start: int = header.find(" uid=\"")
	if start < 0:
		return ""
	start += 6
	var end: int = header.find("\"", start)
	return header.substr(start, end - start) if end > start else ""


# Put `uid_text` into the staged artifact's header line, replacing any UID it
# already carries. The rest of the engine-written bytes are untouched.
func _write_header_uid(artifact_res: String, uid_text: String) -> bool:
	if not uid_text.begins_with("uid://"):
		return true
	var text: String = FileAccess.get_file_as_string(artifact_res)
	var newline: int = text.find("\n")
	if not text.begins_with("[") or newline < 0:
		return false
	var header: String = text.substr(0, newline)
	var existing: int = header.find(" uid=\"")
	if existing >= 0:
		var close: int = header.find("\"", existing + 6)
		header = header.substr(0, existing) + header.substr(close + 1)
	if not header.ends_with("]"):
		return false
	header = header.substr(0, header.length() - 1) + " uid=\"" + uid_text + "\"]"
	var file := FileAccess.open(artifact_res, FileAccess.WRITE)
	if file == null:
		return false
	file.store_string(header + text.substr(newline))
	file.close()
	return true


# Stage the target's own UID into the artifact, then answer with it.
func _ok_carrying_uid(artifact_res: String, target_res: String) -> Dictionary:
	var uid_text: String = _header_uid(target_res)
	if uid_text == "":
		uid_text = _uid_text_for(target_res)
	if uid_text.begins_with("uid://") and not _write_header_uid(artifact_res, uid_text):
		return {"status": "failed", "reason": "save_failed", "exit_code": EXIT_FAILED}
	var uid_id: int = ResourceUID.text_to_id(uid_text) if uid_text.begins_with("uid://") else ResourceUID.INVALID_ID
	return _ok(artifact_res, target_res, uid_id)


func _ok(artifact_res: String, target_res: String, uid_id: int) -> Dictionary:
	return {
		"status": "ok",
		"exit_code": EXIT_OK,
		"artifact": artifact_res,
		"target": target_res,
		"uid": ResourceUID.id_to_text(uid_id) if uid_id != ResourceUID.INVALID_ID else "",
	}


func _build_tree(request: Dictionary) -> Node:
	var root_type: String = str(request.get("root_type", "Node"))
	var root_name: String = str(request.get("root_name", "Root"))
	var root: Node = ClassDB.instantiate(root_type) if _is_node_class(root_type) else Node.new()
	root.name = root_name
	var child_scene: String = str(request.get("child_scene", ""))
	if child_scene != "":
		var packed: PackedScene = ResourceLoader.load(_res(child_scene))
		if packed != null:
			var child: Node = packed.instantiate()
			child.name = str(request.get("child_name", "Child"))
			root.add_child(child)
			child.owner = root
	return root


func _parse_options(args: PackedStringArray) -> Dictionary:
	var options := {}
	var index := 0
	while index < args.size():
		var token: String = args[index]
		if token.begins_with("--") and index + 1 < args.size():
			options[token.substr(2)] = args[index + 1]
			index += 2
		else:
			index += 1
	return options


func _res(path: String) -> String:
	if path.begins_with("res://") or path.begins_with("user://"):
		return path
	return "res://" + path.lstrip("/")


func _read_pairs(path: String) -> Dictionary:
	var file := FileAccess.open(path, FileAccess.READ)
	if file == null:
		return {}
	var pairs := {}
	while not file.eof_reached():
		var line: String = file.get_line()
		var split: int = line.find("=")
		if split > 0:
			pairs[line.substr(0, split)] = line.substr(split + 1)
	file.close()
	return pairs


func _write_pairs(path: String, pairs: Dictionary) -> bool:
	var file := FileAccess.open(path, FileAccess.WRITE)
	if file == null:
		return false
	var keys: Array = pairs.keys()
	keys.sort()
	for key in keys:
		file.store_line("%s=%s" % [str(key), str(pairs[key])])
	file.close()
	return true
