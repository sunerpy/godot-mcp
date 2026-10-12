@tool
extends RefCounted
## Class metadata as the Godot build hosting the editor and the project define
## it: an engine class from ClassDB, or a project script class (`class_name`)
## from the project's global class list. Either way: the inheritance chain, the
## class's own properties (with their default values where the contract can
## spell them), signals and methods, and an engine class's enums. Inherited
## members are not repeated; ask for the parent class.

const VariantCodec = preload("variant_codec.gd")

const MAX_MEMBERS := 512
const SKIPPED_USAGE := PROPERTY_USAGE_CATEGORY | PROPERTY_USAGE_GROUP | PROPERTY_USAGE_SUBGROUP


static func _arguments(list: Array) -> Array:
	var arguments: Array = []
	for argument in list:
		arguments.append({
			"name": str(argument.get("name", "")),
			"type": type_string(int(argument.get("type", TYPE_NIL))),
			"class_name": str(argument.get("class_name", "")),
		})
	return arguments


static func _script_class(name: String) -> Dictionary:
	for entry in ProjectSettings.get_global_class_list():
		if str(entry.get("class", "")) == name:
			return entry
	return {}


static func _property_entry(property: Dictionary, default_value) -> Dictionary:
	var entry := {
		"name": str(property["name"]),
		"type": type_string(int(property.get("type", TYPE_NIL))),
		"class_name": str(property.get("class_name", "")),
		"hint": int(property.get("hint", 0)),
		"hint_string": str(property.get("hint_string", "")),
		"stored": (int(property.get("usage", 0)) & PROPERTY_USAGE_STORAGE) != 0,
		"default": null,
	}
	var encoded: Dictionary = VariantCodec.encode(default_value)
	if encoded["ok"]:
		entry["default"] = encoded["value"]
	return entry


static func _describe_script(name: String, entry: Dictionary) -> Dictionary:
	var script = load(str(entry["path"]))
	if script == null or not (script is Script):
		return {"ok": false, "error_kind": "unavailable", "message": "the script class %s cannot be loaded" % name}
	var chain: Array = []
	var base: String = str(entry.get("base", ""))
	while base != "":
		chain.append(base)
		var parent: Dictionary = _script_class(base)
		base = str(parent.get("base", "")) if not parent.is_empty() else ClassDB.get_parent_class(base)
	var properties: Array = []
	for property in script.get_script_property_list():
		var usage: int = int(property.get("usage", 0))
		if usage & SKIPPED_USAGE or properties.size() >= MAX_MEMBERS:
			continue
		properties.append(_property_entry(property, script.get_property_default_value(property["name"])))
	var signals: Array = []
	for signal_info in script.get_script_signal_list():
		signals.append({"name": str(signal_info["name"]), "arguments": _arguments(signal_info.get("args", []))})
	var methods: Array = []
	for method in script.get_script_method_list():
		if methods.size() >= MAX_MEMBERS:
			break
		var returned: Dictionary = method.get("return", {})
		methods.append({
			"name": str(method["name"]),
			"arguments": _arguments(method.get("args", [])),
			"return_type": type_string(int(returned.get("type", TYPE_NIL))),
			"virtual": false,
		})
	return {"ok": true, "payload": {
		"class": name,
		"script": str(entry["path"]),
		"inherits": chain,
		"instantiable": script.can_instantiate(),
		"properties": properties,
		"signals": signals,
		"methods": methods,
		"enums": {},
		"truncated": properties.size() >= MAX_MEMBERS or methods.size() >= MAX_MEMBERS,
	}}


static func describe(name: String) -> Dictionary:
	if name != "" and not ClassDB.class_exists(name):
		var script_class: Dictionary = _script_class(name)
		if not script_class.is_empty():
			return _describe_script(name, script_class)
	if name == "" or not ClassDB.class_exists(name):
		return {"ok": false, "error_kind": "not_found", "message": "no engine or project class is named %s" % name}
	var chain: Array = []
	var current: String = name
	while current != "":
		chain.append(current)
		current = ClassDB.get_parent_class(current)

	var truncated := false
	var properties: Array = []
	for property in ClassDB.class_get_property_list(name, true):
		if int(property.get("usage", 0)) & SKIPPED_USAGE:
			continue
		if properties.size() >= MAX_MEMBERS:
			truncated = true
			break
		properties.append(_property_entry(property, ClassDB.class_get_property_default_value(name, property["name"])))

	var signals: Array = []
	for signal_info in ClassDB.class_get_signal_list(name, true):
		if signals.size() >= MAX_MEMBERS:
			truncated = true
			break
		signals.append({"name": str(signal_info["name"]), "arguments": _arguments(signal_info.get("args", []))})

	var methods: Array = []
	for method in ClassDB.class_get_method_list(name, true):
		if methods.size() >= MAX_MEMBERS:
			truncated = true
			break
		var returned: Dictionary = method.get("return", {})
		methods.append({
			"name": str(method["name"]),
			"arguments": _arguments(method.get("args", [])),
			"return_type": type_string(int(returned.get("type", TYPE_NIL))),
			"virtual": (int(method.get("flags", 0)) & METHOD_FLAG_VIRTUAL) != 0,
		})

	var enums: Dictionary = {}
	for enum_name in ClassDB.class_get_enum_list(name, true):
		var constants: Dictionary = {}
		for constant in ClassDB.class_get_enum_constants(name, enum_name, true):
			constants[constant] = ClassDB.class_get_integer_constant(name, constant)
		enums[enum_name] = constants

	return {"ok": true, "payload": {
		"class": name,
		"inherits": chain.slice(1),
		"instantiable": ClassDB.can_instantiate(name),
		"properties": properties,
		"signals": signals,
		"methods": methods,
		"enums": enums,
		"truncated": truncated,
	}}
