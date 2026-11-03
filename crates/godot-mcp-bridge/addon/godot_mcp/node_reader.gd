@tool
extends RefCounted
## Reads of one node and of the class catalogue: `editor.node` and
## `editor.types.search`. Neither changes editor state: nothing is opened,
## selected, marked unsaved or added to an undo history.

const MAX_PROPERTIES := 512
const MAX_NAMED := 256
const MAX_CHILDREN := 256
const MAX_SIGNALS := 256
const MAX_CONNECTIONS := 64
const MAX_HINT := 512
const MAX_CLASSES := 500
## One answer stays far below the bridge's 4 MiB frame: past this budget a
## property is named and typed but its value is omitted.
const BUDGET_BYTES := 1048576
const SKIPPED_USAGE := PROPERTY_USAGE_CATEGORY | PROPERTY_USAGE_GROUP | PROPERTY_USAGE_SUBGROUP
const USAGE_NAMES := {
	PROPERTY_USAGE_STORAGE: "storage",
	PROPERTY_USAGE_EDITOR: "editor",
	PROPERTY_USAGE_INTERNAL: "internal",
	PROPERTY_USAGE_READ_ONLY: "read_only",
	PROPERTY_USAGE_SCRIPT_VARIABLE: "script_variable",
}
const CONNECT_FLAG_NAMES := {
	CONNECT_DEFERRED: "deferred",
	CONNECT_PERSIST: "persist",
	CONNECT_ONE_SHOT: "one_shot",
	CONNECT_REFERENCE_COUNTED: "reference_counted",
}


static func _fail(kind: String, message: String) -> Dictionary:
	return {"ok": false, "error_kind": kind, "message": message, "mutation_status": "not_started"}


static func _ok(payload: Dictionary) -> Dictionary:
	return {"ok": true, "payload": payload, "mutation_status": "not_started"}


## `editor.node`: one node of the scene being edited, as the editor holds it.
static func node(handlers, payload: Dictionary) -> Dictionary:
	var found: Dictionary = handlers.open_scene_root(str(payload.get("scene", "")))
	if not found.get("ok", false):
		return found
	var root: Node = found["root"]
	var spelled := str(payload.get("node", "."))
	if spelled.begins_with("/") or spelled.split("/").has(".."):
		return _fail("invalid_input", "`%s` is not a node path relative to the scene root" % spelled)
	var target: Node = root if spelled == "." or spelled == "" else root.get_node_or_null(NodePath(spelled))
	if target == null:
		return _fail("not_found", "%s has no node at %s" % [root.scene_file_path, spelled])
	var named = payload.get("properties")
	if named != null and typeof(named) != TYPE_ARRAY:
		return _fail("invalid_input", "`properties` must be a list of names")

	var script = target.get_script()
	var groups: Array = []
	for group in target.get_groups():
		if not String(group).begins_with("_"):
			groups.append(String(group))
	groups.sort()
	var truncated := false
	var children: Array = []
	for child in target.get_children():
		if children.size() >= MAX_CHILDREN:
			truncated = true
			break
		children.append({"name": String(child.name), "class": child.get_class()})
	var described := {
		"path": handlers.node_path(root, target),
		"class": target.get_class(),
		"script": script.resource_path if script != null and script.resource_path != "" else null,
		"instance": target.scene_file_path if target != root and target.scene_file_path != "" else null,
		"owned": target == root or target.owner == root,
		"groups": groups,
		"children": children,
		"child_count": target.get_child_count(),
	}

	var properties: Array = []
	var missing: Array = []
	var spent := 0
	var listed: Dictionary = {}
	for property in target.get_property_list():
		listed[String(property["name"])] = property
	var names: Array = []
	if named != null:
		for name in named:
			if names.size() >= MAX_NAMED:
				truncated = true
				break
			if listed.has(str(name)):
				names.append(str(name))
			else:
				missing.append(str(name))
	else:
		for property in target.get_property_list():
			var usage: int = int(property.get("usage", 0))
			if usage & SKIPPED_USAGE or not (usage & (PROPERTY_USAGE_STORAGE | PROPERTY_USAGE_EDITOR)):
				continue
			if names.size() >= MAX_PROPERTIES:
				truncated = true
				break
			names.append(String(property["name"]))
	for name in names:
		var property: Dictionary = listed[name]
		var entry := {
			"name": name,
			"type": type_string(int(property.get("type", TYPE_NIL))),
			"class_name": String(property.get("class_name", "")),
			"hint": int(property.get("hint", 0)),
			"hint_string": str(property.get("hint_string", "")).substr(0, MAX_HINT),
			"usage": _usage(int(property.get("usage", 0))),
		}
		if spent > BUDGET_BYTES:
			entry["omitted"] = "budget"
			truncated = true
		else:
			var value = target.get(name)
			entry["value"] = handlers._describe_value(value)
			entry["changed"] = _changed(target, name, value)
			spent += JSON.stringify(entry).length()
		properties.append(entry)

	var signals: Array = []
	for signal_info in target.get_signal_list():
		if signals.size() >= MAX_SIGNALS:
			truncated = true
			break
		var arguments: Array = []
		for argument in signal_info.get("args", []):
			arguments.append({"name": str(argument.get("name", "")), "type": type_string(int(argument.get("type", TYPE_NIL)))})
		var connections: Array = []
		for connection in target.get_signal_connection_list(String(signal_info["name"])):
			if connections.size() >= MAX_CONNECTIONS:
				truncated = true
				break
			var callable: Callable = connection["callable"]
			var receiver = callable.get_object()
			var to = null
			if receiver is Node and (receiver == root or root.is_ancestor_of(receiver)):
				to = handlers.node_path(root, receiver)
			connections.append({
				"to": to,
				"to_class": receiver.get_class() if receiver != null else null,
				"method": String(callable.get_method()),
				"flags": _flags(int(connection.get("flags", 0))),
			})
		signals.append({"name": String(signal_info["name"]), "arguments": arguments, "connections": connections})

	return _ok({
		"scene": root.scene_file_path,
		"digest": handlers.edit_digest(root),
		"node": described,
		"properties": properties,
		"missing_properties": missing,
		"signals": signals,
		"truncated": truncated,
	})


