@tool
extends RefCounted
## `call_method`: an editing call on an object the edit owns.
##
## Some edits have no property to set: painting a TileMapLayer, keying an
## Animation, adding points to a Curve. A batch makes them by calling one of
## the methods below on a node of the scene, or on a resource the scene embeds
## that a `resource` selector picks: a node property, an animation or an
## animation library of an AnimationPlayer or AnimationTree. A resource saved
## in its own file is refused: editing it here would change that file too.
##
## Every listed method changes only its receiver's stored state, so a call is
## undone by putting that state back: the stored properties the call changed
## are written back, or, for a class whose calls add or remove stored
## properties, the receiver is emptied the way CLEARING says and every stored
## property written back. A class in SETTLE brings its stored properties up to
## date with that method after the call and after each undo and redo.
## Arguments are checked against the method's engine signature before the
## call.

const VariantCodec = preload("variant_codec.gd")

# The editing methods, as crates/godot-mcp-domain/src/editing_methods.rs lists
# them; a test holds this copy to that table. Change the table, not this copy.
const EDITING_METHODS := {
	"TileMapLayer": ["set_cell", "erase_cell", "clear", "set_cells_terrain_connect", "set_cells_terrain_path"],
	"GridMap": ["set_cell_item", "clear"],
	"Line2D": ["add_point", "remove_point", "set_point_position", "clear_points"],
	"AnimationMixer": ["add_animation_library", "remove_animation_library", "rename_animation_library"],
	"AnimationLibrary": ["add_animation", "remove_animation", "rename_animation"],
	"Animation": ["add_track", "remove_track", "track_set_path", "track_set_enabled", "track_set_interpolation_type", "track_set_interpolation_loop_wrap", "value_track_set_update_mode", "track_insert_key", "track_remove_key", "track_remove_key_at_time", "track_set_key_value", "track_set_key_time", "track_set_key_transition", "set_length", "set_loop_mode", "set_step"],
	"SpriteFrames": ["add_animation", "remove_animation", "rename_animation", "add_frame", "remove_frame", "set_frame", "clear", "set_animation_speed", "set_animation_loop"],
	"Curve": ["add_point", "remove_point", "set_point_value", "set_point_offset", "set_point_left_tangent", "set_point_right_tangent", "clear_points"],
	"Curve2D": ["add_point", "remove_point", "set_point_position", "set_point_in", "set_point_out", "clear_points"],
	"Curve3D": ["add_point", "remove_point", "set_point_position", "set_point_in", "set_point_out", "set_point_tilt", "clear_points"],
	"Gradient": ["add_point", "remove_point", "set_offset", "set_color"],
	"Theme": ["set_color", "set_constant", "set_font_size", "set_font", "set_icon", "set_stylebox", "clear_color", "clear_constant", "clear_font_size", "clear_font", "clear_icon", "clear_stylebox"],
}
const CLEARING := {"TileMapLayer": ["clear"], "AnimationMixer": ["get_animation_library_list", "remove_animation_library"], "Animation": ["clear"], "Theme": ["clear"]}
const SETTLE := {"TileMapLayer": "update_internals"}
const NEW_RESOURCE_CLASSES := ["Animation", "AnimationLibrary", "Curve", "Curve2D", "Curve3D", "Gradient", "SpriteFrames", "StyleBoxFlat", "Theme"]
const MAX_CALL_ARGUMENTS := 16


static func _refusal(reason: String, detail: String) -> Dictionary:
	return {"ok": false, "reason": reason, "detail": detail}


## The table's class `object` belongs to for `method`, or "" when none lists it.
static func listed_class(object: Object, method: String) -> String:
	for listed in EDITING_METHODS:
		if ClassDB.is_parent_class(object.get_class(), listed) and EDITING_METHODS[listed].has(method):
			return listed
	return ""


## Whether `resource` belongs to the file at `owner_path`: unsaved, or one of
## the sub-resources that file embeds.
static func embedded(resource: Resource, owner_path: String) -> bool:
	var path := resource.resource_path
	return path == "" or path.begins_with(owner_path + "::")