static func _usage(usage: int) -> Array:
	var names: Array = []
	for flag in USAGE_NAMES:
		if usage & flag:
			names.append(USAGE_NAMES[flag])
	return names


static func _flags(flags: int) -> Array:
	var names: Array = []
	for flag in CONNECT_FLAG_NAMES:
		if flags & flag:
			names.append(CONNECT_FLAG_NAMES[flag])
	return names


## Whether `value` differs from the property's default, or null when the
## default cannot be known: a class property's default comes from ClassDB, a
## script variable's from its script.
static func _changed(target: Node, name: String, value):
	var script = target.get_script()
	if script != null:
		for variable in script.get_script_property_list():
			if String(variable.get("name", "")) == name:
				return value != script.get_property_default_value(name)
	for property in ClassDB.class_get_property_list(target.get_class()):
		if String(property.get("name", "")) == name:
			return value != ClassDB.class_get_property_default_value(target.get_class(), name)
	return null


## `editor.types.search`: the engine and script classes whose name contains
## `query`, optionally only those inheriting `base` or only instantiable ones.
static func search(payload: Dictionary) -> Dictionary:
	var query := str(payload.get("query", "")).to_lower()
	var base = payload.get("base")
	var instantiable = payload.get("instantiable")
	var limit := clampi(int(payload.get("limit", 100)), 1, MAX_CLASSES)
	var scripts: Dictionary = {}
	for entry in ProjectSettings.get_global_class_list():
		scripts[String(entry.get("class", ""))] = entry
	if base != null and not ClassDB.class_exists(str(base)) and not scripts.has(str(base)):
		return _fail("not_found", "no engine or script class is named %s" % str(base))
	var found: Array = []
	for name in ClassDB.get_class_list():
		if query != "" and not String(name).to_lower().contains(query):
			continue
		if base != null and not (ClassDB.class_exists(str(base)) and ClassDB.is_parent_class(name, str(base))):
			continue
		var can := ClassDB.can_instantiate(name)
		if instantiable != null and bool(instantiable) != can:
			continue
		found.append({"name": String(name), "kind": "engine", "parent": String(ClassDB.get_parent_class(name)), "instantiable": can, "path": null})
	for name in scripts:
		if query != "" and not String(name).to_lower().contains(query):
			continue
		if base != null and not _inherits(scripts, name, str(base)):
			continue
		var entry: Dictionary = scripts[name]
		var can := not bool(entry.get("is_abstract", false))
		if instantiable != null and bool(instantiable) != can:
			continue
		found.append({"name": name, "kind": "script", "parent": String(entry.get("base", "")), "instantiable": can, "path": String(entry.get("path", ""))})
	found.sort_custom(func(a, b): return a["name"] < b["name"])
	return _ok({
		"classes": found.slice(0, limit),
		"total": found.size(),
		"truncated": found.size() > limit,
	})


## Whether script class `name` is `base` or inherits it, through other script
## classes and then the engine class they extend.
static func _inherits(scripts: Dictionary, name: String, base: String) -> bool:
	var current := name
	for _step in range(64):
		if current == base:
			return true
		if not scripts.has(current):
			return ClassDB.class_exists(current) and ClassDB.class_exists(base) and ClassDB.is_parent_class(current, base)
		current = String(scripts[current].get("base", ""))
	return false