## The object a scene's `call_method` calls: `node`, or the resource of `node`
## the `selector` names, which the scene file at `owner_path` must embed.
static func receiver(node: Node, selector, owner_path: String) -> Dictionary:
	if selector == null:
		return {"ok": true, "object": node}
	if typeof(selector) != TYPE_DICTIONARY or selector.size() != 1:
		return _refusal("invalid_selector", "`resource` names one of property, animation, library")
	var kind := str(selector.keys()[0])
	var name := str(selector[kind])
	var resource = null
	match kind:
		"property":
			var listed := false
			for property in node.get_property_list():
				if str(property["name"]) == name:
					listed = true
					break
			if not listed:
				return _refusal("unknown_property", name)
			resource = node.get(name)
		"animation":
			if not (node is AnimationMixer) or not node.has_animation(name):
				return _refusal("unknown_animation", name)
			resource = node.get_animation(name)
		"library":
			if not (node is AnimationMixer) or not node.has_animation_library(name):
				return _refusal("unknown_animation_library", name)
			resource = node.get_animation_library(name)
		_:
			return _refusal("invalid_selector", "`resource` names one of property, animation, library")
	if not (resource is Resource):
		return _refusal("not_a_resource", "%s %s holds no resource" % [kind, name])
	if not embedded(resource, owner_path):
		return _refusal("external_resource", "%s is the file %s; edit it with resource.batch.apply" % [name, resource.resource_path])
	return {"ok": true, "object": resource}


## One argument: a new embedded resource of a listed class, or a tagged value.
static func decode_argument(tagged) -> Dictionary:
	if typeof(tagged) == TYPE_DICTIONARY and str(tagged.get("type", "")) == "new_resource":
		var value = tagged.get("value")
		if tagged.size() != 2 or typeof(value) != TYPE_DICTIONARY or value.size() != 1:
			return {"ok": false, "reason": "a new resource is {type: new_resource, value: {class}}"}
		var resource_class := str(value.get("class", ""))
		if not NEW_RESOURCE_CLASSES.has(resource_class) or not ClassDB.can_instantiate(resource_class):
			return {"ok": false, "reason": "a new resource is one of " + ", ".join(NEW_RESOURCE_CLASSES)}
		return {"ok": true, "value": ClassDB.instantiate(resource_class)}
	return VariantCodec.decode(tagged)


## Why `arguments` do not fit `method`'s engine signature on `object`, or "".
static func signature_problem(object: Object, method: String, arguments: Array) -> String:
	for info in ClassDB.class_get_method_list(object.get_class()):
		if str(info["name"]) != method:
			continue
		var parameters: Array = info.get("args", [])
		var defaults: Array = info.get("default_args", [])
		var least := parameters.size() - defaults.size()
		if arguments.size() < least or arguments.size() > parameters.size():
			return "%s.%s takes %d to %d arguments, not %d" % [object.get_class(), method, least, parameters.size(), arguments.size()]
		for index in arguments.size():
			var expected := int(parameters[index].get("type", TYPE_NIL))
			var argument = arguments[index]
			var actual := typeof(argument)
			if expected == TYPE_NIL or actual == expected and expected != TYPE_OBJECT:
				continue
			if expected == TYPE_OBJECT:
				var wanted := str(parameters[index].get("class_name", ""))
				if actual == TYPE_NIL or actual == TYPE_OBJECT and (wanted == "" or argument.is_class(wanted)):
					continue
				return "argument %d of %s must be a %s" % [index, method, wanted if wanted != "" else "object"]
			if expected == TYPE_FLOAT and actual == TYPE_INT:
				continue
			if expected == TYPE_INT and actual == TYPE_FLOAT and float(argument) == floor(float(argument)):
				continue
			if [TYPE_STRING, TYPE_STRING_NAME].has(expected) and [TYPE_STRING, TYPE_STRING_NAME].has(actual):
				continue
			if expected == TYPE_NODE_PATH and [TYPE_STRING, TYPE_STRING_NAME].has(actual):
				continue
			return "argument %d of %s must be %s, not %s" % [index, method, type_string(expected), type_string(actual)]
		return ""
	return "%s has no method %s" % [object.get_class(), method]


## Every stored property of `object`, with containers copied so that later
## changes to the object do not change the copy.
static func stored(object: Object) -> Dictionary:
	var values := {}
	for property in object.get_property_list():
		if not (int(property.get("usage", 0)) & PROPERTY_USAGE_STORAGE):
			continue
		var name := str(property["name"])
		if name == "script" or name == "resource_path":
			continue
		var value = object.get(name)
		if value is Array or value is Dictionary:
			value = value.duplicate(true)
		values[name] = value
	return values


## The calls that empty `receiver` as it is now, by its class's CLEARING
## `recipe`: [method] is one call, [list, remove] removes every name that
## `list` returns.
static func clearing_calls(receiver: Object, recipe: Array) -> Array:
	if recipe.size() == 1:
		return [[receiver, recipe[0], []]]
	var calls: Array = []
	for name in receiver.call(recipe[0]):
		calls.append([receiver, recipe[1], [name]])
	return calls


## The calls that write `values` back onto a receiver that was just emptied.
## An empty node path is not written: emptying recreates every node path a
## listed call can change as an empty one, and Godot 4.4.1 and 4.5.1 report
## an engine error whenever an animation track's path is set to an empty one.
static func write_back(receiver: Object, values: Dictionary) -> Array:
	var calls: Array = []
	for name in values:
		var value = values[name]
		if value is NodePath and value.is_empty():
			continue
		calls.append([receiver, "set", [name, value]])
	return calls


## Call `method` of the table's class `listed` on `receiver`, and work out how
## to redo and undo it from the receiver's stored properties before and after.
## Answers {"ok": true, "value", "do", "undo"}, or {"ok": false, "unresolved",
## "detail"} when the call cannot be undone; the receiver is then put back,
## and `unresolved` says that putting it back did not work.
static func call_recorded(receiver: Object, listed: String, method: String, arguments: Array) -> Dictionary:
	var recipe: Array = CLEARING.get(listed, [])
	var settle: String = SETTLE.get(listed, "")
	var before: Dictionary = stored(receiver)
	var clear_before: Array = clearing_calls(receiver, recipe) if not recipe.is_empty() else []
	var value = receiver.callv(method, arguments)
	if settle != "":
		receiver.call(settle)
	var after: Dictionary = stored(receiver)
	var do_calls: Array = []
	var undo_calls: Array = []
	if not recipe.is_empty():
		var settled: Array = [[receiver, settle, []]] if settle != "" else []
		do_calls = clear_before + settled + write_back(receiver, after)
		undo_calls = clearing_calls(receiver, recipe) + settled + write_back(receiver, before)
	elif before.keys() != after.keys():
		# Writing back cannot remove a stored property the call added.
		for name in before:
			receiver.set(name, before[name])
		if settle != "":
			receiver.call(settle)
		var detail := "%s.%s changed which properties the %s stores" % [listed, method, receiver.get_class()]
		if stored(receiver) != before:
			return {"ok": false, "unresolved": true, "detail": detail + ", and they could not be put back"}
		return {"ok": false, "unresolved": false, "detail": detail + "; it cannot be undone"}
	else:
		for name in after:
			if typeof(after[name]) != typeof(before[name]) or after[name] != before[name]:
				do_calls.append([receiver, "set", [name, after[name]]])
				undo_calls.append([receiver, "set", [name, before[name]]])
	if settle != "" and not do_calls.is_empty():
		do_calls.append([receiver, settle, []])
		undo_calls.append([receiver, settle, []])
	return {"ok": true, "value": value, "do": do_calls, "undo": undo_calls}


## The tagged spelling of a call's result, or null when it returned nothing.
static func encode_result(value):
	if value == null:
		return null
	var encoded: Dictionary = VariantCodec.encode(value)
	if encoded["ok"]:
		return encoded["value"]
	return {"type": "unrepresentable", "value": {"variant_type": type_string(typeof(value))}}
